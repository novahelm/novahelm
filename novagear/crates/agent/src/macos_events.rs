//! What the Mac tells us without being asked: which app came to the front, and the
//! displays going to sleep or waking. NovaGear never polls for either; it subscribes to
//! NSWorkspace's notifications and sleeps until one arrives.
//!
//! NSWorkspace delivers those notifications through the process's *main* run loop (a run
//! loop on a thread of our own never sees them; tried, and checked by hand). So on macOS
//! `main` hands its thread to `run_main_loop`, and the agent itself runs on another
//! thread; the handlers here then run on an operation queue, off the main thread.
//!
//! Everything that touches Objective-C is in this file; the rest of the agent sees only
//! `OsEvent`. On other platforms `spawn` returns `None` and nothing ever arrives.

use tokio::sync::mpsc;

/// An OS event the mode rules and deck sleep react to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OsEvent {
    /// The bundle id of the app that just became frontmost.
    Frontmost(String),
    DisplaysAsleep,
    DisplaysAwake,
}

/// Runs `body` (the whole agent) on a thread of its own while this one serves the main
/// run loop, which is what delivers the notifications below. Returns when `body` does.
/// Off macOS it just calls `body`.
pub fn run_main_loop(body: impl FnOnce() + Send + 'static) {
    #[cfg(target_os = "macos")]
    mac::run_main_loop(body);
    #[cfg(not(target_os = "macos"))]
    body();
}

/// Keeps the subscriptions alive; dropping it unsubscribes. Never built off macOS.
#[cfg(target_os = "macos")]
pub use mac::Handle;
#[cfg(not(target_os = "macos"))]
pub struct Handle(());

/// Subscribes to the events. `None` when the platform has none to give.
pub fn spawn(tx: mpsc::UnboundedSender<OsEvent>) -> Option<Handle> {
    #[cfg(target_os = "macos")]
    {
        Some(mac::subscribe(tx))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = tx;
        None
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use super::OsEvent;
    use block2::RcBlock;
    use core_foundation::date::CFDate;
    use core_foundation::runloop::{
        CFRunLoop, CFRunLoopMode, CFRunLoopTimer, CFRunLoopTimerContext, CFRunLoopTimerRef,
        kCFRunLoopDefaultMode,
    };
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, ProtocolObject};
    use objc2_app_kit::{
        NSRunningApplication, NSWorkspace, NSWorkspaceApplicationKey,
        NSWorkspaceDidActivateApplicationNotification, NSWorkspaceScreensDidSleepNotification,
        NSWorkspaceScreensDidWakeNotification,
    };
    use objc2_foundation::{
        NSNotification, NSNotificationCenter, NSNotificationName, NSObjectProtocol,
        NSOperationQueue,
    };
    use std::ffi::c_void;
    use std::ptr::NonNull;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::mpsc;

    pub fn run_main_loop(body: impl FnOnce() + Send + 'static) {
        let main_loop = CFRunLoop::get_current();
        let done = Arc::new(AtomicBool::new(false));
        let (flag, stop_loop) = (done.clone(), main_loop.clone());
        let worker = std::thread::Builder::new()
            .name("novagear-agent".into())
            .spawn(move || {
                // A panic is already reported by the hook; the loop must still end.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
                flag.store(true, Ordering::SeqCst);
                // A timer due now, added from here: unlike a plain stop, it still works if
                // the loop has not started yet, and its callback ends the loop.
                stop_loop.add_timer(
                    &timer(CFDate::now().abs_time(), stop_current),
                    default_mode(),
                );
            })
            .expect("the agent thread starts");
        // A run loop with nothing to wait on returns at once, so park a timer that never
        // fires in practice (about 300 years); the loop then sleeps until a notification
        // or the stop timer.
        main_loop.add_timer(
            &timer(CFDate::now().abs_time() + 1e10, stop_current),
            default_mode(),
        );
        while !done.load(Ordering::SeqCst) {
            CFRunLoop::run_current();
        }
        let _ = worker.join();
    }

    fn default_mode() -> CFRunLoopMode {
        // SAFETY: the constant is valid for the program's life.
        unsafe { kCFRunLoopDefaultMode }
    }

    /// A one-shot timer calling `f`.
    fn timer(at: f64, f: extern "C" fn(CFRunLoopTimerRef, *mut c_void)) -> CFRunLoopTimer {
        let mut ctx = CFRunLoopTimerContext {
            version: 0,
            info: std::ptr::null_mut(),
            retain: None,
            release: None,
            copyDescription: None,
        };
        // The call copies the context, so a local is fine.
        CFRunLoopTimer::new(at, 0.0, 0, 0, f, &mut ctx)
    }

    extern "C" fn stop_current(_: CFRunLoopTimerRef, _: *mut c_void) {
        CFRunLoop::get_current().stop();
    }

    /// The observers, removed from the workspace's center on drop.
    pub struct Handle {
        center: Retained<NSNotificationCenter>,
        tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
        // Keeps the queue the handlers run on alive as long as they can be called.
        _queue: Retained<NSOperationQueue>,
    }

    // SAFETY: NSNotificationCenter is thread-safe, and the tokens are opaque values that
    // are only ever handed back to `removeObserver:`, which it allows from any thread.
    unsafe impl Send for Handle {}

    impl Drop for Handle {
        fn drop(&mut self) {
            for token in &self.tokens {
                // SAFETY: `token` came from this center's addObserver call.
                unsafe {
                    self.center
                        .removeObserver(AsRef::<AnyObject>::as_ref(&**token))
                };
            }
        }
    }

    pub fn subscribe(tx: mpsc::UnboundedSender<OsEvent>) -> Handle {
        let workspace = NSWorkspace::sharedWorkspace();
        // The workspace's own center, not the default one: these notifications are posted
        // only there.
        let center = workspace.notificationCenter();
        let queue = NSOperationQueue::new();
        let mut tokens = Vec::new();
        let mut watch = |name: &NSNotificationName,
                         f: Box<dyn Fn(&NSNotification) + Send + Sync>| {
            let block = RcBlock::new(move |n: NonNull<NSNotification>| {
                // SAFETY: the center passes a live notification for the call's duration.
                f(unsafe { n.as_ref() });
            });
            // SAFETY: no object filter; the block only sends on a channel, so running it
            // on the queue's thread is fine.
            tokens.push(unsafe {
                center.addObserverForName_object_queue_usingBlock(
                    Some(name),
                    None,
                    Some(&queue),
                    &block,
                )
            });
        };
        let t = tx.clone();
        // SAFETY (the three statics): constants AppKit exports.
        watch(
            unsafe { NSWorkspaceDidActivateApplicationNotification },
            Box::new(move |n| {
                if let Some(id) = activated_bundle_id(n) {
                    let _ = t.send(OsEvent::Frontmost(id));
                }
            }),
        );
        let t = tx.clone();
        watch(
            unsafe { NSWorkspaceScreensDidSleepNotification },
            Box::new(move |_| {
                let _ = t.send(OsEvent::DisplaysAsleep);
            }),
        );
        let t = tx.clone();
        watch(
            unsafe { NSWorkspaceScreensDidWakeNotification },
            Box::new(move |_| {
                let _ = t.send(OsEvent::DisplaysAwake);
            }),
        );
        // Whoever is in front right now, so a rule is right before the first switch.
        if let Some(id) = workspace
            .frontmostApplication()
            .and_then(|a| a.bundleIdentifier())
        {
            let _ = tx.send(OsEvent::Frontmost(id.to_string()));
        }
        Handle {
            center,
            tokens,
            _queue: queue,
        }
    }

    /// The bundle id of the app a didActivateApplication notification is about.
    fn activated_bundle_id(n: &NSNotification) -> Option<String> {
        let info = n.userInfo()?;
        // SAFETY: AppKit documents this key as an NSString.
        let key: &AnyObject = unsafe { NSWorkspaceApplicationKey }.as_ref();
        let app = info
            .objectForKey(key)?
            .downcast::<NSRunningApplication>()
            .ok()?;
        Some(app.bundleIdentifier()?.to_string())
    }
}

#[cfg(test)]
mod tests {
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn other_platforms_have_no_observer() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(super::spawn(tx).is_none());
    }
}
