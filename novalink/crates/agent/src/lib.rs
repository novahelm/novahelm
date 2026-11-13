//! NovaLink agent foundations: per-OS directories, logging, flight recorder.

pub mod api;
pub mod channels;
pub mod engine;
pub mod logging;
pub mod paths;
pub mod recorder;
pub mod settings;
pub mod soak;
#[cfg(feature = "fake")]
pub mod testutil;
pub mod winnet;
