use crate::{capture, displays, inject};
use novalink_core::{InputEvent, LocalEvent, Platform};
use novalink_proto::DisplayInfo;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc::UnboundedSender;
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{ClipCursor, GetCursorPos, SetCursorPos};

/// Windows `Platform`. Capture state is process-wide (hook procedures have no user
/// data), so create one per process.
pub struct WinPlatform {
    injector: inject::Injector,
    capturing: AtomicBool,
}

impl WinPlatform {
    pub fn new() -> anyhow::Result<Self> {
        // Physical pixels everywhere. Fails harmlessly if the process already chose.
        // SAFETY: plain call.
        if let Err(e) =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
        {
            tracing::debug!(event = "dpi_awareness_not_set", error = %e);
        }
        Ok(Self {
            injector: inject::Injector::default(),
            capturing: AtomicBool::new(false),
        })
    }

    /// True once if the watchdog reinstalled the hooks since the last call.
    pub fn take_hook_reinstalled(&self) -> bool {
        capture::take_hook_reinstalled()
    }
}

/// Confine the cursor to the single pixel at `p` (or release with `None`).
fn clip_to(p: Option<(i32, i32)>) {
    let rect = p.map(|(x, y)| RECT {
        left: x,
        top: y,
        right: x + 1,
        bottom: y + 1,
    });
    // SAFETY: rect outlives the call; None releases the clip.
    unsafe {
        let _ = ClipCursor(rect.as_ref().map(|r| r as *const RECT));
    }
}

fn os_cursor() -> (i32, i32) {
    let mut p = POINT::default();
    // SAFETY: valid out-pointer.
    let _ = unsafe { GetCursorPos(&mut p) };
    (p.x, p.y)
}

impl Platform for WinPlatform {
    fn displays(&self) -> Vec<DisplayInfo> {
        displays::displays()
    }

    fn start_capture(&self, tx: UnboundedSender<LocalEvent>) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.capturing.swap(true, Ordering::AcqRel),
            "capture already started"
        );
        let r = capture::start(tx);
        if r.is_err() {
            self.capturing.store(false, Ordering::Release);
        }
        r
    }

    /// Hooks swallow physical input; the cursor is pinned to a 1x1 clip rect where it
    /// currently is. The engine parks it at the entry edge first (`warp`), which is how
    /// it ends up out of the way (`ShowCursor` is per-thread, so we cannot hide it).
    fn take_health_events(&self) -> Vec<&'static str> {
        if capture::take_hook_reinstalled() {
            vec!["hook_reinstalled"]
        } else {
            Vec::new()
        }
    }

    fn set_suppress(&self, on: bool) {
        let sh = &capture::SHARED;
        let p = os_cursor();
        if on {
            // Pin first: the hook must never see `suppress` with a stale pin.
            sh.points.suppress_on(&sh.suppress, p);
            clip_to(Some(p));
        } else {
            sh.points.suppress_off(&sh.suppress, p);
            clip_to(None);
        }
    }

    fn inject(&self, ev: &InputEvent) -> anyhow::Result<()> {
        self.injector.inject(ev)
    }

    fn warp(&self, pos: (f64, f64)) {
        let sh = &capture::SHARED;
        let suppressed = sh.suppress.load(Ordering::Acquire);
        let (x, y) = (pos.0.round() as i32, pos.1.round() as i32);
        if suppressed {
            // Publish the new pin, then move the 1x1 clip there (never releasing it, so
            // no physical move escapes); SetCursorPos lands inside the new clip.
            sh.points.pin((x, y));
            clip_to(Some((x, y)));
        }
        // SAFETY: plain call.
        let _ = unsafe { SetCursorPos(x, y) };
        let actual = os_cursor();
        if suppressed {
            if actual != (x, y) {
                // Clamped (e.g. off-screen target): pin where the cursor really is.
                sh.points.pin(actual);
                clip_to(Some(actual));
            }
        } else {
            sh.points.rebase(actual);
        }
    }

    fn cursor_pos(&self) -> (f64, f64) {
        let (x, y) = os_cursor();
        (x as f64, y as f64)
    }

    fn permissions(&self) -> Vec<(&'static str, bool)> {
        // Plan 1 runs in the interactive session only; the secure desktop is Plan 2.
        vec![("session_interactive", true)]
    }
}

impl Drop for WinPlatform {
    fn drop(&mut self) {
        // Never leave the user with a pinned cursor or dead hooks.
        let sh = &capture::SHARED;
        if sh.points.suppress_off(&sh.suppress, os_cursor()) {
            clip_to(None);
        }
        capture::stop();
    }
}
