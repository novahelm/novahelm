//! Peer clock offset estimate (NTP-style, lowest-RTT sample wins).

use std::collections::VecDeque;

const KEEP: usize = 64;

#[derive(Debug, Default, Clone)]
pub struct ClockEstimator {
    /// (rtt_us, offset_us) of the most recent samples.
    samples: VecDeque<(u64, i64)>,
}

impl ClockEstimator {
    /// `sent_local_us` and `recv_local_us` are local clock readings at request
    /// send and reply receipt; `remote_us` is the peer clock when it replied.
    /// Assumes symmetric delay, so the peer stamped at the local midpoint.
    pub fn sample(&mut self, sent_local_us: u64, remote_us: u64, recv_local_us: u64) {
        // A reply that "arrived before it was sent" is nonsense; ignore it.
        let Some(rtt) = recv_local_us.checked_sub(sent_local_us) else {
            return;
        };
        let mid = sent_local_us as i128 + (rtt / 2) as i128;
        let offset = (remote_us as i128 - mid).clamp(i64::MIN as i128, i64::MAX as i128) as i64;
        if self.samples.len() == KEEP {
            self.samples.pop_front();
        }
        self.samples.push_back((rtt, offset));
    }

    fn best(&self) -> Option<(u64, i64)> {
        self.samples.iter().copied().min_by_key(|s| s.0)
    }

    /// remote − local, microseconds.
    pub fn offset_us(&self) -> Option<i64> {
        self.best().map(|s| s.1)
    }

    pub fn rtt_us(&self) -> Option<u64> {
        self.best().map(|s| s.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clock_offset_symmetric_delay() {
        let mut c = ClockEstimator::default();
        assert_eq!(c.offset_us(), None);
        // remote stamped at the midpoint of local send/recv (3_000) + 1s
        c.sample(1_000, 1_000_000 + 3_000, 5_000); // rtt 4ms
        c.sample(10_000, 1_000_000 + 14_000, 18_000); // rtt 8ms (worse, ignored)
        assert_eq!(c.offset_us(), Some(1_000_000));
        assert_eq!(c.rtt_us(), Some(4_000));
    }
    #[test]
    fn recv_before_sent_is_ignored() {
        let mut c = ClockEstimator::default();
        c.sample(5_000, 123, 1_000);
        assert_eq!(c.offset_us(), None);
        assert_eq!(c.rtt_us(), None);
        c.sample(1_000, 11_000, 5_000); // rtt 4000, offset 11_000 - 3_000
        c.sample(9_000, 0, 2_000);
        assert_eq!(c.rtt_us(), Some(4_000));
        assert_eq!(c.offset_us(), Some(8_000));
    }

    #[test]
    fn negative_offset_when_remote_behind() {
        let mut c = ClockEstimator::default();
        c.sample(10_000, 2_000, 14_000); // midpoint 12_000, remote 2_000
        assert_eq!(c.offset_us(), Some(-10_000));
    }

    #[test]
    fn old_best_sample_ages_out_after_64() {
        let mut c = ClockEstimator::default();
        c.sample(0, 1_000, 100); // rtt 100, offset 950
        for i in 0..64u64 {
            c.sample(i * 1000, i * 1000 + 500 + 7, i * 1000 + 1000); // rtt 1000, offset 7
        }
        assert_eq!(c.rtt_us(), Some(1_000));
        assert_eq!(c.offset_us(), Some(7));
    }
}
