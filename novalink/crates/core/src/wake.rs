//! Decides when to send a Wake-on-LAN burst to a sleeping peer and when to
//! give up, and which edge to re-check once the peer reconnects.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use novalink_proto::{DeviceId, Edge};

#[derive(Debug, Clone, Copy)]
struct Waking {
    edge: Edge,
    /// First push of this wake; the deadline counts from here.
    started: Instant,
    last_burst: Instant,
}

#[derive(Debug, Clone)]
pub struct WakeTracker {
    deadline: Duration,
    gap: Duration,
    waking: HashMap<DeviceId, Waking>,
    /// Peers that did not come up, with the instant the wake gave up.
    failed: HashMap<DeviceId, Instant>,
}

impl WakeTracker {
    /// Production values: 20 s deadline, 5 s gap.
    pub fn new(deadline: Duration, gap: Duration) -> Self {
        Self {
            deadline,
            gap,
            waking: HashMap::new(),
            failed: HashMap::new(),
        }
    }

    /// The cursor is pushing against `edge` towards a sleeping `peer`.
    /// True means send a burst now: at most one per `gap` since the last
    /// burst, and none while the peer is failed until `gap` has passed since
    /// the failure, after which a push starts a fresh wake.
    pub fn on_edge(&mut self, peer: DeviceId, edge: Edge, now: Instant) -> bool {
        if let Some(&failed_at) = self.failed.get(&peer) {
            if now.saturating_duration_since(failed_at) < self.gap {
                return false;
            }
            self.failed.remove(&peer);
        }
        match self.waking.get_mut(&peer) {
            Some(w) => {
                w.edge = edge;
                if now.saturating_duration_since(w.last_burst) < self.gap {
                    return false;
                }
                w.last_burst = now;
                true
            }
            None => {
                self.waking.insert(
                    peer,
                    Waking {
                        edge,
                        started: now,
                        last_burst: now,
                    },
                );
                true
            }
        }
    }

    /// The peer connected. If it was waking, returns the edge to re-check and
    /// clears both the waking and failed states.
    pub fn on_connected(&mut self, peer: DeviceId) -> Option<Edge> {
        self.failed.remove(&peer);
        self.waking.remove(&peer).map(|w| w.edge)
    }

    /// Peers whose deadline has passed; they move to failed.
    pub fn on_tick(&mut self, now: Instant) -> Vec<DeviceId> {
        let deadline = self.deadline;
        let expired: Vec<(DeviceId, Instant)> = self
            .waking
            .iter()
            .filter(|(_, w)| now.saturating_duration_since(w.started) >= deadline)
            .map(|(&p, w)| (p, w.started + deadline))
            .collect();
        for &(peer, failed_at) in &expired {
            self.waking.remove(&peer);
            self.failed.insert(peer, failed_at);
        }
        expired.into_iter().map(|(p, _)| p).collect()
    }

    pub fn waking(&self, peer: DeviceId) -> bool {
        self.waking.contains_key(&peer)
    }

    pub fn failed(&self, peer: DeviceId) -> bool {
        self.failed.contains_key(&peer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pc() -> DeviceId {
        DeviceId([7; 32])
    }

    fn tracker() -> WakeTracker {
        WakeTracker::new(Duration::from_secs(20), Duration::from_secs(5))
    }

    #[test]
    fn first_push_sends_and_connect_returns_edge() {
        let t0 = Instant::now();
        let mut w = tracker();
        assert!(w.on_edge(pc(), Edge::Right, t0));
        assert!(w.waking(pc()));
        assert_eq!(w.on_connected(pc()), Some(Edge::Right));
        assert!(!w.waking(pc()) && !w.failed(pc()));
        assert_eq!(w.on_connected(pc()), None);
    }

    #[test]
    fn rests_at_edge_sends_one_burst_per_gap_and_stops_after_failure() {
        let t0 = Instant::now();
        let mut w = tracker();
        let sends = (0..200)
            .filter(|i| w.on_edge(pc(), Edge::Right, t0 + Duration::from_millis(100 * i)))
            .count();
        assert_eq!(sends, 4); // t = 0, 5, 10, 15 s
        assert_eq!(w.on_tick(t0 + Duration::from_secs(20)), vec![pc()]);
        assert!(w.failed(pc()) && !w.waking(pc()));
        assert!(!w.on_edge(pc(), Edge::Right, t0 + Duration::from_secs(21)));
        assert!(w.on_edge(pc(), Edge::Right, t0 + Duration::from_secs(25)));
    }

    #[test]
    fn tick_before_deadline_fails_nothing() {
        let t0 = Instant::now();
        let mut w = tracker();
        w.on_edge(pc(), Edge::Right, t0);
        assert!(w.on_tick(t0 + Duration::from_secs(19)).is_empty());
    }

    #[test]
    fn deadline_counts_from_first_push_not_latest() {
        let t0 = Instant::now();
        let mut w = tracker();
        w.on_edge(pc(), Edge::Right, t0);
        assert!(w.on_edge(pc(), Edge::Right, t0 + Duration::from_secs(15)));
        assert_eq!(w.on_tick(t0 + Duration::from_secs(20)), vec![pc()]);
    }

    #[test]
    fn fresh_wake_after_failure_gets_new_deadline() {
        let t0 = Instant::now();
        let mut w = tracker();
        w.on_edge(pc(), Edge::Right, t0);
        w.on_tick(t0 + Duration::from_secs(20));
        assert!(w.on_edge(pc(), Edge::Left, t0 + Duration::from_secs(26)));
        assert!(!w.failed(pc()) && w.waking(pc()));
        assert!(w.on_tick(t0 + Duration::from_secs(45)).is_empty());
        assert_eq!(w.on_tick(t0 + Duration::from_secs(46)), vec![pc()]);
        assert_eq!(w.on_connected(pc()), None);
    }

    #[test]
    fn connect_clears_failed_state() {
        let t0 = Instant::now();
        let mut w = tracker();
        w.on_edge(pc(), Edge::Right, t0);
        w.on_tick(t0 + Duration::from_secs(20));
        assert_eq!(w.on_connected(pc()), None);
        assert!(!w.failed(pc()));
    }
}
