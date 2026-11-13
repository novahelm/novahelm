use crate::message::{ChannelOpen, Control, Datagram};

/// Largest accepted control frame. File data uses separate streams.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CodecError {
    /// The frame was well-formed but its payload is not a message we know.
    #[error("unknown or malformed message")]
    UnknownMessage,
    /// A frame exceeded [`MAX_FRAME`]; the decoder is poisoned and the session must close.
    #[error("frame too large")]
    FrameTooLarge,
}

pub fn encode_datagram(d: &Datagram) -> Vec<u8> {
    postcard::to_stdvec(d).expect("datagram serialization is infallible")
}

pub fn decode_datagram(bytes: &[u8]) -> Result<Datagram, CodecError> {
    postcard::from_bytes(bytes).map_err(|_| CodecError::UnknownMessage)
}

/// u32 little-endian length prefix followed by the postcard payload.
pub fn encode_control(c: &Control) -> Vec<u8> {
    let payload = postcard::to_stdvec(c).expect("control serialization is infallible");
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend((payload.len() as u32).to_le_bytes());
    out.extend(payload);
    out
}

/// Largest `ChannelOpen` payload a receiver accepts (a 32-byte name plus its length).
pub const MAX_CHANNEL_OPEN: usize = 64;

/// u32 little-endian length prefix followed by the postcard payload.
pub fn encode_channel_open(o: &ChannelOpen) -> Vec<u8> {
    let payload = postcard::to_stdvec(o).expect("channel open serialization is infallible");
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend((payload.len() as u32).to_le_bytes());
    out.extend(payload);
    out
}

/// Decodes a `ChannelOpen` payload (the bytes after the length prefix).
pub fn decode_channel_open(payload: &[u8]) -> Result<ChannelOpen, CodecError> {
    postcard::from_bytes(payload).map_err(|_| CodecError::UnknownMessage)
}

/// Incremental decoder for the control stream.
#[derive(Default)]
pub struct ControlDecoder {
    buf: Vec<u8>,
    poisoned: bool,
}

impl ControlDecoder {
    pub fn push(&mut self, bytes: &[u8]) {
        if !self.poisoned {
            self.buf.extend_from_slice(bytes);
        }
    }

    /// Next complete message, `Ok(None)` if more bytes are needed. A frame that fails
    /// to decode is consumed and reported as `UnknownMessage`; decoding may continue.
    /// After `FrameTooLarge` every call returns that error again.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Control>, CodecError> {
        if self.poisoned {
            return Err(CodecError::FrameTooLarge);
        }
        let Some(len_bytes) = self.buf.first_chunk::<4>() else {
            return Ok(None);
        };
        let len = u32::from_le_bytes(*len_bytes) as usize;
        if len > MAX_FRAME {
            self.poisoned = true;
            self.buf = Vec::new();
            return Err(CodecError::FrameTooLarge);
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let result = postcard::from_bytes(&self.buf[4..4 + len]);
        self.buf.drain(..4 + len);
        result.map(Some).map_err(|_| CodecError::UnknownMessage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys;

    #[test]
    fn datagram_roundtrip() {
        let d = Datagram::Motion {
            seq: 7,
            dx: 1.5,
            dy: -2.0,
            t_us: 123,
        };
        assert_eq!(decode_datagram(&encode_datagram(&d)).unwrap(), d);
    }

    #[test]
    fn held_report_roundtrips_and_is_the_last_variant() {
        let mut dec = ControlDecoder::default();
        let r = Control::HeldReport {
            held_before_release: 3,
            held_after_settle: 1,
        };
        dec.push(&encode_control(&r));
        assert_eq!(dec.next().unwrap(), Some(r));
        dec.push(&encode_control(&Control::PeerInfo { dry_inject: true }));
        assert_eq!(
            dec.next().unwrap(),
            Some(Control::PeerInfo { dry_inject: true })
        );
        // Append-only: the older last variant keeps its wire encoding.
        let pr = encode_control(&Control::PairReveal { nonce: [9; 32] });
        let hr = encode_control(&Control::HeldReport {
            held_before_release: 0,
            held_after_settle: 0,
        });
        assert_eq!(
            pr[4],
            hr[4].wrapping_sub(1),
            "HeldReport must follow PairReveal"
        );
    }

    #[test]
    fn channel_open_roundtrips_and_names_are_validated() {
        let o = ChannelOpen {
            service: "compute".into(),
        };
        let f = encode_channel_open(&o);
        let len = u32::from_le_bytes(f[..4].try_into().unwrap()) as usize;
        assert_eq!(len, f.len() - 4);
        assert_eq!(decode_channel_open(&f[4..]).unwrap(), o);
        assert!(crate::valid_service_name("a-1"));
        assert!(crate::valid_service_name(&"a".repeat(32)));
        assert!(!crate::valid_service_name(""));
        assert!(!crate::valid_service_name(&"a".repeat(33)));
        assert!(!crate::valid_service_name("Abc"));
        assert!(!crate::valid_service_name("a_b"));
        assert!(!crate::valid_service_name("é"));
    }

    #[test]
    fn motion_datagram_is_small() {
        let d = Datagram::Motion {
            seq: u64::MAX,
            dx: 1.0,
            dy: 1.0,
            t_us: u64::MAX,
        };
        assert!(encode_datagram(&d).len() <= 32);
    }

    #[test]
    fn control_frames_split_across_pushes() {
        let a = encode_control(&Control::Key {
            usage: keys::A,
            down: true,
            t_us: 1,
        });
        let b = encode_control(&Control::Leave);
        let all = [a, b].concat();
        let mut dec = ControlDecoder::default();
        dec.push(&all[..3]);
        assert_eq!(dec.next().unwrap(), None);
        dec.push(&all[3..]);
        assert!(matches!(
            dec.next().unwrap(),
            Some(Control::Key {
                usage: 0x04,
                down: true,
                ..
            })
        ));
        assert_eq!(dec.next().unwrap(), Some(Control::Leave));
    }

    #[test]
    fn unknown_control_variant_is_skipped_not_fatal() {
        // frame with valid length prefix but variant index 250
        let mut f = 2u32.to_le_bytes().to_vec();
        f.extend([250, 0]);
        f.extend(encode_control(&Control::Leave));
        let mut dec = ControlDecoder::default();
        dec.push(&f);
        assert!(matches!(dec.next(), Err(CodecError::UnknownMessage)));
        assert_eq!(dec.next().unwrap(), Some(Control::Leave));
    }

    #[test]
    fn oversize_frame_rejected() {
        let mut dec = ControlDecoder::default();
        dec.push(&(17 * 1024 * 1024u32).to_le_bytes());
        assert!(matches!(dec.next(), Err(CodecError::FrameTooLarge)));
        // poisoned: stays failed even if more data arrives
        dec.push(&encode_control(&Control::Leave));
        assert!(matches!(dec.next(), Err(CodecError::FrameTooLarge)));
    }
}
