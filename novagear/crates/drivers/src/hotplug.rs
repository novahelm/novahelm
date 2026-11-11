//! USB hotplug events. `nusb` reports connects and disconnects from the OS, so the agent
//! doesn't have to re-list the bus on a timer. Bluetooth has no such feed here; the
//! agent polls HID for that, only while a Bluetooth device was recently around.

use futures_core::Stream;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};

pub enum Watch {
    /// One `()` per connect or disconnect. The channel closes if the watcher stops.
    Events(Receiver<()>),
    /// The OS gave no event feed; the reason is for the log.
    Unavailable(String),
}

struct Unpark(Thread);
impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Starts a thread that forwards `nusb::watch_devices()` events. The device data in an
/// event is dropped on purpose: the agent re-lists after a quiet moment instead.
pub fn watch() -> Watch {
    let mut stream = match nusb::watch_devices() {
        Ok(w) => Box::pin(w),
        Err(e) => return Watch::Unavailable(e.to_string()),
    };
    let (tx, rx) = channel();
    let spawned = thread::Builder::new()
        .name("usb-hotplug".into())
        .spawn(move || {
            let waker = Waker::from(Arc::new(Unpark(thread::current())));
            let mut cx = Context::from_waker(&waker);
            loop {
                match Pin::new(&mut stream).poll_next(&mut cx) {
                    Poll::Ready(Some(_)) => {
                        if tx.send(()).is_err() {
                            return;
                        }
                    }
                    Poll::Ready(None) => return,
                    Poll::Pending => thread::park(),
                }
            }
        });
    match spawned {
        Ok(_) => Watch::Events(rx),
        Err(e) => Watch::Unavailable(e.to_string()),
    }
}
