//! OBS through obs-websocket v5, which OBS 28+ has built in. Actions use one short
//! connection per request: hello, identify (with the SHA-256 challenge when a password is
//! set), the request, its response. Blocking; the agent runs them on its blocking pool.
//! `ObsLink` is the persistent connection that feeds the deck.

use crate::feeds::{FOREVER, FeedStore, FeedValue};
use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use novagear_core::config::ObsConfig;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tungstenite::{Message, WebSocket};

/// obs-websocket's auth string: base64(sha256(base64(sha256(password + salt)) + challenge)).
pub fn auth_string(password: &str, salt: &str, challenge: &str) -> String {
    let secret = B64.encode(Sha256::digest(format!("{password}{salt}")));
    B64.encode(Sha256::digest(format!("{secret}{challenge}")))
}

fn next_json(ws: &mut WebSocket<TcpStream>) -> Result<Value> {
    loop {
        match ws.read().context("OBS closed the connection")? {
            Message::Text(t) => return Ok(serde_json::from_str(t.as_str())?),
            Message::Close(f) => bail!(
                "OBS refused: {}",
                f.map(|f| f.reason.to_string()).unwrap_or_default()
            ),
            _ => continue,
        }
    }
}

/// A request OBS answered with a failure; keeps the code so callers can tell "this OBS is
/// too old" from other failures.
#[derive(Debug)]
pub struct RequestFailed {
    pub request_type: String,
    pub code: i64,
    pub comment: String,
}

impl std::fmt::Display for RequestFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "OBS: {}", self.comment)
    }
}

impl std::error::Error for RequestFailed {}

/// Opens a socket to OBS and completes hello and identify (with the SHA-256 challenge when
/// a password is set). `subscriptions` is the event mask; `None` leaves OBS's default,
/// which is what one-shot requests want.
fn connect(
    cfg: &ObsConfig,
    subscriptions: Option<u32>,
    ctl: Option<&Ctl>,
) -> Result<WebSocket<TcpStream>> {
    let url = cfg.url.trim();
    let hostport = url
        .strip_prefix("ws://")
        .context("OBS address must start with ws://")?
        .trim_end_matches('/');
    let addr = hostport
        .to_socket_addrs()
        .context("bad OBS address")?
        .next()
        .context("bad OBS address")?;
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).context(
        "OBS isn't reachable; is it running with Tools › WebSocket Server Settings turned on?",
    )?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    // Registered before the handshake so a stop doesn't wait out a silent peer.
    if let Some(ctl) = ctl {
        *lock(&ctl.stream) = stream.try_clone().ok();
        if ctl.is_stopped() {
            bail!("stopped");
        }
    }
    let (mut ws, _) = tungstenite::client(url, stream)
        .map_err(|e| anyhow::anyhow!("OBS handshake failed: {e}"))?;

    let hello = next_json(&mut ws)?;
    let mut identify = json!({ "rpcVersion": 1 });
    if let Some(mask) = subscriptions {
        identify["eventSubscriptions"] = json!(mask);
    }
    if let Some(auth) = hello["d"].get("authentication") {
        if cfg.password.is_empty() {
            bail!("OBS asks for a WebSocket password; add it under OBS in NovaGear's settings");
        }
        let salt = auth["salt"].as_str().unwrap_or_default();
        let challenge = auth["challenge"].as_str().unwrap_or_default();
        identify["authentication"] = json!(auth_string(&cfg.password, salt, challenge));
    }
    ws.send(Message::text(json!({ "op": 1, "d": identify }).to_string()))?;
    let identified = next_json(&mut ws)?;
    if identified["op"] != 2 {
        bail!("OBS didn't accept NovaGear (wrong WebSocket password?)");
    }
    Ok(ws)
}

pub fn request(cfg: &ObsConfig, request_type: &str, data: Value) -> Result<Value> {
    let mut ws = connect(cfg, None, None)?;
    ws.send(Message::text(
        json!({ "op": 6, "d": { "requestType": request_type, "requestId": "novagear", "requestData": data } })
            .to_string(),
    ))?;
    loop {
        let m = next_json(&mut ws)?;
        if m["op"] == 7 && m["d"]["requestId"] == "novagear" {
            let status = &m["d"]["requestStatus"];
            let _ = ws.close(None);
            if status["result"] != true {
                return Err(RequestFailed {
                    request_type: request_type.to_string(),
                    code: status["code"].as_i64().unwrap_or(0),
                    comment: status["comment"]
                        .as_str()
                        .unwrap_or("request failed")
                        .to_string(),
                }
                .into());
            }
            return Ok(m["d"]["responseData"].clone());
        }
    }
}

pub fn set_scene(cfg: &ObsConfig, scene: &str) -> Result<()> {
    request(cfg, "SetCurrentProgramScene", json!({ "sceneName": scene })).map(drop)
}

pub fn toggle_record(cfg: &ObsConfig) -> Result<()> {
    request(cfg, "ToggleRecord", json!({})).map(drop)
}

pub fn toggle_stream(cfg: &ObsConfig) -> Result<()> {
    request(cfg, "ToggleStream", json!({})).map(drop)
}

/// Marks a chapter in the recording. Chapters arrived in OBS 30.2; older builds answer
/// "unknown request" (code 204).
pub fn chapter_marker(cfg: &ObsConfig) -> Result<()> {
    request(cfg, "CreateRecordChapter", json!({}))
        .map(drop)
        .map_err(|e| match e.downcast_ref::<RequestFailed>() {
            Some(f) if f.code == 204 || f.comment.to_lowercase().contains("unknown request") => {
                anyhow::anyhow!("chapter markers need OBS 30.2 or newer")
            }
            _ => e,
        })
}

pub fn replay_save(cfg: &ObsConfig) -> Result<()> {
    request(cfg, "SaveReplayBuffer", json!({})).map(drop)
}

/// OBS's input volume range in dB.
const VOLUME_DB: std::ops::RangeInclusive<f64> = -100.0..=26.0;

/// Changes an input's volume by `step_db`, read-modify-write over two short connections.
pub fn input_volume_step(cfg: &ObsConfig, input: &str, step_db: i16) -> Result<()> {
    let now = request(cfg, "GetInputVolume", json!({ "inputName": input }))?;
    let db = now["inputVolumeDb"]
        .as_f64()
        .context("OBS didn't say the input's volume")?;
    let new = (db + f64::from(step_db)).clamp(*VOLUME_DB.start(), *VOLUME_DB.end());
    request(
        cfg,
        "SetInputVolume",
        json!({ "inputName": input, "inputVolumeDb": new }),
    )
    .map(drop)
}

pub fn input_mute_toggle(cfg: &ObsConfig, input: &str) -> Result<()> {
    request(cfg, "ToggleInputMute", json!({ "inputName": input })).map(drop)
}

// ---- the persistent link ----
//
// While something needs OBS (a mode rule on recording/streaming, or a visible face or
// panel reading an `obs.` feed) one thread holds a socket open and turns OBS's events into
// feeds. When nothing needs it the thread exits, so a closed or unused OBS costs nothing.

/// What mode rules react to. The agent owns the receiving end (`Agent::take_obs_events`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObsEvent {
    Recording(bool),
    Streaming(bool),
    Connected(bool),
}

/// Event subscription bits: General, Scenes, Inputs, Outputs.
const SUBS_BASE: u32 = 1 | 4 | 8 | 64;
/// InputVolumeMeters is high-volume (about 50 events a second), so it is opt-in.
const SUBS_METERS: u32 = 1 << 16;
/// How long a read waits; also how soon the link notices a stop, a change to what is
/// wanted, or that nothing needs OBS any more.
const TICK: Duration = Duration::from_secs(1);
/// How long to wait between attempts while OBS is down.
const RETRY: Duration = Duration::from_secs(30);
/// Meters reach the deck at most this often.
const METER_GAP: Duration = Duration::from_millis(100);
/// Levels go stale when OBS stops sending them.
const METER_TTL: Duration = Duration::from_secs(2);
/// Times and rates are re-polled every `TICK`, so they go stale if a poll is missed.
const POLL_TTL: Duration = Duration::from_secs(3);
/// The shortest span a bitrate is worked out over.
const MIN_RATE_WINDOW: Duration = Duration::from_millis(500);
/// The quietest level a meter shows.
const FLOOR_DB: f64 = -100.0;

const METER_PREFIX: &str = "obs.meter.";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Shared between the link thread and whoever stops it.
#[derive(Default)]
struct Ctl {
    stopped: Mutex<bool>,
    wake: Condvar,
    /// A clone of the live socket, so a stop can cut a blocking read short.
    stream: Mutex<Option<TcpStream>>,
}

impl Ctl {
    fn stop(&self) {
        *lock(&self.stopped) = true;
        self.wake.notify_all();
        if let Some(s) = lock(&self.stream).as_ref() {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    fn is_stopped(&self) -> bool {
        *lock(&self.stopped)
    }

    /// Sleeps up to `d`; false when a stop cut it short.
    fn sleep(&self, d: Duration) -> bool {
        let g = lock(&self.stopped);
        let (g, _) = self
            .wake
            .wait_timeout_while(g, d, |stopped| !*stopped)
            .unwrap_or_else(|e| e.into_inner());
        !*g
    }
}

pub struct ObsLink {
    ctl: Arc<Ctl>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ObsLink {
    /// Links while an `obs.` feed is wanted.
    pub fn spawn(
        cfg: ObsConfig,
        feeds: Arc<FeedStore>,
        events: mpsc::UnboundedSender<ObsEvent>,
    ) -> ObsLink {
        let f = feeds.clone();
        Self::spawn_with(cfg, feeds, events, move || f.is_wanted("obs."))
    }

    /// `needed` says whether anything still wants OBS; the thread exits when it turns false.
    pub fn spawn_with(
        cfg: ObsConfig,
        feeds: Arc<FeedStore>,
        events: mpsc::UnboundedSender<ObsEvent>,
        needed: impl Fn() -> bool + Send + 'static,
    ) -> ObsLink {
        Self::spawn_after(cfg, feeds, events, needed, Vec::new())
    }

    /// Like `spawn_with`, but the new thread first waits for `after` (retired links, see
    /// `retire`) to finish, so their cleanup of the `obs.` feeds lands before this link sets
    /// any. The wait happens on the link's own thread, never the caller's.
    pub fn spawn_after(
        cfg: ObsConfig,
        feeds: Arc<FeedStore>,
        events: mpsc::UnboundedSender<ObsEvent>,
        needed: impl Fn() -> bool + Send + 'static,
        after: Vec<std::thread::JoinHandle<()>>,
    ) -> ObsLink {
        let ctl = Arc::new(Ctl::default());
        let c = ctl.clone();
        let thread = std::thread::Builder::new()
            .name("obs-link".into())
            .spawn(move || {
                for t in after {
                    let _ = t.join();
                }
                // Retired while still waiting on its predecessor: no connect attempt.
                if c.is_stopped() {
                    feeds.clear("obs.connected");
                    return;
                }
                let mut failing = false;
                supervise(
                    &needed,
                    || {
                        // OBS counts as down until a session says otherwise.
                        feeds.set("obs.connected", FeedValue::Number(0.0), None, None, FOREVER);
                        match run_session(&cfg, &feeds, &events, &c, &needed) {
                            Ok(connected) => {
                                if connected {
                                    failing = false;
                                }
                            }
                            // Logged once per outage, and only our own words: never the
                            // password, salt or anything OBS named.
                            Err(e) if !c.is_stopped() => {
                                if !failing {
                                    tracing::info!("OBS link down: {}", crate::failure_log(&e));
                                }
                                failing = true;
                            }
                            Err(_) => {}
                        }
                    },
                    |d| c.sleep(d),
                );
                feeds.clear("obs.connected");
            })
            .expect("spawn the OBS link thread");
        ObsLink {
            ctl,
            thread: Some(thread),
        }
    }

    /// Tells the link to stop without waiting for it; returns its thread to wait on (pass it
    /// to the next `spawn_after`). Its cleanup still runs, on its own thread.
    pub fn retire(mut self) -> Option<std::thread::JoinHandle<()>> {
        self.ctl.stop();
        self.thread.take()
    }

    /// A link whose thread, once told to stop, takes `delay` to clean up and then clears
    /// `obs.connected`: a connect attempt that was slow to give up.
    #[cfg(test)]
    pub(crate) fn slow_to_stop(feeds: Arc<FeedStore>, delay: Duration) -> ObsLink {
        let ctl = Arc::new(Ctl::default());
        let c = ctl.clone();
        let thread = std::thread::spawn(move || {
            while c.sleep(Duration::from_secs(60)) {}
            std::thread::sleep(delay);
            feeds.clear("obs.connected");
        });
        ObsLink {
            ctl,
            thread: Some(thread),
        }
    }

    /// Stops the link and waits for its thread, which clears every `obs.` feed it set.
    pub fn stop(mut self) {
        self.ctl.stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    /// The thread ended on its own (nothing needed OBS any more).
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
}

/// Runs `attempt`, then waits `RETRY` and runs it again, for as long as `needed` holds.
/// `wait` returns false to end the loop (a stop). The clock is injected so the retry
/// cadence can be tested without waiting.
fn supervise(
    needed: &dyn Fn() -> bool,
    mut attempt: impl FnMut(),
    mut wait: impl FnMut(Duration) -> bool,
) {
    while needed() {
        attempt();
        if !needed() || !wait(RETRY) {
            break;
        }
    }
}

/// One connection, from identify to hang-up. `Ok(true)` when it connected and ended
/// cleanly (stop, or nothing needs OBS); `Err` when it could not connect or was cut off.
fn run_session(
    cfg: &ObsConfig,
    feeds: &FeedStore,
    events: &mpsc::UnboundedSender<ObsEvent>,
    ctl: &Ctl,
    needed: &dyn Fn() -> bool,
) -> Result<bool> {
    let mut ws = connect(cfg, Some(SUBS_BASE), Some(ctl))?;
    ws.get_ref().set_read_timeout(Some(TICK))?;
    ws.get_ref()
        .set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut s = Session::new(feeds, events);
    let r = s.run(&mut ws, ctl, needed);
    s.finish();
    *lock(&ctl.stream) = None;
    let _ = ws.close(None);
    r.map(|()| true)
}

/// What the link knows about OBS while connected.
struct Session<'a> {
    feeds: &'a FeedStore,
    events: &'a mpsc::UnboundedSender<ObsEvent>,
    recording: bool,
    streaming: bool,
    /// Bytes sent and when, for the stream's bitrate.
    stream_bytes: Option<(u64, Instant)>,
    meter_keys: BTreeSet<String>,
    last_meter: Option<Instant>,
    next_poll: Instant,
}

impl<'a> Session<'a> {
    fn new(feeds: &'a FeedStore, events: &'a mpsc::UnboundedSender<ObsEvent>) -> Self {
        let s = Session {
            feeds,
            events,
            recording: false,
            streaming: false,
            stream_bytes: None,
            meter_keys: BTreeSet::new(),
            last_meter: None,
            next_poll: Instant::now(),
        };
        s.flag("obs.connected", true);
        s.flag("obs.recording", false);
        s.flag("obs.streaming", false);
        s.emit(ObsEvent::Connected(true));
        s
    }

    fn emit(&self, e: ObsEvent) {
        // Nobody listening yet (or any more) is fine.
        let _ = self.events.send(e);
    }

    fn flag(&self, key: &str, on: bool) {
        self.feeds.set(
            key,
            FeedValue::Number(f64::from(u8::from(on))),
            None,
            None,
            FOREVER,
        );
    }

    fn run(
        &mut self,
        ws: &mut WebSocket<TcpStream>,
        ctl: &Ctl,
        needed: &dyn Fn() -> bool,
    ) -> Result<()> {
        send_request(ws, "scene", "GetCurrentProgramScene")?;
        send_request(ws, "rec", "GetRecordStatus")?;
        send_request(ws, "stream", "GetStreamStatus")?;
        let mut subs = SUBS_BASE;
        loop {
            if ctl.is_stopped() || !needed() {
                return Ok(());
            }
            let want = SUBS_BASE
                | if self.feeds.is_wanted(METER_PREFIX) {
                    SUBS_METERS
                } else {
                    0
                };
            if want != subs {
                ws.send(Message::text(
                    json!({ "op": 3, "d": { "eventSubscriptions": want } }).to_string(),
                ))?;
                subs = want;
                if want & SUBS_METERS == 0 {
                    self.clear_meters();
                }
            }
            if Instant::now() >= self.next_poll {
                self.next_poll = Instant::now() + TICK;
                if self.recording && self.feeds.is_wanted("obs.record.time") {
                    send_request(ws, "rec", "GetRecordStatus")?;
                }
                if self.streaming && self.feeds.is_wanted("obs.stream.") {
                    send_request(ws, "stream", "GetStreamStatus")?;
                }
            }
            match read_message(ws) {
                Ok(Some(m)) => self.handle(&m),
                Ok(None) => {}
                Err(_) if ctl.is_stopped() => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }

    fn handle(&mut self, m: &Value) {
        match m["op"].as_i64() {
            Some(5) => {
                let data = &m["d"]["eventData"];
                match m["d"]["eventType"].as_str() {
                    Some("RecordStateChanged") => {
                        self.set_recording(data["outputActive"].as_bool().unwrap_or(false));
                    }
                    Some("StreamStateChanged") => {
                        self.set_streaming(data["outputActive"].as_bool().unwrap_or(false));
                    }
                    Some("CurrentProgramSceneChanged") => self.set_scene(data),
                    Some("InputVolumeMeters") => self.meters(data),
                    _ => {}
                }
            }
            Some(7) => {
                let status = &m["d"]["requestStatus"];
                if status["result"] != true {
                    // Only the type and code: OBS's comment can name a scene.
                    let e: anyhow::Error = RequestFailed {
                        request_type: m["d"]["requestType"].as_str().unwrap_or("?").to_string(),
                        code: status["code"].as_i64().unwrap_or(0),
                        comment: String::new(),
                    }
                    .into();
                    tracing::debug!("{}", crate::failure_log(&e));
                    return;
                }
                let data = &m["d"]["responseData"];
                match m["d"]["requestId"].as_str() {
                    Some("scene") => {
                        // The response names it `currentProgramSceneName`.
                        self.set_scene(&json!({ "sceneName": data["currentProgramSceneName"] }));
                    }
                    Some("rec") => {
                        self.set_recording(data["outputActive"].as_bool().unwrap_or(false));
                        if self.recording {
                            self.set_time("obs.record.time", data["outputTimecode"].as_str());
                        }
                    }
                    Some("stream") => {
                        self.set_streaming(data["outputActive"].as_bool().unwrap_or(false));
                        if self.streaming {
                            self.stream_stats(data);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn set_scene(&self, data: &Value) {
        if let Some(name) = data["sceneName"].as_str() {
            self.feeds.set(
                "obs.scene",
                FeedValue::Text(name.to_string()),
                None,
                None,
                FOREVER,
            );
        }
    }

    fn set_recording(&mut self, on: bool) {
        if on == self.recording {
            return;
        }
        self.recording = on;
        self.flag("obs.recording", on);
        self.emit(ObsEvent::Recording(on));
        if on {
            // Fetch the time now, not a second from now.
            self.next_poll = Instant::now();
        } else {
            self.feeds.clear("obs.record.time");
        }
    }

    fn set_streaming(&mut self, on: bool) {
        if on == self.streaming {
            return;
        }
        self.streaming = on;
        self.flag("obs.streaming", on);
        self.emit(ObsEvent::Streaming(on));
        self.stream_bytes = None;
        if on {
            self.next_poll = Instant::now();
        } else {
            self.clear_stream_stats();
        }
    }

    fn clear_stream_stats(&self) {
        for k in ["obs.stream.time", "obs.stream.kbps", "obs.stream.dropped"] {
            self.feeds.clear(k);
        }
    }

    fn set_time(&self, key: &str, timecode: Option<&str>) {
        if !self.feeds.is_wanted(key) {
            return;
        }
        let t = short_timecode(timecode.unwrap_or_default());
        if !t.is_empty() {
            self.feeds
                .set(key, FeedValue::Text(t), None, None, POLL_TTL);
        }
    }

    fn stream_stats(&mut self, data: &Value) {
        self.set_time("obs.stream.time", data["outputTimecode"].as_str());
        if let Some(dropped) = data["outputSkippedFrames"].as_f64() {
            self.feeds.set(
                "obs.stream.dropped",
                FeedValue::Number(dropped),
                None,
                None,
                POLL_TTL,
            );
        }
        if let Some(bytes) = data["outputBytes"].as_u64() {
            let now = Instant::now();
            match self.stream_bytes {
                None => self.stream_bytes = Some((bytes, now)),
                // The first poll after going live comes right after the priming one;
                // a rate over a few milliseconds is noise, so keep the older baseline.
                Some((before, at)) if now.duration_since(at) >= MIN_RATE_WINDOW => {
                    let secs = now.duration_since(at).as_secs_f64();
                    let kbps = (bytes.saturating_sub(before) as f64 * 8.0 / 1000.0 / secs).round();
                    self.stream_bytes = Some((bytes, now));
                    self.feeds.set(
                        "obs.stream.kbps",
                        FeedValue::Number(kbps),
                        Some("kbps".into()),
                        None,
                        POLL_TTL,
                    );
                }
                Some(_) => {}
            }
        }
    }

    fn meters(&mut self, data: &Value) {
        let now = Instant::now();
        if self
            .last_meter
            .is_some_and(|t| now.duration_since(t) < METER_GAP)
        {
            return;
        }
        self.last_meter = Some(now);
        for input in data["inputs"].as_array().into_iter().flatten() {
            // The name is user data: it becomes a feed key, and goes in no log.
            let Some(name) = input["inputName"].as_str() else {
                continue;
            };
            let key = format!("{METER_PREFIX}{name}");
            let db = peak_db(&input["inputLevelsMul"]);
            self.feeds.set(
                &key,
                FeedValue::Number(db),
                Some("dB".into()),
                None,
                METER_TTL,
            );
            self.meter_keys.insert(key);
        }
    }

    fn clear_meters(&mut self) {
        for k in std::mem::take(&mut self.meter_keys) {
            self.feeds.clear(&k);
        }
    }

    /// The session is over: nothing the link set may outlive it.
    fn finish(&mut self) {
        self.clear_meters();
        self.clear_stream_stats();
        for k in [
            "obs.scene",
            "obs.record.time",
            "obs.recording",
            "obs.streaming",
        ] {
            self.feeds.clear(k);
        }
        self.feeds
            .set("obs.connected", FeedValue::Number(0.0), None, None, FOREVER);
        if std::mem::take(&mut self.recording) {
            self.emit(ObsEvent::Recording(false));
        }
        if std::mem::take(&mut self.streaming) {
            self.emit(ObsEvent::Streaming(false));
        }
        self.emit(ObsEvent::Connected(false));
    }
}

fn send_request(ws: &mut WebSocket<TcpStream>, id: &str, request_type: &str) -> Result<()> {
    ws.send(Message::text(
        json!({ "op": 6, "d": { "requestType": request_type, "requestId": id, "requestData": {} } })
            .to_string(),
    ))?;
    Ok(())
}

/// One message, or `None` when the read timed out (or the frame wasn't text).
fn read_message(ws: &mut WebSocket<TcpStream>) -> Result<Option<Value>> {
    match ws.read() {
        Ok(Message::Text(t)) => Ok(serde_json::from_str(t.as_str()).ok()),
        Ok(Message::Close(_)) => bail!("OBS closed the connection"),
        Ok(_) => Ok(None),
        Err(tungstenite::Error::Io(e))
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            Ok(None)
        }
        Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
            bail!("OBS closed the connection")
        }
        Err(e) => Err(anyhow::anyhow!("OBS link error: {e}")),
    }
}

/// The loudest channel's peak, in dB. `levels` is OBS's `inputLevelsMul`: per channel,
/// `[magnitude, peak, input peak]`, each a 0..1 multiplier.
fn peak_db(levels: &Value) -> f64 {
    let peak = levels
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|ch| ch[1].as_f64())
        .fold(0.0, f64::max);
    if peak <= 0.0 {
        return FLOOR_DB;
    }
    ((20.0 * peak.log10()).max(FLOOR_DB) * 10.0).round() / 10.0
}

/// OBS's `HH:MM:SS.mmm` as `MM:SS`, or `H:MM:SS` past the first hour; empty if unreadable.
fn short_timecode(t: &str) -> String {
    let t = t.split('.').next().unwrap_or_default();
    let p: Vec<&str> = t.split(':').collect();
    match p.as_slice() {
        [h, m, s] if [h, m, s].iter().all(|x| x.parse::<u32>().is_ok()) => {
            if h.parse::<u32>() == Ok(0) {
                format!("{m}:{s}")
            } else {
                format!("{}:{m}:{s}", h.parse::<u32>().unwrap_or(0))
            }
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_matches_the_obs_websocket_spec_example() {
        // The worked example in obs-websocket's protocol docs.
        assert_eq!(
            auth_string(
                "supersecretpassword",
                "lM1GncleQOaCu9lT1yeUZhFYnqhsLLP1G5lAGo3ixaI=",
                "+IxH4CnCiqpX1rM9scsNynZzbOe4KhDeYcTNS3PDaeY="
            ),
            "1Ct943GAT+6YQUUX47Ia/ncufilbe6+oD6lY+5kaCu4="
        );
    }

    #[test]
    fn refuses_non_ws_addresses() {
        let cfg = ObsConfig {
            url: "http://127.0.0.1:4455".into(),
            password: String::new(),
        };
        assert!(set_scene(&cfg, "x").is_err());
    }

    /// A link retired while it still waits on its predecessor never asks whether OBS is
    /// needed, so it makes no connect attempt.
    #[test]
    fn a_link_retired_while_joining_does_not_connect() {
        let asked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let a = asked.clone();
        let (tx, _rx) = mpsc::unbounded_channel();
        let predecessor = std::thread::spawn(|| std::thread::sleep(Duration::from_millis(300)));
        let link = ObsLink::spawn_after(
            ObsConfig {
                url: "ws://127.0.0.1:9".into(),
                password: String::new(),
            },
            Arc::new(FeedStore::default()),
            tx,
            move || {
                a.store(true, std::sync::atomic::Ordering::SeqCst);
                true
            },
            vec![predecessor],
        );
        link.retire().unwrap().join().unwrap();
        assert!(!asked.load(std::sync::atomic::Ordering::SeqCst));
    }

    type Seen = std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>;

    /// A one-shot-per-connection fake obs-websocket server. `respond` maps a request type to
    /// `(code, responseData)`; code 100 is success. Returns the address and the requests seen.
    fn fake_obs(conns: usize, respond: fn(&str) -> (u32, Value)) -> (ObsConfig, Seen) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let cfg = ObsConfig {
            url: format!("ws://{}", listener.local_addr().unwrap()),
            password: String::new(),
        };
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for _ in 0..conns {
                let (stream, _) = listener.accept().unwrap();
                let mut ws = tungstenite::accept(stream).unwrap();
                ws.send(Message::text(
                    json!({ "op": 0, "d": { "rpcVersion": 1 } }).to_string(),
                ))
                .unwrap();
                ws.read().unwrap();
                ws.send(Message::text(json!({ "op": 2, "d": {} }).to_string()))
                    .unwrap();
                let Message::Text(t) = ws.read().unwrap() else {
                    panic!("expected a request");
                };
                let m: Value = serde_json::from_str(t.as_str()).unwrap();
                let ty = m["d"]["requestType"].as_str().unwrap().to_string();
                log.lock()
                    .unwrap()
                    .push((ty.clone(), m["d"]["requestData"].clone()));
                let (code, data) = respond(&ty);
                ws.send(Message::text(
                    json!({ "op": 7, "d": {
                        "requestType": ty,
                        "requestId": m["d"]["requestId"],
                        "requestStatus": { "result": code == 100, "code": code, "comment": "nope" },
                        "responseData": data,
                    } })
                    .to_string(),
                ))
                .unwrap();
                let _ = ws.read();
            }
        });
        (cfg, seen)
    }

    #[test]
    fn obs_new_requests_shape() {
        let (cfg, seen) = fake_obs(5, |ty| match ty {
            "GetInputVolume" => (100, json!({ "inputVolumeDb": -10.0 })),
            _ => (100, json!({})),
        });
        chapter_marker(&cfg).unwrap();
        replay_save(&cfg).unwrap();
        input_volume_step(&cfg, "Mic", -3).unwrap();
        input_mute_toggle(&cfg, "Mic").unwrap();
        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            [
                ("CreateRecordChapter".to_string(), json!({})),
                ("SaveReplayBuffer".to_string(), json!({})),
                ("GetInputVolume".to_string(), json!({ "inputName": "Mic" })),
                (
                    "SetInputVolume".to_string(),
                    json!({ "inputName": "Mic", "inputVolumeDb": -13.0 })
                ),
                ("ToggleInputMute".to_string(), json!({ "inputName": "Mic" })),
            ]
        );
    }

    #[test]
    fn volume_step_clamps_to_obs_range() {
        let (cfg, seen) = fake_obs(4, |ty| match ty {
            "GetInputVolume" => (100, json!({ "inputVolumeDb": 25.0 })),
            _ => (100, json!({})),
        });
        input_volume_step(&cfg, "Mic", 10).unwrap();
        input_volume_step(&cfg, "Mic", -200).unwrap();
        let sets: Vec<Value> = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| t == "SetInputVolume")
            .map(|(_, d)| d["inputVolumeDb"].clone())
            .collect();
        assert_eq!(sets, [json!(26.0), json!(-100.0)]);
    }

    #[test]
    fn chapter_marker_explains_old_obs() {
        let (cfg, _) = fake_obs(1, |_| (204, json!({})));
        let e = chapter_marker(&cfg).unwrap_err();
        assert!(format!("{e:#}").contains("OBS 30.2"), "{e:#}");
        // Other failures keep OBS's own words.
        let (cfg, _) = fake_obs(1, |_| (501, json!({})));
        let e = chapter_marker(&cfg).unwrap_err();
        assert!(!format!("{e:#}").contains("30.2"), "{e:#}");
    }

    // ---- the persistent link ----

    use crate::feeds::{FeedStore, FeedValue};
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::sync::mpsc as std_mpsc;
    use tokio::sync::mpsc;

    fn wanted(feeds: &FeedStore, keys: &[&str]) {
        feeds.set_wanted(keys.iter().map(|k| k.to_string()).collect::<BTreeSet<_>>());
    }

    fn until(what: &str, mut f: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(std::time::Instant::now() < deadline, "timed out: {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A fake OBS that keeps one connection open. `script` gets the socket after Identified
    /// and the Identify data; it should return when the client hangs up.
    fn fake_link(
        script: impl FnOnce(&mut WebSocket<TcpStream>) + Send + 'static,
    ) -> (ObsConfig, std_mpsc::Receiver<Value>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let cfg = ObsConfig {
            url: format!("ws://{}", listener.local_addr().unwrap()),
            password: String::new(),
        };
        let (tx, rx) = std_mpsc::channel();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            ws.send(Message::text(
                json!({ "op": 0, "d": { "rpcVersion": 1 } }).to_string(),
            ))
            .unwrap();
            let Message::Text(t) = ws.read().unwrap() else {
                panic!("expected identify");
            };
            let m: Value = serde_json::from_str(t.as_str()).unwrap();
            let _ = tx.send(m["d"].clone());
            ws.send(Message::text(json!({ "op": 2, "d": {} }).to_string()))
                .unwrap();
            script(&mut ws);
        });
        (cfg, rx)
    }

    fn event(ws: &mut WebSocket<TcpStream>, ty: &str, data: Value) {
        ws.send(Message::text(
            json!({ "op": 5, "d": { "eventType": ty, "eventData": data } }).to_string(),
        ))
        .unwrap();
    }

    /// Reads until the client hangs up, handing each JSON message to `f`.
    fn drain(ws: &mut WebSocket<TcpStream>, mut f: impl FnMut(&mut WebSocket<TcpStream>, Value)) {
        while let Ok(Message::Text(t)) = ws.read() {
            f(ws, serde_json::from_str(t.as_str()).unwrap());
        }
    }

    fn number(feeds: &FeedStore, key: &str) -> Option<f64> {
        match feeds.get(key)?.value {
            FeedValue::Number(n) => Some(n),
            _ => None,
        }
    }

    #[test]
    fn link_sets_feeds_from_events() {
        let (cfg, _) = fake_link(|ws| {
            event(ws, "RecordStateChanged", json!({ "outputActive": true }));
            event(
                ws,
                "CurrentProgramSceneChanged",
                json!({ "sceneName": "Main" }),
            );
            drain(ws, |_, _| {});
        });
        let feeds = Arc::new(FeedStore::default());
        wanted(&feeds, &["obs.recording"]);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let link = ObsLink::spawn(cfg, feeds.clone(), tx);
        until("recording feed", || {
            number(&feeds, "obs.recording") == Some(1.0)
        });
        assert_eq!(number(&feeds, "obs.connected"), Some(1.0));
        until("scene feed", || {
            feeds.get("obs.scene").map(|f| f.value) == Some(FeedValue::Text("Main".into()))
        });
        assert_eq!(rx.try_recv().unwrap(), ObsEvent::Connected(true));
        assert_eq!(rx.try_recv().unwrap(), ObsEvent::Recording(true));
        link.stop();
        // Nothing is left claiming OBS is up.
        assert!(feeds.get("obs.connected").is_none());
        assert!(feeds.get("obs.recording").is_none());
        assert_eq!(rx.try_recv().unwrap(), ObsEvent::Recording(false));
        assert_eq!(rx.try_recv().unwrap(), ObsEvent::Connected(false));
    }

    #[test]
    fn meters_subscribed_only_while_wanted() {
        let (subs_tx, subs_rx) = std_mpsc::channel();
        let (cfg, identify) = fake_link(move |ws| {
            drain(ws, |ws, m| {
                if m["op"] == 3 {
                    let _ = subs_tx.send(m["d"]["eventSubscriptions"].as_u64().unwrap());
                    ws.send(Message::text(json!({ "op": 2, "d": {} }).to_string()))
                        .unwrap();
                    // A burst, the way OBS sends levels: only some may land.
                    for i in 0..20 {
                        let mul = 0.2 + f64::from(i) * 0.001;
                        event(
                            ws,
                            "InputVolumeMeters",
                            json!({ "inputs": [{ "inputName": "Mic/Aux", "inputLevelsMul": [
                                [0.5, 0.1, 0.1], [0.5, mul, mul]
                            ] }] }),
                        );
                    }
                }
            });
        });
        let feeds = Arc::new(FeedStore::default());
        wanted(&feeds, &["obs.recording"]);
        let (tx, _rx) = mpsc::unbounded_channel();
        let link = ObsLink::spawn(cfg, feeds.clone(), tx);
        let first = identify.recv_timeout(Duration::from_secs(5)).unwrap();
        let subs = first["eventSubscriptions"].as_u64().unwrap();
        // General, Scenes, Inputs and Outputs, and no high-volume meters.
        assert_eq!(subs, 1 | 4 | 8 | 64);
        assert_eq!(subs & (1 << 16), 0);

        wanted(&feeds, &["obs.meter.Mic/Aux"]);
        let re = subs_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(re & (1 << 16), 1 << 16, "Reidentify asks for meters");
        until("meter feed", || feeds.get("obs.meter.Mic/Aux").is_some());
        let db = number(&feeds, "obs.meter.Mic/Aux").unwrap();
        // The loudest channel's peak: 20*log10(~0.2), about -14 dB.
        assert!((-14.1..=-13.9).contains(&db), "{db}");
        // 20 events inside a few milliseconds: the 10/s cap lets one through.
        std::thread::sleep(Duration::from_millis(150));
        let n = feeds.get("obs.meter.Mic/Aux").unwrap().history.len();
        assert!((1..=2).contains(&n), "{n} meter updates");

        // Not wanted any more: unsubscribed, and the level is gone.
        wanted(&feeds, &["obs.recording"]);
        let re = subs_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(re & (1 << 16), 0);
        until("meter feed cleared", || {
            feeds.get("obs.meter.Mic/Aux").is_none()
        });
        link.stop();
    }

    #[test]
    fn link_polls_time_and_stream_stats_while_wanted() {
        let started = std::time::Instant::now();
        let (cfg, _) = fake_link(move |ws| {
            drain(ws, |ws, m| {
                if m["op"] != 6 {
                    return;
                }
                let data = match m["d"]["requestType"].as_str().unwrap() {
                    "GetRecordStatus" => {
                        json!({ "outputActive": true, "outputTimecode": "00:12:41.500" })
                    }
                    "GetStreamStatus" => json!({
                        "outputActive": true,
                        "outputTimecode": "01:02:03.000",
                        "outputSkippedFrames": 7,
                        // 125 kB a second of real time: 1000 kbps.
                        "outputBytes": (started.elapsed().as_secs_f64() * 125_000.0) as u64,
                    }),
                    _ => json!({}),
                };
                ws.send(Message::text(
                    json!({ "op": 7, "d": {
                        "requestType": m["d"]["requestType"], "requestId": m["d"]["requestId"],
                        "requestStatus": { "result": true, "code": 100 }, "responseData": data,
                    } })
                    .to_string(),
                ))
                .unwrap();
            });
        });
        let feeds = Arc::new(FeedStore::default());
        wanted(
            &feeds,
            &[
                "obs.record.time",
                "obs.stream.time",
                "obs.stream.kbps",
                "obs.stream.dropped",
            ],
        );
        let (tx, _rx) = mpsc::unbounded_channel();
        let link = ObsLink::spawn(cfg, feeds.clone(), tx);
        until("record time", || {
            feeds.get("obs.record.time").map(|f| f.value) == Some(FeedValue::Text("12:41".into()))
        });
        until("stream time", || {
            feeds.get("obs.stream.time").map(|f| f.value) == Some(FeedValue::Text("1:02:03".into()))
        });
        assert_eq!(number(&feeds, "obs.stream.dropped"), Some(7.0));
        until("bitrate", || number(&feeds, "obs.stream.kbps").is_some());
        let kbps = number(&feeds, "obs.stream.kbps").unwrap();
        assert!((800.0..=1200.0).contains(&kbps), "{kbps}");
        link.stop();
    }

    #[test]
    fn retries_every_30s_only_while_needed() {
        // A virtual clock: waiting adds to it, and the run ends at 65 s.
        let run = |needed: bool| {
            let now = std::cell::Cell::new(0u64);
            let attempts = std::cell::Cell::new(0u32);
            supervise(
                &|| needed,
                || attempts.set(attempts.get() + 1),
                |d| {
                    now.set(now.get() + d.as_secs());
                    now.get() <= 65
                },
            );
            attempts.get()
        };
        // t = 0, 30 and 60.
        assert_eq!(run(true), 3);
        assert_eq!(run(false), 0);
    }

    #[test]
    fn needing_ends_while_waiting_stops_the_link() {
        // Nothing listens on this port, so every attempt fails; with `needed` false the
        // thread must exit on its own, with no attempt made.
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let cfg = ObsConfig {
            url: format!("ws://127.0.0.1:{closed}"),
            password: String::new(),
        };
        let feeds = Arc::new(FeedStore::default());
        let (tx, _rx) = mpsc::unbounded_channel();
        let link = ObsLink::spawn(cfg, feeds.clone(), tx);
        // Not wanted: gone without waiting 30 s.
        link.stop();
        assert!(feeds.get("obs.connected").is_none());
    }

    #[test]
    fn short_timecodes() {
        assert_eq!(short_timecode("00:12:41.123"), "12:41");
        assert_eq!(short_timecode("01:02:03.000"), "1:02:03");
        assert_eq!(short_timecode("garbage"), "");
    }
}
