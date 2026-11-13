//! Hook watchdog. Windows silently removes a low-level hook whose callback times out
//! (or in some session transitions), with no notification. We prove the hook is alive by
//! injecting a tagged no-op mouse move every 250 ms and requiring the hook to see it
//! within 500 ms; otherwise the hooks are reinstalled. Probes are only sent while
//! suppressing (injected input resets the idle timer); the rest of the time `Passive`
//! compares Raw Input against the hooks without injecting anything.

use std::time::{Duration, Instant};

/// How often a probe is sent.
pub const PROBE_INTERVAL: Duration = Duration::from_millis(250);
/// How long a probe may go unseen before the hook is presumed dead.
pub const PROBE_DEADLINE: Duration = Duration::from_millis(500);

#[derive(Debug, Default)]
pub struct State {
    outstanding: Option<Instant>,
}

impl State {
    /// A probe was injected at `now`. A still-unseen older probe keeps its time.
    pub fn probe_sent(&mut self, now: Instant) {
        self.outstanding.get_or_insert(now);
    }

    /// The hook saw a probe.
    pub fn probe_seen(&mut self, _now: Instant) {
        self.outstanding = None;
    }

    pub fn has_outstanding(&self) -> bool {
        self.outstanding.is_some()
    }

    /// True when a probe has gone unseen for longer than the deadline.
    pub fn needs_reinstall(&self, now: Instant) -> bool {
        self.outstanding
            .is_some_and(|t| now.saturating_duration_since(t) > PROBE_DEADLINE)
    }

    /// Forget the outstanding probe (after reinstalling the hooks).
    pub fn reset(&mut self) {
        self.outstanding = None;
    }
}

/// Passive detection: Raw Input is not affected by hook removal, so if it saw physical
/// input of a class recently while that class's hook saw nothing, the hook is dead.
/// This needs no injected input (so it never keeps the PC awake). Times are `mono_us`.
#[derive(Debug, Default)]
pub struct Passive {
    raw: Option<u64>,
    hook: Option<u64>,
}

/// Ordering slack between Raw Input and the hook for the same physical event.
pub const PASSIVE_GRACE_US: u64 = 100_000;
/// Raw input older than this says nothing about the hook right now.
pub const PASSIVE_WINDOW_US: u64 = 500_000;

impl Passive {
    pub fn raw_seen(&mut self, now_us: u64) {
        self.raw = Some(now_us);
    }

    pub fn hook_seen(&mut self, now_us: u64) {
        self.hook = Some(now_us);
    }

    pub fn needs_reinstall(&self, now_us: u64) -> bool {
        let Some(r) = self.raw else { return false };
        let age = now_us.saturating_sub(r);
        if age <= PASSIVE_GRACE_US || age > PASSIVE_WINDOW_US {
            return false;
        }
        !self.hook.is_some_and(|h| h + PASSIVE_GRACE_US >= r)
    }

    /// Forget the raw observation (after reinstalling the hooks).
    pub fn reset(&mut self, now_us: u64) {
        self.raw = None;
        self.hook = Some(now_us);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn watchdog_reinstall_decision() {
        let mut w = State::default();
        let t = Instant::now();
        w.probe_sent(t);
        assert!(!w.needs_reinstall(t + Duration::from_millis(400)));
        assert!(w.needs_reinstall(t + Duration::from_millis(600)));
        w.probe_seen(t + Duration::from_millis(610));
        assert!(!w.needs_reinstall(t + Duration::from_millis(700)));
    }

    #[test]
    fn no_probe_outstanding_never_reinstalls() {
        let w = State::default();
        assert!(!w.needs_reinstall(Instant::now() + Duration::from_secs(60)));
    }

    #[test]
    fn resend_does_not_extend_the_deadline() {
        let mut w = State::default();
        let t = Instant::now();
        w.probe_sent(t);
        w.probe_sent(t + Duration::from_millis(300));
        assert!(w.needs_reinstall(t + Duration::from_millis(600)));
    }

    #[test]
    fn reset_clears_the_outstanding_probe() {
        let mut w = State::default();
        let t = Instant::now();
        w.probe_sent(t);
        w.reset();
        assert!(!w.has_outstanding());
        assert!(!w.needs_reinstall(t + Duration::from_secs(5)));
    }

    #[test]
    fn passive_raw_seen_hook_silent_reinstalls_after_grace() {
        let mut p = Passive::default();
        p.raw_seen(1_000_000);
        assert!(!p.needs_reinstall(1_050_000)); // inside the grace
        assert!(p.needs_reinstall(1_200_000)); // hook silent > grace
    }

    #[test]
    fn passive_raw_seen_and_hook_seen_is_fine() {
        let mut p = Passive::default();
        p.raw_seen(1_000_000);
        p.hook_seen(1_020_000);
        assert!(!p.needs_reinstall(1_300_000));
        // Hook may run slightly before raw input for the same event.
        let mut p = Passive::default();
        p.hook_seen(1_000_000);
        p.raw_seen(1_040_000);
        assert!(!p.needs_reinstall(1_300_000));
    }

    #[test]
    fn passive_no_raw_means_no_decision() {
        let mut p = Passive::default();
        assert!(!p.needs_reinstall(9_000_000));
        p.hook_seen(1_000);
        assert!(!p.needs_reinstall(9_000_000));
    }

    #[test]
    fn passive_stale_raw_is_ignored_and_reset_clears() {
        let mut p = Passive::default();
        p.raw_seen(1_000_000);
        assert!(!p.needs_reinstall(1_000_000 + 600_000)); // older than 500 ms: idle now
        let mut p = Passive::default();
        p.raw_seen(1_000_000);
        p.reset(1_300_000);
        assert!(!p.needs_reinstall(1_400_000));
    }
}
