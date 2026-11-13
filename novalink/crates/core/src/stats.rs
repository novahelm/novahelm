//! Rolling latency percentiles over time-bucketed HDR histograms.

use hdrhistogram::Histogram;
use serde::Serialize;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    Minute,
    Hour,
    Day,
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize)]
pub struct Percentiles {
    pub count: u64,
    pub p50_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
}

/// A ring of `N` buckets, each covering `unit_secs`.
struct Ring<const N: usize> {
    unit_secs: u64,
    slots: [Option<(u64, Histogram<u64>)>; N],
}

impl<const N: usize> Ring<N> {
    fn new(unit_secs: u64) -> Self {
        Self {
            unit_secs,
            slots: std::array::from_fn(|_| None),
        }
    }

    fn record(&mut self, secs: u64, us: u64) {
        let idx = secs / self.unit_secs;
        let slot = &mut self.slots[(idx % N as u64) as usize];
        if slot.as_ref().is_none_or(|(i, _)| *i != idx) {
            *slot = Some((idx, Histogram::new(3).expect("valid sigfig")));
        }
        if let Some((_, h)) = slot {
            let _ = h.record(us.max(1));
        }
    }

    fn merge(&self, secs: u64, into: &mut Histogram<u64>) {
        let cur = secs / self.unit_secs;
        let oldest = (cur + 1).saturating_sub(N as u64);
        for (idx, h) in self.slots.iter().flatten() {
            if (oldest..=cur).contains(idx) {
                let _ = into.add(h);
            }
        }
    }
}

pub struct LatencyStats {
    base: Option<Instant>,
    seconds: Ring<60>,
    minutes: Ring<60>,
    hours: Ring<24>,
}

impl Default for LatencyStats {
    fn default() -> Self {
        Self {
            base: None,
            seconds: Ring::new(1),
            minutes: Ring::new(60),
            hours: Ring::new(3600),
        }
    }
}

impl LatencyStats {
    pub fn record(&mut self, now: Instant, us: u64) {
        let base = *self.base.get_or_insert(now);
        let secs = now.saturating_duration_since(base).as_secs();
        self.seconds.record(secs, us);
        self.minutes.record(secs, us);
        self.hours.record(secs, us);
    }

    pub fn snapshot(&self, now: Instant, window: Window) -> Percentiles {
        let Some(base) = self.base else {
            return Percentiles::default();
        };
        let secs = now.saturating_duration_since(base).as_secs();
        let mut h = Histogram::<u64>::new(3).expect("valid sigfig");
        match window {
            Window::Minute => self.seconds.merge(secs, &mut h),
            Window::Hour => self.minutes.merge(secs, &mut h),
            Window::Day => self.hours.merge(secs, &mut h),
        }
        if h.is_empty() {
            return Percentiles::default();
        }
        Percentiles {
            count: h.len(),
            p50_us: h.value_at_quantile(0.50),
            p95_us: h.value_at_quantile(0.95),
            p99_us: h.value_at_quantile(0.99),
            max_us: h.max(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    #[test]
    fn percentiles_and_window_expiry() {
        let t = Instant::now();
        let mut s = LatencyStats::default();
        for ms in 1..=100 {
            s.record(t, ms * 1000);
        }
        let p = s.snapshot(t, Window::Minute);
        assert_eq!(p.count, 100);
        assert!((98_000..=100_000).contains(&p.p99_us), "{p:?}");
        assert_eq!(
            s.snapshot(t + Duration::from_secs(61), Window::Minute)
                .count,
            0
        );
        assert_eq!(
            s.snapshot(t + Duration::from_secs(61), Window::Hour).count,
            100
        );
        assert_eq!(
            s.snapshot(t + Duration::from_secs(3601), Window::Hour)
                .count,
            0
        );
        assert_eq!(
            s.snapshot(t + Duration::from_secs(3601), Window::Day).count,
            100
        );
    }
    #[test]
    fn empty_snapshot_is_default() {
        let s = LatencyStats::default();
        assert_eq!(
            s.snapshot(Instant::now(), Window::Day),
            Percentiles::default()
        );
    }
}
