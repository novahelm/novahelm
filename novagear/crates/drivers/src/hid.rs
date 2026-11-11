//! The single owner of hidapi. Every enumeration and every device open in the agent goes
//! through [`with`], which runs on one dedicated thread that lives as long as the process.
//!
//! Why: hidapi's macOS backend keeps ONE global `IOHIDManager` and schedules it on the run
//! loop of whichever thread first initialises hidapi. Every later enumeration hands newly
//! seen devices (a Bluetooth controller connecting, a mouse re-plugging) to that run loop.
//! When that first thread is a short-lived one (a tokio blocking-pool thread exits after
//! 10 s idle), its run loop is freed with it, and the next device arrival calls
//! `CFRunLoopAddSource` on freed memory: a pointer-authentication trap (EXC_BREAKPOINT /
//! SIGTRAP) a minute or so after launch. The same thread also serves the `CFRunLoopRunInMode`
//! that hidapi pumps during each enumeration, so keeping the owner permanent keeps all of
//! that on one valid thread. The thread blocks on its channel: it costs nothing while idle.
//!
//! Devices opened here get their own read thread inside hidapi, so the caller may take the
//! returned `HidDevice` to any thread.

use anyhow::{Context, Result, anyhow, bail};
use hidapi::HidApi;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{OnceLock, mpsc};
use std::thread::ThreadId;

type Slot = Option<HidApi>;
type Job = Box<dyn FnOnce(&mut Slot) + Send>;

/// How many `HidApi` values were ever constructed; the owner makes exactly one.
static CONSTRUCTED: AtomicUsize = AtomicUsize::new(0);

/// The hid thread's id, set by that thread before it takes its first job.
static OWNER_THREAD: OnceLock<ThreadId> = OnceLock::new();

static OWNER: OnceLock<std::result::Result<mpsc::Sender<Job>, String>> = OnceLock::new();

fn owner() -> Result<&'static mpsc::Sender<Job>> {
    OWNER
        .get_or_init(|| {
            let (tx, rx) = mpsc::channel::<Job>();
            std::thread::Builder::new()
                .name("hid".into())
                .spawn(move || {
                    let _ = OWNER_THREAD.set(std::thread::current().id());
                    let mut api: Slot = None;
                    // Ends only when every sender is gone, which never happens: the sender
                    // lives in a static.
                    while let Ok(job) = rx.recv() {
                        job(&mut api);
                    }
                })
                .map(|_| tx)
                .map_err(|e| format!("starting the hid thread: {e}"))
        })
        .as_ref()
        .map_err(|e| anyhow!("{e}"))
}

/// Runs `f` on the hid thread against a freshly enumerated device list (one enumeration per
/// call). Blocks until it is done, so call it from a blocking context, never from the hid
/// thread itself (that is an error, not a deadlock). In tests and debug builds a panic in
/// `f` comes back as an error and leaves the thread usable; the release profile aborts on
/// panic, so there it ends the process like any other panic.
pub fn with<R: Send + 'static>(f: impl FnOnce(&HidApi) -> R + Send + 'static) -> Result<R> {
    if OWNER_THREAD.get() == Some(&std::thread::current().id()) {
        // The hid thread waiting on itself would never wake.
        bail!("hid::with called from the hid thread");
    }
    let (back_tx, back_rx) = mpsc::sync_channel::<Result<R>>(1);
    let job: Job = Box::new(move |slot| {
        let r = catch_unwind(AssertUnwindSafe(|| -> Result<R> {
            match slot.as_mut() {
                Some(api) => api.refresh_devices().context("listing HID devices")?,
                None => {
                    CONSTRUCTED.fetch_add(1, Ordering::SeqCst);
                    *slot = Some(HidApi::new().context("starting hidapi")?);
                }
            }
            Ok(f(slot.as_ref().expect("set above")))
        }))
        .unwrap_or_else(|_| Err(anyhow!("the hid thread's job panicked")));
        let _ = back_tx.send(r);
    });
    owner()?
        .send(job)
        .map_err(|_| anyhow!("the hid thread is gone"))?;
    back_rx.recv().context("the hid thread is gone")?
}

/// The hid thread's id, for tests and diagnostics.
pub fn owner_thread_id() -> Result<ThreadId> {
    with(|_| std::thread::current().id())
}

/// How many `HidApi` values were constructed in this process.
pub fn constructed() -> usize {
    CONSTRUCTED.load(Ordering::SeqCst)
}
