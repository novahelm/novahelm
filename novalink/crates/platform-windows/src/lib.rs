//! Windows platform for NovaLink: low-level hooks + Raw Input capture, SendInput
//! injection, per-monitor-DPI displays, scancode <-> USB HID mapping, and a
//! watchdog that notices Windows silently removing a low-level hook.
//!
//! The pure logic (`keymap`, `logic`, `watchdog::State`) is platform-independent so
//! its tests run anywhere; everything that touches Win32 is `#[cfg(windows)]`.

pub mod keymap;
pub mod logic;
pub mod watchdog;

#[cfg(windows)]
pub mod capture;
#[cfg(windows)]
pub mod displays;
#[cfg(windows)]
pub mod inject;

#[cfg(windows)]
mod platform;
#[cfg(windows)]
pub use platform::WinPlatform;
