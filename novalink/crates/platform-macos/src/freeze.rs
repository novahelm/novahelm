//! While another machine has the pointer, the Mac's own cursor is frozen where it left
//! (`CGAssociateMouseAndMouseCursorPosition(0)`) and hidden. macOS can undo either in the
//! middle of a visit, and then the Mac's arrow shows and moves along with the remote one.
//! The event tap checks on each physical move and puts both back, logging that it did.
//!
//! Hides are counted: every one we make is undone when the pointer comes back, so the Mac
//! is never left without an arrow.

use crate::ffi::*;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How far (points) the cursor may stray from where it was frozen before it counts as
/// unfrozen; event locations are fractional.
const DRIFT: f64 = 2.0;
/// Re-hide at most this often, in case `CGCursorIsVisible` disagrees with what shows.
const REHIDE_GAP: Duration = Duration::from_millis(250);

pub fn drifted(pin: (f64, f64), at: (f64, f64)) -> bool {
    (at.0 - pin.0).abs() > DRIFT || (at.1 - pin.1).abs() > DRIFT
}

#[derive(Default)]
struct State {
    /// Where the cursor is frozen; `Some` while another machine has the pointer.
    pin: Option<(f64, f64)>,
    /// Hides we hold, each undone on thaw.
    hides: u32,
    last_hide: Option<Instant>,
    unfrozen: u32,
    unhidden: u32,
}

/// One lock covers freeze, thaw and repair, so a repair never hides after a thaw.
#[derive(Default)]
pub struct Freeze {
    st: Mutex<State>,
}

fn hide(st: &mut State) {
    // SAFETY: plain call.
    let rc = unsafe { CGDisplayHideCursor(CGMainDisplayID()) };
    if rc == 0 {
        st.hides += 1;
        st.last_hide = Some(Instant::now());
    } else {
        tracing::warn!(event = "mac_cursor_hide_failed", rc);
    }
}

impl Freeze {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Freeze the cursor at `at` and hide it.
    pub fn freeze(&self, at: (f64, f64)) {
        let mut st = self.lock();
        if st.pin.is_some() {
            return;
        }
        st.pin = Some(at);
        st.unfrozen = 0;
        st.unhidden = 0;
        // SAFETY: plain call.
        let rc = unsafe { CGAssociateMouseAndMouseCursorPosition(0) };
        if rc != 0 {
            tracing::warn!(event = "mac_cursor_freeze_failed", rc);
        }
        if st.hides == 0 {
            hide(&mut st);
        }
    }

    /// Undo every hide we made and let the cursor follow the mouse again.
    pub fn thaw(&self) {
        let mut st = self.lock();
        if st.pin.take().is_none() {
            return;
        }
        // SAFETY: plain calls; extra shows past zero are ignored by the OS.
        unsafe {
            for _ in 0..st.hides {
                CGDisplayShowCursor(CGMainDisplayID());
            }
            CGAssociateMouseAndMouseCursorPosition(1);
        }
        st.hides = 0;
        if st.unfrozen + st.unhidden > 0 {
            tracing::info!(
                event = "mac_cursor_repaired",
                unfrozen = st.unfrozen,
                unhidden = st.unhidden
            );
        }
    }

    /// From the event tap, for a physical move at `at`: re-freeze and re-hide if macOS
    /// undid either. Logs the first of each per visit; the totals come at thaw.
    pub fn repair(&self, at: (f64, f64)) {
        let mut st = self.lock();
        let Some(pin) = st.pin else {
            return;
        };
        if drifted(pin, at) {
            // SAFETY: plain calls.
            unsafe {
                CGAssociateMouseAndMouseCursorPosition(0);
                CGWarpMouseCursorPosition(CGPoint { x: pin.0, y: pin.1 });
            }
            st.unfrozen += 1;
            if st.unfrozen == 1 {
                tracing::warn!(
                    event = "mac_cursor_unfrozen",
                    x = at.0,
                    y = at.1,
                    pin_x = pin.0,
                    pin_y = pin.1
                );
            }
        }
        // SAFETY: plain call.
        let visible = unsafe { CGCursorIsVisible() };
        let due = st.last_hide.is_none_or(|t| t.elapsed() >= REHIDE_GAP);
        if visible && due {
            hide(&mut st);
            st.unhidden += 1;
            if st.unhidden == 1 {
                tracing::warn!(event = "mac_cursor_unhidden", hides = st.hides);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_jitter_is_not_drift() {
        assert!(!drifted((100.0, 100.0), (101.5, 98.5)));
        assert!(drifted((100.0, 100.0), (103.0, 100.0)));
        assert!(drifted((100.0, 100.0), (100.0, 96.0)));
    }
}
