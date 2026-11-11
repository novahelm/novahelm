//! Stream Deck + over HID: 8 keys (120×120 JPEG), 4 push dials and an 800×100 touch strip.
//! The device lives on its own thread because hidapi is blocking; the agent talks to it
//! through a command channel and gets inputs back as events.

use crate::catalog::STREAM_DECK_PLUS;
use anyhow::{Context, Result, bail};
use hidapi::HidDevice;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub const KEYS: usize = 8;
pub const DIALS: usize = 4;
pub const KEY_PX: u32 = 120;
pub const STRIP_W: u32 = 800;
pub const STRIP_H: u32 = 100;

const PACKET: usize = 1024;
const KEY_HEADER: usize = 8;
const STRIP_HEADER: usize = 16;
/// How long a read waits before checking for commands. Short enough that drawing feels
/// immediate, long enough that an idle deck costs nothing measurable.
const POLL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "input")]
pub enum Input {
    Key { index: usize, down: bool },
    DialTurn { index: usize, detents: i8 },
    DialPress { index: usize, down: bool },
    Touch { x: u16, y: u16, long: bool },
    Swipe { from_x: u16, to_x: u16 },
}

pub enum Command {
    KeyImage { index: usize, jpeg: Vec<u8> },
    StripImage { x: u16, width: u16, jpeg: Vec<u8> },
    Brightness(u8),
    Reset,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeckInfo {
    pub serial: String,
    pub firmware: String,
}

/// Decodes one input report. Keys and dial presses arrive as whole state arrays, so the
/// previous arrays are kept to report only what changed.
pub struct Decoder {
    keys: [bool; KEYS],
    dials: [bool; DIALS],
}

impl Default for Decoder {
    fn default() -> Self {
        Decoder {
            keys: [false; KEYS],
            dials: [false; DIALS],
        }
    }
}

impl Decoder {
    pub fn decode(&mut self, r: &[u8]) -> Vec<Input> {
        let mut out = Vec::new();
        if r.len() < 5 || r[0] != 0x01 {
            return out;
        }
        let u16le = |i: usize| {
            r.get(i..i + 2)
                .map_or(0, |b| u16::from_le_bytes([b[0], b[1]]))
        };
        match r[1] {
            0x00 => {
                for (i, now) in r.iter().skip(4).take(KEYS).map(|&b| b != 0).enumerate() {
                    if self.keys[i] != now {
                        self.keys[i] = now;
                        out.push(Input::Key {
                            index: i,
                            down: now,
                        });
                    }
                }
            }
            0x02 => match r[4] {
                1 | 2 => out.push(Input::Touch {
                    x: u16le(6),
                    y: u16le(8),
                    long: r[4] == 2,
                }),
                3 => out.push(Input::Swipe {
                    from_x: u16le(6),
                    to_x: u16le(10),
                }),
                _ => {}
            },
            0x03 => {
                let vals = r.iter().skip(5).take(DIALS).enumerate();
                if r[4] == 0x00 {
                    for (i, now) in vals.map(|(i, &b)| (i, b != 0)) {
                        if self.dials[i] != now {
                            self.dials[i] = now;
                            out.push(Input::DialPress {
                                index: i,
                                down: now,
                            });
                        }
                    }
                } else {
                    for (i, &b) in vals {
                        if b != 0 {
                            out.push(Input::DialTurn {
                                index: i,
                                detents: b as i8,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
        out
    }
}

/// Splits a key image into the device's 1024-byte output reports.
pub fn key_packets(index: usize, jpeg: &[u8]) -> Vec<Vec<u8>> {
    let chunks: Vec<&[u8]> = jpeg.chunks(PACKET - KEY_HEADER).collect();
    let n = chunks.len();
    chunks
        .into_iter()
        .enumerate()
        .map(|(page, chunk)| {
            let len = chunk.len() as u16;
            let mut p = vec![0x02, 0x07, index as u8, u8::from(page + 1 == n)];
            p.extend_from_slice(&len.to_le_bytes());
            p.extend_from_slice(&(page as u16).to_le_bytes());
            p.extend_from_slice(chunk);
            p.resize(PACKET, 0);
            p
        })
        .collect()
}

/// Splits a touch strip image (drawn at `x`, full strip height) into output reports.
pub fn strip_packets(x: u16, width: u16, jpeg: &[u8]) -> Vec<Vec<u8>> {
    let chunks: Vec<&[u8]> = jpeg.chunks(PACKET - STRIP_HEADER).collect();
    let n = chunks.len();
    chunks
        .into_iter()
        .enumerate()
        .map(|(page, chunk)| {
            let mut p = vec![0x02, 0x0c];
            for v in [x, 0, width, STRIP_H as u16] {
                p.extend_from_slice(&v.to_le_bytes());
            }
            p.push(u8::from(page + 1 == n));
            p.extend_from_slice(&(page as u16).to_le_bytes());
            p.extend_from_slice(&(chunk.len() as u16).to_le_bytes());
            p.push(0);
            p.extend_from_slice(chunk);
            p.resize(PACKET, 0);
            p
        })
        .collect()
}

fn feature_string(dev: &HidDevice, id: u8, skip: usize) -> String {
    let mut buf = [0u8; 32];
    buf[0] = id;
    match dev.get_feature_report(&mut buf) {
        Ok(n) => String::from_utf8_lossy(&buf[skip.min(n)..n])
            .trim_matches(char::from(0))
            .trim()
            .to_string(),
        Err(_) => String::new(),
    }
}

/// The slice of the device the thread needs, so tests can stand a slow fake in for HID.
trait Hid {
    fn write_report(&self, data: &[u8]) -> Result<()>;
    fn feature_report(&self, data: &[u8]) -> Result<()>;
    fn read_report(&self, buf: &mut [u8], timeout_ms: i32) -> Result<usize>;
}

impl Hid for HidDevice {
    fn write_report(&self, data: &[u8]) -> Result<()> {
        self.write(data).map(|_| ()).context("writing to the deck")
    }
    fn feature_report(&self, data: &[u8]) -> Result<()> {
        self.send_feature_report(data).context("setting the deck")
    }
    fn read_report(&self, buf: &mut [u8], timeout_ms: i32) -> Result<usize> {
        self.read_timeout(buf, timeout_ms)
            .map_err(|e| anyhow::anyhow!("deck read failed: {e}"))
    }
}

fn set_brightness(dev: &dyn Hid, pct: u8) -> Result<()> {
    let mut buf = [0u8; 32];
    buf[..3].copy_from_slice(&[0x03, 0x08, pct.min(100)]);
    dev.feature_report(&buf).context("setting deck brightness")
}

fn write_all(dev: &dyn Hid, packets: Vec<Vec<u8>>) -> Result<()> {
    for p in packets {
        dev.write_report(&p)?;
    }
    Ok(())
}

pub struct Deck {
    pub info: DeckInfo,
    pub commands: mpsc::Sender<Command>,
}

/// Opens the first Stream Deck + and starts its thread. `on_input` runs on that thread for
/// every key, dial or touch, with the moment its report was read; `on_gone` runs once if the
/// deck is unplugged or fails.
pub fn open(
    on_input: impl Fn(Input, Instant) + Send + 'static,
    on_gone: impl FnOnce(String) + Send + 'static,
) -> Result<Deck> {
    // Found and opened on the hid thread; the device then lives on the deck's own thread.
    let dev = crate::hid::with(|api| -> Result<HidDevice> {
        let Some(di) = api
            .device_list()
            .find(|d| {
                d.vendor_id() == STREAM_DECK_PLUS.vid
                    && d.product_id() == STREAM_DECK_PLUS.pid
                    && d.usage_page() == 0x0c
            })
            .or_else(|| {
                api.device_list().find(|d| {
                    d.vendor_id() == STREAM_DECK_PLUS.vid && d.product_id() == STREAM_DECK_PLUS.pid
                })
            })
        else {
            bail!("no Stream Deck + connected");
        };
        di.open_device(api)
            .context("opening the Stream Deck + (another app may hold it)")
    })??;
    let info = DeckInfo {
        serial: feature_string(&dev, 0x06, 2),
        firmware: feature_string(&dev, 0x05, 6),
    };
    let (tx, rx) = mpsc::channel::<Command>();
    std::thread::Builder::new()
        .name("streamdeck".into())
        .spawn(move || {
            let reason = run(&dev, &rx, &on_input);
            on_gone(reason);
        })?;
    Ok(Deck { info, commands: tx })
}

/// Queues `cmd`, keeping only the newest image per key and per strip region (the same `x`
/// and `width`): a redraw that outruns the device (the hold bar redraws every 100 ms, a
/// panel every second) must not build a backlog of stale pictures. The newer image goes to
/// the back, so its order against a later Reset or Brightness is kept.
fn enqueue(pending: &mut VecDeque<Command>, cmd: Command) {
    match &cmd {
        Command::KeyImage { index, .. } => {
            pending.retain(|c| !matches!(c, Command::KeyImage { index: i, .. } if i == index));
        }
        Command::StripImage { x, width, .. } => {
            pending.retain(|c| {
                !matches!(c, Command::StripImage { x: px, width: pw, .. } if px == x && pw == width)
            });
        }
        _ => {}
    }
    pending.push_back(cmd);
}

/// Reads whatever input the device has ready, without waiting, and passes it on.
fn poll_input(
    dev: &dyn Hid,
    decoder: &mut Decoder,
    buf: &mut [u8],
    timeout_ms: i32,
    on_input: &dyn Fn(Input, Instant),
) -> Result<usize> {
    let n = dev.read_report(buf, timeout_ms)?;
    if n > 0 {
        // Stamped as the report arrives, before anything can queue it: how long a key was
        // held is decided from these times, never from when the agent got round to the
        // input (a busy agent must not turn a tap into a hold).
        let at = Instant::now();
        for i in decoder.decode(&buf[..n]) {
            on_input(i, at);
        }
    }
    Ok(n)
}

fn run(dev: &dyn Hid, rx: &mpsc::Receiver<Command>, on_input: &dyn Fn(Input, Instant)) -> String {
    let mut decoder = Decoder::default();
    let mut buf = [0u8; 64];
    let mut pending: VecDeque<Command> = VecDeque::new();
    let mut closed = false;
    loop {
        loop {
            match rx.try_recv() {
                Ok(c) => enqueue(&mut pending, c),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    closed = true;
                    break;
                }
            }
        }
        let Some(cmd) = pending.pop_front() else {
            if closed {
                return "agent closed the deck".into();
            }
            if let Err(e) = poll_input(
                dev,
                &mut decoder,
                &mut buf,
                POLL.as_millis() as i32,
                on_input,
            ) {
                return format!("{e:#}");
            }
            continue;
        };
        // Input is read between commands, not only after the whole backlog: a write backlog
        // must not delay when a key-up is read (and stamped) by more than one command's time.
        for _ in 0..16 {
            match poll_input(dev, &mut decoder, &mut buf, 0, on_input) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => return format!("{e:#}"),
            }
        }
        let r = match cmd {
            Command::KeyImage { index, jpeg } => write_all(dev, key_packets(index, &jpeg)),
            Command::StripImage { x, width, jpeg } => {
                write_all(dev, strip_packets(x, width, &jpeg))
            }
            Command::Brightness(p) => set_brightness(dev, p),
            Command::Reset => dev.feature_report(&[0x03, 0x02]),
        };
        if let Err(e) = r {
            return format!("{e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_key_changes_only() {
        let mut d = Decoder::default();
        let mut r = [0u8; 14];
        r[0] = 0x01;
        r[4 + 2] = 1;
        assert_eq!(
            d.decode(&r),
            vec![Input::Key {
                index: 2,
                down: true
            }]
        );
        assert_eq!(d.decode(&r), vec![], "held key reports nothing new");
        r[4 + 2] = 0;
        assert_eq!(
            d.decode(&r),
            vec![Input::Key {
                index: 2,
                down: false
            }]
        );
    }

    #[test]
    fn decodes_dials_and_touch() {
        let mut d = Decoder::default();
        let turn = [0x01, 0x03, 0, 0, 0x01, 0, 0xFE, 0, 3];
        assert_eq!(
            d.decode(&turn),
            vec![
                Input::DialTurn {
                    index: 1,
                    detents: -2
                },
                Input::DialTurn {
                    index: 3,
                    detents: 3
                }
            ]
        );
        let press = [0x01, 0x03, 0, 0, 0x00, 1, 0, 0, 0];
        assert_eq!(
            d.decode(&press),
            vec![Input::DialPress {
                index: 0,
                down: true
            }]
        );
        let tap = [0x01, 0x02, 0, 0, 1, 0, 0x2c, 0x01, 50, 0];
        assert_eq!(
            d.decode(&tap),
            vec![Input::Touch {
                x: 300,
                y: 50,
                long: false
            }]
        );
        let swipe = [0x01, 0x02, 0, 0, 3, 0, 100, 0, 10, 0, 0x90, 0x01, 10, 0];
        assert_eq!(
            d.decode(&swipe),
            vec![Input::Swipe {
                from_x: 100,
                to_x: 400
            }]
        );
    }

    #[test]
    fn packs_images_into_numbered_reports() {
        let jpeg = vec![0xAB; 2000];
        let p = key_packets(5, &jpeg);
        assert_eq!(p.len(), 2);
        assert!(p.iter().all(|x| x.len() == 1024));
        // 2000 bytes = 1016 + 984 at 1016 payload bytes per report.
        assert_eq!(&p[0][..8], &[0x02, 0x07, 5, 0, 0xF8, 0x03, 0, 0]);
        assert_eq!(&p[1][..8], &[0x02, 0x07, 5, 1, 0xD8, 0x03, 1, 0]);

        let s = strip_packets(200, 200, &jpeg);
        assert_eq!(&s[0][..4], &[0x02, 0x0c, 200, 0]);
        assert_eq!(&s[0][6..10], &[200, 0, 100, 0]);
        assert_eq!(s[s.len() - 1][10], 1, "last page flagged");
    }

    use std::sync::Mutex;

    /// A device whose writes each take `write_ms`, with one input report that "arrives"
    /// at `report_at` after `start`.
    struct Slow {
        start: Instant,
        write_ms: u64,
        report_at: Duration,
        report: Mutex<Option<Vec<u8>>>,
        writes: Mutex<usize>,
    }
    impl Hid for Slow {
        fn write_report(&self, _: &[u8]) -> Result<()> {
            std::thread::sleep(Duration::from_millis(self.write_ms));
            *self.writes.lock().unwrap() += 1;
            Ok(())
        }
        fn feature_report(&self, _: &[u8]) -> Result<()> {
            Ok(())
        }
        fn read_report(&self, buf: &mut [u8], _: i32) -> Result<usize> {
            let mut r = self.report.lock().unwrap();
            if self.start.elapsed() >= self.report_at
                && let Some(rep) = r.take()
            {
                buf[..rep.len()].copy_from_slice(&rep);
                return Ok(rep.len());
            }
            drop(r);
            std::thread::sleep(Duration::from_millis(1));
            Ok(0)
        }
    }

    #[test]
    fn input_is_read_between_queued_writes() {
        let start = Instant::now();
        let dev = Slow {
            start,
            write_ms: 50,
            report_at: Duration::from_millis(120),
            // key 0 down; the decoder reports it as a change.
            report: Mutex::new(Some({
                let mut r = vec![0u8; 14];
                r[0] = 0x01;
                r[4] = 1;
                r
            })),
            writes: Mutex::new(0),
        };
        let (tx, rx) = mpsc::channel();
        // Twenty different regions: a backlog that coalescing does not merge.
        for i in 0..20u16 {
            tx.send(Command::StripImage {
                x: i * 10,
                width: 10,
                jpeg: vec![1; 10],
            })
            .unwrap();
        }
        drop(tx);
        let seen: Mutex<Vec<Instant>> = Mutex::new(Vec::new());
        let reason = run(&dev, &rx, &|_, at| seen.lock().unwrap().push(at));
        assert_eq!(reason, "agent closed the deck");
        assert_eq!(*dev.writes.lock().unwrap(), 20, "every strip image written");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let lag = seen[0] - (start + Duration::from_millis(120));
        // One write is 50 ms; reading only after the backlog would lag about 900 ms.
        assert!(lag < Duration::from_millis(110), "stamped {lag:?} late");
    }

    #[test]
    fn only_the_newest_image_per_key_is_pending() {
        let mut q = VecDeque::new();
        for n in 0..20u8 {
            enqueue(
                &mut q,
                Command::KeyImage {
                    index: 3,
                    jpeg: vec![n],
                },
            );
        }
        enqueue(
            &mut q,
            Command::KeyImage {
                index: 4,
                jpeg: vec![0],
            },
        );
        enqueue(&mut q, Command::Reset);
        enqueue(
            &mut q,
            Command::KeyImage {
                index: 3,
                jpeg: vec![99],
            },
        );
        let order: Vec<String> = q
            .iter()
            .map(|c| match c {
                Command::KeyImage { index, jpeg } => format!("k{index}:{}", jpeg[0]),
                Command::Reset => "reset".into(),
                _ => "other".into(),
            })
            .collect();
        // Key 3's old images are gone; its newest sits after the Reset it followed.
        assert_eq!(order, ["k4:0", "reset", "k3:99"]);
    }

    #[test]
    fn only_the_newest_image_per_strip_region_is_pending() {
        let strip = |x: u16, width: u16, n: u8| Command::StripImage {
            x,
            width,
            jpeg: vec![n],
        };
        let mut q = VecDeque::new();
        for n in 0..20u8 {
            enqueue(&mut q, strip(0, 200, n));
        }
        enqueue(&mut q, strip(200, 200, 1));
        enqueue(&mut q, Command::Reset);
        enqueue(&mut q, strip(0, 200, 99));
        // The full-width pop-up is its own region, not a copy of the first segment.
        enqueue(&mut q, strip(0, 800, 7));
        let order: Vec<String> = q
            .iter()
            .map(|c| match c {
                Command::StripImage { x, width, jpeg } => format!("s{x}/{width}:{}", jpeg[0]),
                Command::Reset => "reset".into(),
                _ => "other".into(),
            })
            .collect();
        assert_eq!(order, ["s200/200:1", "reset", "s0/200:99", "s0/800:7"]);
    }
}
