//! One authenticated QUIC connection: unreliable datagrams plus one reliable control stream,
//! with 100 ms heartbeats, dead-peer detection and clock-offset estimation.
use crate::impair::Impairment;
use crate::tls::device_id_of_cert;
use anyhow::{Context, Result, bail};
// Every `t_us` this module stamps or compares is on the process-wide clock that capture
// and the engine use (Ruling 30), so clock offsets translate capture stamps correctly.
use novalink_core::time::mono_us;
use novalink_core::{ClockEstimator, Liveness, SeqFilter};
use novalink_proto::{
    ChannelOpen, CodecError, Control, ControlDecoder, Datagram, DeviceId, MAX_CHANNEL_OPEN,
    decode_channel_open, decode_datagram, encode_channel_open, encode_control, encode_datagram,
    valid_service_name,
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(100);
const MISSED_LIMIT: u32 = 3;
const LIVENESS_POLL: Duration = Duration::from_millis(20);
const HELLO_TIMEOUT: Duration = Duration::from_secs(2);
const EVENT_QUEUE: usize = 1024;
/// A service channel must name its service this soon after it opens.
const CHANNEL_OPEN_TIMEOUT: Duration = Duration::from_secs(2);

// QUIC application close codes.
const CODE_NORMAL: u32 = 0;
const CODE_HEARTBEAT: u32 = 1;
const CODE_PROTOCOL: u32 = 2;
/// The acceptor does not consider the dialer paired.
pub const CODE_NOT_PAIRED: u32 = 3;
/// Stream error code for a service channel the acceptor refused (bad or late header).
pub const CODE_BAD_CHANNEL: u32 = 4;

#[derive(Debug)]
pub enum SessionEvent {
    /// A Motion or Scroll datagram that passed the sequence filter.
    Datagram(Datagram),
    Control(Control),
    /// The peer opened a service channel and named a valid service.
    Channel {
        service: String,
        send: quinn::SendStream,
        recv: quinn::RecvStream,
    },
    Closed(CloseReason),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum CloseReason {
    HeartbeatTimeout,
    PeerClosed,
    Transport(String),
    Protocol(String),
    /// The peer closed because it does not consider us paired.
    NotPaired,
}

struct Shared {
    conn: quinn::Connection,
    tx: mpsc::Sender<SessionEvent>,
    closed: AtomicBool,
    frozen: AtomicBool,
    hb_seq: AtomicU64,
    liveness: Mutex<Liveness>,
    clock: Mutex<ClockEstimator>,
    filter: Mutex<SeqFilter>,
    impair: Option<(Impairment, Instant)>,
}

impl Shared {
    /// Records the first close reason, tears the connection down and reports it once.
    async fn close(&self, reason: CloseReason) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let (code, text) = match &reason {
            CloseReason::HeartbeatTimeout => (CODE_HEARTBEAT, "heartbeat timeout".to_string()),
            CloseReason::Protocol(m) => (CODE_PROTOCOL, m.clone()),
            CloseReason::NotPaired => (CODE_NOT_PAIRED, "not paired".to_string()),
            _ => (CODE_NORMAL, String::new()),
        };
        self.conn.close(text_code(code), text.as_bytes());
        let _ = self.tx.send(SessionEvent::Closed(reason)).await;
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn send_datagram_bytes(&self, bytes: Vec<u8>) -> Result<()> {
        if self.frozen.load(Ordering::SeqCst) {
            return Ok(());
        }
        let Some((imp, start)) = &self.impair else {
            return self.conn.send_datagram(bytes.into()).map_err(Into::into);
        };
        if imp.in_flap(start.elapsed()) || imp.roll_loss() {
            return Ok(());
        }
        let delay = imp.delay();
        if delay.is_zero() {
            return self.conn.send_datagram(bytes.into()).map_err(Into::into);
        }
        let conn = self.conn.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = conn.send_datagram(bytes.into());
        });
        Ok(())
    }

    fn beat(&self) {
        if let Ok(mut l) = self.liveness.lock() {
            l.beat(Instant::now());
        }
    }

    fn is_dead(&self) -> bool {
        self.liveness
            .lock()
            .map(|l| l.dead(Instant::now()))
            .unwrap_or(false)
    }
}

/// Refuses a channel: the opener sees both halves fail with `CODE_BAD_CHANNEL`, not a clean EOF.
pub fn refuse_channel(mut send: quinn::SendStream, mut recv: quinn::RecvStream) {
    let _ = send.reset(text_code(CODE_BAD_CHANNEL));
    let _ = recv.stop(text_code(CODE_BAD_CHANNEL));
}

fn text_code(code: u32) -> quinn::VarInt {
    quinn::VarInt::from_u32(code)
}

fn map_conn_err(e: &quinn::ConnectionError) -> CloseReason {
    match e {
        quinn::ConnectionError::ApplicationClosed(c)
            if c.error_code == text_code(CODE_PROTOCOL) =>
        {
            CloseReason::Protocol(String::from_utf8_lossy(&c.reason).into_owned())
        }
        quinn::ConnectionError::ApplicationClosed(c)
            if c.error_code == text_code(CODE_NOT_PAIRED) =>
        {
            CloseReason::NotPaired
        }
        quinn::ConnectionError::ApplicationClosed(_)
        | quinn::ConnectionError::ConnectionClosed(_) => CloseReason::PeerClosed,
        other => CloseReason::Transport(other.to_string()),
    }
}

pub struct Session {
    peer: DeviceId,
    shared: Arc<Shared>,
    ctrl: tokio::sync::Mutex<quinn::SendStream>,
    events: mpsc::Receiver<SessionEvent>,
    tasks: Vec<JoinHandle<()>>,
}

fn peer_of(conn: &quinn::Connection) -> Result<DeviceId> {
    let ident = conn
        .peer_identity()
        .context("peer presented no certificate")?;
    let certs = ident
        .downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
        .map_err(|_| anyhow::anyhow!("unexpected peer identity type"))?;
    certs
        .first()
        .and_then(device_id_of_cert)
        .context("peer certificate has no ed25519 key")
}

async fn read_hello(recv: &mut quinn::RecvStream, dec: &mut ControlDecoder) -> Result<Control> {
    let mut buf = [0u8; 4096];
    loop {
        match dec.next() {
            Ok(Some(c)) => return Ok(c),
            Ok(None) => {}
            Err(e) => bail!("bad hello frame: {e}"),
        }
        let n = recv
            .read(&mut buf)
            .await
            .context("read hello")?
            .context("control stream closed before hello")?;
        dec.push(&buf[..n]);
    }
}

impl Session {
    /// Establishes a session on an authenticated connection, honouring `NOVALINK_IMPAIR`.
    /// A protocol-version mismatch closes the connection with a protocol error and fails.
    pub async fn establish(
        conn: quinn::Connection,
        me: DeviceId,
        hello: Control,
    ) -> Result<(Session, Control)> {
        Self::establish_with(conn, me, hello, Impairment::from_env()).await
    }

    pub async fn establish_with(
        conn: quinn::Connection,
        me: DeviceId,
        hello: Control,
        impair: Option<Impairment>,
    ) -> Result<(Session, Control)> {
        let result = tokio::time::timeout(HELLO_TIMEOUT, Self::handshake(&conn, me, &hello))
            .await
            .unwrap_or_else(|_| {
                Err((
                    CODE_NORMAL,
                    anyhow::anyhow!("timed out waiting for peer hello"),
                ))
            });
        let (peer, send, recv, dec, peer_hello) = match result {
            Ok(v) => v,
            Err((code, e)) => {
                conn.close(text_code(code), format!("{e:#}").as_bytes());
                return Err(e);
            }
        };
        let (tx, events) = mpsc::channel(EVENT_QUEUE);
        let mut liveness = Liveness::new(HEARTBEAT_INTERVAL, MISSED_LIMIT);
        liveness.beat(Instant::now());
        let shared = Arc::new(Shared {
            conn,
            tx,
            closed: AtomicBool::new(false),
            frozen: AtomicBool::new(false),
            hb_seq: AtomicU64::new(0),
            liveness: Mutex::new(liveness),
            clock: Mutex::new(ClockEstimator::default()),
            filter: Mutex::new(SeqFilter::default()),
            impair: impair.map(|i| (i, Instant::now())),
        });
        let tasks = vec![
            tokio::spawn(heartbeat_loop(shared.clone())),
            tokio::spawn(liveness_loop(shared.clone())),
            tokio::spawn(datagram_loop(shared.clone())),
            tokio::spawn(control_loop(shared.clone(), recv, dec)),
            tokio::spawn(channel_loop(shared.clone())),
        ];
        Ok((
            Session {
                peer,
                shared,
                ctrl: tokio::sync::Mutex::new(send),
                events,
                tasks,
            },
            peer_hello,
        ))
    }

    /// Opens/accepts the control stream and swaps Hellos. The error carries the QUIC
    /// close code to use.
    #[allow(clippy::type_complexity)]
    async fn handshake(
        conn: &quinn::Connection,
        me: DeviceId,
        hello: &Control,
    ) -> Result<
        (
            DeviceId,
            quinn::SendStream,
            quinn::RecvStream,
            ControlDecoder,
            Control,
        ),
        (u32, anyhow::Error),
    > {
        let io = |e: anyhow::Error| (CODE_NORMAL, e);
        let Control::Hello { proto_version, .. } = hello else {
            return Err((
                CODE_PROTOCOL,
                anyhow::anyhow!("first message must be Hello"),
            ));
        };
        let peer = peer_of(conn).map_err(io)?;
        if peer == me {
            return Err((CODE_PROTOCOL, anyhow::anyhow!("peer has our own device id")));
        }
        let (mut send, mut recv) = match conn.side() {
            quinn::Side::Client => conn.open_bi().await,
            quinn::Side::Server => conn.accept_bi().await,
        }
        .map_err(|e| io(e.into()))?;
        // The dialer's stream is invisible to the acceptor until the first bytes arrive.
        send.write_all(&encode_control(hello))
            .await
            .map_err(|e| io(e.into()))?;
        let mut dec = ControlDecoder::default();
        let mut peer_hello = read_hello(&mut recv, &mut dec).await.map_err(io)?;
        if let Control::Hello { name, .. } = &mut peer_hello {
            *name = crate::sanitize_name(name);
        }
        match &peer_hello {
            Control::Hello {
                proto_version: pv, ..
            } if pv == proto_version => {}
            Control::Hello {
                proto_version: pv, ..
            } => {
                return Err((
                    CODE_PROTOCOL,
                    anyhow::anyhow!("protocol version mismatch (peer {pv}, us {proto_version})"),
                ));
            }
            _ => {
                return Err((
                    CODE_PROTOCOL,
                    anyhow::anyhow!("first message must be Hello"),
                ));
            }
        }
        Ok((peer, send, recv, dec, peer_hello))
    }

    /// Closes the connection with `reason` (which picks the QUIC close code).
    pub async fn close_with(&self, reason: CloseReason) {
        self.shared.close(reason).await;
    }

    pub fn peer(&self) -> DeviceId {
        self.peer
    }

    pub fn send_datagram(&self, d: &Datagram) -> Result<()> {
        self.shared.send_datagram_bytes(encode_datagram(d))
    }

    pub async fn send_control(&self, c: &Control) -> Result<()> {
        let sh = &self.shared;
        if sh.frozen.load(Ordering::SeqCst) {
            return Ok(());
        }
        let bytes = encode_control(c);
        // Holding the lock across the delay keeps frames in order.
        let mut stream = self.ctrl.lock().await;
        if let Some((imp, start)) = &sh.impair {
            let stall = imp.flap_remaining(start.elapsed());
            tokio::time::sleep(stall + imp.delay()).await;
        }
        stream.write_all(&bytes).await?;
        Ok(())
    }

    /// Opens a service channel to the peer: a bidirectional stream whose first bytes name
    /// `service` (1-32 bytes of `[a-z0-9-]`). Fails without sending anything for a bad name.
    pub async fn open_channel(
        &self,
        service: &str,
    ) -> Result<(quinn::SendStream, quinn::RecvStream)> {
        self.channel_opener().open(service).await
    }

    /// A cloneable handle that opens channels without borrowing the session.
    pub fn channel_opener(&self) -> ChannelOpener {
        ChannelOpener {
            conn: self.shared.conn.clone(),
        }
    }

    /// Opens a bare bidirectional stream with no channel header, for tests that send
    /// malformed headers.
    #[cfg(any(test, feature = "test-util"))]
    pub async fn open_raw_stream_for_test(&self) -> Result<(quinn::SendStream, quinn::RecvStream)> {
        Ok(self.shared.conn.open_bi().await?)
    }

    pub fn events(&mut self) -> &mut mpsc::Receiver<SessionEvent> {
        &mut self.events
    }

    pub fn rtt_us(&self) -> Option<u64> {
        self.shared.clock.lock().ok()?.rtt_us()
    }

    /// Peer clock minus ours, in microseconds.
    pub fn clock_offset_us(&self) -> Option<i64> {
        self.shared.clock.lock().ok()?.offset_us()
    }

    pub fn lost_datagrams(&self) -> u64 {
        self.shared.filter.lock().map(|f| f.lost()).unwrap_or(0)
    }

    /// Keying material exported from this connection's TLS session; identical on both
    /// ends, so pairing codes can be bound to the exact channel.
    pub fn tls_binding(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        self.shared
            .conn
            .export_keying_material(&mut out, b"novalink pairing", b"")
            .expect("32 bytes is within the TLS exporter limit");
        out
    }

    /// Stops sending anything (heartbeats, acks, data) while keeping the socket open.
    #[cfg(any(test, feature = "test-util"))]
    pub fn freeze_for_test(&self) {
        self.shared.frozen.store(true, Ordering::SeqCst);
    }
}

/// Opens service channels on one connection; see [`Session::open_channel`].
#[derive(Clone)]
pub struct ChannelOpener {
    conn: quinn::Connection,
}

impl ChannelOpener {
    pub async fn open(&self, service: &str) -> Result<(quinn::SendStream, quinn::RecvStream)> {
        if !valid_service_name(service) {
            bail!("invalid service name {service:?}");
        }
        let (mut send, recv) = self.conn.open_bi().await.context("open channel")?;
        let header = encode_channel_open(&ChannelOpen {
            service: service.to_string(),
        });
        send.write_all(&header)
            .await
            .context("write channel header")?;
        Ok((send, recv))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        self.shared
            .conn
            .close(text_code(CODE_NORMAL), b"session dropped");
        for t in &self.tasks {
            t.abort();
        }
    }
}

async fn heartbeat_loop(sh: Arc<Shared>) {
    let mut tick = tokio::time::interval(HEARTBEAT_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    while !sh.is_closed() {
        tick.tick().await;
        let seq = sh.hb_seq.fetch_add(1, Ordering::Relaxed);
        let hb = Datagram::Heartbeat {
            seq,
            t_us: mono_us(),
        };
        if let Err(e) = sh.send_datagram_bytes(encode_datagram(&hb)) {
            let reason = match e.downcast_ref::<quinn::SendDatagramError>() {
                Some(quinn::SendDatagramError::ConnectionLost(ce)) => map_conn_err(ce),
                _ => CloseReason::Transport(e.to_string()),
            };
            sh.close(reason).await;
        }
    }
}

async fn liveness_loop(sh: Arc<Shared>) {
    while !sh.is_closed() {
        tokio::time::sleep(LIVENESS_POLL).await;
        if sh.is_dead() {
            sh.close(CloseReason::HeartbeatTimeout).await;
        }
    }
}

async fn datagram_loop(sh: Arc<Shared>) {
    loop {
        let bytes = match sh.conn.read_datagram().await {
            Ok(b) => b,
            Err(e) => return sh.close(map_conn_err(&e)).await,
        };
        let Ok(d) = decode_datagram(&bytes) else {
            continue;
        };
        match d {
            Datagram::Heartbeat { seq, t_us } => {
                sh.beat();
                let ack = Datagram::HeartbeatAck {
                    seq,
                    echo_t_us: t_us,
                    t_us: mono_us(),
                };
                let _ = sh.send_datagram_bytes(encode_datagram(&ack));
            }
            Datagram::HeartbeatAck {
                echo_t_us, t_us, ..
            } => {
                sh.beat();
                if let Ok(mut c) = sh.clock.lock() {
                    c.sample(echo_t_us, t_us, mono_us());
                }
            }
            Datagram::Motion { seq, .. } | Datagram::Scroll { seq, .. } => {
                let fresh = sh.filter.lock().map(|mut f| f.accept(seq)).unwrap_or(false);
                // A full queue means the consumer is behind; stale motion is worthless.
                if fresh {
                    let _ = sh.tx.try_send(SessionEvent::Datagram(d));
                }
            }
        }
    }
}

async fn control_loop(sh: Arc<Shared>, mut recv: quinn::RecvStream, mut dec: ControlDecoder) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        loop {
            match dec.next() {
                Ok(Some(c)) => {
                    if sh.tx.send(SessionEvent::Control(c)).await.is_err() {
                        return;
                    }
                }
                Ok(None) => break,
                Err(CodecError::UnknownMessage) => {}
                Err(e @ CodecError::FrameTooLarge) => {
                    return sh.close(CloseReason::Protocol(e.to_string())).await;
                }
            }
        }
        match recv.read(&mut buf).await {
            Ok(Some(n)) => dec.push(&buf[..n]),
            Ok(None) => return sh.close(CloseReason::PeerClosed).await,
            Err(quinn::ReadError::ConnectionLost(e)) => return sh.close(map_conn_err(&e)).await,
            Err(e) => return sh.close(CloseReason::Transport(e.to_string())).await,
        }
    }
}

/// Accepts service channels beside the control stream. Each stream gets its own task so a
/// slow header cannot hold up the next one.
async fn channel_loop(sh: Arc<Shared>) {
    loop {
        let Ok((send, recv)) = sh.conn.accept_bi().await else {
            // The datagram loop reports the close.
            return;
        };
        let tx = sh.tx.clone();
        tokio::spawn(async move {
            let accepted = tokio::time::timeout(CHANNEL_OPEN_TIMEOUT, read_channel_open(recv))
                .await
                .unwrap_or_else(|_| Err("timed out waiting for channel header"));
            match accepted {
                Ok((service, recv)) => {
                    let _ = tx
                        .send(SessionEvent::Channel {
                            service,
                            send,
                            recv,
                        })
                        .await;
                }
                Err(why) => {
                    tracing::debug!("refused service channel: {why}");
                    let mut send = send;
                    let _ = send.reset(text_code(CODE_BAD_CHANNEL));
                }
            }
        });
    }
}

/// Reads and validates the `ChannelOpen` header. On failure the receive side is stopped.
async fn read_channel_open(
    mut recv: quinn::RecvStream,
) -> Result<(String, quinn::RecvStream), &'static str> {
    let fail = |recv: &mut quinn::RecvStream, why| {
        let _ = recv.stop(text_code(CODE_BAD_CHANNEL));
        Err(why)
    };
    let mut len = [0u8; 4];
    if recv.read_exact(&mut len).await.is_err() {
        return fail(&mut recv, "channel header truncated");
    }
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > MAX_CHANNEL_OPEN {
        return fail(&mut recv, "channel header size out of range");
    }
    let mut payload = vec![0u8; len];
    if recv.read_exact(&mut payload).await.is_err() {
        return fail(&mut recv, "channel header truncated");
    }
    match decode_channel_open(&payload) {
        Ok(o) if valid_service_name(&o.service) => Ok((o.service, recv)),
        Ok(_) => fail(&mut recv, "invalid service name"),
        Err(_) => fail(&mut recv, "malformed channel header"),
    }
}
