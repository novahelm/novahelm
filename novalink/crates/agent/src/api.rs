//! The local HTTP API: loopback only, bearer-token authenticated, Host-header checked.
//!
//! [`router`] builds the app (unit-testable without a socket); [`serve`] binds
//! `127.0.0.1:7979` and serves it. Device ids are 64-character hex strings in all JSON.

use crate::engine::{CaptureState, EngineHandle, MetricsReport};
use crate::soak::{SoakSlot, duration_from_hours};
use anyhow::{Context, Result};
use axum::extract::{Path as UrlPath, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use hmac::{Hmac, Mac};
use novalink_proto::{DeviceId, Edge};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use std::path::Path;
use subtle::ConstantTimeEq;

pub const API_ADDR: &str = "127.0.0.1:7979";
const ALLOWED_HOSTS: [&str; 2] = ["127.0.0.1:7979", "localhost:7979"];
const PROOF_LABEL: &[u8] = b"novalink-server-proof";
const NONCE_BYTES: usize = 16;
const TOKEN_FILE: &str = "api-token";
const DEFAULT_EVENT_LIMIT: usize = 200;
const MAX_EVENT_LIMIT: usize = 5000;

#[derive(Clone)]
struct App {
    handle: EngineHandle,
    token: String,
    soak: SoakSlot,
    /// When the API started; discovery gets a grace period before doctor complains.
    started: std::time::Instant,
}

/// Reads `<dir>/api-token`, creating it (32 random bytes, hex, mode 0600) if absent.
pub fn load_or_create_token(dir: &Path) -> Result<String> {
    use std::io::Write;
    crate::paths::create_private_dir(dir).context("create config dir")?;
    let path = dir.join(TOKEN_FILE);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    match opts.open(&path) {
        Ok(mut f) => {
            let mut bytes = [0u8; 32];
            rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut bytes);
            let token = hex(&bytes);
            f.write_all(token.as_bytes())?;
            f.sync_all()?;
            Ok(token)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let t = std::fs::read_to_string(&path).context("read api token")?;
            let t = t.trim().to_string();
            anyhow::ensure!(!t.is_empty(), "{} is empty; delete it", path.display());
            Ok(t)
        }
        Err(e) => Err(e).context("create api token"),
    }
}

/// A fresh random nonce for [`server_proof`].
pub fn new_nonce() -> [u8; NONCE_BYTES] {
    let mut n = [0u8; NONCE_BYTES];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut n);
    n
}

pub fn encode_hex(bytes: &[u8]) -> String {
    hex(bytes)
}

fn proof_mac(token: &str, nonce: &[u8]) -> Hmac<Sha256> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(token.as_bytes()).expect("hmac takes any key length");
    mac.update(PROOF_LABEL);
    mac.update(nonce);
    mac
}

/// HMAC-SHA256 keyed with the token's bytes over `"novalink-server-proof" || nonce`, hex.
/// Lets a client confirm the listener knows the token before it sends the token.
pub fn server_proof(token: &str, nonce: &[u8]) -> String {
    hex(&proof_mac(token, nonce).finalize().into_bytes())
}

/// Checks a hex proof in constant time.
pub fn verify_proof(token: &str, nonce: &[u8], proof_hex: &str) -> bool {
    let Some(bytes) = decode_hex(proof_hex) else {
        return false;
    };
    proof_mac(token, nonce).verify_slice(&bytes).is_ok()
}

pub fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok())
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn parse_id(s: &str) -> Option<DeviceId> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(DeviceId(out))
}

/// `DeviceId` serializes as a 32-number array; the API shows it as hex.
fn hexify(v: Value) -> Value {
    match v {
        Value::Array(a) => {
            let bytes: Option<Vec<u8>> = (a.len() == 32)
                .then(|| {
                    a.iter()
                        .map(|x| x.as_u64().and_then(|n| u8::try_from(n).ok()))
                        .collect()
                })
                .flatten();
            match bytes {
                Some(b) => Value::String(hex(&b)),
                None => Value::Array(a.into_iter().map(hexify).collect()),
            }
        }
        Value::Object(m) => Value::Object(m.into_iter().map(|(k, v)| (k, hexify(v))).collect()),
        other => other,
    }
}

fn json_of<T: Serialize>(v: &T) -> Json<Value> {
    Json(hexify(serde_json::to_value(v).unwrap_or(Value::Null)))
}

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

fn err(code: StatusCode, msg: impl Into<String>) -> ApiError {
    ApiError(code, msg.into())
}

type ApiResult = Result<Json<Value>, ApiError>;

/// Picks a device by full 64-hex id, a hex id prefix of at least 8 characters, or a
/// (case-insensitive) name from `cands`. A name or prefix matching several is ambiguous.
fn resolve(q: &str, cands: &[(DeviceId, String)]) -> Result<DeviceId, ApiError> {
    let ambiguous = || err(StatusCode::BAD_REQUEST, "ambiguous name, use the id");
    if let Some(id) = parse_id(q) {
        return Ok(id);
    }
    if q.len() >= 8 && q.bytes().all(|b| b.is_ascii_hexdigit()) {
        let p = q.to_ascii_lowercase();
        let mut hits = cands
            .iter()
            .filter(|(id, _)| id.to_string().starts_with(&p));
        match (hits.next(), hits.next()) {
            (Some((id, _)), None) => return Ok(*id),
            (Some(_), Some(_)) => return Err(ambiguous()),
            _ => {}
        }
    }
    let mut hits = cands.iter().filter(|(_, n)| n.eq_ignore_ascii_case(q));
    match (hits.next(), hits.next()) {
        (Some((id, _)), None) => Ok(*id),
        (Some(_), Some(_)) => Err(ambiguous()),
        _ => Err(err(StatusCode::NOT_FOUND, format!("no device matches {q}"))),
    }
}

fn peer_cands(h: &EngineHandle) -> Vec<(DeviceId, String)> {
    h.status()
        .peers
        .into_iter()
        .map(|p| (p.id, p.name))
        .collect()
}

async fn guard(State(app): State<App>, req: Request, next: Next) -> Response {
    let host_ok = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|h| ALLOWED_HOSTS.contains(&h.to_ascii_lowercase().as_str()));
    if !host_ok {
        return err(StatusCode::MISDIRECTED_REQUEST, "host not allowed").into_response();
    }
    if req.uri().path() == "/v1/hello" {
        // Proves the server knows the token, without needing it from the caller.
        return next.run(req).await;
    }
    let given = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("");
    let ok: bool = given.as_bytes().ct_eq(app.token.as_bytes()).into();
    if !ok {
        return err(StatusCode::UNAUTHORIZED, "missing or wrong token").into_response();
    }
    next.run(req).await
}

pub fn router(handle: EngineHandle, token: String) -> Router {
    let app = App {
        handle,
        token,
        soak: SoakSlot::default(),
        started: std::time::Instant::now(),
    };
    Router::new()
        .route("/v1/hello", get(hello))
        .route("/v1/status", get(status))
        .route("/v1/metrics", get(metrics))
        .route("/v1/events", get(events))
        .route("/v1/discovered", get(discovered))
        .route("/v1/pair", post(pair))
        .route("/v1/pair/pending", get(pair_pending))
        .route("/v1/pair/answer", post(pair_answer))
        .route("/v1/devices/{id}", delete(unpair))
        .route("/v1/layout", put(layout))
        .route("/v1/settings", put(settings))
        .route("/v1/doctor", get(doctor))
        .route("/v1/permissions/request", post(permissions_request))
        .route("/v1/shutdown", post(shutdown))
        .route(
            "/v1/services/{service}",
            put(service_put).delete(service_delete),
        )
        .route(
            "/v1/soak",
            post(soak_start).get(soak_status).delete(soak_stop),
        )
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
}

/// Binds `127.0.0.1:7979` (loopback only).
pub async fn bind() -> Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(API_ADDR)
        .await
        .with_context(|| format!("bind {API_ADDR}"))
}

/// Serves the API on a bound listener until the task is dropped.
pub async fn serve_on(
    listener: tokio::net::TcpListener,
    handle: EngineHandle,
    token: String,
) -> Result<()> {
    // On a shutdown request, finish the in-flight replies (including its own) first.
    let stop = handle.clone();
    axum::serve(listener, router(handle, token))
        .with_graceful_shutdown(async move { stop.shutdown_requested().await })
        .await?;
    Ok(())
}

/// Binds `127.0.0.1:7979` and serves until the task is dropped.
pub async fn serve(handle: EngineHandle, token: String) -> Result<()> {
    serve_on(bind().await?, handle, token).await
}

/// Whether the listener at `addr` is a NovaLink agent holding `token`: it must answer
/// `/v1/hello` with a valid proof. The token itself is never sent.
pub async fn proven_agent_at(addr: std::net::SocketAddr, token: &str) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let nonce = new_nonce();
    let ask = async {
        let mut s = tokio::net::TcpStream::connect(addr).await.ok()?;
        let req = format!(
            "GET /v1/hello?nonce={} HTTP/1.1\r\nHost: {API_ADDR}\r\nConnection: close\r\n\r\n",
            hex(&nonce)
        );
        s.write_all(req.as_bytes()).await.ok()?;
        let mut reply = Vec::new();
        s.take(64 * 1024).read_to_end(&mut reply).await.ok()?;
        let text = String::from_utf8(reply).ok()?;
        let (head, body) = text.split_once("\r\n\r\n")?;
        head.starts_with("HTTP/1.1 200").then_some(())?;
        let v: Value = serde_json::from_str(body.trim()).ok()?;
        Some(v["proof"].as_str()?.to_string())
    };
    match tokio::time::timeout(std::time::Duration::from_secs(2), ask).await {
        Ok(Some(proof)) => verify_proof(token, &nonce, &proof),
        _ => false,
    }
}

#[derive(Deserialize)]
struct HelloQuery {
    nonce: String,
}

async fn hello(State(a): State<App>, Query(q): Query<HelloQuery>) -> ApiResult {
    let nonce = decode_hex(&q.nonce)
        .filter(|n| n.len() == NONCE_BYTES)
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "nonce must be 32 hex characters"))?;
    Ok(Json(json!({ "proof": server_proof(&a.token, &nonce) })))
}

async fn status(State(a): State<App>) -> ApiResult {
    Ok(json_of(&a.handle.status()))
}

async fn metrics(State(a): State<App>) -> ApiResult {
    Ok(json_of(&a.handle.metrics()))
}

#[derive(Deserialize)]
struct EventsQuery {
    limit: Option<usize>,
}

async fn events(State(a): State<App>, Query(q): Query<EventsQuery>) -> ApiResult {
    let n = q.limit.unwrap_or(DEFAULT_EVENT_LIMIT).min(MAX_EVENT_LIMIT);
    Ok(Json(Value::Array(a.handle.recent_events(n))))
}

async fn discovered(State(a): State<App>) -> ApiResult {
    let v: Vec<Value> = a
        .handle
        .discovered()
        .into_iter()
        .map(|d| json!({ "id": d.id.to_string(), "name": d.name, "addrs": d.addrs }))
        .collect();
    Ok(Json(Value::Array(v)))
}

#[derive(Deserialize)]
struct PairBody {
    name: Option<String>,
    id: Option<String>,
}

async fn pair(State(a): State<App>, Json(b): Json<PairBody>) -> ApiResult {
    let q =
        b.id.or(b.name)
            .ok_or_else(|| err(StatusCode::BAD_REQUEST, "give a name or an id"))?;
    let cands: Vec<_> = a
        .handle
        .discovered()
        .into_iter()
        .map(|d| (d.id, d.name))
        .collect();
    let id = resolve(&q, &cands)?;
    a.handle
        .request_pair(id)
        .await
        .map_err(|e| err(StatusCode::CONFLICT, format!("{e:#}")))?;
    Ok(Json(json!({ "ok": true, "id": id.to_string() })))
}

async fn pair_pending(State(a): State<App>) -> ApiResult {
    Ok(json_of(&a.handle.pending_pairs()))
}

#[derive(Deserialize)]
struct AnswerBody {
    id: String,
    accept: bool,
}

async fn pair_answer(State(a): State<App>, Json(b): Json<AnswerBody>) -> ApiResult {
    let cands: Vec<_> = a
        .handle
        .pending_pairs()
        .into_iter()
        .map(|p| (p.peer, p.name))
        .collect();
    let id = resolve(&b.id, &cands)?;
    if !cands.iter().any(|(p, _)| *p == id) {
        return Err(err(
            StatusCode::NOT_FOUND,
            "no pairing request from that device",
        ));
    }
    a.handle.answer_pair(id, b.accept).await;
    Ok(Json(json!({ "ok": true })))
}

async fn unpair(State(a): State<App>, UrlPath(id): UrlPath<String>) -> ApiResult {
    let id = resolve(&id, &peer_cands(&a.handle))?;
    if !peer_cands(&a.handle).iter().any(|(p, _)| *p == id) {
        return Err(err(StatusCode::NOT_FOUND, "that device is not paired"));
    }
    a.handle
        .unpair(id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct LayoutBody {
    edge: String,
    peer: String,
}

async fn layout(State(a): State<App>, Json(b): Json<LayoutBody>) -> ApiResult {
    let edge = match b.edge.to_ascii_lowercase().as_str() {
        "left" => Edge::Left,
        "right" => Edge::Right,
        "top" => Edge::Top,
        "bottom" => Edge::Bottom,
        other => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!("edge must be left, right, top or bottom, not {other}"),
            ));
        }
    };
    let cands = peer_cands(&a.handle);
    let id = resolve(&b.peer, &cands)?;
    if !cands.iter().any(|(p, _)| *p == id) {
        return Err(err(StatusCode::NOT_FOUND, "that device is not paired"));
    }
    a.handle.set_layout(edge, id);
    Ok(Json(json!({ "ok": true })))
}

/// Settings left out keep their current value.
#[derive(Deserialize)]
struct SettingsBody {
    swap_ctrl_cmd: Option<bool>,
}

async fn settings(State(a): State<App>, Json(b): Json<SettingsBody>) -> ApiResult {
    let mut s = a.handle.status().settings;
    if let Some(v) = b.swap_ctrl_cmd {
        s.swap_ctrl_cmd = v;
    }
    a.handle.set_settings(s).map_err(|e| {
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("couldn't save the settings: {e:#}"),
        )
    })?;
    Ok(Json(json!(s)))
}

#[derive(Deserialize)]
struct SoakBody {
    peer: String,
    hours: f64,
    /// Allow driving a peer that is not running with `--dry-inject`.
    #[serde(default)]
    allow_live: bool,
}

/// Starts a soak against a paired, connected peer; returns at once.
async fn soak_start(State(a): State<App>, Json(b): Json<SoakBody>) -> ApiResult {
    let duration = duration_from_hours(b.hours).ok_or_else(|| {
        err(
            StatusCode::BAD_REQUEST,
            "hours must be above 0 and at most 720",
        )
    })?;
    let cands = peer_cands(&a.handle);
    let id = resolve(&b.peer, &cands)?;
    if !cands.iter().any(|(p, _)| *p == id) {
        return Err(err(StatusCode::NOT_FOUND, "that device is not paired"));
    }
    let status = a.handle.status();
    let Some(p) = status.peers.iter().find(|p| p.id == id && p.connected) else {
        return Err(err(StatusCode::CONFLICT, "that device is not connected"));
    };
    if p.dry_inject != Some(true) && !b.allow_live {
        return Err(err(
            StatusCode::CONFLICT,
            "peer is not in --dry-inject; refusing to type into it",
        ));
    }
    a.soak
        .start(&a.handle, id, duration)
        .map_err(|e| err(StatusCode::CONFLICT, e))?;
    Ok(Json(json!({ "ok": true, "peer": id.to_string() })))
}

/// Ends the running soak after its final accounting pass.
async fn soak_stop(State(a): State<App>) -> ApiResult {
    if a.soak.stop() {
        Ok(Json(json!({ "ok": true })))
    } else {
        Err(err(StatusCode::NOT_FOUND, "no soak is running"))
    }
}

/// Progress of the running soak, or the latest report.
async fn soak_status(State(a): State<App>) -> ApiResult {
    Ok(json_of(&a.soak.snapshot()))
}

#[derive(Debug, Serialize)]
struct Check {
    name: String,
    ok: bool,
    detail: String,
    advice: String,
}

fn check(name: impl Into<String>, ok: bool, detail: impl Into<String>, advice: &str) -> Check {
    Check {
        name: name.into(),
        ok,
        detail: detail.into(),
        advice: if ok {
            String::new()
        } else {
            advice.to_string()
        },
    }
}

fn permission_advice(name: &str) -> String {
    match name {
        "accessibility" => {
            "Turn on novalink-agent in System Settings → Privacy & Security → Accessibility.".into()
        }
        "input_monitoring" => {
            "Turn on novalink-agent in System Settings → Privacy & Security → Input Monitoring.".into()
        }
        "session_interactive" => {
            "Run NovaLink from your signed-in desktop session, not from a service or a locked screen."
                .into()
        }
        other => format!("Grant the {other} permission to novalink-agent."),
    }
}

/// How long discovery may find nothing before doctor points at the OS blocking it.
const DISCOVERY_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

/// Fails when nothing has been seen on the network for a while. A paired device counts:
/// once one is known, discovery isn't needed to reach it.
fn discovery_check(seen_any: bool, waited: std::time::Duration, windows: bool) -> Check {
    let ok = seen_any || waited < DISCOVERY_GRACE;
    check(
        "discovery",
        ok,
        if seen_any {
            "other NovaLink devices found"
        } else if ok {
            "looking for other NovaLink devices"
        } else {
            "no other NovaLink devices found yet"
        },
        if windows {
            "If another one is running, allow NovaLink on Private networks in Windows Firewall and make sure your network is set to Private."
        } else {
            "If another one is running, allow NovaLink under System Settings → Privacy & Security → Local Network."
        },
    )
}

#[cfg_attr(not(windows), allow(dead_code))]
const FIREWALL_ADVICE: &str = "Run NovaLink-Setup.exe again and choose Yes when it offers to allow NovaLink through Windows Firewall.";

/// Windows only: whether the network is Public and whether the firewall lets NovaLink in,
/// the two things that stop devices finding each other.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_network_checks(facts: Result<crate::winnet::Facts, String>) -> Vec<Check> {
    use crate::winnet::{Category, Firewall, firewall, public_advice, public_networks};
    let f = match facts {
        Ok(f) => f,
        Err(e) => {
            return vec![check(
                "firewall",
                false,
                format!("couldn't read the network settings: {e}"),
                "Make sure your network is set to Private and NovaLink is allowed on Private networks in Windows Firewall.",
            )];
        }
    };
    let public = public_networks(&f);
    let named: Vec<String> = f
        .networks
        .iter()
        .map(|n| {
            let kind = match n.category {
                Category::Public => "Public",
                Category::Private => "Private",
                Category::Domain => "Domain",
            };
            format!("“{}” ({kind})", n.name)
        })
        .collect();
    let network = if f.networks.is_empty() {
        check(
            "network",
            false,
            "not connected to a network",
            "Connect this PC to the same network as your other computer.",
        )
    } else {
        check(
            "network",
            public.is_empty(),
            named.join(", "),
            &public_advice(&public),
        )
    };
    let (ok, detail) = match firewall(&f) {
        Firewall::Allowed => (true, "NovaLink is allowed on Private networks"),
        Firewall::Off => (true, "Windows Firewall is off for Private networks"),
        Firewall::Blocked => (false, "a Windows Firewall rule blocks NovaLink"),
        Firewall::NoRule => (false, "no Windows Firewall rule lets NovaLink in"),
    };
    vec![network, check("firewall", ok, detail, FIREWALL_ADVICE)]
}

/// What to do when capture isn't running, by platform and by why.
fn capture_advice(state: CaptureState, windows: bool) -> &'static str {
    match (state, windows) {
        (CaptureState::Running, _) => "",
        (_, true) => {
            "Input hooks couldn't start. Restart NovaLink from the tray; if it keeps happening, check the logs."
        }
        (CaptureState::WaitingForPermission, false) => {
            "Grant Accessibility and Input Monitoring to novalink-agent in System Settings → Privacy & Security."
        }
        (CaptureState::Failed, false) => {
            "Input capture failed even though permissions are granted. Turn NovaLink off and on from the menu; if it keeps happening, check the logs."
        }
    }
}

async fn permissions_request(State(a): State<App>) -> ApiResult {
    a.handle.request_permissions();
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct ServiceBody {
    port: u16,
    secret: String,
}

async fn service_put(
    State(a): State<App>,
    UrlPath(service): UrlPath<String>,
    Json(b): Json<ServiceBody>,
) -> ApiResult {
    a.handle
        .services()
        .set(&service, b.port, &b.secret)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    Ok(Json(json!({ "ok": true })))
}

async fn service_delete(State(a): State<App>, UrlPath(service): UrlPath<String>) -> ApiResult {
    if !a.handle.services().remove(&service) {
        return Err(err(StatusCode::NOT_FOUND, "no such service"));
    }
    Ok(Json(json!({ "ok": true })))
}

/// Replies first; `main` then runs the normal shutdown and exits.
async fn shutdown(State(a): State<App>) -> ApiResult {
    a.handle.request_shutdown();
    Ok(Json(json!({ "ok": true })))
}

async fn doctor(State(a): State<App>) -> ApiResult {
    let mut checks = Vec::new();
    let st = a.handle.status();
    let capturing = st.capture == CaptureState::Running;
    for (name, ok) in a.handle.permissions() {
        // macOS keeps answering "not granted" in a process started before the grant, but
        // the event tap only runs with both, so running capture settles it.
        let ok = ok || (capturing && matches!(name, "accessibility" | "input_monitoring"));
        let mut c = check(name, ok, if ok { "granted" } else { "not granted" }, "");
        if !ok {
            c.advice = permission_advice(name);
        }
        checks.push(c);
    }
    checks.push(check(
        "capture",
        capturing,
        if capturing {
            "reading this machine's keyboard and mouse"
        } else {
            "not reading this machine's keyboard and mouse"
        },
        capture_advice(st.capture, cfg!(windows)),
    ));
    let connected: Vec<_> = st.peers.iter().filter(|p| p.connected).collect();
    checks.push(check(
        "peer_connected",
        !connected.is_empty(),
        format!(
            "{} of {} paired devices connected",
            connected.len(),
            st.peers.len()
        ),
        if st.peers.is_empty() {
            "Pair a device with: novalink pair <name>."
        } else {
            "Make sure the other machine is awake, on the same network and running novalink-agent."
        },
    ));
    for p in &st.peers {
        let rtt = p
            .rtt_us
            .map(|us| format!("{:.1} ms", us as f64 / 1000.0))
            .unwrap_or_else(|| "unknown".into());
        let advice = if p.connected {
            "Use a wired connection between the machines for the lowest latency."
        } else {
            "Make sure that machine is awake, on the same network and running novalink-agent."
        };
        let good_path = matches!(p.path, novalink_net::netif::PathKind::Wired);
        let ok = p.connected && (good_path || p.rtt_us.is_some_and(|us| us < 10_000));
        checks.push(check(
            format!("path:{}", p.name),
            ok,
            if p.connected {
                format!("{:?}, round trip {rtt}", p.path)
            } else {
                "not connected".into()
            },
            advice,
        ));
    }
    #[cfg(windows)]
    {
        let facts = tokio::task::spawn_blocking(|| {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            crate::winnet::probe(&exe)
        })
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
        checks.extend(windows_network_checks(facts));
    }
    checks.push(discovery_check(
        !st.peers.is_empty() || !a.handle.discovered().is_empty(),
        a.started.elapsed(),
        cfg!(windows),
    ));
    let offsets = !connected.is_empty() && connected.iter().all(|p| p.clock_offset_us.is_some());
    checks.push(check(
        "clock_offset",
        offsets,
        if offsets {
            "estimated for every connected device"
        } else {
            "no estimate yet"
        },
        "Wait a few seconds after connecting; if it persists, restart both agents.",
    ));
    checks.extend(history_checks(&a.handle.metrics()));
    Ok(Json(json!({ "checks": checks })))
}

/// Reconnects in the last hour and safety releases in the last day.
fn history_checks(m: &MetricsReport) -> Vec<Check> {
    vec![
        check(
            "reconnects",
            m.reconnects_last_hour < 3,
            format!(
                "{} in the last hour; last close: {}",
                m.reconnects_last_hour,
                m.last_reconnect_reason.as_deref().unwrap_or("none")
            ),
            "The link keeps dropping; check Wi-Fi signal or switch to a cable.",
        ),
        check(
            "safety_releases",
            m.safety_releases_last_day == 0,
            format!("{} in the last day", m.safety_releases_last_day),
            "Held keys were force-released; run novalink events and report it if this repeats.",
        ),
    ]
}
#[cfg(all(test, feature = "fake"))]
mod tests {
    use super::*;
    use crate::testutil::{one_engine, two_engines, wait_until};
    use axum::body::Body;
    use axum::http::{Request, Response};
    use novalink_net::discovery::Discovered;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    const TOKEN: &str = "test-token";
    const HOST: &str = "127.0.0.1:7979";

    async fn call(
        app: &Router,
        method: &str,
        path: &str,
        token: Option<&str>,
        host: &str,
        body: Option<Value>,
    ) -> Response<Body> {
        let mut r = Request::builder()
            .method(method)
            .uri(path)
            .header("host", host);
        if let Some(t) = token {
            r = r.header("authorization", format!("Bearer {t}"));
        }
        let body = match body {
            Some(b) => {
                r = r.header("content-type", "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        app.clone().oneshot(r.body(body).unwrap()).await.unwrap()
    }

    async fn get(app: &Router, path: &str, token: Option<&str>, host: &str) -> Response<Body> {
        call(app, "GET", path, token, host, None).await
    }

    async fn body_json(r: Response<Body>) -> Value {
        let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        serde_json::from_slice(&b).unwrap_or(Value::Null)
    }

    async fn app() -> (
        Router,
        crate::engine::EngineHandle,
        novalink_core::FakePlatform,
    ) {
        let (h, fake) = one_engine().await;
        (router(h.clone(), TOKEN.into()), h, fake)
    }

    #[tokio::test]
    async fn services_register_and_unregister_over_the_api() {
        let (app, h, _fake) = app().await;
        let body = json!({"port": 4000, "secret": "s3"});
        let put = |tok, b| call(&app, "PUT", "/v1/services/compute", tok, HOST, Some(b));
        assert_eq!(put(None, body.clone()).await.status(), 401);
        assert_eq!(put(Some(TOKEN), body.clone()).await.status(), 200);
        assert!(h.services().remove("compute"));
        assert_eq!(put(Some(TOKEN), body).await.status(), 200);
        let bad = json!({"port": 4000, "secret": "two words"});
        assert_eq!(put(Some(TOKEN), bad).await.status(), 400);
        let del = |tok| call(&app, "DELETE", "/v1/services/compute", tok, HOST, None);
        assert_eq!(del(None).await.status(), 401);
        assert_eq!(del(Some(TOKEN)).await.status(), 200);
        assert_eq!(del(Some(TOKEN)).await.status(), 404);
        let r = call(
            &app,
            "PUT",
            "/v1/services/Bad_Name",
            Some(TOKEN),
            HOST,
            Some(json!({"port": 1, "secret": "x"})),
        )
        .await;
        assert_eq!(r.status(), 400);
    }

    #[tokio::test]
    async fn rejects_missing_token() {
        let (app, ..) = app().await;
        assert_eq!(get(&app, "/v1/status", None, HOST).await.status(), 401);
    }

    #[tokio::test]
    async fn rejects_wrong_token() {
        let (app, ..) = app().await;
        let r = get(&app, "/v1/status", Some("nope"), HOST).await;
        assert_eq!(r.status(), 401);
    }

    #[tokio::test]
    async fn rejects_foreign_host() {
        let (app, ..) = app().await;
        let r = get(&app, "/v1/status", Some(TOKEN), "evil.example:7979").await;
        assert_eq!(r.status(), 421);
        let r = get(&app, "/v1/status", Some(TOKEN), "127.0.0.1:80").await;
        assert_eq!(r.status(), 421);
    }

    #[tokio::test]
    async fn status_ok_with_token() {
        let (app, ..) = app().await;
        let r = get(&app, "/v1/status", Some(TOKEN), HOST).await;
        assert_eq!(r.status(), 200);
        let j = body_json(r).await;
        assert!(j["me"].is_string());
        assert_eq!(j["me"].as_str().unwrap().len(), 64);
        let r = get(&app, "/v1/status", Some(TOKEN), "localhost:7979").await;
        assert_eq!(r.status(), 200);
    }

    #[tokio::test]
    async fn shutdown_requires_token_and_signals_engine() {
        let (app, h, _fake) = app().await;
        assert_eq!(
            call(&app, "POST", "/v1/shutdown", None, HOST, None)
                .await
                .status(),
            401
        );
        let pending = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            h.shutdown_requested(),
        )
        .await;
        assert!(pending.is_err(), "a rejected request must not signal");
        let r = call(&app, "POST", "/v1/shutdown", Some(TOKEN), HOST, None).await;
        assert_eq!(r.status(), 200);
        assert_eq!(body_json(r).await["ok"], true);
        tokio::time::timeout(std::time::Duration::from_secs(1), h.shutdown_requested())
            .await
            .expect("shutdown was requested");
    }

    #[tokio::test]
    async fn shutdown_reply_is_delivered_before_the_api_stops() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (h, _fake) = one_engine().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_on(listener, h.clone(), TOKEN.into()));
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        let req = format!(
            "POST /v1/shutdown HTTP/1.1\r\nHost: {HOST}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        s.write_all(req.as_bytes()).await.unwrap();
        let mut reply = String::new();
        s.read_to_string(&mut reply).await.unwrap();
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        assert!(reply.contains("\"ok\":true"), "{reply}");
        assert!(h.is_shutdown_requested());
        tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .expect("server stops after the reply")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn permissions_request_calls_platform() {
        let (app, _h, fake) = app().await;
        let r = call(&app, "POST", "/v1/permissions/request", None, HOST, None).await;
        assert_eq!(r.status(), 401);
        assert_eq!(fake.requests(), 0);
        let r = call(
            &app,
            "POST",
            "/v1/permissions/request",
            Some(TOKEN),
            HOST,
            None,
        )
        .await;
        assert_eq!(r.status(), 200);
        assert_eq!(fake.requests(), 1);
    }

    #[tokio::test]
    async fn doctor_capture_check_follows_capture_state() {
        let (h, _fake) = crate::testutil::one_engine_with(|p| p.deny_capture(true)).await;
        let app = router(h, TOKEN.into());
        let j = body_json(get(&app, "/v1/doctor", Some(TOKEN), HOST).await).await;
        let c = j["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "capture")
            .unwrap();
        assert_eq!(c["ok"], false);
        assert_eq!(
            c["advice"],
            capture_advice(CaptureState::WaitingForPermission, cfg!(windows))
        );
        let (app, ..) = self::app().await;
        let j = body_json(get(&app, "/v1/doctor", Some(TOKEN), HOST).await).await;
        let c = j["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "capture")
            .unwrap();
        assert_eq!(c["ok"], true);
    }

    #[test]
    fn discovery_check_waits_then_points_at_local_network() {
        use std::time::Duration;
        assert!(discovery_check(false, Duration::from_secs(5), false).ok);
        let c = discovery_check(false, DISCOVERY_GRACE, false);
        assert!(!c.ok && c.advice.contains("Local Network"), "{c:?}");
        let c = discovery_check(false, DISCOVERY_GRACE, true);
        assert!(!c.ok && c.advice.contains("Private"), "{c:?}");
        assert!(discovery_check(true, Duration::from_secs(3600), false).ok);
    }

    #[test]
    fn windows_network_checks_name_the_public_network_and_the_firewall() {
        use crate::winnet::{Category, Facts, Network, PROFILE_PRIVATE, Rule};
        let net = |category| Network {
            name: "HomeNetwork".into(),
            category,
        };
        let mut f = Facts {
            networks: vec![net(Category::Public)],
            firewall_on_private: true,
            rules: vec![],
        };
        let c = windows_network_checks(Ok(f.clone()));
        assert_eq!((c[0].name.as_str(), c[0].ok), ("network", false));
        assert!(c[0].advice.contains("“HomeNetwork”"), "{:?}", c[0]);
        assert_eq!((c[1].name.as_str(), c[1].ok), ("firewall", false));
        assert!(c[1].advice.contains("NovaLink-Setup.exe"), "{:?}", c[1]);

        f.networks = vec![net(Category::Private)];
        f.rules = vec![Rule {
            allow: true,
            profiles: PROFILE_PRIVATE,
        }];
        let c = windows_network_checks(Ok(f.clone()));
        assert!(c.iter().all(|c| c.ok), "{c:?}");
        assert_eq!(c[0].detail, "“HomeNetwork” (Private)");

        f.networks.clear();
        assert!(!windows_network_checks(Ok(f))[0].ok);
        let c = windows_network_checks(Err("boom".into()));
        assert!(
            c.len() == 1 && !c[0].ok && c[0].detail.contains("boom"),
            "{c:?}"
        );
    }

    #[test]
    fn capture_advice_fits_platform_and_state() {
        use CaptureState::*;
        assert!(capture_advice(WaitingForPermission, false).contains("Input Monitoring"));
        let failed = capture_advice(Failed, false);
        assert!(
            failed.contains("even though permissions are granted"),
            "{failed}"
        );
        assert!(!failed.contains("Grant"));
        for st in [WaitingForPermission, Failed] {
            let w = capture_advice(st, true);
            assert!(
                w.contains("from the tray") && !w.contains("System Settings"),
                "{w}"
            );
        }
        assert_eq!(capture_advice(Running, false), "");
    }

    #[tokio::test]
    async fn doctor_reports_missing_permission_with_advice() {
        // Without the permission the event tap can't run, so capture is denied too.
        let (h, _fake) = crate::testutil::one_engine_with(|p| p.deny_capture(true)).await;
        let app = router(h, TOKEN.into());
        let j = body_json(get(&app, "/v1/doctor", Some(TOKEN), "localhost:7979").await).await;
        let c = j["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "input_monitoring")
            .unwrap();
        assert_eq!(c["ok"], false);
        assert!(c["advice"].as_str().unwrap().contains("Input Monitoring"));
    }

    #[tokio::test]
    async fn doctor_trusts_running_capture_over_a_stale_macos_permission_answer() {
        // macOS keeps answering "not granted" in a process that started before the grant,
        // but the event tap only runs once both are granted.
        let (app, _h, fake) = app().await;
        fake.set_permissions(vec![("accessibility", false), ("input_monitoring", false)]);
        let j = body_json(get(&app, "/v1/doctor", Some(TOKEN), HOST).await).await;
        let checks = j["checks"].as_array().unwrap();
        let capture = checks.iter().find(|c| c["name"] == "capture").unwrap();
        assert_eq!(capture["ok"], true, "{j}");
        for name in ["accessibility", "input_monitoring"] {
            let c = checks.iter().find(|c| c["name"] == name).unwrap();
            assert_eq!(c["ok"], true, "{c}");
            assert_eq!(c["advice"], "", "{c}");
        }
    }

    #[tokio::test]
    async fn doctor_flags_no_peers_and_passes_granted_permission() {
        let (app, _h, fake) = app().await;
        fake.set_permissions(vec![("accessibility", true)]);
        let j = body_json(get(&app, "/v1/doctor", Some(TOKEN), HOST).await).await;
        let checks = j["checks"].as_array().unwrap();
        let acc = checks
            .iter()
            .find(|c| c["name"] == "accessibility")
            .unwrap();
        assert_eq!(acc["ok"], true);
        let peers = checks
            .iter()
            .find(|c| c["name"] == "peer_connected")
            .unwrap();
        assert_eq!(peers["ok"], false);
        assert!(!peers["advice"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn doctor_with_connected_peer_reports_path_and_clock() {
        let (a, _pa, _b, _pb) = two_engines().await;
        let app = router(a, TOKEN.into());
        let j = body_json(get(&app, "/v1/doctor", Some(TOKEN), HOST).await).await;
        let checks = j["checks"].as_array().unwrap();
        let by = |n: &str| checks.iter().find(|c| c["name"] == n).unwrap();
        assert_eq!(by("peer_connected")["ok"], true);
        assert_eq!(by("path:b")["ok"], true);
        assert_eq!(by("clock_offset")["ok"], true);
        assert!(checks.iter().any(|c| c["name"] == "reconnects"));
        assert!(checks.iter().any(|c| c["name"] == "safety_releases"));
    }

    fn metrics(rec: u64, why: Option<&str>, rel: u64) -> MetricsReport {
        MetricsReport {
            per_peer: vec![],
            safety_releases: 99,
            reconnects: 99,
            reconnects_last_hour: rec,
            last_reconnect_reason: why.map(Into::into),
            safety_releases_last_day: rel,
        }
    }

    #[test]
    fn history_checks_use_windowed_counts_not_lifetime() {
        let c = history_checks(&metrics(0, None, 0));
        assert!(c.iter().all(|c| c.ok), "lifetime totals must not matter");
        let c = history_checks(&metrics(3, Some("HeartbeatTimeout"), 1));
        assert!(c.iter().all(|c| !c.ok && !c.advice.is_empty()));
        assert!(c[0].detail.contains("HeartbeatTimeout"));
    }

    #[tokio::test]
    async fn hello_proof_verifies_only_with_the_right_token() {
        let (app, ..) = app().await;
        let nonce = new_nonce();
        let path = format!("/v1/hello?nonce={}", encode_hex(&nonce));
        // No bearer token needed, but the Host check still applies.
        let r = get(&app, &path, None, HOST).await;
        assert_eq!(r.status(), 200);
        let proof = body_json(r).await["proof"].as_str().unwrap().to_string();
        assert!(verify_proof(TOKEN, &nonce, &proof));
        assert!(!verify_proof("other-token", &nonce, &proof));
        assert!(!verify_proof(TOKEN, &new_nonce(), &proof));
        assert_eq!(
            get(&app, &path, None, "evil.example:7979").await.status(),
            421
        );
        assert_eq!(
            get(&app, "/v1/hello?nonce=zz", None, HOST).await.status(),
            400
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn proven_agent_is_recognised_only_with_the_right_token() {
        let (h, _fake) = one_engine().await;
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, router(h, TOKEN.into())).await });
        assert!(proven_agent_at(addr, TOKEN).await);
        assert!(!proven_agent_at(addr, "other-token").await);
        // Something that isn't NovaLink.
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let other = l.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            while let Ok((mut s, _)) = l.accept().await {
                let _ = s
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 14\r\n\r\n{\"proof\":\"00\"}")
                    .await;
            }
        });
        assert!(!proven_agent_at(other, TOKEN).await);
        // Nothing listening.
        let free: std::net::SocketAddr = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        assert!(!proven_agent_at(free, TOKEN).await);
    }

    #[test]
    fn ambiguous_names_are_rejected_and_id_prefixes_work() {
        let (a, b) = (DeviceId([1; 32]), DeviceId([2; 32]));
        let dup = vec![(a, "Mac".to_string()), (b, "mac".to_string())];
        let e = resolve("MAC", &dup).err().unwrap();
        assert_eq!(e.0, StatusCode::BAD_REQUEST);
        assert_eq!(e.1, "ambiguous name, use the id");
        assert_eq!(resolve(&a.to_string(), &dup).ok(), Some(a));
        assert_eq!(resolve(&a.to_string()[..8], &dup).ok(), Some(a));
        assert_eq!(
            resolve("nobody", &dup).err().unwrap().0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn answer_requires_a_pending_request() {
        let (app, ..) = app().await;
        let id = DeviceId([9; 32]).to_string();
        let r = call(
            &app,
            "POST",
            "/v1/pair/answer",
            Some(TOKEN),
            HOST,
            Some(json!({"id": id, "accept": true})),
        )
        .await;
        assert_eq!(r.status(), 404);
    }

    #[tokio::test]
    async fn metrics_events_discovered_are_json() {
        let (app, ..) = app().await;
        let m = body_json(get(&app, "/v1/metrics", Some(TOKEN), HOST).await).await;
        assert!(m["safety_releases"].is_number());
        let e = body_json(get(&app, "/v1/events?limit=5", Some(TOKEN), HOST).await).await;
        assert!(e.is_array() && e.as_array().unwrap().len() <= 5);
        let d = body_json(get(&app, "/v1/discovered", Some(TOKEN), HOST).await).await;
        assert_eq!(d, json!([]));
    }

    #[tokio::test]
    async fn pairing_over_the_api_then_unpair() {
        let (ha, _pa) = one_engine().await;
        let (hb, _pb) = one_engine().await;
        let (ia, ib) = (ha.status().me, hb.status().me);
        ha.manager().hint_peer(Discovered {
            id: ib,
            name: "bee".into(),
            addrs: vec![hb.manager().local_addr()],
        });
        let (app_a, app_b) = (
            router(ha.clone(), TOKEN.into()),
            router(hb.clone(), TOKEN.into()),
        );
        let d = body_json(get(&app_a, "/v1/discovered", Some(TOKEN), HOST).await).await;
        assert_eq!(d[0]["name"], "bee");

        let r = call(
            &app_a,
            "POST",
            "/v1/pair",
            Some(TOKEN),
            HOST,
            Some(json!({"name":"bee"})),
        )
        .await;
        assert_eq!(r.status(), 200);
        wait_until(|| !ha.pending_pairs().is_empty() && !hb.pending_pairs().is_empty()).await;
        let pa = body_json(get(&app_a, "/v1/pair/pending", Some(TOKEN), HOST).await).await;
        let pb = body_json(get(&app_b, "/v1/pair/pending", Some(TOKEN), HOST).await).await;
        assert_eq!(pa[0]["peer"], ib.to_string());
        assert_eq!(pa[0]["code"], pb[0]["code"]);
        assert!(pa[0]["name"].is_string());

        let ans = |app: &Router, id: String| {
            let app = app.clone();
            async move {
                call(
                    &app,
                    "POST",
                    "/v1/pair/answer",
                    Some(TOKEN),
                    HOST,
                    Some(json!({"id": id, "accept": true})),
                )
                .await
            }
        };
        assert_eq!(ans(&app_a, ib.to_string()).await.status(), 200);
        assert_eq!(ans(&app_b, ia.to_string()).await.status(), 200);
        wait_until(|| ha.status().peers.len() == 1 && hb.status().peers.len() == 1).await;
        assert!(ha.pending_pairs().is_empty());

        let r = call(
            &app_a,
            "DELETE",
            &format!("/v1/devices/{ib}"),
            Some(TOKEN),
            HOST,
            None,
        )
        .await;
        assert_eq!(r.status(), 200);
        assert!(ha.status().peers.is_empty());
        let r = call(
            &app_a,
            "DELETE",
            &format!("/v1/devices/{ib}"),
            Some(TOKEN),
            HOST,
            None,
        )
        .await;
        assert_eq!(r.status(), 404);
    }

    #[tokio::test]
    async fn settings_put_changes_only_what_it_names_and_shows_in_status() {
        let (a, _pa) = one_engine().await;
        let app = router(a.clone(), TOKEN.into());
        let put = |body| call(&app, "PUT", "/v1/settings", Some(TOKEN), HOST, Some(body));
        let r = put(json!({"swap_ctrl_cmd": true})).await;
        assert_eq!(r.status(), 200);
        assert!(a.status().settings.swap_ctrl_cmd);
        assert_eq!(put(json!({})).await.status(), 200);
        assert!(a.status().settings.swap_ctrl_cmd);
        let r = call(&app, "GET", "/v1/status", Some(TOKEN), HOST, None).await;
        let v: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap())
                .unwrap();
        assert_eq!(v["settings"]["swap_ctrl_cmd"], true);
        assert_eq!(put(json!({"swap_ctrl_cmd": "yes"})).await.status(), 422);
    }

    #[tokio::test]
    async fn layout_put_resolves_peer_by_name() {
        let (a, _pa, b, _pb) = two_engines().await;
        let app = router(a.clone(), TOKEN.into());
        let r = call(
            &app,
            "PUT",
            "/v1/layout",
            Some(TOKEN),
            HOST,
            Some(json!({"edge":"left","peer":"b"})),
        )
        .await;
        assert_eq!(r.status(), 200);
        let nb = a
            .status()
            .layout
            .neighbor(a.status().me, novalink_proto::Edge::Left);
        assert_eq!(nb, Some(b.status().me));
        let r = call(
            &app,
            "PUT",
            "/v1/layout",
            Some(TOKEN),
            HOST,
            Some(json!({"edge":"sideways","peer":"b"})),
        )
        .await;
        assert_eq!(r.status(), 400);
        let r = call(
            &app,
            "PUT",
            "/v1/layout",
            Some(TOKEN),
            HOST,
            Some(json!({"edge":"left","peer":"zzz"})),
        )
        .await;
        assert_eq!(r.status(), 404);
    }

    #[tokio::test]
    async fn soak_requires_auth_and_a_connected_peer_then_reports() {
        let (a, _pa, _b, _pb) = crate::testutil::two_engines_dry(true).await;
        let app = router(a.clone(), TOKEN.into());
        let body = json!({"peer":"b","hours":0.0003});
        let r = call(&app, "POST", "/v1/soak", None, HOST, Some(body.clone())).await;
        assert_eq!(r.status(), 401);
        let r = get(&app, "/v1/soak", None, HOST).await;
        assert_eq!(r.status(), 401);
        let r = get(&app, "/v1/soak", Some(TOKEN), HOST).await;
        assert_eq!(body_json(r).await["state"], "idle");
        for bad in [
            json!({"peer":"b","hours":0}),
            json!({"peer":"b","hours":1e9}),
        ] {
            let r = call(&app, "POST", "/v1/soak", Some(TOKEN), HOST, Some(bad)).await;
            assert_eq!(r.status(), 400);
        }
        let r = call(
            &app,
            "POST",
            "/v1/soak",
            Some(TOKEN),
            HOST,
            Some(json!({"peer":"zzz","hours":1})),
        )
        .await;
        assert_eq!(r.status(), 404);
        let r = call(
            &app,
            "POST",
            "/v1/soak",
            Some(TOKEN),
            HOST,
            Some(body.clone()),
        )
        .await;
        assert_eq!(r.status(), 200);
        let r = call(&app, "POST", "/v1/soak", Some(TOKEN), HOST, Some(body)).await;
        assert_eq!(r.status(), 409, "only one soak at a time");
        let r = get(&app, "/v1/soak", Some(TOKEN), HOST).await;
        assert_eq!(body_json(r).await["state"], "running");
        let mut done = Value::Null;
        for _ in 0..100 {
            let r = get(&app, "/v1/soak", Some(TOKEN), HOST).await;
            let v = body_json(r).await;
            if v["state"] == "done" {
                done = v;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(done["state"], "done", "{done}");
        assert_eq!(done["report"]["stuck_after_release"], 0);
        assert!(done["report"]["duration"].is_number());
    }

    #[tokio::test]
    async fn soak_refuses_a_live_peer_unless_allowed_and_can_be_stopped() {
        let (a, _pa, _b, _pb) = two_engines().await;
        let app = router(a.clone(), TOKEN.into());
        let r = call(
            &app,
            "POST",
            "/v1/soak",
            Some(TOKEN),
            HOST,
            Some(json!({"peer":"b","hours":1})),
        )
        .await;
        assert_eq!(r.status(), 409);
        let v = body_json(r).await;
        assert_eq!(
            v["error"],
            "peer is not in --dry-inject; refusing to type into it"
        );
        let r = get(&app, "/v1/status", Some(TOKEN), HOST).await;
        assert_eq!(body_json(r).await["peers"][0]["dry_inject"], false);
        let r = call(&app, "DELETE", "/v1/soak", Some(TOKEN), HOST, None).await;
        assert_eq!(r.status(), 404, "nothing to stop yet");
        let body = json!({"peer":"b","hours":1,"allow_live":true});
        let r = call(&app, "POST", "/v1/soak", Some(TOKEN), HOST, Some(body)).await;
        assert_eq!(r.status(), 200);
        let r = call(&app, "DELETE", "/v1/soak", None, HOST, None).await;
        assert_eq!(r.status(), 401);
        let r = call(&app, "DELETE", "/v1/soak", Some(TOKEN), HOST, None).await;
        assert_eq!(r.status(), 200);
        let mut state = Value::Null;
        for _ in 0..100 {
            let r = get(&app, "/v1/soak", Some(TOKEN), HOST).await;
            state = body_json(r).await;
            if state["state"] != "running" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(state["state"], "stopped", "{state}");
        assert_eq!(state["report"]["stuck_after_release"], 0);
        assert_eq!(state["report"]["unanswered_leaves"], 0);
    }

    #[test]
    fn token_is_created_private_then_reused() {
        let dir = tempfile::tempdir().unwrap();
        let t1 = load_or_create_token(dir.path()).unwrap();
        assert_eq!(t1.len(), 64);
        assert!(t1.bytes().all(|b| b.is_ascii_hexdigit()));
        let t2 = load_or_create_token(dir.path()).unwrap();
        assert_eq!(t1, t2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = std::fs::metadata(dir.path().join("api-token")).unwrap();
            assert_eq!(m.permissions().mode() & 0o777, 0o600);
        }
    }
}
