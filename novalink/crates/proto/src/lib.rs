//! NovaLink wire protocol: messages, codec and HID usage constants.

pub mod codec;
pub mod keys;
pub mod message;

pub use codec::{
    CodecError, ControlDecoder, MAX_CHANNEL_OPEN, decode_channel_open, decode_datagram,
    encode_channel_open, encode_control, encode_datagram,
};
pub use message::*;
