//! The agent's event loop: local input -> crossings / remote input, remote input ->
//! injection, the single release routine, latency telemetry and the flight recorder.
//!
//! All mutable state lives in one [`Core`] behind a mutex. Handlers are synchronous: they
//! mutate state, call the (synchronous) platform and queue outgoing control messages in an
//! outbox. The actor task flushes the outbox after releasing the lock, strictly in order.

use crate::recorder::{FlightRecorder, write_records};
use crate::settings::{Settings, swap_ctrl_cmd};
use anyhow::Result;
use novalink_core::time::mono_us;
use novalink_core::{
    Action, ControlState, HeldInputs, InputEvent, LatencyStats, Layout, LocalEvent, Percentiles,
    Platform, Window, entry_position, hit_edge,
};
use novalink_net::manager::{Manager, ManagerConfig, ManagerEvent, PeerStatus, Timings};
use novalink_proto::{Control, Datagram, DeviceId, DisplayInfo, Edge, EntryPoint};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

/// How often offsets and percentiles are refreshed, and the display poll multiple.
const TICK: Duration = Duration::from_millis(100);
const DISPLAY_POLL_TICKS: u32 = 20;
/// Limit on one outgoing control message; a dead peer must not stall the loop.
const SEND_TIMEOUT: Duration = Duration::from_millis(300);
/// A latency sample above this is a spike.
const SPIKE_ABS_US: u64 = 50_000;
const SPIKE_P99_FACTOR: u64 = 4;
/// Below this a sample is never a spike (scheduler jitter on a fast LAN).
const SPIKE_FLOOR_US: u64 = 2_000;
const SPIKE_MIN_GAP: Duration = Duration::from_secs(60);
/// One motion event in this many is recorded.
const MOTION_SAMPLE: u64 = 10;
/// At most one recorder dump per reason in this long.
const DUMP_MIN_GAP: Duration = Duration::from_secs(10);
const HEALTH_POLL: Duration = Duration::from_millis(250);
/// How often a refused input capture is tried again: soon when a permission is
/// missing (the user is likely granting it now), slowly for any other failure.
const CAPTURE_RETRY: Duration = Duration::from_secs(1);
const CAPTURE_RETRY_FAILED: Duration = Duration::from_secs(5);

/// Rate limit for recorder dumps: one per reason per `gap`.
struct DumpLimiter {
    gap: Duration,
    last: HashMap<String, Instant>,
}

impl DumpLimiter {
    fn new(gap: Duration) -> Self {
        Self {
            gap,
            last: HashMap::new(),
        }
    }

    fn allow(&mut self, reason: &str, now: Instant) -> bool {
        match self.last.get(reason) {
            Some(t) if now.saturating_duration_since(*t) < self.gap => false,
            _ => {
                self.last.insert(reason.to_string(), now);
                true
            }
        }
    }
}

pub struct EngineOpts {
    pub dry_inject: bool,
    pub recorder_span: Duration,
    pub log_dir: PathBuf,
    pub settings: Settings,
    /// Where changed settings are saved.
    pub settings_path: PathBuf,
}

/// Whether this machine's own keyboard and mouse are being read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureState {
    Running,
    /// The OS refused capture and a permission is missing; retried until granted.
    WaitingForPermission,
    /// Capture failed with every permission granted; retried slowly.
    Failed,
}

#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub me: DeviceId,
    pub name: String,
    pub controller: DeviceId,
    pub focus: DeviceId,
    pub peers: Vec<PeerStatus>,
    pub layout: Layout,
    pub displays: Vec<DisplayInfo>,
    pub capture: CaptureState,
    pub settings: Settings,
}

#[derive(Clone, Debug, Serialize)]
pub struct MetricsReport {
    /// Per peer: (id, last minute, last hour, last day).
    pub per_peer: Vec<(DeviceId, Percentiles, Percentiles, Percentiles)>,
    pub safety_releases: u64,
    pub reconnects: u64,
    /// Reconnects in the last hour.
    pub reconnects_last_hour: u64,
    /// Why the connection closed before the most recent reconnect in the last hour.
    pub last_reconnect_reason: Option<String>,
    /// Safety releases in the last day.
    pub safety_releases_last_day: u64,
}

enum Out {
    To(DeviceId, Control),
    All(Control),
}

enum Cmd {
    BroadcastLayout,
    /// Soak: cross to `peer` (or back to this machine) as if the pointer hit an edge.
    Cross(DeviceId, EntryPoint),
    /// Test only: panic while holding the core lock.
    #[cfg(feature = "fake")]
    PanicForTest,
    Shutdown(oneshot::Sender<()>),
}

struct Core {
    services: crate::channels::Services,
    me: DeviceId,
    name: String,
    platform: Arc<dyn Platform>,
    manager: Arc<Manager>,
    /// The channel `platform.start_capture` feeds, kept to retry a refused capture.
    local_tx: mpsc::UnboundedSender<LocalEvent>,
    capture: CaptureState,
    capture_retry_at: Instant,
    control: ControlState,
    layout: Layout,
    held_injected: HeldInputs,
    held_remote: HashMap<DeviceId, HeldInputs>,
    /// Physical keys/buttons whose down reached the local OS (focus was here) and whose
    /// up has not been seen. Their repeats and ups are never forwarded to a remote focus,
    /// which never saw the down.
    held_local: HeldInputs,
    seq: u64,
    stats: HashMap<DeviceId, LatencyStats>,
    minute_p99: HashMap<DeviceId, u64>,
    offsets: HashMap<DeviceId, i64>,
    connected: HashSet<DeviceId>,
    ever_connected: HashSet<DeviceId>,
    peer_displays: HashMap<DeviceId, Vec<DisplayInfo>>,
    my_displays: Vec<DisplayInfo>,
    safety_releases: u64,
    reconnects: u64,
    reconnect_log: TimedLog<String>,
    release_log: TimedLog<()>,
    last_lost: HashMap<DeviceId, String>,
    recorder: FlightRecorder,
    motion_n: u64,
    last_spike_dump: Option<Instant>,
    dump_limiter: DumpLimiter,
    edge_latch: Option<Edge>,
    last_health: Instant,
    dry_inject: bool,
    log_dir: PathBuf,
    settings: Settings,
    settings_path: PathBuf,
    outbox: Vec<Out>,
    pending_pairs: Vec<PendingPair>,
    /// `HeldReport`s received from driven peers, and the sum of their counts.
    held_reports: u64,
    held_before: u64,
    held_after: u64,
    /// Leaves awaiting their settle check.
    settles: Vec<Settle>,
    peer_dry: HashMap<DeviceId, bool>,
    /// Counts sessions per peer; a new number means a new session.
    session_gen: HashMap<DeviceId, u64>,
    connected_at: HashMap<DeviceId, Instant>,
}

/// How long after a `Leave` the held set is checked again.
const SETTLE: Duration = Duration::from_millis(500);

struct Settle {
    peer: DeviceId,
    due: Instant,
    before: u32,
    /// The peer entered again meanwhile, so keys it holds now are legitimate.
    reentered: bool,
}

/// What a controller learned from `HeldReport`s.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SoakCounters {
    pub reports: u64,
    pub held_before: u64,
    pub held_after: u64,
    pub safety_releases: u64,
    pub reconnects: u64,
}

const RECONNECT_WINDOW: Duration = Duration::from_secs(3600);
const RELEASE_WINDOW: Duration = Duration::from_secs(24 * 3600);

/// Timestamped events, oldest first, pruned by age.
#[derive(Debug)]
struct TimedLog<T> {
    items: VecDeque<(Instant, T)>,
}

impl<T> TimedLog<T> {
    fn new() -> Self {
        Self {
            items: VecDeque::new(),
        }
    }

    fn push(&mut self, at: Instant, v: T) {
        self.items.push_back((at, v));
    }

    fn prune(&mut self, now: Instant, span: Duration) {
        self.items
            .retain(|(t, _)| now.saturating_duration_since(*t) <= span);
    }

    fn count_within(&self, now: Instant, span: Duration) -> u64 {
        self.items
            .iter()
            .filter(|(t, _)| now.saturating_duration_since(*t) <= span)
            .count() as u64
    }

    /// The newest entry within `span`, chosen by timestamp rather than position.
    fn latest_within(&self, now: Instant, span: Duration) -> Option<&T> {
        self.items
            .iter()
            .filter(|(t, _)| now.saturating_duration_since(*t) <= span)
            .max_by_key(|(t, _)| *t)
            .map(|(_, v)| v)
    }
}

/// A pairing code waiting for the user's decision.
#[derive(Clone, Debug, Serialize)]
pub struct PendingPair {
    pub peer: DeviceId,
    pub name: String,
    pub code: String,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn short(id: &DeviceId) -> String {
    id.to_string().chars().take(8).collect()
}

impl Core {
    fn rec(&mut self, mut v: Value) {
        if let Value::Object(m) = &mut v {
            m.insert("t_us".into(), json!(mono_us()));
        }
        self.recorder.push(Instant::now(), v);
    }

    /// Snapshot under the lock, write on a blocking thread; at most one per reason per 10 s.
    fn dump(&mut self, reason: &str) {
        let now = Instant::now();
        if !self.dump_limiter.allow(reason, now) {
            return;
        }
        let records = self.recorder.snapshot(now);
        let dir = self.log_dir.clone();
        let reason = reason.to_string();
        let write = move || {
            if let Err(e) = write_records(&records, &dir, &reason) {
                tracing::warn!("flight recorder dump ({reason}) failed: {e:#}");
            }
        };
        if tokio::runtime::Handle::try_current().is_ok() {
            drop(tokio::task::spawn_blocking(write));
        } else {
            write();
        }
    }

    fn to(&mut self, p: DeviceId, c: Control) {
        self.outbox.push(Out::To(p, c));
    }

    /// The single release routine. Releases everything injected for a remote controller.
    /// Only a `leave` is a normal release; anything else with keys held is a safety release.
    fn release(&mut self, why: &str) {
        let evs = self.held_injected.release_all();
        if evs.is_empty() {
            return;
        }
        if !self.dry_inject {
            for e in &evs {
                if let Err(err) = self.platform.inject(e) {
                    tracing::warn!("release inject failed: {err:#}");
                }
            }
        }
        self.rec(json!({"kind":"release","why":why,"count":evs.len()}));
        if why != "leave" && why != "settings" {
            self.safety_releases += 1;
            self.release_log.push(Instant::now(), ());
            tracing::warn!(why, count = evs.len(), "safety release");
            self.dump("safety_release");
        }
    }

    /// Forget keys we forwarded to `peer`. Used when we leave it (its own Leave releases them).
    fn forget_remote(&mut self, peer: DeviceId) {
        self.held_remote.remove(&peer);
    }

    fn apply(&mut self, actions: Vec<Action>, why: &'static str) {
        for a in actions {
            self.rec(json!({"kind":"action","why":why,"action":format!("{a:?}").split(['(', ' ']).next()}));
            match a {
                Action::Broadcast(c) => self.outbox.push(Out::All(c)),
                Action::Send(p, c) => {
                    if matches!(c, Control::Leave) {
                        self.forget_remote(p);
                    }
                    self.to(p, c);
                }
                Action::SetSuppress(on) => self.platform.set_suppress(on),
                Action::WarpTo(_) if self.dry_inject => {}
                Action::WarpTo(entry) => {
                    let pos = entry_position(&self.platform.displays(), entry);
                    self.platform.warp(pos);
                }
                Action::ReleaseInjected => self.release(why),
                Action::ResetLocalSync => {}
            }
        }
    }

    fn cross(&mut self, to: DeviceId, entry: EntryPoint) {
        let acts = self.control.cross_to(to, entry);
        self.apply(acts, "cross");
    }

    // ---- local input ----

    fn on_local(&mut self, ev: LocalEvent) {
        if !ev.physical {
            self.on_local_injected(&ev);
            return;
        }
        // Releases never claim: only presses, motion and scroll show intent.
        let releases = matches!(
            ev.event,
            InputEvent::Key { down: false, .. } | InputEvent::Button { down: false, .. }
        );
        if !releases {
            let acts = self.control.on_local_physical();
            if !acts.is_empty() {
                self.apply(acts, "claim_local");
            }
        }
        let focus = self.control.focus();
        if focus == self.me {
            self.held_local.record(&ev.event);
            if matches!(ev.event, InputEvent::Motion { .. }) {
                self.check_edge_cross();
            }
        } else if self.held_local.contains(&ev.event) {
            // A repeat or the up of an input held down locally across the crossing:
            // the local OS owns it (Ruling 33 passes the up through locally).
            if matches!(
                ev.event,
                InputEvent::Key { down: false, .. } | InputEvent::Button { down: false, .. }
            ) {
                self.held_local.record(&ev.event);
            }
        } else {
            self.forward(&ev, focus);
        }
    }

    fn check_edge_cross(&mut self) {
        let Some((edge, frac)) = hit_edge(&self.my_displays, self.platform.cursor_pos()) else {
            return;
        };
        if let Some(n) = self.layout.neighbor(self.me, edge)
            && self.connected.contains(&n)
        {
            self.cross(
                n,
                EntryPoint {
                    edge: edge.opposite(),
                    frac,
                },
            );
        }
    }

    /// Our own injection moved the cursor; when driven by a remote controller and it
    /// reaches an edge, tell the controller.
    fn on_local_injected(&mut self, ev: &LocalEvent) {
        let controller = self.control.controller();
        if !matches!(ev.event, InputEvent::Motion { .. })
            || controller == self.me
            || self.control.focus() != self.me
        {
            return;
        }
        match hit_edge(&self.my_displays, self.platform.cursor_pos()) {
            Some((edge, frac)) if self.layout.neighbor(self.me, edge).is_some() => {
                // Once per arrival on the edge, not once per motion event.
                if self.edge_latch != Some(edge) {
                    self.edge_latch = Some(edge);
                    self.rec(json!({"kind":"edge_hit","edge":format!("{edge:?}")}));
                    self.to(controller, Control::EdgeHit { edge, frac });
                }
            }
            _ => self.edge_latch = None,
        }
    }

    fn forward(&mut self, ev: &LocalEvent, focus: DeviceId) {
        let t_us = ev.t_us;
        match ev.event {
            InputEvent::Motion { dx, dy } => {
                self.seq += 1;
                self.sample_motion("out");
                let d = Datagram::Motion {
                    seq: self.seq,
                    dx,
                    dy,
                    t_us,
                };
                self.manager.send_datagram(focus, &d);
            }
            InputEvent::Scroll { dx, dy } => {
                self.seq += 1;
                let d = Datagram::Scroll {
                    seq: self.seq,
                    dx,
                    dy,
                    t_us,
                };
                self.manager.send_datagram(focus, &d);
            }
            InputEvent::Key { usage, down } => {
                self.held_remote.entry(focus).or_default().record(&ev.event);
                self.rec(json!({"kind":"key","dir":"out","down":down}));
                self.to(focus, Control::Key { usage, down, t_us });
            }
            InputEvent::Button { button, down } => {
                self.held_remote.entry(focus).or_default().record(&ev.event);
                self.rec(json!({"kind":"button","dir":"out","down":down}));
                self.to(focus, Control::Button { button, down, t_us });
            }
        }
    }

    fn sample_motion(&mut self, dir: &str) {
        self.motion_n += 1;
        if self.motion_n.is_multiple_of(MOTION_SAMPLE) {
            self.rec(json!({"kind":"motion","dir":dir}));
        }
    }

    // ---- remote input ----

    fn inject_remote(&mut self, mut ev: InputEvent, peer: DeviceId, t_us: u64) {
        if let InputEvent::Key { usage, .. } = &mut ev
            && self.settings.swap_ctrl_cmd
        {
            *usage = swap_ctrl_cmd(*usage);
        }
        if !self.dry_inject
            && let Err(e) = self.platform.inject(&ev)
        {
            tracing::warn!("inject failed: {e:#}");
        }
        self.held_injected.record(&ev);
        self.sample_latency(peer, t_us);
    }

    fn sample_latency(&mut self, peer: DeviceId, t_us: u64) {
        let Some(off) = self.offsets.get(&peer).copied() else {
            return;
        };
        let sent_local = t_us as i128 - off as i128;
        let us = (mono_us() as i128 - sent_local).max(0) as u64;
        let now = Instant::now();
        let p99 = self.minute_p99.get(&peer).copied().unwrap_or(0);
        self.stats.entry(peer).or_default().record(now, us);
        let spike =
            us >= SPIKE_FLOOR_US && (us > SPIKE_ABS_US || (p99 > 0 && us > SPIKE_P99_FACTOR * p99));
        if spike
            && self
                .last_spike_dump
                .is_none_or(|t| now.duration_since(t) >= SPIKE_MIN_GAP)
        {
            self.last_spike_dump = Some(now);
            self.rec(json!({"kind":"latency_spike","us":us,"p99_us":p99}));
            self.dump("latency_spike");
        }
    }

    // ---- network events ----

    fn on_manager(&mut self, ev: ManagerEvent) {
        match ev {
            ManagerEvent::Connected { peer, hello } => self.on_connected(peer, hello),
            ManagerEvent::Lost { peer, reason } => {
                self.last_lost.insert(peer, format!("{reason:?}"));
                self.on_lost(peer)
            }
            ManagerEvent::Datagram { peer, d } => self.on_datagram(peer, d),
            ManagerEvent::Control { peer, c } => self.on_control(peer, c),
            ManagerEvent::PairRequest { peer, name, code } => {
                self.pending_pairs.retain(|p| p.peer != peer);
                self.pending_pairs.push(PendingPair { peer, name, code });
            }
            ManagerEvent::Paired { peer } | ManagerEvent::PairRejected { peer } => {
                self.pending_pairs.retain(|p| p.peer != peer);
            }
            ManagerEvent::Channel {
                peer,
                service,
                send,
                recv,
            } => {
                self.rec(json!({"kind":"channel","peer":short(&peer),"service":service}));
                crate::channels::accept_incoming(self.services.clone(), peer, service, send, recv);
            }
        }
    }

    fn on_connected(&mut self, peer: DeviceId, hello: Control) {
        let replaced = self.connected.contains(&peer);
        self.rec(json!({"kind":"connected","peer":short(&peer),"replaced":replaced}));
        if replaced {
            // The old session is gone without a Lost: reset everything held for the peer.
            self.reset_peer(peer, "session_replaced");
        }
        if let Control::Hello { displays, .. } = hello {
            self.peer_displays.insert(peer, displays);
        }
        self.connected.insert(peer);
        self.connected_at.insert(peer, Instant::now());
        *self.session_gen.entry(peer).or_insert(0) += 1;
        self.to(
            peer,
            Control::PeerInfo {
                dry_inject: self.dry_inject,
            },
        );
        if !self.ever_connected.insert(peer) {
            self.reconnects += 1;
            let why = self
                .last_lost
                .get(&peer)
                .cloned()
                .unwrap_or_else(|| "unknown".into());
            self.reconnect_log.push(Instant::now(), why);
            self.dump("reconnect");
        }
        // A peer that connects late must learn who controls, or two controllers coexist.
        if self.control.controller() == self.me && self.control.clock() > 0 {
            let clock = self.control.clock();
            self.to(peer, Control::Claim { clock });
        }
        if self.layout.version > 0 {
            let c = Control::Layout {
                version: self.layout.version,
                links: self.layout.links.clone(),
            };
            self.to(peer, c);
        }
    }

    fn on_lost(&mut self, peer: DeviceId) {
        self.rec(json!({"kind":"lost","peer":short(&peer)}));
        self.connected.remove(&peer);
        self.reset_peer(peer, "peer_lost");
    }

    /// Terminal for the peer's held state: release what it injected here, drop what we
    /// forwarded to it, and let the control state give focus/control back.
    fn reset_peer(&mut self, peer: DeviceId, why: &'static str) {
        self.offsets.remove(&peer);
        self.minute_p99.remove(&peer);
        self.settles.retain(|s| s.peer != peer);
        self.peer_dry.remove(&peer);
        self.connected_at.remove(&peer);
        if let Some(mut h) = self.held_remote.remove(&peer)
            && !h.is_empty()
        {
            h.release_all();
            self.safety_releases += 1;
            self.release_log.push(Instant::now(), ());
            self.rec(json!({"kind":"release","why":why,"remote":true}));
            self.dump("safety_release");
        }
        let acts = self.control.on_peer_lost(peer);
        self.apply(acts, why);
    }

    fn on_datagram(&mut self, peer: DeviceId, d: Datagram) {
        if !self.driven_by(peer) {
            return;
        }
        match d {
            Datagram::Motion { dx, dy, t_us, .. } => {
                self.sample_motion("in");
                self.inject_remote(InputEvent::Motion { dx, dy }, peer, t_us);
            }
            Datagram::Scroll { dx, dy, t_us, .. } => {
                self.inject_remote(InputEvent::Scroll { dx, dy }, peer, t_us);
            }
            Datagram::Heartbeat { .. } | Datagram::HeartbeatAck { .. } => {}
        }
    }

    /// True when `peer` is the current remote controller.
    fn driven_by(&self, peer: DeviceId) -> bool {
        peer != self.me && self.control.controller() == peer
    }

    fn on_control(&mut self, peer: DeviceId, c: Control) {
        match c {
            Control::Key { usage, down, t_us } if self.driven_by(peer) => {
                self.rec(json!({"kind":"key","dir":"in","down":down}));
                self.inject_remote(InputEvent::Key { usage, down }, peer, t_us);
            }
            Control::Button { button, down, t_us } if self.driven_by(peer) => {
                self.rec(json!({"kind":"button","dir":"in","down":down}));
                self.inject_remote(InputEvent::Button { button, down }, peer, t_us);
            }
            Control::Enter { entry } => self.on_enter(peer, entry),
            Control::Leave if self.driven_by(peer) => {
                let acts = self.control.on_leave(peer);
                let before = self.held_injected.len() as u32;
                self.apply(acts, "leave");
                // Checked again after the settle, on a later tick, so a key that
                // arrives after the release shows up.
                self.settles.push(Settle {
                    peer,
                    due: Instant::now() + SETTLE,
                    before,
                    reentered: false,
                });
            }
            Control::HeldReport {
                held_before_release,
                held_after_settle,
            } => {
                self.held_reports += 1;
                self.held_before += u64::from(held_before_release);
                self.held_after += u64::from(held_after_settle);
                self.rec(json!({"kind":"held_report","before":held_before_release,"after":held_after_settle}));
            }
            Control::PeerInfo { dry_inject } => {
                self.peer_dry.insert(peer, dry_inject);
            }
            Control::ReleaseAll if self.driven_by(peer) => self.release("release_all"),
            Control::EdgeHit { edge, frac } => self.on_edge_hit(peer, edge, frac),
            Control::Claim { clock } => {
                let acts = self.control.on_claim(peer, clock);
                if !acts.is_empty() {
                    self.apply(acts, "claim_remote");
                }
            }
            Control::Displays(d) => {
                self.peer_displays.insert(peer, d);
            }
            Control::Layout { version, links } => {
                if self.layout.merge(version, links) {
                    self.rec(json!({"kind":"layout","version":version}));
                }
            }
            // Input from a non-controller, stray Leave/ReleaseAll, handshake and pairing
            // messages: dropped.
            _ => {}
        }
    }

    fn on_enter(&mut self, peer: DeviceId, entry: EntryPoint) {
        // Guard: we are suppressing for a remote focus; hand that focus back first.
        if self.control.suppressing() {
            let old = self.control.focus();
            self.to(old, Control::Leave);
            self.forget_remote(old);
            self.platform.set_suppress(false);
        }
        if self.control.controller() != peer {
            self.release("controller_change");
        }
        self.edge_latch = None;
        for s in self.settles.iter_mut().filter(|s| s.peer == peer) {
            s.reentered = true;
        }
        let acts = self.control.on_enter(peer, entry);
        self.apply(acts, "enter");
    }

    fn on_edge_hit(&mut self, peer: DeviceId, edge: Edge, frac: f32) {
        if self.control.controller() != self.me || peer == self.me || self.control.focus() != peer {
            return;
        }
        let Some(n) = self.layout.neighbor(peer, edge) else {
            return;
        };
        if n == self.me || self.connected.contains(&n) {
            self.cross(
                n,
                EntryPoint {
                    edge: edge.opposite(),
                    frac,
                },
            );
        }
    }

    // ---- periodic ----

    fn on_tick(&mut self, poll_displays: bool) {
        if self.capture != CaptureState::Running && Instant::now() >= self.capture_retry_at {
            self.try_capture();
        }
        for p in self.manager.peers() {
            if let Some(o) = p.clock_offset_us {
                self.offsets.insert(p.id, o);
            }
        }
        let now = Instant::now();
        self.reconnect_log.prune(now, RECONNECT_WINDOW);
        self.release_log.prune(now, RELEASE_WINDOW);
        for (id, s) in &self.stats {
            self.minute_p99
                .insert(*id, s.snapshot(now, Window::Minute).p99_us);
        }
        let (due, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.settles)
            .into_iter()
            .partition(|s| s.due <= now);
        self.settles = rest;
        for s in due {
            // Keys held now belong to whoever controls now, unless that is still the
            // peer that left.
            let after = if s.reentered || self.control.controller() != s.peer {
                0
            } else {
                self.held_injected.len() as u32
            };
            self.to(
                s.peer,
                Control::HeldReport {
                    held_before_release: s.before,
                    held_after_settle: after,
                },
            );
        }
        if now.saturating_duration_since(self.last_health) >= HEALTH_POLL {
            self.last_health = now;
            for ev in self.platform.take_health_events() {
                self.rec(json!({"kind":"health","event":ev}));
                self.dump(ev);
            }
        }
        if poll_displays {
            let d = self.platform.displays();
            if d != self.my_displays {
                self.my_displays = d.clone();
                self.rec(json!({"kind":"displays","count":d.len()}));
                self.outbox.push(Out::All(Control::Displays(d)));
            }
        }
    }

    /// Last-resort release: honors `dry_inject`, unsuppresses, dumps `engine_panic`.
    fn emergency(&mut self) {
        let evs = self.held_injected.release_all();
        if !self.dry_inject {
            for e in &evs {
                let _ = self.platform.inject(e);
            }
        }
        self.platform.set_suppress(false);
        self.rec(json!({"kind":"engine_panic","released":evs.len()}));
        if let Err(e) = self
            .recorder
            .dump(Instant::now(), &self.log_dir, "engine_panic")
        {
            tracing::warn!("engine_panic dump failed: {e:#}");
        }
    }

    fn shutdown(&mut self) {
        self.release("shutdown");
        if self.control.suppressing() {
            let focus = self.control.focus();
            self.to(focus, Control::Leave);
            self.forget_remote(focus);
        }
        self.platform.set_suppress(false);
        self.rec(json!({"kind":"shutdown"}));
    }

    fn status(&self) -> Status {
        let mut peers = self.manager.peers();
        for p in &mut peers {
            p.dry_inject = self.peer_dry.get(&p.id).copied();
        }
        Status {
            me: self.me,
            name: self.name.clone(),
            controller: self.control.controller(),
            focus: self.control.focus(),
            peers,
            layout: self.layout.clone(),
            displays: self.platform.displays(),
            capture: self.capture,
            settings: self.settings,
        }
    }

    /// Tries to start input capture and records how it went. A refusal is not fatal:
    /// the rest of the agent works without local input.
    fn try_capture(&mut self) {
        match self.platform.start_capture(self.local_tx.clone()) {
            Ok(()) => {
                if self.capture != CaptureState::Running {
                    tracing::info!(event = "capture_started");
                    self.rec(json!({"kind":"capture","state":"running"}));
                }
                self.capture = CaptureState::Running;
            }
            Err(e) => {
                let missing = self.platform.permissions().iter().any(|&(_, ok)| !ok);
                let state = if missing {
                    CaptureState::WaitingForPermission
                } else {
                    CaptureState::Failed
                };
                if state != self.capture {
                    tracing::warn!(event = "capture_unavailable", ?state, "{e:#}");
                    self.rec(json!({"kind":"capture","state":state}));
                }
                self.capture = state;
                let wait = if missing {
                    CAPTURE_RETRY
                } else {
                    CAPTURE_RETRY_FAILED
                };
                self.capture_retry_at = Instant::now() + wait;
            }
        }
    }

    fn metrics(&self) -> MetricsReport {
        let now = Instant::now();
        let mut per_peer: Vec<_> = self
            .stats
            .iter()
            .map(|(id, s)| {
                (
                    *id,
                    s.snapshot(now, Window::Minute),
                    s.snapshot(now, Window::Hour),
                    s.snapshot(now, Window::Day),
                )
            })
            .collect();
        per_peer.sort_by_key(|p| p.0);
        MetricsReport {
            per_peer,
            safety_releases: self.safety_releases,
            reconnects: self.reconnects,
            reconnects_last_hour: self.reconnect_log.count_within(now, RECONNECT_WINDOW),
            last_reconnect_reason: self
                .reconnect_log
                .latest_within(now, RECONNECT_WINDOW)
                .cloned(),
            safety_releases_last_day: self.release_log.count_within(now, RELEASE_WINDOW),
        }
    }
}

pub struct Engine;

/// How the engine task ended when nobody asked it to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineExit {
    Panicked,
    Ended,
}

/// Cheap to clone: every clone talks to the same engine. The task handle is shared so
/// that exactly one clone supervises or shuts it down.
#[derive(Clone)]
pub struct EngineHandle {
    core: Arc<Mutex<Core>>,
    manager: Arc<Manager>,
    cmd: mpsc::UnboundedSender<Cmd>,
    /// Feeds the same channel the platform capture does.
    local: mpsc::UnboundedSender<LocalEvent>,
    task: Arc<Mutex<Option<JoinHandle<()>>>>,
    /// Set by the API's shutdown request; `main` waits on it.
    shutdown_req: watch::Sender<bool>,
    services: crate::channels::Services,
}

impl Engine {
    pub async fn start(
        platform: Arc<dyn Platform>,
        manager_cfg: ManagerConfig,
        opts: EngineOpts,
    ) -> Result<EngineHandle> {
        Self::start_with(platform, manager_cfg, Timings::default(), opts).await
    }

    pub async fn start_with(
        platform: Arc<dyn Platform>,
        mut cfg: ManagerConfig,
        timings: Timings,
        opts: EngineOpts,
    ) -> Result<EngineHandle> {
        let my_displays = platform.displays();
        cfg.hello_displays = my_displays.clone();
        let name = cfg.name.clone();
        let (manager, mut mgr_rx) = Manager::start_with(cfg, timings).await?;
        let manager = Arc::new(manager);
        let me = manager.id();
        let (local_tx, mut local_rx) = mpsc::unbounded_channel();
        let services = crate::channels::Services::default();
        let core = Arc::new(Mutex::new(Core {
            services: services.clone(),
            me,
            name,
            platform,
            manager: manager.clone(),
            local_tx: local_tx.clone(),
            // Replaced by the first attempt just below.
            capture: CaptureState::WaitingForPermission,
            capture_retry_at: Instant::now(),
            control: ControlState::new(me),
            layout: Layout::default(),
            held_injected: HeldInputs::default(),
            held_remote: HashMap::new(),
            held_local: HeldInputs::default(),
            seq: 0,
            stats: HashMap::new(),
            minute_p99: HashMap::new(),
            offsets: HashMap::new(),
            connected: HashSet::new(),
            ever_connected: HashSet::new(),
            peer_displays: HashMap::new(),
            my_displays,
            safety_releases: 0,
            reconnects: 0,
            reconnect_log: TimedLog::new(),
            release_log: TimedLog::new(),
            last_lost: HashMap::new(),
            recorder: FlightRecorder::new(opts.recorder_span),
            motion_n: 0,
            last_spike_dump: None,
            dump_limiter: DumpLimiter::new(DUMP_MIN_GAP),
            edge_latch: None,
            last_health: Instant::now(),
            dry_inject: opts.dry_inject,
            log_dir: opts.log_dir,
            settings: opts.settings,
            settings_path: opts.settings_path,
            outbox: Vec::new(),
            pending_pairs: Vec::new(),
            held_reports: 0,
            held_before: 0,
            held_after: 0,
            settles: Vec::new(),
            peer_dry: HashMap::new(),
            session_gen: HashMap::new(),
            connected_at: HashMap::new(),
        }));
        lock(&core).try_capture();
        let (cmd, mut cmd_rx) = mpsc::unbounded_channel();
        let task = {
            let core = core.clone();
            let manager = manager.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(TICK);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                let mut ticks = 0u32;
                // Runs `f` on the core, then flushes its outbox in order.
                macro_rules! step {
                    ($c:ident => $body:expr) => {{
                        let outs = {
                            let mut $c = lock(&core);
                            $body;
                            std::mem::take(&mut $c.outbox)
                        };
                        flush(&manager, outs).await;
                    }};
                }
                loop {
                    tokio::select! {
                        Some(ev) = local_rx.recv() => step!(c => c.on_local(ev)),
                        Some(ev) = mgr_rx.recv() => step!(c => c.on_manager(ev)),
                        _ = tick.tick() => {
                            ticks = ticks.wrapping_add(1);
                            step!(c => c.on_tick(ticks.is_multiple_of(DISPLAY_POLL_TICKS)));
                        }
                        cmd = cmd_rx.recv() => match cmd {
                            Some(Cmd::Cross(to, entry)) => step!(c => {
                                if to == c.me || c.connected.contains(&to) {
                                    c.cross(to, entry);
                                }
                            }),
                            Some(Cmd::BroadcastLayout) => step!(c => {
                                let m = Control::Layout {
                                    version: c.layout.version,
                                    links: c.layout.links.clone(),
                                };
                                c.outbox.push(Out::All(m));
                            }),
                            #[cfg(feature = "fake")]
                            Some(Cmd::PanicForTest) => {
                                let _held = lock(&core);
                                panic!("test-induced engine panic");
                            }
                            Some(Cmd::Shutdown(ack)) => {
                                step!(c => c.shutdown());
                                let _ = ack.send(());
                                break;
                            }
                            None => {
                                step!(c => c.shutdown());
                                break;
                            }
                        },
                    }
                }
            })
        };
        Ok(EngineHandle {
            core,
            manager,
            cmd,
            local: local_tx,
            task: Arc::new(Mutex::new(Some(task))),
            shutdown_req: watch::channel(false).0,
            services,
        })
    }
}

async fn flush(manager: &Manager, outs: Vec<Out>) {
    for o in outs {
        let r = match o {
            Out::To(p, c) => tokio::time::timeout(SEND_TIMEOUT, manager.send_control(p, &c))
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("send timed out"))),
            Out::All(c) => {
                let _ = tokio::time::timeout(SEND_TIMEOUT * 2, manager.broadcast(&c)).await;
                Ok(())
            }
        };
        if let Err(e) = r {
            tracing::debug!("control send failed: {e:#}");
        }
    }
}

impl EngineHandle {
    /// The services this device offers to paired devices.
    pub fn services(&self) -> &crate::channels::Services {
        &self.services
    }

    pub fn status(&self) -> Status {
        lock(&self.core).status()
    }

    pub fn metrics(&self) -> MetricsReport {
        lock(&self.core).metrics()
    }

    /// The newest `n` flight-recorder records, oldest first.
    pub fn recent_events(&self, n: usize) -> Vec<Value> {
        lock(&self.core).recorder.recent(n)
    }

    /// Applies and saves `settings`. Keys held for a remote controller are released first:
    /// their ups would otherwise be mapped differently from their downs and stick.
    pub fn set_settings(&self, settings: Settings) -> Result<()> {
        let path = {
            let mut c = lock(&self.core);
            if c.settings.swap_ctrl_cmd != settings.swap_ctrl_cmd {
                c.release("settings");
            }
            c.settings = settings;
            c.settings_path.clone()
        };
        settings.save(&path)?;
        Ok(())
    }

    /// Link `peer` on this machine's `from_edge`; takes effect at once and is broadcast.
    pub fn set_layout(&self, from_edge: Edge, peer: DeviceId) {
        {
            let mut c = lock(&self.core);
            let me = c.me;
            c.layout.set_link(me, from_edge, peer);
        }
        let _ = self.cmd.send(Cmd::BroadcastLayout);
    }

    /// Who currently has the focus.
    pub(crate) fn focus(&self) -> DeviceId {
        lock(&self.core).control.focus()
    }

    /// Feed one event into the engine exactly as if the platform had captured it.
    pub(crate) fn send_local(&self, ev: LocalEvent) {
        let _ = self.local.send(ev);
    }

    /// Cross to `to` (a peer, or this machine to come back) as an edge hit would.
    pub(crate) fn cross_to(&self, to: DeviceId, entry: EntryPoint) {
        let _ = self.cmd.send(Cmd::Cross(to, entry));
    }

    pub(crate) fn soak_counters(&self) -> SoakCounters {
        let c = lock(&self.core);
        SoakCounters {
            reports: c.held_reports,
            held_before: c.held_before,
            held_after: c.held_after,
            safety_releases: c.safety_releases,
            reconnects: c.reconnects,
        }
    }

    /// Identifies the current session with `peer`; `None` while disconnected.
    pub(crate) fn session(&self, peer: DeviceId) -> Option<u64> {
        let c = lock(&self.core);
        c.connected
            .contains(&peer)
            .then(|| c.session_gen.get(&peer).copied())
            .flatten()
    }

    /// Who currently controls.
    pub(crate) fn controller(&self) -> DeviceId {
        lock(&self.core).control.controller()
    }

    /// How long the current session with `peer` has been up.
    pub(crate) fn connected_for(&self, peer: DeviceId) -> Option<Duration> {
        lock(&self.core)
            .connected_at
            .get(&peer)
            .map(|t| t.elapsed())
    }

    /// The cached clock offset for `peer` (remote minus local, microseconds).
    #[doc(hidden)]
    pub fn cached_offset(&self, peer: DeviceId) -> Option<i64> {
        lock(&self.core).offsets.get(&peer).copied()
    }

    /// A best-effort emergency release for a panic hook. Never blocks (skips if the
    /// engine lock is held elsewhere); the supervisor is the reliable path.
    pub fn emergency_release(&self) -> impl Fn() + Send + Sync + 'static {
        let core = self.core.clone();
        move || {
            let mut c = match core.try_lock() {
                Ok(c) => c,
                Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => return,
            };
            c.emergency();
        }
    }

    /// Waits for the engine task to end on its own. Call only while no shutdown has been
    /// requested: any end (panic or not) is a failure, so everything is released here
    /// (blocking lock, poison tolerated) and a flight record `engine_panic` is written.
    /// The caller should then exit with a non-zero code.
    pub async fn supervise(&self) -> EngineExit {
        let task = lock(&self.task).take();
        let Some(task) = task else {
            // Another clone already owns the task.
            return std::future::pending().await;
        };
        let exit = match task.await {
            Err(e) if e.is_panic() => EngineExit::Panicked,
            _ => EngineExit::Ended,
        };
        tracing::error!(?exit, "engine task ended; releasing everything");
        lock(&self.core).emergency();
        exit
    }

    /// Test only: make the engine task panic while it holds the core lock.
    #[cfg(feature = "fake")]
    #[doc(hidden)]
    pub fn panic_for_test(&self) {
        let _ = self.cmd.send(Cmd::PanicForTest);
    }

    pub fn manager(&self) -> &Manager {
        &self.manager
    }

    /// Release everything, hand the cursor back, then stop.
    pub async fn shutdown(self) {
        let (tx, rx) = oneshot::channel();
        if self.cmd.send(Cmd::Shutdown(tx)).is_ok() {
            let _ = rx.await;
        }
        let task = lock(&self.task).take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    /// Asks the OS to prompt for the permissions input capture needs.
    pub fn request_permissions(&self) {
        let platform = lock(&self.core).platform.clone();
        platform.request_permissions();
    }

    /// Asks `main` to shut the agent down; the request is remembered for every waiter.
    pub fn request_shutdown(&self) {
        self.shutdown_req.send_replace(true);
    }

    /// Whether [`request_shutdown`](Self::request_shutdown) was called.
    pub fn is_shutdown_requested(&self) -> bool {
        *self.shutdown_req.borrow()
    }

    /// Resolves once [`request_shutdown`](Self::request_shutdown) was called.
    pub async fn shutdown_requested(&self) {
        let _ = self.shutdown_req.subscribe().wait_for(|&r| r).await;
    }

    /// Platform permission states, as `(name, granted)`.
    pub fn permissions(&self) -> Vec<(&'static str, bool)> {
        let platform = lock(&self.core).platform.clone();
        platform.permissions()
    }

    /// Start pairing with a discovered device.
    pub async fn request_pair(&self, peer: DeviceId) -> Result<()> {
        self.manager.request_pair(peer).await
    }

    /// Pairing codes awaiting a decision.
    pub fn pending_pairs(&self) -> Vec<PendingPair> {
        lock(&self.core).pending_pairs.clone()
    }

    /// Devices seen on the network that are not paired.
    pub fn discovered(&self) -> Vec<novalink_net::discovery::Discovered> {
        self.manager.discovered_unpaired()
    }

    /// Answer a pending pairing code.
    pub async fn answer_pair(&self, peer: DeviceId, accept: bool) {
        self.manager.answer_pair(peer, accept).await;
    }

    /// Remove `peer` from the trust store; its session closes within a tick.
    pub fn unpair(&self, peer: DeviceId) -> Result<()> {
        self.manager.unpair(peer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timed_log_windows_by_age_and_latest_by_time() {
        let t0 = Instant::now();
        let now = t0 + Duration::from_secs(10_000);
        let mut l = TimedLog::new();
        // Pushed out of time order on purpose: "latest" must follow timestamps.
        l.push(now - Duration::from_secs(100), "newest");
        l.push(now - Duration::from_secs(3000), "older");
        l.push(now - Duration::from_secs(5000), "too old");
        assert_eq!(l.count_within(now, RECONNECT_WINDOW), 2);
        assert_eq!(l.latest_within(now, RECONNECT_WINDOW), Some(&"newest"));
        assert_eq!(l.count_within(now, RELEASE_WINDOW), 3);
        l.prune(now, RECONNECT_WINDOW);
        assert_eq!(l.items.len(), 2);
        assert_eq!(
            l.latest_within(now + RECONNECT_WINDOW * 2, RECONNECT_WINDOW),
            None
        );
    }

    #[test]
    fn dump_limiter_allows_one_per_reason_per_gap() {
        let t = Instant::now();
        let mut l = DumpLimiter::new(Duration::from_secs(10));
        assert!(l.allow("safety_release", t));
        assert!(!l.allow("safety_release", t + Duration::from_secs(9)));
        assert!(l.allow("reconnect", t + Duration::from_secs(9)));
        assert!(l.allow("safety_release", t + Duration::from_secs(10)));
    }
}
