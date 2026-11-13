//! The session manager: one encrypted session per paired peer (full mesh), reconnect with
//! backoff, mDNS discovery with last-known-address fallback, and the pairing flow.
//!
//! Roles: for a trusted pair the lower [`DeviceId`] dials and the higher accepts, so there
//! is exactly one connection. Each session is owned by one actor task; the [`Manager`]
//! talks to it through a command channel and the actor reports through [`ManagerEvent`].
//!
//! Pairing (commit-then-reveal, see [`crate::pairing`]): the requester dials in pairing
//! mode and sends `PairCommit`; the acceptor answers `PairReveal`; the requester then
//! reveals; the acceptor checks the reveal against the commitment (constant time). Both
//! sides show the same 6-digit code and exchange `PairConfirm`; only when both accepted is
//! the peer added to the trust store. Rejection, protocol errors, or 60 s of silence close
//! the session and trust nobody.
use crate::discovery::{Discovered, Discovery};
use crate::identity::Identity;
use crate::netif::{self, PathKind};
use crate::pairing::{commitment, new_nonce, sas_code};
use crate::session::{CODE_NOT_PAIRED, CloseReason, Session, SessionEvent};
use crate::tls::{self, PeerPolicy, device_id_of_cert};
use crate::trust::{PairedDevice, TrustStore};
use anyhow::{Context, Result, bail};
use novalink_core::Backoff;
use novalink_proto::{Control, Datagram, DeviceId, DisplayInfo, PROTO_VERSION};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::{AbortHandle, JoinSet};
use tokio::time::Instant;

/// How long a pairing may take from connect to both sides confirming.
pub const PAIR_TIMEOUT: Duration = Duration::from_secs(60);
/// After an inbound pairing fails, further inbound pairing from that IP is refused this long.
pub const PAIR_COOLDOWN: Duration = Duration::from_secs(30);
/// Each handshake step of a pairing (commit, reveal) must complete within this long.
const PAIR_STEP_TIMEOUT: Duration = Duration::from_secs(5);
/// A dialed session is announced only after surviving this long, so a peer that does not
/// consider us paired can say so first.
const SETTLE: Duration = Duration::from_millis(100);
const DIAL_TIMEOUT: Duration = Duration::from_millis(300);
/// After the first candidate address works, wait this long for better ones.
const CANDIDATE_GRACE: Duration = Duration::from_millis(50);
const STATS_INTERVAL: Duration = Duration::from_millis(100);
/// Lets a final `PairConfirm { accepted: false }` leave before the connection closes.
const REJECT_FLUSH: Duration = Duration::from_millis(100);
/// A session that died younger than this makes the dialer wait before retrying.
const FLAP_WINDOW: Duration = Duration::from_millis(300);
const FLAP_PENALTY: Duration = Duration::from_millis(200);
const NO_ADDR_WAIT: Duration = Duration::from_millis(200);
const POLL: Duration = Duration::from_millis(100);
/// A stale-trust probe is exempt from the inbound cool-down only if it opens the session
/// this soon after the connection took the inbound slot (before the Hello exchange), and only this many times per source IP per period
/// (Ruling 47): otherwise a connector could hold the pairing slot and then dodge the
/// cool-down by dressing up as a probe.
const STALE_PROBE_WINDOW: Duration = Duration::from_millis(500);
const STALE_PROBE_BUDGET: usize = 3;
const STALE_PROBE_PERIOD: Duration = Duration::from_secs(60);
/// A dialer whose peer answered NotPaired retries this rarely, so its probes stay within
/// the peer's exemption budget. A fresh pairing does not wait for it: the pairing
/// session itself becomes the live session.
const NOT_PAIRED_REDIAL: Duration = Duration::from_secs(25);
const EVENT_QUEUE: usize = 1024;
const CMD_QUEUE: usize = 1024;

/// Pairing and settle timings; the defaults are the production values.
#[derive(Clone, Copy, Debug)]
pub struct Timings {
    /// Absolute limit for a whole pairing, never extended by traffic.
    pub pair_timeout: Duration,
    /// Limit for each of the commit and reveal steps.
    pub pair_step_timeout: Duration,
    pub pair_cooldown: Duration,
    pub settle: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            pair_timeout: PAIR_TIMEOUT,
            pair_step_timeout: PAIR_STEP_TIMEOUT,
            pair_cooldown: PAIR_COOLDOWN,
            settle: SETTLE,
        }
    }
}

pub struct ManagerConfig {
    pub identity: Identity,
    pub trust: Arc<Mutex<TrustStore>>,
    pub name: String,
    pub bind: SocketAddr,
    pub hello_displays: Vec<DisplayInfo>,
    pub discovery: bool,
}

#[derive(Debug)]
pub enum ManagerEvent {
    Connected {
        peer: DeviceId,
        hello: Control,
    },
    Datagram {
        peer: DeviceId,
        d: Datagram,
    },
    Control {
        peer: DeviceId,
        c: Control,
    },
    Lost {
        peer: DeviceId,
        reason: CloseReason,
    },
    PairRequest {
        peer: DeviceId,
        name: String,
        code: String,
    },
    Paired {
        peer: DeviceId,
    },
    PairRejected {
        peer: DeviceId,
    },
    /// A paired peer opened a service channel and named a valid service.
    Channel {
        peer: DeviceId,
        service: String,
        send: quinn::SendStream,
        recv: quinn::RecvStream,
    },
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct PeerStatus {
    pub id: DeviceId,
    pub name: String,
    pub connected: bool,
    pub addr: Option<SocketAddr>,
    pub path: PathKind,
    pub rtt_us: Option<u64>,
    pub clock_offset_us: Option<i64>,
    pub lost_datagrams: u64,
    pub reconnects: u64,
    pub last_close: Option<CloseReason>,
    /// Whether the peer's agent reported `--dry-inject` (filled in by the engine).
    pub dry_inject: Option<bool>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    Requester,
    Acceptor,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Normal,
    Pair(Role),
}

enum Cmd {
    Datagram(Datagram),
    Control(Control, oneshot::Sender<Result<()>>),
    Answer(bool),
    OpenChannel(
        String,
        oneshot::Sender<Result<(quinn::SendStream, quinn::RecvStream)>>,
    ),
    /// Replaced by a newer session: stop without reporting.
    Shutdown,
}

struct SessionHandle {
    epoch: u64,
    cmd: mpsc::Sender<Cmd>,
    pairing: bool,
    addr: SocketAddr,
    path: PathKind,
    rtt_us: Option<u64>,
    clock_offset_us: Option<i64>,
    lost: u64,
}

#[derive(Default)]
struct PeerStat {
    ever_connected: bool,
    reconnects: u64,
    last_close: Option<CloseReason>,
}

#[derive(Default)]
struct State {
    sessions: HashMap<DeviceId, SessionHandle>,
    /// Peers with a pairing in progress, and which side we are.
    pending: HashMap<DeviceId, Role>,
    discovered: HashMap<DeviceId, Discovered>,
    stats: HashMap<DeviceId, PeerStat>,
    dialers: HashSet<DeviceId>,
    next_epoch: u64,
    /// At most one unsolicited inbound pairing at a time.
    inbound_pairing: bool,
    /// Source IPs whose inbound pairing attempts are refused until the instant.
    cooldowns: HashMap<std::net::IpAddr, Instant>,
    /// Recent cool-down-exempt stale-trust probes, per source IP.
    probe_exempt: HashMap<std::net::IpAddr, VecDeque<Instant>>,
}

/// Admits trusted peers and peers with a pairing pending. Because the acceptor of a
/// pairing has no prior request for the dialer, one unsolicited inbound pairing is
/// admitted at a time; it still must open with a valid `PairCommit` and be confirmed by
/// a human on both machines before anything is trusted.
struct ManagerPolicy {
    trust: Arc<Mutex<TrustStore>>,
    state: Arc<Mutex<State>>,
}

impl PeerPolicy for ManagerPolicy {
    fn allow(&self, peer: &DeviceId) -> bool {
        if lock(&self.trust).is_trusted(peer) {
            return true;
        }
        let st = lock(&self.state);
        st.pending.contains_key(peer) || !st.inbound_pairing
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct Inner {
    me: DeviceId,
    name: String,
    displays: Vec<DisplayInfo>,
    trust: Arc<Mutex<TrustStore>>,
    state: Arc<Mutex<State>>,
    endpoint: quinn::Endpoint,
    events: mpsc::Sender<ManagerEvent>,
    changed: Notify,
    timings: Timings,
    tasks: Mutex<Vec<AbortHandle>>,
}

pub struct Manager {
    inner: Arc<Inner>,
    _discovery: Option<Discovery>,
}

fn peer_id(conn: &quinn::Connection) -> Result<DeviceId> {
    let ident = conn.peer_identity().context("no peer certificate")?;
    let certs = ident
        .downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
        .map_err(|_| anyhow::anyhow!("unexpected peer identity type"))?;
    certs
        .first()
        .and_then(device_id_of_cert)
        .context("peer certificate has no ed25519 key")
}

/// Candidate order for a pairing dial: duplicates dropped, then routable addresses before
/// link-local ones before VPN ones, otherwise in the order discovery listed them.
fn pair_dial_order(mut addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let mut seen = HashSet::new();
    addrs.retain(|a| seen.insert(*a));
    addrs.sort_by_key(|a| {
        (
            netif::rank(netif::classify("", a.ip())),
            netif::is_link_local(a.ip()),
        )
    });
    addrs
}

/// Whether `nonce` is the one `commit` committed `id` to. Constant time.
fn reveal_matches(id: &DeviceId, nonce: &[u8; 32], commit: &[u8; 32]) -> bool {
    commitment(id, nonce).ct_eq(commit).into()
}

impl Manager {
    pub async fn start(cfg: ManagerConfig) -> Result<(Manager, mpsc::Receiver<ManagerEvent>)> {
        Self::start_with(cfg, Timings::default()).await
    }

    pub async fn start_with(
        cfg: ManagerConfig,
        timings: Timings,
    ) -> Result<(Manager, mpsc::Receiver<ManagerEvent>)> {
        let me = cfg.identity.device_id();
        let state = Arc::new(Mutex::new(State::default()));
        let policy = Arc::new(ManagerPolicy {
            trust: cfg.trust.clone(),
            state: state.clone(),
        });
        let endpoint = bind_with_retry(&cfg.identity, cfg.bind, policy).await?;
        let port = endpoint.local_addr()?.port();
        let (tx, rx) = mpsc::channel(EVENT_QUEUE);
        let inner = Arc::new(Inner {
            me,
            name: cfg.name,
            displays: cfg.hello_displays,
            trust: cfg.trust,
            state,
            endpoint,
            events: tx,
            changed: Notify::new(),
            timings,
            tasks: Mutex::new(Vec::new()),
        });
        inner.track(tokio::spawn(inner.clone().accept_loop()));
        for d in lock(&inner.trust).list() {
            inner.ensure_dialer(d.id);
        }
        let mut discovery = None;
        if cfg.discovery {
            let (d, mut found) = Discovery::start(me, &inner.name, port)?;
            discovery = Some(d);
            let i = inner.clone();
            inner.track(tokio::spawn(async move {
                while let Some(d) = found.recv().await {
                    i.hint(d);
                }
            }));
        }
        Ok((
            Manager {
                inner,
                _discovery: discovery,
            },
            rx,
        ))
    }

    pub fn id(&self) -> DeviceId {
        self.inner.me
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.inner
            .endpoint
            .local_addr()
            .expect("bound endpoint has an address")
    }

    /// Tells the manager where a device may be reached, as discovery would.
    pub fn hint_peer(&self, d: Discovered) {
        self.inner.hint(d);
    }

    /// Sends a motion/scroll datagram; silently dropped if the peer is not connected.
    pub fn send_datagram(&self, to: DeviceId, d: &Datagram) {
        let st = lock(&self.inner.state);
        if let Some(h) = st.sessions.get(&to).filter(|h| !h.pairing) {
            let _ = h.cmd.try_send(Cmd::Datagram(*d));
        }
    }

    pub async fn send_control(&self, to: DeviceId, c: &Control) -> Result<()> {
        let cmd = {
            let st = lock(&self.inner.state);
            st.sessions
                .get(&to)
                .filter(|h| !h.pairing)
                .map(|h| h.cmd.clone())
        }
        .context("peer is not connected")?;
        let (tx, rx) = oneshot::channel();
        cmd.send(Cmd::Control(c.clone(), tx))
            .await
            .map_err(|_| anyhow::anyhow!("session ended"))?;
        rx.await.map_err(|_| anyhow::anyhow!("session ended"))?
    }

    /// Opens a service channel to a connected, paired peer. The peer sees
    /// [`ManagerEvent::Channel`]. Fails if there is no live session or the name is invalid
    /// (1-32 bytes of `[a-z0-9-]`).
    pub async fn open_channel(
        &self,
        to: DeviceId,
        service: &str,
    ) -> Result<(quinn::SendStream, quinn::RecvStream)> {
        let cmd = {
            let st = lock(&self.inner.state);
            st.sessions
                .get(&to)
                .filter(|h| !h.pairing)
                .map(|h| h.cmd.clone())
        }
        .context("peer is not connected")?;
        let (tx, rx) = oneshot::channel();
        cmd.send(Cmd::OpenChannel(service.to_string(), tx))
            .await
            .map_err(|_| anyhow::anyhow!("session ended"))?;
        rx.await.map_err(|_| anyhow::anyhow!("session ended"))?
    }

    /// Best effort to every connected peer.
    pub async fn broadcast(&self, c: &Control) {
        let peers: Vec<DeviceId> = lock(&self.inner.state)
            .sessions
            .iter()
            .filter(|(_, h)| !h.pairing)
            .map(|(id, _)| *id)
            .collect();
        for p in peers {
            let _ = self.send_control(p, c).await;
        }
    }

    /// Status of every paired device.
    pub fn peers(&self) -> Vec<PeerStatus> {
        let devices = lock(&self.inner.trust).list();
        let st = lock(&self.inner.state);
        devices
            .into_iter()
            .map(|d| {
                let h = st.sessions.get(&d.id).filter(|h| !h.pairing);
                let stat = st.stats.get(&d.id);
                PeerStatus {
                    id: d.id,
                    name: d.name,
                    connected: h.is_some(),
                    addr: h.map(|h| h.addr),
                    path: h.map(|h| h.path).unwrap_or(PathKind::Other),
                    rtt_us: h.and_then(|h| h.rtt_us),
                    clock_offset_us: h.and_then(|h| h.clock_offset_us),
                    lost_datagrams: h.map(|h| h.lost).unwrap_or(0),
                    reconnects: stat.map(|s| s.reconnects).unwrap_or(0),
                    last_close: stat.and_then(|s| s.last_close.clone()),
                    dry_inject: None,
                }
            })
            .collect()
    }

    /// Devices seen on the network that we have not paired with.
    pub fn discovered_unpaired(&self) -> Vec<Discovered> {
        let trust = lock(&self.inner.trust);
        let st = lock(&self.inner.state);
        st.discovered
            .values()
            .filter(|d| !trust.is_trusted(&d.id))
            .cloned()
            .collect()
    }

    /// Starts pairing with a discovered device. Returns once the pairing session is up;
    /// the code then arrives as [`ManagerEvent::PairRequest`].
    pub async fn request_pair(&self, id: DeviceId) -> Result<()> {
        let inner = &self.inner;
        if id == inner.me {
            bail!("cannot pair with ourselves");
        }
        if lock(&inner.trust).is_trusted(&id) {
            bail!("already paired");
        }
        let addrs = {
            let mut st = lock(&inner.state);
            let addrs = st
                .discovered
                .get(&id)
                .map(|d| d.addrs.clone())
                .unwrap_or_default();
            if addrs.is_empty() {
                bail!("no known address for that device");
            }
            if st.pending.contains_key(&id) {
                bail!("pairing already in progress");
            }
            st.pending.insert(id, Role::Requester);
            addrs
        };
        if let Err(e) = inner.dial(id, addrs, Mode::Pair(Role::Requester)).await {
            inner.end_pairing(&id);
            return Err(e);
        }
        Ok(())
    }

    /// Forget a paired device. Its session notices on its next tick and closes.
    pub fn unpair(&self, id: DeviceId) -> Result<()> {
        lock(&self.inner.trust).remove(&id)
    }

    /// The user's decision about the code on screen. No-op without a pairing session.
    pub async fn answer_pair(&self, id: DeviceId, accept: bool) {
        let cmd = lock(&self.inner.state)
            .sessions
            .get(&id)
            .filter(|h| h.pairing)
            .map(|h| h.cmd.clone());
        if let Some(cmd) = cmd {
            let _ = cmd.send(Cmd::Answer(accept)).await;
        }
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        for t in lock(&self.inner.tasks).drain(..) {
            t.abort();
        }
        self.inner.endpoint.close(0u32.into(), b"shutdown");
    }
}

/// A restarted process may race the previous socket's release; retry briefly on a fixed port.
async fn bind_with_retry(
    identity: &Identity,
    bind: SocketAddr,
    policy: Arc<dyn PeerPolicy>,
) -> Result<quinn::Endpoint> {
    let mut tries = 0;
    loop {
        match tls::endpoint(identity, bind, policy.clone()) {
            Ok(ep) => return Ok(ep),
            Err(e) if bind.port() != 0 && tries < 40 => {
                tracing::debug!("bind {bind} failed ({e:#}); retrying");
                tries += 1;
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

impl Inner {
    fn track(&self, h: tokio::task::JoinHandle<()>) {
        lock(&self.tasks).push(h.abort_handle());
    }

    async fn emit(&self, ev: ManagerEvent) {
        let _ = self.events.send(ev).await;
    }

    fn hint(&self, d: Discovered) {
        if d.id == self.me {
            return;
        }
        lock(&self.state).discovered.insert(d.id, d);
        self.changed.notify_waiters();
    }

    fn is_trusted(&self, id: &DeviceId) -> bool {
        lock(&self.trust).is_trusted(id)
    }

    fn cool_down(&self, ip: std::net::IpAddr) {
        lock(&self.state)
            .cooldowns
            .insert(ip, Instant::now() + self.timings.pair_cooldown);
    }

    /// Spends one of `ip`'s stale-probe exemptions; false once the budget is used up.
    fn take_probe_exemption(&self, ip: std::net::IpAddr) -> bool {
        let now = Instant::now();
        let mut st = lock(&self.state);
        st.probe_exempt.retain(|_, q| {
            q.back()
                .is_some_and(|t| now.duration_since(*t) < STALE_PROBE_PERIOD)
        });
        let q = st.probe_exempt.entry(ip).or_default();
        while q
            .front()
            .is_some_and(|t| now.duration_since(*t) >= STALE_PROBE_PERIOD)
        {
            q.pop_front();
        }
        if q.len() >= STALE_PROBE_BUDGET {
            return false;
        }
        q.push_back(now);
        true
    }

    fn end_pairing(&self, peer: &DeviceId) {
        let mut st = lock(&self.state);
        if st.pending.remove(peer) == Some(Role::Acceptor) {
            st.inbound_pairing = false;
        }
    }

    fn hello(&self) -> Control {
        Control::Hello {
            proto_version: PROTO_VERSION,
            name: self.name.clone(),
            displays: self.displays.clone(),
        }
    }

    /// The lower id dials; start the keep-connected loop for a trusted peer if it is ours to dial.
    fn ensure_dialer(self: &Arc<Self>, peer: DeviceId) {
        if self.me >= peer || !lock(&self.state).dialers.insert(peer) {
            return;
        }
        self.track(tokio::spawn(self.clone().dial_loop(peer)));
    }

    async fn accept_loop(self: Arc<Self>) {
        while let Some(incoming) = self.endpoint.accept().await {
            let inner = self.clone();
            tokio::spawn(async move {
                let result = async {
                    let conn = incoming.await?;
                    inner.handle_inbound(conn).await
                }
                .await;
                if let Err(e) = result {
                    tracing::debug!("inbound connection dropped: {e:#}");
                }
            });
        }
    }

    async fn handle_inbound(self: &Arc<Self>, conn: quinn::Connection) -> Result<()> {
        let peer = peer_id(&conn)?;
        let mode = if self.is_trusted(&peer) {
            if peer > self.me {
                // The higher id never dials a trusted peer.
                conn.close(0u32.into(), b"wrong direction");
                bail!("trusted peer {peer} dialled the wrong way");
            }
            Mode::Normal
        } else {
            let mut st = lock(&self.state);
            let ip = conn.remote_address().ip();
            st.cooldowns.retain(|_, until| *until > Instant::now());
            if st.cooldowns.contains_key(&ip) {
                drop(st);
                conn.close(CODE_NOT_PAIRED.into(), b"cooling down");
                bail!("inbound pairing from {ip} is cooling down");
            }
            if st.inbound_pairing || st.pending.contains_key(&peer) {
                drop(st);
                conn.close(0u32.into(), b"busy");
                bail!("pairing already in progress");
            }
            st.inbound_pairing = true;
            st.pending.insert(peer, Role::Acceptor);
            Mode::Pair(Role::Acceptor)
        };
        // The stale-probe window runs from here, when the inbound slot was taken, so a
        // stalled Hello exchange counts against it (Ruling 47).
        self.start_session(conn, mode, false, Instant::now()).await
    }

    /// Dials every candidate address at once and keeps the best working path
    /// (lowest `rank`, then lowest RTT). A pairing dials one address at a time instead.
    async fn dial(
        self: &Arc<Self>,
        peer: DeviceId,
        addrs: Vec<SocketAddr>,
        mode: Mode,
    ) -> Result<()> {
        if matches!(mode, Mode::Pair(_)) {
            return self.dial_one_by_one(peer, addrs, mode).await;
        }
        let mut set = JoinSet::new();
        for addr in addrs {
            let ep = self.endpoint.clone();
            set.spawn(async move {
                let connecting = ep.connect(addr, "novalink").ok()?;
                tokio::time::timeout(DIAL_TIMEOUT, connecting)
                    .await
                    .ok()?
                    .ok()
            });
        }
        let mut winners: Vec<quinn::Connection> = Vec::new();
        let mut deadline: Option<Instant> = None;
        loop {
            let next = match deadline {
                Some(d) => match tokio::time::timeout_at(d, set.join_next()).await {
                    Ok(n) => n,
                    Err(_) => break,
                },
                None => set.join_next().await,
            };
            let Some(res) = next else { break };
            if let Ok(Some(conn)) = res {
                if peer_id(&conn).is_ok_and(|id| id == peer) {
                    winners.push(conn);
                    deadline.get_or_insert(Instant::now() + CANDIDATE_GRACE);
                } else {
                    conn.close(0u32.into(), b"unexpected device");
                }
            }
        }
        set.abort_all();
        winners.sort_by_key(|c| {
            (
                netif::rank(netif::classify("", c.remote_address().ip())),
                c.rtt(),
            )
        });
        let mut winners = winners.into_iter();
        let best = winners.next().context("no candidate address answered")?;
        for extra in winners {
            extra.close(0u32.into(), b"duplicate path");
        }
        self.start_session(best, mode, true, Instant::now()).await
    }

    /// The acceptor holds a single pairing slot and cools the source down when the
    /// connection in it fails. Racing paths would let each side keep a different one
    /// (Ruling 48), so the candidates are tried in preference order and the first that
    /// answers as `peer` is the only connection the acceptor ever sees.
    async fn dial_one_by_one(
        self: &Arc<Self>,
        peer: DeviceId,
        addrs: Vec<SocketAddr>,
        mode: Mode,
    ) -> Result<()> {
        for addr in pair_dial_order(addrs) {
            let Ok(connecting) = self.endpoint.connect(addr, "novalink") else {
                continue;
            };
            let Ok(Ok(conn)) = tokio::time::timeout(DIAL_TIMEOUT, connecting).await else {
                continue;
            };
            if peer_id(&conn).is_ok_and(|id| id == peer) {
                return self.start_session(conn, mode, true, Instant::now()).await;
            }
            conn.close(0u32.into(), b"unexpected device");
        }
        bail!("no candidate address answered")
    }

    /// Hello exchange, trust re-check, registration, and the session actor.
    async fn start_session(
        self: &Arc<Self>,
        conn: quinn::Connection,
        mode: Mode,
        dialed: bool,
        slot_taken: Instant,
    ) -> Result<()> {
        let addr = conn.remote_address();
        let peer = peer_id(&conn)?;
        let pairing = matches!(mode, Mode::Pair(_));
        let acceptor = matches!(mode, Mode::Pair(Role::Acceptor));
        let abort = |this: &Self| {
            if pairing {
                this.end_pairing(&peer);
            }
            // A failed inbound pairing establish (e.g. no Hello) held the slot (Ruling 28).
            if acceptor {
                this.cool_down(addr.ip());
            }
        };
        let (session, hello) = match Session::establish(conn, self.me, self.hello()).await {
            Ok(v) => v,
            Err(e) => {
                abort(self);
                return Err(e);
            }
        };
        // Defence in depth: the TLS policy may have raced a trust change.
        let allowed = self.is_trusted(&peer) || lock(&self.state).pending.contains_key(&peer);
        if !allowed {
            drop(session);
            bail!("peer {peer} is neither trusted nor pairing");
        }
        if !pairing && dialed {
            let _ = lock(&self.trust).note_addr(&peer, addr);
        }
        let (cmd_tx, cmd_rx) = mpsc::channel(CMD_QUEUE);
        let epoch = {
            let mut st = lock(&self.state);
            let epoch = st.next_epoch;
            st.next_epoch += 1;
            let old = st.sessions.insert(
                peer,
                SessionHandle {
                    epoch,
                    cmd: cmd_tx,
                    pairing,
                    addr,
                    path: netif::classify("", addr.ip()),
                    rtt_us: None,
                    clock_offset_us: None,
                    lost: 0,
                },
            );
            if let Some(old) = old {
                let _ = old.cmd.try_send(Cmd::Shutdown);
            }
            if !pairing {
                let stat = st.stats.entry(peer).or_default();
                if stat.ever_connected {
                    stat.reconnects += 1;
                }
                stat.ever_connected = true;
            }
            epoch
        };
        self.changed.notify_waiters();
        let actor = Actor {
            inner: self.clone(),
            session,
            peer,
            epoch,
            addr,
            hello,
            cmds: cmd_rx,
            settle: dialed && !pairing,
            slot_taken,
        };
        tokio::spawn(actor.run(mode));
        Ok(())
    }

    fn candidates(&self, peer: &DeviceId) -> Vec<SocketAddr> {
        let mut out: Vec<SocketAddr> = lock(&self.state)
            .discovered
            .get(peer)
            .map(|d| d.addrs.clone())
            .unwrap_or_default();
        if let Some(d) = lock(&self.trust).list().into_iter().find(|d| d.id == *peer) {
            for a in d.last_addrs {
                if !out.contains(&a) {
                    out.push(a);
                }
            }
        }
        out
    }

    fn last_close_not_paired(&self, peer: &DeviceId) -> bool {
        lock(&self.state)
            .stats
            .get(peer)
            .is_some_and(|s| s.last_close == Some(CloseReason::NotPaired))
    }

    fn has_session(&self, peer: &DeviceId) -> bool {
        lock(&self.state).sessions.contains_key(peer)
    }

    /// Keeps one session up to a trusted peer we are responsible for dialing.
    async fn dial_loop(self: Arc<Self>, peer: DeviceId) {
        let mut backoff = Backoff::default();
        let mut up_since: Option<Instant> = None;
        loop {
            if !self.is_trusted(&peer) {
                lock(&self.state).dialers.remove(&peer);
                return;
            }
            if self.has_session(&peer) {
                up_since.get_or_insert_with(Instant::now);
                let _ = tokio::time::timeout(POLL, self.changed.notified()).await;
                continue;
            }
            // Backoff resets only for a session that stayed up; one that died young
            // (e.g. a peer that no longer trusts us) keeps backing off.
            if up_since.is_some() && self.last_close_not_paired(&peer) {
                up_since = None;
                // The peer does not trust us: retry rarely, but wake at once if a
                // session (e.g. a fresh pairing) comes up or trust goes away.
                let until = Instant::now() + NOT_PAIRED_REDIAL;
                while Instant::now() < until && !self.has_session(&peer) && self.is_trusted(&peer) {
                    let _ = tokio::time::timeout(POLL, self.changed.notified()).await;
                }
                continue;
            }
            if let Some(t) = up_since.take() {
                if t.elapsed() < FLAP_WINDOW {
                    tokio::time::sleep(FLAP_PENALTY).await;
                } else {
                    backoff.reset();
                }
            }
            tokio::time::sleep(backoff.next_delay()).await;
            if self.has_session(&peer) {
                continue;
            }
            let addrs = self.candidates(&peer);
            if addrs.is_empty() {
                let _ = tokio::time::timeout(NO_ADDR_WAIT, self.changed.notified()).await;
                continue;
            }
            match self.dial(peer, addrs, Mode::Normal).await {
                Ok(()) => {}
                Err(e) => tracing::debug!("dial {peer} failed: {e:#}"),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// Session actor
// ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    AwaitCommit,
    AwaitReveal,
    Confirming,
}

struct PairState {
    role: Role,
    stage: Stage,
    peer_name: String,
    my_nonce: [u8; 32],
    peer_commit: Option<[u8; 32]>,
    my_answer: Option<bool>,
    peer_answer: Option<bool>,
    /// The code was handed to the user.
    shown: bool,
    /// The peer opened with `PairConfirm { accepted: false }` within
    /// `STALE_PROBE_WINDOW` of taking the inbound slot: the settle probe of a peer that still trusts
    /// us (stale trust), not a pairing attempt.
    stale_probe: bool,
}

enum Progress {
    Pending,
    /// Both sides accepted.
    Done,
    /// Rejected, mismatched, malformed or failed to send.
    Aborted,
    /// An untrusted peer opened with something other than `PairCommit`.
    NotPaired,
}

impl PairState {
    /// Whether a failure should be reported: the user saw a code, or the user started it.
    fn report_failure(&self) -> bool {
        self.shown || self.role == Role::Requester
    }

    fn evaluate(&self) -> Progress {
        match (self.my_answer, self.peer_answer) {
            (Some(false), _) | (_, Some(false)) => Progress::Aborted,
            (Some(true), Some(true)) => Progress::Done,
            _ => Progress::Pending,
        }
    }
}

enum End {
    Closed(CloseReason),
    /// Pairing ended without both sides accepting (reported if the user had seen a code).
    PairFailed,
    /// Closed before being announced; nothing to report.
    Unannounced(CloseReason),
    /// Replaced by a newer session, or the manager is going away.
    Quiet,
}

struct Actor {
    inner: Arc<Inner>,
    session: Session,
    peer: DeviceId,
    epoch: u64,
    addr: SocketAddr,
    hello: Control,
    cmds: mpsc::Receiver<Cmd>,
    /// Dialed normal session: announce `Connected` only after it survives the settle time.
    settle: bool,
    /// When this connection took its slot (before the Hello exchange).
    slot_taken: Instant,
}

impl Actor {
    async fn run(mut self, mode: Mode) {
        let mut pair = match mode {
            Mode::Normal => None,
            Mode::Pair(role) => Some(PairState {
                role,
                stage: match role {
                    Role::Requester => Stage::AwaitReveal,
                    Role::Acceptor => Stage::AwaitCommit,
                },
                peer_name: match &self.hello {
                    Control::Hello { name, .. } => name.clone(),
                    _ => String::new(),
                },
                my_nonce: new_nonce(),
                peer_commit: None,
                my_answer: None,
                peer_answer: None,
                shown: false,
                stale_probe: false,
            }),
        };
        let acceptor = matches!(mode, Mode::Pair(Role::Acceptor));
        let end = self.drive(&mut pair).await;
        // Every failed inbound pairing holds the single slot for a while, so its source
        // cools down (Ruling 28) -- except a stale-trust probe, which is answered with
        // NotPaired and must not block the peer from pairing afresh.
        let stale_probe = pair.as_ref().is_some_and(|p| p.stale_probe);
        if acceptor
            && matches!(end, End::PairFailed)
            && !(stale_probe && self.inner.take_probe_exemption(self.addr.ip()))
        {
            self.inner.cool_down(self.addr.ip());
        }
        self.finish(pair.is_some(), end).await;
    }

    async fn drive(&mut self, pair: &mut Option<PairState>) -> End {
        let inner = self.inner.clone();
        let peer = self.peer;
        let t = inner.timings;
        // One absolute limit for the whole pairing; traffic never extends it. The commit
        // and reveal steps each get a short window of their own inside it.
        let hard = Instant::now() + t.pair_timeout;
        let step = |now: Instant| hard.min(now + t.pair_step_timeout);
        let mut deadline = step(Instant::now());
        let mut settle_at: Option<Instant> = None;
        let mut held: Vec<ManagerEvent> = Vec::new();
        if let Some(p) = pair.as_ref() {
            if p.role == Role::Requester {
                let commit = commitment(&inner.me, &p.my_nonce);
                if self
                    .session
                    .send_control(&Control::PairCommit { commit })
                    .await
                    .is_err()
                {
                    return self.pair_failed(true).await;
                }
            }
        } else if self.settle {
            // Probe: a peer that does not consider us paired answers this with NotPaired.
            // Paired peers ignore it.
            let _ = self
                .session
                .send_control(&Control::PairConfirm { accepted: false })
                .await;
            settle_at = Some(Instant::now() + t.settle);
        } else {
            inner.emit(self.connected()).await;
        }
        let mut tick = tokio::time::interval(STATS_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let progress = tokio::select! {
                ev = self.session.events().recv() => match ev {
                    // Closed is terminal: anything queued behind it is ignored.
                    None => return self.closed(pair, settle_at, CloseReason::PeerClosed).await,
                    Some(SessionEvent::Closed(r)) => return self.closed(pair, settle_at, r).await,
                    Some(SessionEvent::Datagram(d)) => {
                        if pair.is_none() && settle_at.is_none() {
                            let _ = inner.events.try_send(ManagerEvent::Datagram { peer, d });
                        }
                        continue;
                    }
                    Some(SessionEvent::Channel { service, send, recv }) => {
                        // Only an established session with a still-trusted peer may reach the
                        // application; a revoke can land before the next tick closes the session.
                        if pair.is_none() && inner.is_trusted(&peer) {
                            let ev = ManagerEvent::Channel { peer, service, send, recv };
                            if settle_at.is_some() {
                                held.push(ev);
                            } else {
                                inner.emit(ev).await;
                            }
                        } else {
                            crate::session::refuse_channel(send, recv);
                        }
                        continue;
                    }
                    Some(SessionEvent::Control(c)) => match pair.as_mut() {
                        Some(p) => {
                            let before = p.stage;
                            let r = self.pair_control(p, c).await;
                            if p.stage != before {
                                deadline = if p.stage == Stage::Confirming {
                                    hard
                                } else {
                                    step(Instant::now())
                                };
                            }
                            r
                        }
                        None => {
                            if !matches!(
                                c,
                                Control::PairCommit { .. }
                                    | Control::PairReveal { .. }
                                    | Control::PairConfirm { .. }
                            ) {
                                let ev = ManagerEvent::Control { peer, c };
                                if settle_at.is_some() {
                                    held.push(ev);
                                } else {
                                    inner.emit(ev).await;
                                }
                            }
                            continue;
                        }
                    },
                },
                cmd = self.cmds.recv() => match cmd {
                    None | Some(Cmd::Shutdown) => return End::Quiet,
                    Some(Cmd::Datagram(d)) => {
                        let _ = self.session.send_datagram(&d);
                        continue;
                    }
                    Some(Cmd::Control(c, reply)) => {
                        let _ = reply.send(self.session.send_control(&c).await);
                        continue;
                    }
                    Some(Cmd::OpenChannel(service, reply)) => {
                        // Off the actor task: opening can wait on stream limits.
                        let opener = self.session.channel_opener();
                        tokio::spawn(async move {
                            let _ = reply.send(opener.open(&service).await);
                        });
                        continue;
                    }
                    Some(Cmd::Answer(accept)) => match pair.as_mut() {
                        Some(p) => self.pair_answer(p, accept).await,
                        None => continue,
                    },
                },
                _ = tick.tick() => {
                    if pair.is_none() && !inner.is_trusted(&peer) {
                        let r = CloseReason::Protocol("peer is no longer trusted".into());
                        return if settle_at.is_some() { End::Unannounced(r) } else { End::Closed(r) };
                    }
                    self.publish_stats();
                    continue;
                }
                _ = tokio::time::sleep_until(settle_at.unwrap_or(hard)), if settle_at.is_some() => {
                    settle_at = None;
                    inner.emit(self.connected()).await;
                    for ev in held.drain(..) {
                        // Trust can be revoked while the session settles: a held channel is checked again.
                        match ev {
                            ManagerEvent::Channel { send, recv, .. } if !inner.is_trusted(&peer) => {
                                crate::session::refuse_channel(send, recv);
                            }
                            ev => inner.emit(ev).await,
                        }
                    }
                    continue;
                }
                _ = tokio::time::sleep_until(deadline), if pair.is_some() => Progress::Aborted,
            };
            match progress {
                Progress::Pending => {}
                Progress::Aborted | Progress::NotPaired => {
                    let (shown, rejected_locally) = pair
                        .as_ref()
                        .map(|p| (p.report_failure(), p.my_answer == Some(false)))
                        .unwrap_or_default();
                    if matches!(progress, Progress::NotPaired) {
                        self.session.close_with(CloseReason::NotPaired).await;
                    } else if rejected_locally {
                        tokio::time::sleep(REJECT_FLUSH).await;
                    }
                    return self.pair_failed(shown).await;
                }
                Progress::Done => {
                    let Some(p) = pair.take() else { continue };
                    if !self.complete_pairing(&p).await {
                        *pair = Some(p);
                        return self.pair_failed(true).await;
                    }
                }
            }
        }
    }

    fn connected(&self) -> ManagerEvent {
        ManagerEvent::Connected {
            peer: self.peer,
            hello: self.hello.clone(),
        }
    }

    /// The session closed. During pairing that is a failed pairing; before a dialed
    /// session was announced it is a silent failure.
    async fn closed(
        &self,
        pair: &Option<PairState>,
        settle_at: Option<Instant>,
        reason: CloseReason,
    ) -> End {
        if let Some(p) = pair {
            return self.pair_failed(p.report_failure()).await;
        }
        if settle_at.is_some() {
            return End::Unannounced(reason);
        }
        End::Closed(reason)
    }

    /// Reports a failed pairing, but only if the user was ever shown a code for it.
    async fn pair_failed(&self, shown: bool) -> End {
        if shown {
            self.inner
                .emit(ManagerEvent::PairRejected { peer: self.peer })
                .await;
        }
        End::PairFailed
    }

    async fn pair_control(&self, p: &mut PairState, c: Control) -> Progress {
        let inner = &self.inner;
        match (p.stage, c) {
            (Stage::AwaitCommit, Control::PairCommit { commit }) if p.role == Role::Acceptor => {
                p.peer_commit = Some(commit);
                let reveal = Control::PairReveal { nonce: p.my_nonce };
                if self.session.send_control(&reveal).await.is_err() {
                    return Progress::Aborted;
                }
                p.stage = Stage::AwaitReveal;
                Progress::Pending
            }
            (Stage::AwaitReveal, Control::PairReveal { nonce }) => {
                match (p.role, p.peer_commit) {
                    (Role::Acceptor, Some(commit)) => {
                        if !reveal_matches(&self.peer, &nonce, &commit) {
                            tracing::warn!(
                                "pairing reveal from {} does not match its commitment",
                                self.peer
                            );
                            return Progress::Aborted;
                        }
                    }
                    (Role::Requester, _) => {
                        let reveal = Control::PairReveal { nonce: p.my_nonce };
                        if self.session.send_control(&reveal).await.is_err() {
                            return Progress::Aborted;
                        }
                    }
                    (Role::Acceptor, None) => return Progress::Aborted,
                }
                let code = sas_code(
                    &inner.me,
                    &p.my_nonce,
                    &self.peer,
                    &nonce,
                    &self.session.tls_binding(),
                );
                p.stage = Stage::Confirming;
                p.shown = true;
                inner
                    .emit(ManagerEvent::PairRequest {
                        peer: self.peer,
                        name: p.peer_name.clone(),
                        code,
                    })
                    .await;
                Progress::Pending
            }
            (Stage::Confirming, Control::PairConfirm { accepted }) => {
                p.peer_answer = Some(accepted);
                p.evaluate()
            }
            (Stage::AwaitCommit, other) => {
                p.stale_probe = p.role == Role::Acceptor
                    && matches!(other, Control::PairConfirm { accepted: false })
                    && self.slot_taken.elapsed() <= STALE_PROBE_WINDOW;
                Progress::NotPaired
            }
            _ => Progress::Aborted,
        }
    }

    async fn pair_answer(&self, p: &mut PairState, accept: bool) -> Progress {
        if p.stage != Stage::Confirming || p.my_answer.is_some() {
            return Progress::Pending;
        }
        p.my_answer = Some(accept);
        if self
            .session
            .send_control(&Control::PairConfirm { accepted: accept })
            .await
            .is_err()
        {
            return Progress::Aborted;
        }
        p.evaluate()
    }

    /// Both sides accepted: trust the peer and carry on as a normal session.
    async fn complete_pairing(&self, p: &PairState) -> bool {
        let inner = &self.inner;
        let device = PairedDevice {
            id: self.peer,
            name: p.peer_name.clone(),
            last_addrs: match p.role {
                // The acceptor only sees the dialer's ephemeral port.
                Role::Requester => vec![self.addr],
                Role::Acceptor => vec![],
            },
        };
        if let Err(e) = lock(&inner.trust).add(device) {
            tracing::error!("could not save pairing with {}: {e:#}", self.peer);
            return false;
        }
        {
            let mut st = lock(&inner.state);
            if let Some(h) = st
                .sessions
                .get_mut(&self.peer)
                .filter(|h| h.epoch == self.epoch)
            {
                h.pairing = false;
            }
            if st.pending.remove(&self.peer) == Some(Role::Acceptor) {
                st.inbound_pairing = false;
            }
            let stat = st.stats.entry(self.peer).or_default();
            stat.ever_connected = true;
        }
        inner.ensure_dialer(self.peer);
        inner.emit(ManagerEvent::Paired { peer: self.peer }).await;
        inner
            .emit(ManagerEvent::Connected {
                peer: self.peer,
                hello: self.hello.clone(),
            })
            .await;
        true
    }

    fn publish_stats(&self) {
        let (rtt, off, lost) = (
            self.session.rtt_us(),
            self.session.clock_offset_us(),
            self.session.lost_datagrams(),
        );
        let mut st = lock(&self.inner.state);
        if let Some(h) = st
            .sessions
            .get_mut(&self.peer)
            .filter(|h| h.epoch == self.epoch)
        {
            h.rtt_us = rtt;
            h.clock_offset_us = off;
            h.lost = lost;
        }
    }

    /// Deregisters this session if it is still the current one and reports how it ended.
    async fn finish(self, was_pairing: bool, end: End) {
        let inner = &self.inner;
        let current = {
            let mut st = lock(&inner.state);
            let current = st
                .sessions
                .get(&self.peer)
                .is_some_and(|h| h.epoch == self.epoch);
            if current {
                st.sessions.remove(&self.peer);
                if was_pairing && st.pending.remove(&self.peer) == Some(Role::Acceptor) {
                    st.inbound_pairing = false;
                }
                if let End::Closed(r) | End::Unannounced(r) = &end {
                    st.stats.entry(self.peer).or_default().last_close = Some(r.clone());
                }
            }
            current
        };
        inner.changed.notify_waiters();
        if let (true, End::Closed(reason)) = (current, end) {
            inner
                .emit(ManagerEvent::Lost {
                    peer: self.peer,
                    reason,
                })
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_dial_order_tries_lan_then_link_local_then_vpn() {
        let a = |s: &str| s.parse::<SocketAddr>().unwrap();
        let order = pair_dial_order(vec![
            a("100.70.0.2:47474"),
            a("169.254.10.2:47474"),
            a("[fe80::2]:47474"),
            a("192.168.1.20:47474"),
            a("169.254.10.2:47474"),
            a("10.0.0.20:47474"),
        ]);
        assert_eq!(
            order,
            vec![
                a("192.168.1.20:47474"),
                a("10.0.0.20:47474"),
                a("169.254.10.2:47474"),
                a("[fe80::2]:47474"),
                a("100.70.0.2:47474"),
            ]
        );
    }

    #[test]
    fn reveal_must_match_commitment_and_id() {
        let id = DeviceId([1; 32]);
        let nonce = [5; 32];
        let commit = commitment(&id, &nonce);
        assert!(reveal_matches(&id, &nonce, &commit));
        assert!(!reveal_matches(&id, &[6; 32], &commit));
        assert!(!reveal_matches(&DeviceId([2; 32]), &nonce, &commit));
    }
}
