//! Sending Wake-on-LAN bursts on the engine's behalf.

use novalink_net::wol;

/// Sends a magic packet to each address. The engine calls it from its event loop, so it
/// must not block.
pub trait Waker: Send + Sync + 'static {
    fn wake(&self, macs: Vec<[u8; 6]>);
}

/// Broadcasts for real, on a blocking thread because the send takes a moment.
pub struct NetWaker;

impl Waker for NetWaker {
    fn wake(&self, macs: Vec<[u8; 6]>) {
        drop(tokio::task::spawn_blocking(move || {
            match wol::send_wake(&macs) {
                Ok(0) => tracing::error!(count = macs.len(), "wake burst: every send failed"),
                Ok(n) => tracing::info!(sent = n, "wake burst sent"),
                Err(e) => tracing::warn!("wake burst failed: {e:#}"),
            }
        }));
    }
}

/// Records what it was asked to send instead of sending it.
#[cfg(feature = "fake")]
#[derive(Clone, Default)]
pub struct FakeWaker {
    sent: std::sync::Arc<std::sync::Mutex<Vec<Vec<[u8; 6]>>>>,
}

#[cfg(feature = "fake")]
impl FakeWaker {
    pub fn sent(&self) -> Vec<Vec<[u8; 6]>> {
        self.sent.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[cfg(feature = "fake")]
impl Waker for FakeWaker {
    fn wake(&self, macs: Vec<[u8; 6]>) {
        self.sent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(macs);
    }
}
