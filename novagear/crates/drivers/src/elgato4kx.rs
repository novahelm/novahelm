//! The Elgato 4K X's vendor channel, used for one setting: its USB speed mode.
//!
//! On a Mac the card connects at 5 Gb/s until its "10 Gbps mode" is switched on. The two
//! modes are two USB identities: `0fd9:009c` (5 Gbps mode) and `0fd9:009b` (10 Gbps mode).
//! The switch is stored on the card, and changing it makes the card drop off the bus and
//! come back under the other product id.
//!
//! Commands travel through the UVC extension unit (unit 4 on interface 0) as plain class
//! control requests on endpoint 0, so nothing is claimed and macOS's own camera driver keeps
//! the interface:
//!
//! 1. `SET_CUR` selector 2: the request length, two bytes, little endian.
//! 2. `SET_CUR` selector 1: the request.
//! 3. `GET_CUR` selector 2: the reply length.
//! 4. `GET_CUR` selector 1, that many bytes: the reply.
//!
//! A request is `a1`, a length byte (the bytes between it and the checksum), `00 00`, the
//! command, `00 00 00`, the arguments, and a checksum byte that makes all bytes sum to zero
//! modulo 256. A reply has the same shape; a long one sets the length byte's top bit and
//! carries a two-byte length.
//!
//! Decoded on 2026-11-16 by tracing Elgato Studio 1.1.0 on one card (firmware
//! `bcdDevice 0x0130`), and checked by switching that card both ways with this driver. The
//! read needs no status poll first (Elgato Studio sends one, command `0x67`). Writing is
//! refused unless the card's reply to the read looks exactly like that card's did.

use crate::catalog::ELGATO;
use anyhow::{Context, Result, bail, ensure};
use novagear_core::{UsbId, UsbSpeedMode};
use std::time::Duration;

/// 5 Gbps mode, the default on a Mac.
pub const MODE_5G: UsbId = UsbId {
    vid: ELGATO,
    pid: 0x009c,
};
/// 10 Gbps mode.
pub const MODE_10G: UsbId = UsbId {
    vid: ELGATO,
    pid: 0x009b,
};
/// The firmware the protocol was decoded on, as bcdDevice.
pub const DECODED_ON: u16 = 0x0130;

const INTERFACE: u16 = 0;
const UNIT: u16 = 4;
const SEL_DATA: u8 = 1;
const SEL_LEN: u8 = 2;
const UVC_SET_CUR: u8 = 0x01;
const UVC_GET_CUR: u8 = 0x81;
const TIMEOUT: Duration = Duration::from_secs(1);

const HEADER: u8 = 0xa1;
const CMD_GET_USB_MODE: u8 = 0x8d;
const CMD_SET_USB_MODE: u8 = 0x8e;
/// The read reply's exact length on the card it was decoded on.
const USB_MODE_REPLY_LEN: usize = 137;
/// The card's byte for each mode, in the read reply and the set request.
const WIRE_5G: u8 = 0x00;
const WIRE_10G: u8 = 0x03;

/// Whether a USB id is the 4K X, in either mode.
pub fn is_4kx(usb: UsbId) -> bool {
    usb == MODE_5G || usb == MODE_10G
}

/// The mode a 4K X is in, from the product id it shows.
pub fn mode_of(usb: UsbId) -> Option<UsbSpeedMode> {
    if usb == MODE_5G {
        Some(UsbSpeedMode::Gbps5)
    } else if usb == MODE_10G {
        Some(UsbSpeedMode::Gbps10)
    } else {
        None
    }
}

/// The product id the card shows in a mode.
pub fn usb_for(mode: UsbSpeedMode) -> UsbId {
    match mode {
        UsbSpeedMode::Gbps5 => MODE_5G,
        UsbSpeedMode::Gbps10 => MODE_10G,
    }
}

fn wire(mode: UsbSpeedMode) -> u8 {
    match mode {
        UsbSpeedMode::Gbps5 => WIRE_5G,
        UsbSpeedMode::Gbps10 => WIRE_10G,
    }
}

fn checksum(bytes: &[u8]) -> u8 {
    bytes
        .iter()
        .fold(0u8, |a, b| a.wrapping_add(*b))
        .wrapping_neg()
}

/// Builds one request: header, length, `00 00`, command, `00 00 00`, arguments, checksum.
pub fn request(command: u8, args: &[u8]) -> Vec<u8> {
    let len = 6 + args.len();
    assert!(len < 0x80, "long requests are not used");
    let mut r = vec![HEADER, len as u8, 0, 0, command, 0, 0, 0];
    r.extend_from_slice(args);
    r.push(checksum(&r));
    r
}

/// The request that reads the USB speed mode.
pub fn get_usb_mode_request() -> Vec<u8> {
    request(CMD_GET_USB_MODE, &[])
}

/// The request that sets the USB speed mode.
pub fn set_usb_mode_request(mode: UsbSpeedMode) -> Vec<u8> {
    request(CMD_SET_USB_MODE, &[1, 0, 0, 0, wire(mode), 0, 0, 0])
}

/// Checks a reply's header, length and checksum and returns its body (the bytes between
/// the length and the checksum).
pub fn parse_reply(reply: &[u8]) -> Result<&[u8]> {
    ensure!(reply.len() >= 3, "reply too short ({} bytes)", reply.len());
    ensure!(
        reply[0] == HEADER,
        "reply starts {:#04x}, not {HEADER:#04x}",
        reply[0]
    );
    let (start, len) = if reply[1] & 0x80 != 0 {
        (
            3,
            (usize::from(reply[1] & 0x7f) << 8) | usize::from(reply[2]),
        )
    } else {
        (2, usize::from(reply[1]))
    };
    ensure!(
        reply.len() == start + len + 1,
        "reply is {} bytes but says {}",
        reply.len(),
        start + len + 1
    );
    ensure!(checksum(reply) == 0, "reply checksum does not add up");
    Ok(&reply[start..start + len])
}

/// Reads the mode out of a reply to [`get_usb_mode_request`]. Strict on purpose: anything
/// that differs from the decoded layout is an error, and an error here stops a write.
pub fn parse_usb_mode_reply(reply: &[u8]) -> Result<UsbSpeedMode> {
    ensure!(
        reply.len() == USB_MODE_REPLY_LEN,
        "the card's reply is {} bytes, not the {USB_MODE_REPLY_LEN} NovaGear knows",
        reply.len()
    );
    let body = parse_reply(reply)?;
    ensure!(
        body[..5] == [0, 1, 0, 0, 0] && body[6..9] == [0, 0, 0],
        "the card's reply is laid out differently from the one NovaGear knows"
    );
    match body[5] {
        WIRE_5G => Ok(UsbSpeedMode::Gbps5),
        WIRE_10G => Ok(UsbSpeedMode::Gbps10),
        m => bail!("the card reports an unknown USB mode {m:#04x}"),
    }
}

/// The two control requests the transport needs, so the protocol can be tested without a
/// card.
pub trait Channel {
    fn set_cur(&mut self, selector: u8, data: &[u8]) -> Result<()>;
    fn get_cur(&mut self, selector: u8, len: usize) -> Result<Vec<u8>>;
}

/// Sends one request and returns the reply, checked by [`parse_reply`].
pub fn transact(ch: &mut dyn Channel, req: &[u8]) -> Result<Vec<u8>> {
    let len = u16::try_from(req.len()).context("request too long")?;
    ch.set_cur(SEL_LEN, &len.to_le_bytes())
        .context("sending the request length")?;
    ch.set_cur(SEL_DATA, req).context("sending the request")?;
    let n = ch.get_cur(SEL_LEN, 2).context("reading the reply length")?;
    ensure!(
        n.len() == 2,
        "the reply length came back as {} bytes",
        n.len()
    );
    let n = usize::from(u16::from_le_bytes([n[0], n[1]]));
    ensure!(n > 0, "the card gave no reply");
    let reply = ch.get_cur(SEL_DATA, n).context("reading the reply")?;
    ensure!(
        reply.len() == n,
        "the reply came back as {} bytes, not {n}",
        reply.len()
    );
    parse_reply(&reply)?;
    Ok(reply)
}

/// Reads the USB speed mode the card has stored.
pub fn get_usb_speed_mode(ch: &mut dyn Channel) -> Result<UsbSpeedMode> {
    parse_usb_mode_reply(&transact(ch, &get_usb_mode_request())?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOutcome {
    /// The card was already in that mode; nothing was written.
    Already,
    /// The card took the new mode and is about to reconnect under the other product id.
    Switching,
}

/// Stores a new USB speed mode. Reads first and refuses to write unless that reply looks
/// exactly like the decoded one. The card reconnects after a successful write.
pub fn set_usb_speed_mode(ch: &mut dyn Channel, mode: UsbSpeedMode) -> Result<SetOutcome> {
    let now = get_usb_speed_mode(ch).context("not switching: the mode check failed")?;
    if now == mode {
        return Ok(SetOutcome::Already);
    }
    let reply = transact(ch, &set_usb_mode_request(mode))?;
    ensure!(
        parse_reply(&reply)?.is_empty(),
        "the card answered the switch with something unexpected"
    );
    Ok(SetOutcome::Switching)
}

/// The vendor channel on a real card, through nusb's device-level control requests.
/// On macOS these go straight to the device without claiming interface 0, so the system
/// camera driver keeps it.
pub struct UsbChannel {
    dev: nusb::Device,
}

impl Channel for UsbChannel {
    fn set_cur(&mut self, selector: u8, data: &[u8]) -> Result<()> {
        let n = control_out(&self.dev, selector, data)?;
        ensure!(n == data.len(), "the card took {n} of {} bytes", data.len());
        Ok(())
    }

    fn get_cur(&mut self, selector: u8, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        let n = control_in(&self.dev, selector, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }
}

fn control(selector: u8, request: u8) -> nusb::transfer::Control {
    nusb::transfer::Control {
        control_type: nusb::transfer::ControlType::Class,
        recipient: nusb::transfer::Recipient::Interface,
        request,
        value: u16::from(selector) << 8,
        index: (UNIT << 8) | INTERFACE,
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "android"))]
fn control_out(dev: &nusb::Device, selector: u8, data: &[u8]) -> Result<usize> {
    Ok(dev.control_out_blocking(control(selector, UVC_SET_CUR), data, TIMEOUT)?)
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "android"))]
fn control_in(dev: &nusb::Device, selector: u8, buf: &mut [u8]) -> Result<usize> {
    Ok(dev.control_in_blocking(control(selector, UVC_GET_CUR), buf, TIMEOUT)?)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "android")))]
fn control_out(_: &nusb::Device, _: u8, _: &[u8]) -> Result<usize> {
    bail!("the 4K X's USB mode can only be changed on macOS and Linux")
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "android")))]
fn control_in(_: &nusb::Device, _: u8, _: &mut [u8]) -> Result<usize> {
    bail!("the 4K X's USB mode can only be changed on macOS and Linux")
}

/// What was found when opening a card, for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    pub usb: UsbId,
    /// bcdDevice.
    pub device_version: u16,
}

/// Opens the 4K X with this serial (any 4K X when `None`). Fails when none, or more than
/// one without a serial to choose by, is connected.
pub fn open(serial: Option<&str>) -> Result<(UsbChannel, Opened)> {
    let mut found = nusb::list_devices()
        .context("listing USB devices")?
        .filter(|d| {
            is_4kx(UsbId {
                vid: d.vendor_id(),
                pid: d.product_id(),
            })
        })
        .filter(|d| serial.is_none() || d.serial_number() == serial);
    let d = found.next().context("no Elgato 4K X connected")?;
    ensure!(
        found.next().is_none(),
        "more than one Elgato 4K X is connected; say which"
    );
    let opened = Opened {
        usb: UsbId {
            vid: d.vendor_id(),
            pid: d.product_id(),
        },
        device_version: d.device_version(),
    };
    let dev = d.open().context("opening the Elgato 4K X")?;
    Ok((UsbChannel { dev }, opened))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).unwrap())
            .collect()
    }

    /// A read reply with the decoded layout; the bytes after the mode are not known to
    /// matter, so they are zero here.
    fn mode_reply(mode: u8) -> Vec<u8> {
        let mut r = hex("a1 80 85 00 01 00 00 00");
        r.push(mode);
        r.extend([0, 0, 0]);
        r.resize(USB_MODE_REPLY_LEN - 1, 0);
        r.push(checksum(&r));
        r
    }

    #[test]
    fn requests_match_the_traced_bytes() {
        assert_eq!(get_usb_mode_request(), hex("a1 06 00 00 8d 00 00 00 cc"));
        assert_eq!(
            set_usb_mode_request(UsbSpeedMode::Gbps10),
            hex("a1 0e 00 00 8e 00 00 00 01 00 00 00 03 00 00 00 bf")
        );
        assert_eq!(
            set_usb_mode_request(UsbSpeedMode::Gbps5),
            hex("a1 0e 00 00 8e 00 00 00 01 00 00 00 00 00 00 00 c2")
        );
        // Elgato Studio's status poll has the same checksum rule.
        let poll = hex("a0 06 00 00 67 00 00 00 f3");
        assert_eq!(checksum(&poll[..poll.len() - 1]), 0xf3);
    }

    #[test]
    fn the_switch_reply_parses_to_an_empty_body() {
        assert_eq!(parse_reply(&hex("a1 00 5f")).unwrap(), &[] as &[u8]);
    }

    #[test]
    fn replies_with_a_bad_header_length_or_checksum_are_refused() {
        assert!(parse_reply(&hex("a0 00 60")).is_err(), "header");
        assert!(parse_reply(&hex("a1 00 60")).is_err(), "checksum");
        assert!(parse_reply(&hex("a1 01 5e")).is_err(), "length");
        assert!(parse_reply(&hex("a1")).is_err(), "short");
        let mut long = mode_reply(WIRE_10G);
        long.push(0);
        assert!(parse_reply(&long).is_err(), "trailing byte");
    }

    #[test]
    fn mode_replies_read_as_documented() {
        assert_eq!(
            parse_usb_mode_reply(&mode_reply(0x03)).unwrap(),
            UsbSpeedMode::Gbps10
        );
        assert_eq!(
            parse_usb_mode_reply(&mode_reply(0x00)).unwrap(),
            UsbSpeedMode::Gbps5
        );
        let r = mode_reply(0x03);
        assert_eq!(r[..12], hex("a1 80 85 00 01 00 00 00 03 00 00 00")[..]);
    }

    #[test]
    fn the_reply_a_real_card_gave_in_10_gbps_mode_parses() {
        // Read from a 4K X (firmware 0x0130) on 2026-11-16, without a status poll first:
        // 137 bytes, zero after the mode, checksum 0x56.
        let mut real = hex("a1 80 85 00 01 00 00 00 03");
        real.resize(136, 0);
        real.push(0x56);
        assert_eq!(parse_reply(&real).unwrap().len(), 133);
        assert_eq!(parse_usb_mode_reply(&real).unwrap(), UsbSpeedMode::Gbps10);
        assert_eq!(real, mode_reply(0x03));
    }

    #[test]
    fn odd_mode_replies_are_refused() {
        assert!(parse_usb_mode_reply(&mode_reply(0x01)).is_err(), "mode 1");
        let mut shifted = mode_reply(0x03);
        shifted[4] = 2;
        let n = shifted.len();
        shifted[n - 1] = checksum(&shifted[..n - 1]);
        assert!(parse_usb_mode_reply(&shifted).is_err(), "layout");
        let short = {
            let mut r = hex("a1 0a 00 01 00 00 00 03 00 00 00 00");
            r.push(checksum(&r));
            r
        };
        assert!(parse_reply(&short).is_ok());
        assert!(parse_usb_mode_reply(&short).is_err(), "length");
    }

    #[test]
    fn product_ids_name_the_mode() {
        assert_eq!(mode_of(MODE_5G), Some(UsbSpeedMode::Gbps5));
        assert_eq!(mode_of(MODE_10G), Some(UsbSpeedMode::Gbps10));
        assert_eq!(
            mode_of(UsbId {
                vid: ELGATO,
                pid: 0x0066
            }),
            None
        );
        for m in [UsbSpeedMode::Gbps5, UsbSpeedMode::Gbps10] {
            assert_eq!(mode_of(usb_for(m)), Some(m));
        }
    }

    /// A card that answers from a script and records what it was sent.
    struct FakeCard {
        mode: u8,
        sent: Vec<(u8, Vec<u8>)>,
        pending: Vec<u8>,
        mode_reply: Option<Vec<u8>>,
    }

    impl FakeCard {
        fn new(mode: u8) -> FakeCard {
            FakeCard {
                mode,
                sent: Vec::new(),
                pending: Vec::new(),
                mode_reply: None,
            }
        }
        fn writes(&self) -> usize {
            self.sent
                .iter()
                .filter(|(s, d)| *s == SEL_DATA && d.get(4) == Some(&CMD_SET_USB_MODE))
                .count()
        }
    }

    impl Channel for FakeCard {
        fn set_cur(&mut self, selector: u8, data: &[u8]) -> Result<()> {
            self.sent.push((selector, data.to_vec()));
            if selector == SEL_DATA {
                self.pending = match data[4] {
                    CMD_GET_USB_MODE => self
                        .mode_reply
                        .clone()
                        .unwrap_or_else(|| mode_reply(self.mode)),
                    CMD_SET_USB_MODE => {
                        self.mode = data[12];
                        hex("a1 00 5f")
                    }
                    _ => bail!("unknown command"),
                };
            }
            Ok(())
        }
        fn get_cur(&mut self, selector: u8, len: usize) -> Result<Vec<u8>> {
            Ok(match selector {
                SEL_LEN => (self.pending.len() as u16).to_le_bytes().to_vec(),
                _ => self.pending[..len].to_vec(),
            })
        }
    }

    #[test]
    fn the_transport_sends_length_then_request() {
        let mut card = FakeCard::new(WIRE_10G);
        assert_eq!(get_usb_speed_mode(&mut card).unwrap(), UsbSpeedMode::Gbps10);
        assert_eq!(
            card.sent,
            vec![
                (SEL_LEN, vec![9, 0]),
                (SEL_DATA, hex("a1 06 00 00 8d 00 00 00 cc"))
            ]
        );
    }

    #[test]
    fn setting_reads_first_then_writes() {
        let mut card = FakeCard::new(WIRE_10G);
        assert_eq!(
            set_usb_speed_mode(&mut card, UsbSpeedMode::Gbps5).unwrap(),
            SetOutcome::Switching
        );
        assert_eq!(card.mode, WIRE_5G);
        assert_eq!(card.writes(), 1);
        assert_eq!(
            card.sent.last().unwrap().1,
            hex("a1 0e 00 00 8e 00 00 00 01 00 00 00 00 00 00 00 c2")
        );
    }

    #[test]
    fn setting_the_current_mode_writes_nothing() {
        let mut card = FakeCard::new(WIRE_10G);
        assert_eq!(
            set_usb_speed_mode(&mut card, UsbSpeedMode::Gbps10).unwrap(),
            SetOutcome::Already
        );
        assert_eq!(card.writes(), 0);
    }

    #[test]
    fn an_unfamiliar_read_reply_stops_the_write() {
        let mut card = FakeCard::new(WIRE_10G);
        let mut odd = mode_reply(WIRE_10G);
        odd[5] = 9;
        let n = odd.len();
        odd[n - 1] = checksum(&odd[..n - 1]);
        card.mode_reply = Some(odd);
        let e = set_usb_speed_mode(&mut card, UsbSpeedMode::Gbps5).unwrap_err();
        assert!(format!("{e:#}").contains("not switching"), "{e:#}");
        assert_eq!(card.writes(), 0);
        assert_eq!(card.mode, WIRE_10G);
    }
}
