//! Heartbeat liveness detection and reconnect backoff.

use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Liveness {
    limit: Duration,
    last: Option<Instant>,
}

impl Liveness {
    /// The caller must call `beat(now)` when the session is established;
    /// until the first beat `dead` is always false, so a peer that is silent
    /// from the start would otherwise never be declared dead.
    pub fn new(interval: Duration, missed_limit: u32) -> Self {
        Self {
            limit: interval.saturating_mul(missed_limit),
            last: None,
        }
    }

    pub fn beat(&mut self, now: Instant) {
        self.last = Some(now);
    }

    /// Requires an initial `beat` (see `new`).
    /// Dead once more than `interval * missed_limit` has passed since the
    /// last beat. Never dead before the first beat.
    pub fn dead(&self, now: Instant) -> bool {
        self.last
            .is_some_and(|l| now.saturating_duration_since(l) > self.limit)
    }
}

/// Reconnect delays: 0, 50, 100, 200, 400, 800 ms, then 1000 ms.
#[derive(Debug, Default, Clone)]
pub struct Backoff {
    attempt: u32,
}

impl Backoff {
    pub fn next_delay(&mut self) -> Duration {
        let ms = match self.attempt {
            0 => 0,
            n => 50u64.saturating_mul(1u64 << (n - 1).min(10)).min(1000),
        };
        self.attempt = self.attempt.saturating_add(1);
        Duration::from_millis(ms)
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    #[test]
    fn dead_after_three_missed() {
        let t = Instant::now();
        let mut l = Liveness::new(Duration::from_millis(100), 3);
        l.beat(t);
        assert!(!l.dead(t + Duration::from_millis(299)));
        assert!(l.dead(t + Duration::from_millis(301)));
    }
    #[test]
    fn never_dead_before_first_beat() {
        let l = Liveness::new(Duration::from_millis(100), 3);
        assert!(!l.dead(Instant::now() + Duration::from_secs(10)));
    }
    #[test]
    fn backoff_sequence_caps_and_resets() {
        let mut b = Backoff::default();
        let got: Vec<u64> = (0..8).map(|_| b.next_delay().as_millis() as u64).collect();
        assert_eq!(got, [0, 50, 100, 200, 400, 800, 1000, 1000]);
        b.reset();
        assert_eq!(b.next_delay(), Duration::ZERO);
    }
    #[test]
    fn backoff_does_not_overflow() {
        let mut b = Backoff::default();
        for _ in 0..1000 {
            assert!(b.next_delay() <= Duration::from_millis(1000));
        }
    }
}
