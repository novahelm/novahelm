//! Shared protocol between the user agent, its SYSTEM helper and the Windows
//! service, plus the agent's routing rules. Pure: the pipe server/client that
//! use it are Windows-only and live elsewhere.

use std::path::Path;

use novalink_core::input::InputEvent;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub const HELPER_PROTO: u16 = 1;

/// Largest accepted frame; messages here are tiny.
pub const MAX_FRAME: usize = 1024 * 1024;

/// The service's control pipe.
pub const CONTROL_PIPE: &str = r"\\.\pipe\novalink-service";

pub fn helper_pipe_name(session: u32) -> String {
    format!(r"\\.\pipe\novalink-helper-{session}")
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ToHelper {
    Hello { proto: u16 },
    Inject(InputEvent),
    ReleaseAll,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopKind {
    Normal,
    Protected,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum FromHelper {
    Desktop(DesktopKind),
    Cursor { x: f64, y: f64 },
}

/// Control pipe, agent side to service.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ToService {
    Starting,
    Sync(Vec<u8>),
    SetOff(bool),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum FromService {
    Go,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FrameError {
    /// The length prefix exceeds [`MAX_FRAME`]; the reader is poisoned.
    #[error("frame too large")]
    TooLarge,
    /// The frame was consumed but is not a message of the requested type.
    #[error("malformed message")]
    Malformed,
}

/// u32 little-endian length prefix followed by the postcard payload.
pub fn encode<T: Serialize>(m: &T) -> Vec<u8> {
    let payload = postcard::to_stdvec(m).expect("pipe message serialization is infallible");
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend((payload.len() as u32).to_le_bytes());
    out.extend(payload);
    out
}

/// Incremental frame decoder for a pipe.
#[derive(Default)]
pub struct FrameReader {
    buf: Vec<u8>,
    poisoned: bool,
}

impl FrameReader {
    pub fn push(&mut self, bytes: &[u8]) {
        if !self.poisoned {
            self.buf.extend_from_slice(bytes);
        }
    }

    /// Next complete message, `Ok(None)` if more bytes are needed. A frame that
    /// fails to decode is consumed and reported as `Malformed`. After `TooLarge`
    /// every call returns that error again.
    #[allow(clippy::should_implement_trait)]
    pub fn next<T: DeserializeOwned>(&mut self) -> Result<Option<T>, FrameError> {
        if self.poisoned {
            return Err(FrameError::TooLarge);
        }
        let Some(len_bytes) = self.buf.first_chunk::<4>() else {
            return Ok(None);
        };
        let len = u32::from_le_bytes(*len_bytes) as usize;
        if len > MAX_FRAME {
            self.poisoned = true;
            self.buf = Vec::new();
            return Err(FrameError::TooLarge);
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let msg = postcard::from_bytes(&self.buf[4..4 + len]).map_err(|_| FrameError::Malformed);
        self.buf.drain(..4 + len);
        msg.map(Some)
    }
}

/// `Default` is the normal desktop; Winlogon, Screen-saver and anything else is protected.
pub fn desktop_kind(name: &str) -> DesktopKind {
    if name.eq_ignore_ascii_case("Default") {
        DesktopKind::Normal
    } else {
        DesktopKind::Protected
    }
}

/// Only the installed agent may drive the helper. Compared as strings, ignoring
/// case and treating `/` like `\`, so Windows paths can be checked on any OS.
pub fn caller_allowed(image: &Path, program_files: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().replace('/', r"\").to_lowercase();
    let expected = format!(
        r"{}\NovaLink\novalink-agent.exe",
        program_files
            .to_string_lossy()
            .trim_end_matches(['\\', '/'])
    );
    norm(image) == norm(Path::new(&expected))
}

/// Which injector the agent should use. The helper is used only while it is up
/// and the desktop is protected.
#[derive(Default, Debug)]
pub struct Route {
    helper_up: bool,
    /// Last desktop kind the helper reported.
    reported: Option<DesktopKind>,
}

impl Route {
    fn effective(&self) -> DesktopKind {
        match (self.helper_up, self.reported) {
            (true, Some(k)) => k,
            _ => DesktopKind::Normal,
        }
    }

    /// Returns true when the effective desktop changed.
    pub fn on_helper(&mut self, up: bool) -> bool {
        let before = self.effective();
        self.helper_up = up;
        if !up {
            self.reported = None;
        }
        self.effective() != before
    }

    /// Returns true when the effective desktop changed.
    pub fn on_desktop(&mut self, k: DesktopKind) -> bool {
        let before = self.effective();
        self.reported = Some(k);
        self.effective() != before
    }

    pub fn use_helper(&self) -> bool {
        self.effective() == DesktopKind::Protected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use novalink_proto::MouseButton;

    fn feed_bytewise<T: DeserializeOwned>(r: &mut FrameReader, bytes: &[u8]) -> Vec<T> {
        let mut out = Vec::new();
        for b in bytes {
            r.push(&[*b]);
            while let Some(m) = r.next::<T>().unwrap() {
                out.push(m);
            }
        }
        out
    }

    #[test]
    fn frames_round_trip_split_anywhere() {
        let a = ToHelper::Inject(InputEvent::Key {
            usage: 4,
            down: true,
        });
        let b = ToHelper::Inject(InputEvent::Button {
            button: MouseButton::Left,
            down: false,
        });
        let mut bytes = encode(&a);
        bytes.extend(encode(&b));
        let mut r = FrameReader::default();
        assert_eq!(feed_bytewise::<ToHelper>(&mut r, &bytes), vec![a, b]);

        let c = FromHelper::Cursor { x: 12.5, y: -3.0 };
        let d = FromHelper::Desktop(DesktopKind::Protected);
        let mut bytes = encode(&c);
        bytes.extend(encode(&d));
        assert_eq!(feed_bytewise::<FromHelper>(&mut r, &bytes), vec![c, d]);
    }

    #[test]
    fn control_messages_round_trip() {
        let m = ToService::Sync(vec![1, 2, 3]);
        let mut r = FrameReader::default();
        r.push(&encode(&m));
        assert_eq!(r.next::<ToService>().unwrap(), Some(m));
        r.push(&encode(&FromService::Go));
        assert_eq!(r.next::<FromService>().unwrap(), Some(FromService::Go));
    }

    #[test]
    fn oversized_frame_errors() {
        let mut r = FrameReader::default();
        r.push(&(2u32 * 1024 * 1024).to_le_bytes());
        assert_eq!(r.next::<ToHelper>(), Err(FrameError::TooLarge));
        assert_eq!(r.next::<ToHelper>(), Err(FrameError::TooLarge));
    }

    #[test]
    fn malformed_frame_is_consumed() {
        let mut r = FrameReader::default();
        r.push(&[1, 0, 0, 0, 0xFF]);
        assert_eq!(r.next::<ToHelper>(), Err(FrameError::Malformed));
        r.push(&encode(&ToHelper::ReleaseAll));
        assert_eq!(r.next::<ToHelper>().unwrap(), Some(ToHelper::ReleaseAll));
    }

    #[test]
    fn pipe_names() {
        assert_eq!(helper_pipe_name(3), r"\\.\pipe\novalink-helper-3");
        assert_eq!(CONTROL_PIPE, r"\\.\pipe\novalink-service");
    }

    #[test]
    fn desktop_names() {
        assert_eq!(desktop_kind("Default"), DesktopKind::Normal);
        assert_eq!(desktop_kind("default"), DesktopKind::Normal);
        assert_eq!(desktop_kind("Winlogon"), DesktopKind::Protected);
        assert_eq!(desktop_kind("Screen-saver"), DesktopKind::Protected);
    }

    #[test]
    fn caller_allowed_only_program_files_agent() {
        let pf = Path::new(r"C:\Program Files");
        assert!(caller_allowed(
            Path::new(r"C:\Program Files\NovaLink\novalink-agent.exe"),
            pf
        ));
        assert!(caller_allowed(
            Path::new(r"c:\program files\novalink\NOVALINK-AGENT.EXE"),
            pf
        ));
        assert!(caller_allowed(
            Path::new("C:/Program Files/NovaLink/novalink-agent.exe"),
            pf
        ));
        assert!(!caller_allowed(
            Path::new(r"C:\Users\th3br\AppData\Local\Programs\NovaLink\novalink-agent.exe"),
            pf
        ));
        assert!(!caller_allowed(
            Path::new(r"C:\Program Files\NovaLink\novalink-agent.exe.evil"),
            pf
        ));
        assert!(!caller_allowed(
            Path::new(r"C:\Program Files\Other\NovaLink\novalink-agent.exe"),
            pf
        ));
    }

    #[test]
    fn route_switches_only_on_effective_change() {
        let mut r = Route::default();
        assert!(!r.use_helper());
        assert!(!r.on_desktop(DesktopKind::Protected)); // helper down: effective stays Normal
        assert!(!r.use_helper());
        assert!(r.on_helper(true)); // now Protected
        assert!(r.use_helper());
        assert!(!r.on_desktop(DesktopKind::Protected));
        assert!(r.on_desktop(DesktopKind::Normal));
        assert!(!r.use_helper());
        assert!(!r.on_helper(false));
    }

    #[test]
    fn helper_loss_resets_to_normal() {
        let mut r = Route::default();
        r.on_helper(true);
        r.on_desktop(DesktopKind::Protected);
        assert!(r.on_helper(false));
        assert!(!r.use_helper());
        // A returning helper has to report again before it is trusted.
        assert!(!r.on_helper(true));
        assert!(!r.use_helper());
    }
}
