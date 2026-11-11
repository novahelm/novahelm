//! Deck sleep: after `deck_sleep_min` idle minutes, or when the displays sleep, the deck
//! goes dark. The first input afterwards only wakes it.

use novagear_drivers::streamdeck::{DIALS, Input, KEYS};
use std::time::Duration;
use tokio::time::Instant;

/// What the deck should do with an input.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Handle it as usual.
    Pass,
    /// The release of the press that woke the deck: do nothing with it.
    Swallow,
    /// It woke the deck: redraw, and do nothing else with it.
    Wake,
}

#[derive(Debug, Default)]
pub struct SleepTimer {
    idle: Option<Duration>,
    last_input: Option<Instant>,
    asleep: bool,
    /// Keys and dials whose press woke the deck; their release is swallowed too, so a
    /// hold or an approval cannot start from, or be cut short by, the waking press.
    up_keys: [bool; KEYS],
    up_dials: [bool; DIALS],
}

impl SleepTimer {
    /// Applies the configured idle minutes (0 never sleeps). Turning sleep off wakes the
    /// deck; it reports whether that happened.
    pub fn configure(&mut self, minutes: u32, now: Instant) -> bool {
        self.idle = (minutes > 0).then(|| Duration::from_secs(u64::from(minutes) * 60));
        self.last_input.get_or_insert(now);
        if self.idle.is_none() && self.asleep {
            self.wake(now);
            return true;
        }
        false
    }

    pub fn asleep(&self) -> bool {
        self.asleep
    }

    /// Whether it is time to sleep.
    pub fn due(&self, now: Instant) -> bool {
        !self.asleep && self.deadline().is_some_and(|d| now >= d)
    }

    /// When it will sleep if nothing happens; `None` when it never will.
    pub fn deadline(&self) -> Option<Instant> {
        if self.asleep {
            return None;
        }
        Some(self.last_input? + self.idle?)
    }

    pub fn sleep(&mut self) {
        if self.idle.is_some() {
            self.asleep = true;
        }
    }

    pub fn wake(&mut self, now: Instant) {
        self.asleep = false;
        self.last_input = Some(now);
    }

    /// Forgets swallowed releases still owed (the deck went away with a key down).
    pub fn clear_swallowed(&mut self) {
        self.up_keys = [false; KEYS];
        self.up_dials = [false; DIALS];
    }

    /// Notes an input and says what to do with it.
    pub fn input(&mut self, input: &Input, now: Instant) -> Verdict {
        let (key, dial, down) = match input {
            Input::Key { index, down } => (Some(*index), None, *down),
            Input::DialPress { index, down } => (None, Some(*index), *down),
            _ => (None, None, true),
        };
        if self.asleep {
            self.wake(now);
            if down {
                if let Some(i) = key.filter(|i| *i < KEYS) {
                    self.up_keys[i] = true;
                }
                if let Some(i) = dial.filter(|i| *i < DIALS) {
                    self.up_dials[i] = true;
                }
            }
            return Verdict::Wake;
        }
        self.last_input = Some(now);
        // A press that gets through means any earlier release was lost: forget it, so
        // this press's own release is not swallowed (it could cut a hold short or turn a
        // tap into a completed hold).
        if down {
            if let Some(i) = key.filter(|i| *i < KEYS) {
                self.up_keys[i] = false;
            }
            if let Some(i) = dial.filter(|i| *i < DIALS) {
                self.up_dials[i] = false;
            }
        }
        if !down {
            if let Some(i) = key.filter(|i| *i < KEYS)
                && std::mem::take(&mut self.up_keys[i])
            {
                return Verdict::Swallow;
            }
            if let Some(i) = dial.filter(|i| *i < DIALS)
                && std::mem::take(&mut self.up_dials[i])
            {
                return Verdict::Swallow;
            }
        }
        Verdict::Pass
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(index: usize, down: bool) -> Input {
        Input::Key { index, down }
    }

    fn timer(min: u32, now: Instant) -> SleepTimer {
        let mut t = SleepTimer::default();
        t.configure(min, now);
        t
    }

    #[test]
    fn sleeps_after_the_idle_minutes() {
        let t0 = Instant::now();
        let t = timer(1, t0);
        assert!(!t.due(t0 + Duration::from_secs(59)));
        assert!(t.due(t0 + Duration::from_secs(61)));
        assert_eq!(t.deadline(), Some(t0 + Duration::from_secs(60)));
    }

    #[test]
    fn input_pushes_the_deadline_out() {
        let t0 = Instant::now();
        let mut t = timer(1, t0);
        let t1 = t0 + Duration::from_secs(50);
        assert_eq!(t.input(&key(0, true), t1), Verdict::Pass);
        assert!(!t.due(t0 + Duration::from_secs(61)));
        assert!(t.due(t1 + Duration::from_secs(61)));
    }

    #[test]
    fn zero_minutes_never_sleeps() {
        let t0 = Instant::now();
        let mut t = timer(0, t0);
        assert!(!t.due(t0 + Duration::from_secs(86_400)));
        t.sleep();
        assert!(!t.asleep());
        assert_eq!(t.deadline(), None);
    }

    #[test]
    fn the_waking_press_and_its_release_are_swallowed() {
        let t0 = Instant::now();
        let mut t = timer(1, t0);
        t.sleep();
        assert!(t.asleep());
        let later = t0 + Duration::from_secs(120);
        assert_eq!(t.input(&key(2, true), later), Verdict::Wake);
        assert!(!t.asleep());
        // Its release is swallowed once; another key's is not.
        assert_eq!(t.input(&key(3, false), later), Verdict::Pass);
        assert_eq!(t.input(&key(2, false), later), Verdict::Swallow);
        assert_eq!(t.input(&key(2, false), later), Verdict::Pass);
        assert_eq!(
            t.input(
                &Input::DialPress {
                    index: 1,
                    down: true
                },
                later
            ),
            Verdict::Pass
        );
    }

    #[test]
    fn a_lost_release_does_not_swallow_the_next_real_one() {
        let t0 = Instant::now();
        let mut t = timer(1, t0);
        t.sleep();
        assert_eq!(t.input(&key(2, true), t0), Verdict::Wake);
        // The release never comes; the next press and its release both pass.
        assert_eq!(t.input(&key(2, true), t0), Verdict::Pass);
        assert_eq!(t.input(&key(2, false), t0), Verdict::Pass);
        // And clearing on a deck reopening drops what is owed.
        t.sleep();
        t.input(&key(4, true), t0);
        t.clear_swallowed();
        assert_eq!(t.input(&key(4, false), t0), Verdict::Pass);
    }

    #[test]
    fn a_turn_wakes_without_leaving_a_release_behind() {
        let t0 = Instant::now();
        let mut t = timer(1, t0);
        t.sleep();
        assert_eq!(
            t.input(
                &Input::DialTurn {
                    index: 0,
                    detents: 1
                },
                t0
            ),
            Verdict::Wake
        );
        assert_eq!(t.input(&key(0, false), t0), Verdict::Pass);
    }

    #[test]
    fn turning_sleep_off_wakes_the_deck() {
        let t0 = Instant::now();
        let mut t = timer(1, t0);
        t.sleep();
        assert!(t.configure(0, t0));
        assert!(!t.asleep());
    }
}
