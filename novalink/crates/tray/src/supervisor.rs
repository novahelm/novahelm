//! Restart pacing for the agent the tray supervises.

use std::time::Duration;

const STEPS_MS: [u64; 6] = [500, 1_000, 2_000, 4_000, 8_000, 10_000];

/// How long the agent must stay up before the next failure starts the backoff over.
pub const STABLE_AFTER: Duration = Duration::from_secs(30);

/// Creating the tray icon at login can fail while the taskbar isn't ready: retry this often
/// for this long before giving up.
pub const TRAY_RETRY_EVERY: Duration = Duration::from_secs(2);
pub const TRAY_RETRY_FOR: Duration = Duration::from_secs(60);

/// Delays between agent restarts: 0.5, 1, 2, 4, 8, then 10 seconds for every later attempt.
#[derive(Debug, Default)]
pub struct Backoff {
    attempt: usize,
}

impl Backoff {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn next_delay(&mut self) -> Duration {
        let ms = STEPS_MS[self.attempt.min(STEPS_MS.len() - 1)];
        self.attempt = self.attempt.saturating_add(1);
        Duration::from_millis(ms)
    }

    /// Call once the agent has stayed up for `STABLE_AFTER`.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supervisor_backoff_caps_and_resets() {
        let mut b = Backoff::new();
        let secs: Vec<f64> = (0..7).map(|_| b.next_delay().as_secs_f64()).collect();
        assert_eq!(secs, [0.5, 1.0, 2.0, 4.0, 8.0, 10.0, 10.0]);
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_millis(500));
    }
}
