//! Pure NovaLink logic: layout, edge math, input event types.

pub mod control;
pub mod held;
pub mod input;
pub mod layout;

pub use control::{Action, ControlState};
pub use held::HeldInputs;
pub use input::{InputEvent, LocalEvent};
pub use layout::{Layout, entry_position, hit_edge};
pub mod clock;
pub mod liveness;
pub mod seqfilter;
pub mod stats;

pub use clock::ClockEstimator;
pub use liveness::{Backoff, Liveness};
pub use seqfilter::SeqFilter;
pub use stats::{LatencyStats, Percentiles, Window};
pub mod platform;
pub mod time;
#[cfg(feature = "fake")]
pub use platform::FakePlatform;
pub use platform::Platform;
