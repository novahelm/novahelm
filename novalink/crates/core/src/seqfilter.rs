//! Drops stale or duplicate motion packets and counts sequence gaps.

#[derive(Debug, Default, Clone)]
pub struct SeqFilter {
    last: Option<u64>,
    lost: u64,
}

impl SeqFilter {
    /// True only if `seq` is strictly greater than the last accepted one.
    pub fn accept(&mut self, seq: u64) -> bool {
        match self.last {
            Some(last) if seq <= last => return false,
            Some(last) => self.lost = self.lost.saturating_add(seq - last - 1),
            None => {}
        }
        self.last = Some(seq);
        true
    }

    /// Forget the high-water mark (call on reconnect, when the peer's
    /// sequence restarts). The accumulated `lost` total is kept.
    pub fn reset(&mut self) {
        self.last = None;
    }

    /// Total number of skipped sequence numbers between accepted packets.
    pub fn lost(&self) -> u64 {
        self.lost
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn seq_drops_stale_and_counts_gaps() {
        let mut f = SeqFilter::default();
        assert!(f.accept(1));
        assert!(f.accept(4));
        assert!(!f.accept(3));
        assert!(!f.accept(4));
        assert_eq!(f.lost(), 2);
    }

    #[test]
    fn reset_accepts_restarted_sequence_and_keeps_lost() {
        let mut f = SeqFilter::default();
        assert!(f.accept(1));
        assert!(f.accept(4));
        assert!(f.accept(100));
        let lost = f.lost();
        f.reset();
        assert!(f.accept(1));
        assert_eq!(f.lost(), lost);
    }
}
