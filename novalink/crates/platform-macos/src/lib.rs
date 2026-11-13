//! macOS platform for NovaLink: CGEventTap capture, CGEventPost injection,
//! display enumeration and virtual-keycode <-> USB HID mapping.
//! The whole crate is empty on non-macOS targets.
#![cfg(target_os = "macos")]

pub mod capture;
pub mod displays;
mod ffi;
pub mod inject;
pub mod keymap;

use ffi::*;
use novalink_core::{InputEvent, LocalEvent, Platform};
use novalink_proto::DisplayInfo;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc::UnboundedSender;

pub struct MacPlatform {
    injector: inject::Injector,
    suppress: Arc<AtomicBool>,
    capturing: AtomicBool,
    /// Whether we currently hold a cursor-hide (hide/show calls are counted by the OS).
    hidden: AtomicBool,
}

impl MacPlatform {
    pub fn new() -> anyhow::Result<Self> {
        enable_background_cursor();
        Ok(Self {
            injector: inject::Injector::new()?,
            suppress: Arc::new(AtomicBool::new(false)),
            capturing: AtomicBool::new(false),
            hidden: AtomicBool::new(false),
        })
    }
}

impl Platform for MacPlatform {
    fn displays(&self) -> Vec<DisplayInfo> {
        displays::displays()
    }

    fn start_capture(&self, tx: UnboundedSender<LocalEvent>) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.capturing.swap(true, Ordering::AcqRel),
            "capture already started"
        );
        let r = capture::start(tx, self.suppress.clone());
        if r.is_err() {
            self.capturing.store(false, Ordering::Release);
        }
        r
    }

    fn take_health_events(&self) -> Vec<&'static str> {
        if capture::take_tap_reenabled() {
            vec!["tap_reenabled"]
        } else {
            Vec::new()
        }
    }

    fn set_suppress(&self, on: bool) {
        self.suppress.store(on, Ordering::Release);
        // SAFETY: plain CG calls; hide/show are balanced via `hidden`.
        unsafe {
            if on {
                CGAssociateMouseAndMouseCursorPosition(0);
                if !self.hidden.swap(true, Ordering::AcqRel) {
                    CGDisplayHideCursor(CGMainDisplayID());
                }
            } else {
                if self.hidden.swap(false, Ordering::AcqRel) {
                    CGDisplayShowCursor(CGMainDisplayID());
                }
                CGAssociateMouseAndMouseCursorPosition(1);
            }
        }
    }

    fn inject(&self, ev: &InputEvent) -> anyhow::Result<()> {
        self.injector.inject(ev)
    }

    fn warp(&self, pos: (f64, f64)) {
        self.injector.warp(pos);
        if !self.suppress.load(Ordering::Acquire) {
            // A warp freezes local mouse input for ~250 ms unless re-associated.
            // SAFETY: plain call.
            unsafe { CGAssociateMouseAndMouseCursorPosition(1) };
        }
    }

    fn cursor_pos(&self) -> (f64, f64) {
        self.injector.cursor_pos()
    }

    fn permissions(&self) -> Vec<(&'static str, bool)> {
        // SAFETY: plain queries.
        let (ax, im) = unsafe {
            (
                AXIsProcessTrusted(),
                IOHIDCheckAccess(IOHID_REQUEST_LISTEN_EVENT) == IOHID_ACCESS_GRANTED,
            )
        };
        vec![("accessibility", ax), ("input_monitoring", im)]
    }

    fn request_permissions(&self) {
        use core_foundation::base::TCFType;
        use core_foundation::boolean::CFBoolean;
        use core_foundation::dictionary::CFDictionary;
        use core_foundation::string::CFString;
        // SAFETY: the key is a CF constant owned by the framework (get rule); the
        // dictionary and value outlive the call.
        unsafe {
            let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt as _);
            let opts = CFDictionary::from_CFType_pairs(&[(key, CFBoolean::true_value())]);
            AXIsProcessTrustedWithOptions(opts.as_concrete_TypeRef() as *const _);
            CGRequestListenEventAccess();
        }
    }
}

impl Drop for MacPlatform {
    fn drop(&mut self) {
        // Never leave the user with a frozen/hidden cursor.
        if self.suppress.load(Ordering::Acquire) {
            self.set_suppress(false);
        }
    }
}

/// Allow this (possibly background) process to hide/set the cursor.
fn enable_background_cursor() {
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::string::CFString;
    let key = CFString::from_static_string("SetsCursorInBackground");
    // SAFETY: private but long-stable CGS call; key/value outlive it.
    let rc = unsafe {
        let conn = _CGSDefaultConnection();
        CGSSetConnectionProperty(
            conn,
            conn,
            key.as_concrete_TypeRef() as *const _,
            CFBoolean::true_value().as_concrete_TypeRef() as *const _,
        )
    };
    if rc != 0 {
        tracing::warn!(event = "background_cursor_unavailable", rc);
    }
}
