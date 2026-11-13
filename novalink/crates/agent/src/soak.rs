//! Soak mode: synthetic input through the engine's normal remote path for hours, with
//! stuck-key accounting after every `Leave`.
//!
//! The driver crosses to `peer` and feeds [`LocalEvent`]s into the engine exactly as the
//! platform capture would (motion at 1 kHz in bursts, random key chords, periodic return
//! crossings). Nothing here injects into this machine. The driven side should run with
//! `--dry-inject` so a long soak never types into real apps. After each `Leave` the
//! driven side replies `Control::HeldReport { count }` with what it still holds after
//! releasing; any non-zero count is a stuck key.

use crate::engine::EngineHandle;
use novalink_core::time::mono_us;
use novalink_core::{InputEvent, LocalEvent, Percentiles};
use novalink_proto::{DeviceId, Edge, EntryPoint, keys};
use serde::Serialize;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long to wait for the driven side's report after a `Leave`.
const REPORT_WAIT: Duration = Duration::from_secs(2);
/// How long, at the very end, to wait for reports still in flight.
const FINAL_WAIT: Duration = Duration::from_secs(3);
/// A claim by another machine is only undone (a reconnect handover) this soon after
/// the session came up; later it is a real user and the soak ends.
const RECLAIM_WINDOW: Duration = Duration::from_secs(2);
const CROSS_RETRY: Duration = Duration::from_millis(100);
const TICK: Duration = Duration::from_millis(1);
const MAX_HOURS: f64 = 24.0 * 30.0;

#[derive(Clone, Debug)]
pub struct SoakConfig {
    /// A return-crossing (a `Leave`) every this long, uniformly in the range.
    pub return_every: (Duration, Duration),
    /// A key chord every this long.
    pub chord_every: (Duration, Duration),
    /// Pause between motion bursts.
    pub burst_gap: (Duration, Duration),
    /// Events (1 ms apart) in a motion burst.
    pub burst_len: (u32, u32),
}

impl Default for SoakConfig {
    fn default() -> Self {
        Self {
            return_every: (Duration::from_secs(10), Duration::from_secs(60)),
            chord_every: (Duration::from_millis(50), Duration::from_millis(500)),
            burst_gap: (Duration::from_millis(50), Duration::from_millis(400)),
            burst_len: (100, 400),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct SoakReport {
    #[serde(serialize_with = "secs")]
    pub duration: Duration,
    /// Times the engine moved focus to the peer.
    pub crossings: u64,
    /// Return crossings sent (each makes the peer release and report).
    pub leaves: u64,
    /// `HeldReport`s received during the run.
    pub held_reports: u64,
    /// Key events (downs and ups) sent.
    pub keys_sent: u64,
    /// Keys the peer held when each `Leave` arrived (it releases them).
    pub held_before_release_total: u64,
    /// Keys the peer still held a settle period after releasing: stuck.
    pub stuck_after_release: u64,
    /// Leaves whose session stayed up but that got no report.
    pub unanswered_leaves: u64,
    /// Leaves in flight when the connection dropped (the peer's own loss release
    /// covers those; informational).
    pub leaves_lost_to_disconnect: u64,
    pub outcome: SoakOutcome,
    pub abort_reason: Option<String>,
    pub safety_releases: u64,
    pub reconnects: u64,
    /// The driver's own hour window for this peer. Datagrams flow driver to peer, so the
    /// authoritative capture-to-inject latency is `novalink metrics` on the peer.
    pub latency_hour: Percentiles,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SoakOutcome {
    Completed,
    Stopped,
    Aborted,
}

fn secs<S: serde::Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_f64(d.as_secs_f64())
}

/// Live counters, readable while a run is in progress.
#[derive(Default, Debug)]
pub struct Progress {
    pub crossings: AtomicU64,
    pub leaves: AtomicU64,
    pub keys_sent: AtomicU64,
    /// Set to end the run early, after the final accounting pass.
    pub stop: AtomicBool,
}

/// xorshift64*: plenty for picking intervals and keys.
struct Rng(u64);

impl Rng {
    fn seeded() -> Self {
        let mut b = [0u8; 8];
        rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut b);
        Self(u64::from_le_bytes(b) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn between(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo {
            lo
        } else {
            lo + self.next() % (hi - lo + 1)
        }
    }

    fn duration(&mut self, r: (Duration, Duration)) -> Duration {
        Duration::from_micros(self.between(r.0.as_micros() as u64, r.1.as_micros() as u64))
    }
}

const MODIFIERS: [u16; 4] = [
    keys::LEFT_CTRL,
    keys::LEFT_SHIFT,
    keys::LEFT_ALT,
    keys::LEFT_GUI,
];

/// Events of one chord as `(offset from start, event)`: modifiers down, key down, hold,
/// key up, modifiers up.
fn chord(rng: &mut Rng) -> Vec<(Duration, InputEvent)> {
    let mut mods: Vec<u16> = MODIFIERS
        .iter()
        .copied()
        .filter(|_| rng.next().is_multiple_of(4))
        .collect();
    mods.truncate(3);
    let key = rng.between(u64::from(keys::A), u64::from(keys::N0)) as u16;
    let hold = rng.between(10, 80);
    let ev = |usage, down| InputEvent::Key { usage, down };
    let mut out = Vec::new();
    let mut t = 0;
    for &m in &mods {
        out.push((Duration::from_millis(t), ev(m, true)));
        t += 1;
    }
    out.push((Duration::from_millis(t + 2), ev(key, true)));
    out.push((Duration::from_millis(t + 2 + hold), ev(key, false)));
    for (t, &m) in (t + 3 + hold..).zip(mods.iter().rev()) {
        out.push((Duration::from_millis(t), ev(m, false)));
    }
    out
}

fn local(event: InputEvent) -> LocalEvent {
    LocalEvent {
        event,
        physical: true,
        t_us: mono_us(),
        pos: (0.0, 0.0),
    }
}

/// Drives `peer` for `duration` with the default mix. See [`run_with`].
pub async fn run(handle: &EngineHandle, duration: Duration, peer: DeviceId) -> SoakReport {
    run_with(handle, duration, peer, SoakConfig::default()).await
}

pub async fn run_with(
    handle: &EngineHandle,
    duration: Duration,
    peer: DeviceId,
    cfg: SoakConfig,
) -> SoakReport {
    run_tracked(handle, duration, peer, cfg, &Progress::default()).await
}

/// Leaves awaiting their report, each tagged with the peer session it was sent on.
struct Ledger {
    outstanding: std::collections::VecDeque<Option<u64>>,
    seen: u64,
    lost_to_disconnect: u64,
}

impl Ledger {
    fn new(seen: u64) -> Self {
        Self {
            outstanding: Default::default(),
            seen,
            lost_to_disconnect: 0,
        }
    }

    fn sent(&mut self, handle: &EngineHandle, peer: DeviceId) {
        self.outstanding.push_back(handle.session(peer));
        self.sync(handle, peer);
    }

    /// Leaves whose session ended can never be answered: count them as lost to a
    /// disconnect. Then the reports that arrived answer the oldest of the rest.
    fn sync(&mut self, handle: &EngineHandle, peer: DeviceId) {
        let now = handle.session(peer);
        let before = self.outstanding.len();
        self.outstanding.retain(|s| s.is_some() && *s == now);
        self.lost_to_disconnect += (before - self.outstanding.len()) as u64;
        let reports = handle.soak_counters().reports;
        let new = (reports - self.seen) as usize;
        self.seen = reports;
        for _ in 0..new.min(self.outstanding.len()) {
            self.outstanding.pop_front();
        }
    }

    /// Waits until every Leave is answered or lost, or `limit` passes.
    async fn settle(&mut self, handle: &EngineHandle, peer: DeviceId, limit: Duration) {
        let end = Instant::now() + limit;
        loop {
            self.sync(handle, peer);
            if self.outstanding.is_empty() || Instant::now() >= end {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

pub async fn run_tracked(
    handle: &EngineHandle,
    duration: Duration,
    peer: DeviceId,
    cfg: SoakConfig,
    progress: &Progress,
) -> SoakReport {
    let me = handle.status().me;
    let entry = |edge| EntryPoint { edge, frac: 0.5 };
    let c0 = handle.soak_counters();
    let reports0 = c0.reports;
    let mut outcome = SoakOutcome::Completed;
    let mut abort_reason = None;
    let mut rng = Rng::seeded();
    let start = Instant::now();
    let deadline = start + duration;
    let mut pending: Vec<(Instant, InputEvent)> = Vec::new();
    let mut next_chord = start + rng.duration(cfg.chord_every);
    let mut next_return = start + rng.duration(cfg.return_every);
    let mut next_burst = start;
    let mut burst_left = 0u32;
    let mut next_cross = start;
    let mut on_peer = false;
    let mut leaves = 0u64;
    let mut ledger = Ledger::new(reports0);
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
    loop {
        ticker.tick().await;
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if progress.stop.load(Ordering::Relaxed) {
            outcome = SoakOutcome::Stopped;
            break;
        }
        if handle.focus() != peer {
            // Not (or no longer) crossed: whatever was queued is meaningless.
            pending.clear();
            burst_left = 0;
            on_peer = false;
            if handle.controller() != me {
                match handle.connected_for(peer) {
                    // A reconnect hands control to the higher id: take it back, as a
                    // user touching the mouse would.
                    Some(up) if up <= RECLAIM_WINDOW => {}
                    // Someone took control on a settled session: leave them alone.
                    Some(_) => {
                        outcome = SoakOutcome::Aborted;
                        abort_reason =
                            Some("another machine took control; not fighting a real user".into());
                        break;
                    }
                    None => {}
                }
            }
            if now >= next_cross {
                if handle.controller() != me {
                    // Reconnect handover (see above), at the retry cadence.
                    handle.send_local(local(InputEvent::Motion { dx: 0.0, dy: 0.0 }));
                }
                handle.cross_to(peer, entry(Edge::Left));
                next_cross = now + CROSS_RETRY;
            }
            continue;
        }
        if !on_peer {
            on_peer = true;
            progress.crossings.fetch_add(1, Ordering::Relaxed);
        }
        if now >= next_return {
            // Anything still queued (often a key-up) is dropped: the Leave must cope.
            pending.clear();
            burst_left = 0;
            handle.cross_to(me, entry(Edge::Right));
            leaves += 1;
            progress.leaves.fetch_add(1, Ordering::Relaxed);
            ledger.sent(handle, peer);
            ledger.settle(handle, peer, REPORT_WAIT).await;
            on_peer = false;
            next_cross = Instant::now();
            next_return = Instant::now() + rng.duration(cfg.return_every);
            continue;
        }
        if now >= next_chord {
            for (off, ev) in chord(&mut rng) {
                pending.push((now + off, ev));
            }
            next_chord = now + rng.duration(cfg.chord_every);
        }
        if pending.iter().any(|(t, _)| *t <= now) {
            let mut due: Vec<_> = Vec::new();
            pending.retain(|p| {
                let is_due = p.0 <= now;
                if is_due {
                    due.push(*p);
                }
                !is_due
            });
            due.sort_by_key(|p| p.0);
            for (_, ev) in due {
                handle.send_local(local(ev));
                progress.keys_sent.fetch_add(1, Ordering::Relaxed);
            }
        }
        if burst_left == 0 && now >= next_burst {
            burst_left = rng.between(cfg.burst_len.0.into(), cfg.burst_len.1.into()) as u32;
            next_burst =
                now + Duration::from_millis(u64::from(burst_left)) + rng.duration(cfg.burst_gap);
        }
        if burst_left > 0 {
            burst_left -= 1;
            let dx = rng.between(0, 6) as f32 - 3.0;
            let dy = rng.between(0, 6) as f32 - 3.0;
            handle.send_local(local(InputEvent::Motion { dx, dy }));
        }
    }
    // Final accounting: hand control back (a last Leave) and wait for every report.
    if handle.focus() == peer {
        pending.clear();
        handle.cross_to(me, entry(Edge::Right));
        leaves += 1;
        progress.leaves.fetch_add(1, Ordering::Relaxed);
        ledger.sent(handle, peer);
    }
    // The settle check runs even when the run ends while not crossed.
    ledger.settle(handle, peer, FINAL_WAIT).await;
    let c = handle.soak_counters();
    let answered = c.reports - reports0;
    let latency_hour = handle
        .metrics()
        .per_peer
        .iter()
        .find(|p| p.0 == peer)
        .map(|p| p.2)
        .unwrap_or_default();
    SoakReport {
        duration: start.elapsed(),
        crossings: progress.crossings.load(Ordering::Relaxed),
        leaves,
        held_reports: answered,
        keys_sent: progress.keys_sent.load(Ordering::Relaxed),
        held_before_release_total: c.held_before - c0.held_before,
        stuck_after_release: c.held_after - c0.held_after,
        unanswered_leaves: ledger.outstanding.len() as u64,
        leaves_lost_to_disconnect: ledger.lost_to_disconnect,
        outcome,
        abort_reason,
        safety_releases: c.safety_releases - c0.safety_releases,
        reconnects: c.reconnects - c0.reconnects,
        latency_hour,
    }
}

enum State {
    Idle,
    Running {
        peer: DeviceId,
        started: Instant,
        duration: Duration,
        progress: Arc<Progress>,
    },
    Done(Box<SoakReport>),
    Failed(String),
}

/// The agent's single soak slot: at most one run at a time, the latest result kept.
#[derive(Clone)]
pub struct SoakSlot(Arc<Mutex<State>>);

impl Default for SoakSlot {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(State::Idle)))
    }
}

fn locked(m: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn duration_from_hours(hours: f64) -> Option<Duration> {
    (hours.is_finite() && hours > 0.0 && hours <= MAX_HOURS)
        .then(|| Duration::from_secs_f64(hours * 3600.0))
}

impl SoakSlot {
    /// Starts a run in the background. Errs if one is already running.
    pub fn start(
        &self,
        handle: &EngineHandle,
        peer: DeviceId,
        duration: Duration,
    ) -> Result<(), &'static str> {
        let progress = Arc::new(Progress::default());
        {
            let mut s = locked(&self.0);
            if matches!(*s, State::Running { .. }) {
                return Err("a soak is already running");
            }
            *s = State::Running {
                peer,
                started: Instant::now(),
                duration,
                progress: progress.clone(),
            };
        }
        let (slot, handle) = (self.0.clone(), handle.clone());
        tokio::spawn(async move {
            let inner = {
                let (handle, progress) = (handle.clone(), progress.clone());
                tokio::spawn(async move {
                    run_tracked(&handle, duration, peer, SoakConfig::default(), &progress).await
                })
            };
            let next = match inner.await {
                Ok(report) => State::Done(Box::new(report)),
                Err(e) => State::Failed(format!("soak task died: {e}")),
            };
            *locked(&slot) = next;
        });
        Ok(())
    }

    /// Progress while running, the report when done.
    /// Asks a running soak to end after its final accounting. False when none runs.
    pub fn stop(&self) -> bool {
        match &*locked(&self.0) {
            State::Running { progress, .. } => {
                progress.stop.store(true, Ordering::Relaxed);
                true
            }
            _ => false,
        }
    }

    pub fn snapshot(&self) -> Value {
        match &*locked(&self.0) {
            State::Idle => json!({ "state": "idle" }),
            State::Running {
                peer,
                started,
                duration,
                progress,
            } => json!({
                "state": "running",
                "peer": peer.to_string(),
                "elapsed_secs": started.elapsed().as_secs_f64(),
                "duration_secs": duration.as_secs_f64(),
                "crossings": progress.crossings.load(Ordering::Relaxed),
                "leaves": progress.leaves.load(Ordering::Relaxed),
                "keys_sent": progress.keys_sent.load(Ordering::Relaxed),
            }),
            State::Done(r) => {
                let state = match r.outcome {
                    SoakOutcome::Completed => "done",
                    SoakOutcome::Stopped => "stopped",
                    SoakOutcome::Aborted => "aborted",
                };
                json!({ "state": state, "report": r })
            }
            State::Failed(m) => json!({ "state": "failed", "error": m }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords_release_everything_they_press_in_time_order() {
        let mut rng = Rng::seeded();
        for _ in 0..200 {
            let mut c = chord(&mut rng);
            c.sort_by_key(|e| e.0);
            let mut held = std::collections::HashSet::new();
            for (_, ev) in &c {
                if let InputEvent::Key { usage, down } = ev {
                    if *down {
                        assert!(held.insert(*usage), "pressed twice");
                    } else {
                        assert!(held.remove(usage), "released while up");
                    }
                }
            }
            assert!(held.is_empty());
        }
    }

    #[cfg(feature = "fake")]
    async fn crossed_pair() -> (
        EngineHandle,
        EngineHandle,
        novalink_core::FakePlatform,
        novalink_core::FakePlatform,
    ) {
        use crate::testutil::{two_engines, wait_until};
        let (a, pa, b, pb) = two_engines().await;
        let peer = b.status().me;
        a.send_local(local(InputEvent::Motion { dx: 0.0, dy: 0.0 }));
        a.cross_to(
            peer,
            EntryPoint {
                edge: Edge::Left,
                frac: 0.5,
            },
        );
        wait_until(|| a.focus() == peer).await;
        (a, b, pa, pb)
    }

    #[cfg(feature = "fake")]
    #[tokio::test]
    async fn leave_with_a_key_held_is_reported_released_after_the_settle() {
        use crate::testutil::wait_until;
        let (a, b, _pa, pb) = crossed_pair().await;
        a.send_local(local(InputEvent::Key {
            usage: keys::A,
            down: true,
        }));
        wait_until(|| !pb.held_now().is_empty()).await;
        a.cross_to(
            a.status().me,
            EntryPoint {
                edge: Edge::Right,
                frac: 0.5,
            },
        );
        // Not inline: nothing arrives before the settle.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(a.soak_counters().reports, 0);
        wait_until(|| a.soak_counters().reports == 1).await;
        let c = a.soak_counters();
        assert_eq!((c.held_before, c.held_after), (1, 0));
        assert!(pb.held_now().is_empty());
        drop(b);
    }

    #[cfg(feature = "fake")]
    #[tokio::test]
    async fn a_key_arriving_after_the_release_is_counted_as_stuck() {
        use crate::testutil::{wait_until, wait_until_within};
        use novalink_proto::Control;
        let (a, b, _pa, pb) = crossed_pair().await;
        let (me, peer) = (a.status().me, b.status().me);
        a.send_local(local(InputEvent::Key {
            usage: keys::A,
            down: true,
        }));
        wait_until(|| !pb.held_now().is_empty()).await;
        a.cross_to(
            me,
            EntryPoint {
                edge: Edge::Right,
                frac: 0.5,
            },
        );
        wait_until(|| pb.held_now().is_empty()).await;
        // A stray key-down on the controller's stream after the Leave.
        a.manager()
            .send_control(
                peer,
                &Control::Key {
                    usage: keys::B,
                    down: true,
                    t_us: mono_us(),
                },
            )
            .await
            .unwrap();
        wait_until_within(Duration::from_secs(3), || a.soak_counters().reports == 1).await;
        let c = a.soak_counters();
        assert_eq!(
            (c.held_before, c.held_after),
            (1, 1),
            "the measure must be able to fail"
        );
    }

    #[cfg(feature = "fake")]
    #[tokio::test]
    async fn a_nonzero_report_is_summed() {
        use crate::testutil::{two_engines, wait_until};
        use novalink_proto::Control;
        let (a, _pa, b, _pb) = two_engines().await;
        b.manager()
            .send_control(
                a.status().me,
                &Control::HeldReport {
                    held_before_release: 1,
                    held_after_settle: 2,
                },
            )
            .await
            .unwrap();
        wait_until(|| a.soak_counters().reports == 1).await;
        assert_eq!(a.soak_counters().held_after, 2);
    }

    #[cfg(feature = "fake")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stop_ends_the_run_with_final_accounting() {
        use crate::testutil::wait_until;
        let (a, b, _pa, _pb) = crossed_pair().await;
        let progress = Arc::new(Progress::default());
        let task = {
            let (a, p, peer) = (a.clone(), progress.clone(), b.status().me);
            tokio::spawn(async move {
                run_tracked(
                    &a,
                    Duration::from_secs(120),
                    peer,
                    SoakConfig::default(),
                    &p,
                )
                .await
            })
        };
        wait_until(|| progress.keys_sent.load(Ordering::Relaxed) > 0).await;
        progress.stop.store(true, Ordering::Relaxed);
        let r = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.outcome, SoakOutcome::Stopped);
        assert!(r.duration < Duration::from_secs(10));
        assert_eq!(r.leaves, 1, "the final pass hands control back");
        assert_eq!(
            (r.held_reports, r.unanswered_leaves, r.stuck_after_release),
            (1, 0, 0)
        );
    }

    #[cfg(feature = "fake")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_real_claim_on_a_settled_session_aborts_the_run() {
        use crate::testutil::wait_until_within;
        let (a, b, _pa, pb) = crossed_pair().await;
        let peer = b.status().me;
        let progress = Arc::new(Progress::default());
        let task = {
            let (a, p) = (a.clone(), progress.clone());
            tokio::spawn(async move {
                run_tracked(
                    &a,
                    Duration::from_secs(120),
                    peer,
                    SoakConfig::default(),
                    &p,
                )
                .await
            })
        };
        wait_until_within(Duration::from_secs(10), || {
            a.connected_for(peer)
                .is_some_and(|d| d > Duration::from_millis(2200))
        })
        .await;
        pb.push_physical_key(keys::B, true);
        let r = tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.outcome, SoakOutcome::Aborted, "{r:?}");
        assert!(r.abort_reason.is_some());
        assert!(r.duration < Duration::from_secs(15));
        // It did not retake control.
        assert_eq!(a.controller(), peer);
    }

    #[test]
    fn hours_are_validated() {
        assert_eq!(duration_from_hours(1.0), Some(Duration::from_secs(3600)));
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY, 1e9] {
            assert_eq!(duration_from_hours(bad), None);
        }
    }
}
