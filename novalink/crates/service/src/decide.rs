//! Pure decisions for the service: which helper process should be running.

use std::time::Duration;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum HelperAction {
    Start(u32),
    Stop,
    Restart(u32),
    Nothing,
}

/// `console` is the active console session, `running` the session the helper was
/// started for, `exited` whether that helper process has ended, and `user_changed`
/// whether a user signed in or out of the console session since the helper started. The
/// helper's pipe is only open to the logon SID it saw at creation, and the session id
/// stays the same across a sign-in, so a changed user needs a fresh helper too.
pub fn helper_action(
    console: Option<u32>,
    running: Option<u32>,
    exited: bool,
    user_changed: bool,
) -> HelperAction {
    match (console, running) {
        (None, Some(_)) => HelperAction::Stop,
        (None, None) => HelperAction::Nothing,
        (Some(c), None) => HelperAction::Start(c),
        (Some(c), Some(r)) if c != r || exited || user_changed => HelperAction::Restart(c),
        (Some(_), Some(_)) => HelperAction::Nothing,
    }
}

/// Delay before restarting a helper that exited on its own: 1 s, doubling to 30 s, back
/// to 1 s once a helper has stayed up for 60 s.
#[derive(Debug)]
pub struct RestartBackoff {
    next: Duration,
}

impl RestartBackoff {
    const MIN: Duration = Duration::from_secs(1);
    const MAX: Duration = Duration::from_secs(30);
    const STABLE: Duration = Duration::from_secs(60);

    pub fn new() -> Self {
        Self { next: Self::MIN }
    }

    /// The helper ran for `uptime` and exited: how long to wait before starting another.
    pub fn after_exit(&mut self, uptime: Duration) -> Duration {
        if uptime >= Self::STABLE {
            self.next = Self::MIN;
        }
        let delay = self.next;
        self.next = (delay * 2).min(Self::MAX);
        delay
    }

    /// Starts over, for a restart the service chose to make itself.
    pub fn reset(&mut self) {
        self.next = Self::MIN;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn helper_follows_console_session() {
        assert_eq!(
            helper_action(Some(1), None, false, false),
            HelperAction::Start(1)
        );
        assert_eq!(
            helper_action(Some(1), Some(1), false, false),
            HelperAction::Nothing
        );
        assert_eq!(
            helper_action(Some(2), Some(1), false, false),
            HelperAction::Restart(2)
        );
        assert_eq!(
            helper_action(Some(1), Some(1), true, false),
            HelperAction::Restart(1)
        );
        assert_eq!(
            helper_action(None, Some(1), false, false),
            HelperAction::Stop
        );
        assert_eq!(
            helper_action(None, None, false, false),
            HelperAction::Nothing
        );
    }

    #[test]
    fn helper_restarts_when_the_user_changes() {
        assert_eq!(
            helper_action(Some(1), Some(1), false, true),
            HelperAction::Restart(1)
        );
        // Nothing to restart when no helper runs or nobody is at the console.
        assert_eq!(
            helper_action(Some(1), None, false, true),
            HelperAction::Start(1)
        );
        assert_eq!(
            helper_action(None, None, false, true),
            HelperAction::Nothing
        );
    }

    #[test]
    fn backoff_doubles_to_30s() {
        let mut b = RestartBackoff::new();
        let delays: Vec<_> = (0..7).map(|_| b.after_exit(secs(1))).collect();
        assert_eq!(
            delays,
            [1, 2, 4, 8, 16, 30, 30].map(secs).to_vec(),
            "1 s doubling, capped at 30 s"
        );
    }

    #[test]
    fn backoff_resets_after_a_stable_minute() {
        let mut b = RestartBackoff::new();
        for _ in 0..4 {
            b.after_exit(secs(1));
        }
        assert_eq!(b.after_exit(secs(59)), secs(16));
        assert_eq!(b.after_exit(secs(60)), secs(1));
        assert_eq!(b.after_exit(secs(1)), secs(2));
        b.reset();
        assert_eq!(b.after_exit(secs(1)), secs(1));
    }
}
