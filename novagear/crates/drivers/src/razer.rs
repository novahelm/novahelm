//! Razer mice over HID feature reports, using the protocol OpenRazer documents: a 90-byte
//! report with a command class/id, arguments and an XOR checksum. Calls block for a few
//! milliseconds, so the agent runs them on its blocking pool.

use crate::catalog::RAZER;
use crate::devices::Quirks;
use anyhow::{Context, Result, bail};
use hidapi::{HidApi, HidDevice};
use std::time::Duration;

const REPORT: usize = 90;
const VARSTORE: u8 = 0x01;
/// Every LED zone at once, for the extended matrix commands.
const ALL_LEDS: u8 = 0x00;

/// Builds one request report (without the leading HID report id).
pub fn request(tid: u8, class: u8, id: u8, size: u8, args: &[u8]) -> [u8; REPORT] {
    let mut r = [0u8; REPORT];
    r[1] = tid;
    r[5] = size;
    r[6] = class;
    r[7] = id;
    r[8..8 + args.len()].copy_from_slice(args);
    r[88] = r[2..88].iter().fold(0, |a, b| a ^ b);
    r
}

pub struct Mouse {
    dev: HidDevice,
    tid: u8,
    pub pid: u16,
    pub name: String,
}

impl Mouse {
    /// Opens the control interface of the first Razer mouse with this product id.
    pub fn open(api: &HidApi, pid: u16, quirks: &Quirks) -> Result<Mouse> {
        // Never guessed: a report sent to the wrong interface makes macOS drop the mouse
        // off the bus. Interfaces come from the device record.
        let iface = quirks
            .interface
            .context("no known control interface for this Razer model yet")?;
        let di = api
            .device_list()
            .find(|d| {
                d.vendor_id() == RAZER && d.product_id() == pid && d.interface_number() == iface
            })
            .context("Razer mouse not connected")?;
        let dev = di
            .open_device(api)
            .context("opening the Razer mouse (is Synapse running?)")?;
        let name = di.product_string().unwrap_or("Razer mouse").to_string();
        Ok(Mouse {
            dev,
            tid: quirks.transaction_id.unwrap_or(0xff),
            pid,
            name,
        })
    }

    fn call(&self, class: u8, id: u8, size: u8, args: &[u8]) -> Result<[u8; REPORT]> {
        let mut out = [0u8; REPORT + 1];
        out[1..].copy_from_slice(&request(self.tid, class, id, size, args));
        self.dev
            .send_feature_report(&out)
            .context("sending to the mouse")?;
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(2));
            let mut back = [0u8; REPORT + 1];
            // back[0] is the report id (0); the response follows.
            self.dev
                .get_feature_report(&mut back)
                .context("reading from the mouse")?;
            let mut resp = [0u8; REPORT];
            resp.copy_from_slice(&back[1..]);
            match resp[0] {
                0x02 if resp[6] == class && resp[7] == id => return Ok(resp),
                // 0x00 not ready yet, 0x01 busy, 0x02 an answer to an earlier call.
                0x00..=0x02 => continue,
                0x05 => bail!("the mouse does not support command {class:#04x}/{id:#04x}"),
                s => bail!("the mouse answered status {s:#04x}"),
            }
        }
        bail!("the mouse did not answer")
    }

    pub fn dpi(&self) -> Result<(u16, u16)> {
        let r = self.call(0x04, 0x85, 0x07, &[VARSTORE])?;
        Ok((
            u16::from_be_bytes([r[9], r[10]]),
            u16::from_be_bytes([r[11], r[12]]),
        ))
    }

    pub fn set_dpi(&self, dpi: u16) -> Result<()> {
        let dpi = dpi.clamp(100, 26000).to_be_bytes();
        self.call(
            0x04,
            0x05,
            0x07,
            &[VARSTORE, dpi[0], dpi[1], dpi[0], dpi[1], 0, 0],
        )
        .map(drop)
    }

    pub fn set_static_color(&self, rgb: [u8; 3]) -> Result<()> {
        self.call(
            0x0f,
            0x02,
            0x09,
            &[VARSTORE, ALL_LEDS, 0x01, 0, 0, 0x01, rgb[0], rgb[1], rgb[2]],
        )
        .map(drop)
    }

    pub fn set_brightness(&self, level: u8) -> Result<()> {
        self.call(0x0f, 0x04, 0x03, &[VARSTORE, ALL_LEDS, level])
            .map(drop)
    }

    pub fn firmware(&self) -> Result<String> {
        let r = self.call(0x00, 0x81, 0x02, &[])?;
        Ok(format!("{}.{}", r[8], r[9]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_layout_and_checksum() {
        let r = request(
            0x1f,
            0x04,
            0x05,
            0x07,
            &[0x01, 0x03, 0x20, 0x03, 0x20, 0, 0],
        );
        assert_eq!(&r[..9], &[0x00, 0x1f, 0, 0, 0, 0x07, 0x04, 0x05, 0x01]);
        let xor = 0x07 ^ 0x04 ^ 0x05 ^ 0x01 ^ 0x03 ^ 0x20 ^ 0x03 ^ 0x20;
        assert_eq!(r[88], xor);
    }

    #[test]
    fn open_refuses_without_a_known_interface() {
        let e = crate::hid::with(|api| Mouse::open(api, 0x0099, &Quirks::default()).err())
            .unwrap()
            .unwrap();
        assert!(format!("{e:#}").contains("no known control interface"));
    }
}
