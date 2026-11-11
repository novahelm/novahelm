//! Local HTTP API and the panel pages. Bound to 127.0.0.1; every `/api` call needs the
//! token (bearer header, or `?token=` for EventSource), and the Host header must name
//! this machine so a web page can't reach the API through DNS rebinding.

use crate::Agent;
use crate::clients::{self, Caller};
use crate::feeds::{FeedState, FeedValue};
use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Extension, Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use novagear_core::config::Config;
use novagear_core::proof::{agent_proof, same, valid_nonce};
use novagear_core::{API_PORT, Action};
use serde::Deserialize;
use serde_json::json;
use std::convert::Infallible;
use std::sync::Arc;

const PANEL: &str = include_str!("../panel/index.html");
const PROMPTER: &str = include_str!("../panel/prompter.html");
// The NovaGear app icon (brand/icons/build-icons.sh writes both).
const FAVICON: &str = include_str!("../panel/favicon.svg");
const TOUCH_ICON: &[u8] = include_bytes!("../panel/apple-touch-icon.png");

type AppState = Arc<Agent>;

pub fn router(agent: Arc<Agent>) -> Router {
    let api = Router::new()
        .route("/devices", get(devices))
        .route("/status", get(status))
        .route("/action", post(action))
        .route("/config", get(get_config).put(put_config))
        .route("/prompter", get(prompter))
        .route("/lights/{id}/identify", post(identify))
        .route("/events", get(events))
        .route("/logs", get(logs))
        .route("/panel-code", post(panel_code))
        .route("/deck", get(deck))
        .route("/icons/{name}", get(icon))
        .route("/images/{name}", get(image))
        .route("/import/streamdeck", post(import_streamdeck))
        // Feeds are capped at 4 KB; the cap applies to every caller.
        .route(
            "/feeds/{key}",
            put(put_feed)
                .delete(delete_feed)
                .layer(DefaultBodyLimit::max(FEED_BODY_MAX)),
        )
        .route("/feeds", get(list_feeds))
        // 8 KB for a request. There is no approve route, by design: the deck's Approve hold
        // is the only thing that says yes (`approvals.rs`).
        .route(
            "/approvals",
            post(create_approval)
                .get(list_approvals)
                .layer(DefaultBodyLimit::max(APPROVAL_BODY_MAX)),
        )
        .route("/approvals/{id}", get(get_approval).delete(cancel_approval))
        .route("/approvals/{id}/deny", post(deny_approval))
        // Templates and image uploads are for the panel and the app, never for clients.
        .route("/templates", get(list_templates))
        .route("/templates/{name}/apply", post(apply_template))
        .route(
            "/images",
            put(put_image).layer(DefaultBodyLimit::max(crate::templates::IMAGE_MAX_BYTES)),
        )
        // What the template dialog and the OBS action editors pick from.
        .route("/obs/scenes", get(obs_scenes))
        .route("/obs/inputs", get(obs_inputs))
        .route("/clients", get(list_clients).post(add_client))
        .route("/clients/{name}", delete(remove_client))
        .route("/clients/me/events", get(client_events))
        .layer(middleware::from_fn_with_state(agent.clone(), require_token))
        // Added after the token layer: clients call these before they hold a token.
        .route("/hello", get(hello))
        .route("/panel-session", post(panel_session));
    Router::new()
        .route("/", get(|| async { Html(PANEL) }))
        .route("/prompter", get(|| async { Html(PROMPTER) }))
        .route(
            "/favicon.svg",
            get(|| async { ([(header::CONTENT_TYPE, "image/svg+xml")], FAVICON) }),
        )
        .route(
            "/apple-touch-icon.png",
            get(|| async { ([(header::CONTENT_TYPE, "image/png")], TOUCH_ICON) }),
        )
        .nest("/api", api)
        .layer(middleware::from_fn(check_host))
        .with_state(agent)
}

fn host_allowed(host: &str) -> bool {
    let local = [
        format!("127.0.0.1:{API_PORT}"),
        format!("localhost:{API_PORT}"),
    ];
    local.iter().any(|h| h == host)
}

async fn check_host(req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if !host_allowed(host) {
        return (StatusCode::FORBIDDEN, "wrong host").into_response();
    }
    next.run(req).await
}

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

/// Decides who is calling, once. Clients are held to the `client_may` allowlist here, before
/// any handler (or body) runs; the handlers that are staff-only check the `Caller` again.
async fn require_token(
    State(agent): State<AppState>,
    Query(q): Query<TokenQuery>,
    mut req: Request,
    next: Next,
) -> Response {
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "));
    let given = bearer.or(q.token.as_deref()).unwrap_or("");
    let caller = if same(given, &agent.token) {
        Caller::Agent
    } else if crate::lock(&agent.panel).is_session(given) {
        Caller::Panel
    } else if let Some(name) = crate::lock(&agent.clients).who(given) {
        Caller::Client(name)
    } else {
        return unauthorized();
    };
    if matches!(caller, Caller::Client(_))
        && !clients::client_may(req.method().as_str(), req.uri().path())
    {
        return (StatusCode::FORBIDDEN, "this token can't use that route").into_response();
    }
    req.extensions_mut().insert(caller);
    next.run(req).await
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "missing or wrong token").into_response()
}

fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, "not allowed for this token").into_response()
}

/// The kind of an error, never its text: that could hold user input, paths or secrets.
pub(crate) fn error_kind(e: &anyhow::Error) -> String {
    e.chain()
        .find_map(|c| c.downcast_ref::<std::io::Error>())
        .map_or_else(|| "other".into(), |io| format!("{:?}", io.kind()))
}

/// `err` that first logs a warning with a fixed label and the error kind only.
fn err_logged(label: &str, e: anyhow::Error) -> Response {
    tracing::warn!(kind = %error_kind(&e), "{label}");
    err(e)
}

fn err(e: anyhow::Error) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": format!("{e:#}") })),
    )
        .into_response()
}

#[derive(Deserialize)]
struct HelloQuery {
    nonce: String,
}

/// Proves this is the real agent: an HMAC of the caller's nonce keyed by the token.
async fn hello(State(a): State<AppState>, Query(q): Query<HelloQuery>) -> Response {
    if !valid_nonce(&q.nonce) {
        return (StatusCode::BAD_REQUEST, "bad nonce").into_response();
    }
    Json(json!({ "proof": agent_proof(&a.token, &q.nonce) })).into_response()
}

/// A key icon. Only a plain file name inside the icons folder: no paths, no `..`.
async fn icon(State(a): State<AppState>, Path(name): Path<String>) -> Response {
    let plain = !name.is_empty()
        && name.ends_with(".png")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.contains("..");
    if !plain {
        return (StatusCode::BAD_REQUEST, "bad icon name").into_response();
    }
    let path = a.icons_dir().join(&name);
    match tokio::task::spawn_blocking(move || std::fs::read(path)).await {
        Ok(Ok(bytes)) => ([(header::CONTENT_TYPE, "image/png")], bytes).into_response(),
        _ => (StatusCode::NOT_FOUND, "no such icon").into_response(),
    }
}

/// An image the deck can draw, for the panel's key previews: the same lookup the deck
/// uses (user images, legacy icons, then the bundled set). Panel and app only; the panel
/// fetches it with its Authorization header and shows it from a blob URL.
async fn image(State(a): State<AppState>, Path(name): Path<String>) -> Response {
    // Plain names only: no control characters, no backslash, no `..`. A bundled name has
    // one slash (`lucide/hand.svg`); `FsImages::raw` never reads the disk for it.
    if name.is_empty()
        || name.len() > 128
        || name.chars().any(|c| c.is_control() || c == '\\')
        || name.contains("..")
        || name.starts_with('/')
    {
        return (StatusCode::BAD_REQUEST, "bad image name").into_response();
    }
    let lookup = a.clone();
    let found = tokio::task::spawn_blocking(move || lookup.image_bytes(&name)).await;
    let Ok(Some(bytes)) = found else {
        return (StatusCode::NOT_FOUND, "no such image").into_response();
    };
    let kind = image_type(&bytes);
    (
        [
            (header::CONTENT_TYPE, kind),
            // The browser must trust the type given, never guess a scriptable one.
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            // Opened on its own (a pasted link), an SVG gets no scripts and no origin.
            (
                header::CONTENT_SECURITY_POLICY,
                "sandbox; default-src 'none'; style-src 'unsafe-inline'",
            ),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        bytes.into_owned(),
    )
        .into_response()
}

/// A content type from the first bytes, for the formats the deck reads.
fn image_type(b: &[u8]) -> &'static str {
    match image::guess_format(b) {
        Ok(image::ImageFormat::Png) => "image/png",
        Ok(image::ImageFormat::Jpeg) => "image/jpeg",
        Ok(image::ImageFormat::Gif) => "image/gif",
        _ if b.trim_ascii_start().starts_with(b"<") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

async fn status(State(a): State<AppState>, Extension(caller): Extension<Caller>) -> Response {
    // A client sees that the agent is up and its version; problems can carry file names
    // and the modes are the user's own naming.
    if !caller.is_staff() {
        return Json(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "network_blocked": a.network_blocked(),
        }))
        .into_response();
    }
    let mut v = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "network_blocked": a.network_blocked(),
        "problems": a.problems(),
        "firmware_updates": a.firmware_updates(),
    });
    // The count and the oldest request, for the tray and the panel. Staff only: a title
    // can hold what a program is about to do.
    v["approvals"] = a.approvals_status();
    // `mode` and `modes`.
    if let (Some(v), serde_json::Value::Object(m)) = (v.as_object_mut(), a.mode_status()) {
        v.extend(m);
    }
    Json(v).into_response()
}

async fn deck(State(a): State<AppState>) -> impl IntoResponse {
    Json(json!({ "page": a.current_page() }))
}

/// Imports the Stream Deck app's profiles (blocking file work, so off the async thread).
async fn import_streamdeck(State(a): State<AppState>) -> Response {
    match tokio::task::spawn_blocking(move || a.import_streamdeck(None)).await {
        Ok(Ok(summary)) => Json(summary).into_response(),
        Ok(Err(e)) => err_logged("profile import failed", e),
        Err(e) => err_logged("profile import failed", e.into()),
    }
}

/// A single-use, one-minute code for the panel URL, so the token itself never reaches the browser.
async fn panel_code(State(a): State<AppState>) -> Response {
    match crate::lock(&a.panel).issue_code(std::time::Instant::now()) {
        Ok(code) => Json(json!({ "code": code })).into_response(),
        Err(e) => err(e),
    }
}

#[derive(Deserialize)]
struct SessionRequest {
    code: String,
}

async fn panel_session(State(a): State<AppState>, Json(r): Json<SessionRequest>) -> Response {
    match crate::lock(&a.panel).redeem(&r.code, std::time::Instant::now()) {
        Some(token) => Json(json!({ "token": token })).into_response(),
        None => (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "this link has expired; open the panel again" })),
        )
            .into_response(),
    }
}

async fn devices(State(a): State<AppState>) -> impl IntoResponse {
    Json(a.devices())
}

async fn action(State(a): State<AppState>, Json(act): Json<Action>) -> Response {
    match a.run(act).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => err(e),
    }
}

async fn get_config(State(a): State<AppState>) -> impl IntoResponse {
    // Serialized from the shared config, not a copy of it.
    let cfg = a.config_arc();
    Json(&*cfg).into_response()
}

async fn put_config(State(a): State<AppState>, Json(cfg): Json<Config>) -> Response {
    match a.set_config(cfg) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => err_logged("config save failed", e),
    }
}

async fn prompter(State(a): State<AppState>) -> impl IntoResponse {
    Json(json!({ "run": a.prompter(), "config": a.config_arc().prompter }))
}

async fn identify(State(a): State<AppState>, Path(id): Path<String>) -> Response {
    match a.identify_light(&id).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => err_logged("identify failed", e),
    }
}

/// The agent's events, plus warnings and errors from the log as `{"type":"log"}` events.
async fn events(State(a): State<AppState>) -> impl IntoResponse {
    use tokio::sync::broadcast::error::RecvError;
    // The live log feed is optional: if it ever closes, the agent's own events go on.
    let stream = futures_util::stream::unfold(
        (a.subscribe(), Some(crate::logfile::live())),
        |(mut rx, mut log)| async move {
            loop {
                let data = tokio::select! {
                    r = rx.recv() => match r {
                        Ok(s) => s,
                        // A slow panel missed some events; it refetches on the next one.
                        Err(RecvError::Lagged(_)) => continue,
                        Err(RecvError::Closed) => return None,
                    },
                    r = async { log.as_mut().expect("guarded").recv().await }, if log.is_some() => match r {
                        Ok(line) => match serde_json::from_str::<serde_json::Value>(&line) {
                            Ok(v) => json!({ "type": "log", "data": v }).to_string(),
                            Err(_) => continue,
                        },
                        Err(RecvError::Lagged(_)) => continue,
                        Err(RecvError::Closed) => {
                            log = None;
                            continue;
                        }
                    },
                };
                return Some((Ok::<_, Infallible>(Event::default().data(data)), (rx, log)));
            }
        },
    );
    Sse::new(stream).keep_alive(KeepAlive::default())
}

const FEED_BODY_MAX: usize = 4096;
const FEED_TTL_DEFAULT_S: u64 = 120;
const FEED_TTL_MAX_S: u64 = 86_400;

/// Letters, digits and `. _ -`, so a key can't smuggle anything into a log or a path.
fn valid_feed_key(key: &str) -> bool {
    (1..=64).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutFeed {
    value: FeedValue,
    unit: Option<String>,
    state: Option<FeedState>,
    ttl_s: Option<u64>,
}

fn too_many(why: &'static str) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, "1")],
        why,
    )
        .into_response()
}

/// The checks every feed write shares: a sane key, the caller's prefix, the rate. `None`
/// means go ahead. Staff may write any key and aren't rate limited.
fn feed_gate(a: &Agent, caller: &Caller, key: &str) -> Option<Response> {
    if let Caller::Client(c) = caller {
        let prefix_ok = key
            .strip_prefix(c.as_str())
            .is_some_and(|r| r.len() > 1 && r.starts_with('.'));
        if !prefix_ok {
            return Some(forbidden());
        }
    }
    if !valid_feed_key(key) {
        return Some((StatusCode::BAD_REQUEST, "bad feed key").into_response());
    }
    if let Caller::Client(c) = caller
        && !crate::lock(&a.limits).allow_feed(c)
    {
        return Some(too_many("too many feed updates"));
    }
    None
}

/// Sets a feed. The value is never logged.
async fn put_feed(
    State(a): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(key): Path<String>,
    body: Bytes,
) -> Response {
    if let Some(r) = feed_gate(&a, &caller, &key) {
        return r;
    }
    let Ok(f) = serde_json::from_slice::<PutFeed>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            "body is {value, unit?, state?, ttl_s?}",
        )
            .into_response();
    };
    let ttl = f.ttl_s.unwrap_or(FEED_TTL_DEFAULT_S);
    if !(1..=FEED_TTL_MAX_S).contains(&ttl)
        || f.unit.as_ref().is_some_and(|u| u.chars().count() > 16)
    {
        return (
            StatusCode::BAD_REQUEST,
            "ttl_s is 1-86400; unit at most 16 characters",
        )
            .into_response();
    }
    let ttl = std::time::Duration::from_secs(ttl);
    match &caller {
        // Each client holds at most 200 keys; updating one it already has is always fine.
        Caller::Client(c) => match a.client_put_feed(c, &key, f.value, f.unit, f.state, ttl) {
            Ok(()) => {}
            Err(crate::ClientPutError::Gone) => return unauthorized(),
            Err(crate::ClientPutError::Full) => {
                return too_many("too many feed keys; delete one first");
            }
        },
        _ => a.feeds.set(&key, f.value, f.unit, f.state, ttl),
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn delete_feed(
    State(a): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(key): Path<String>,
) -> Response {
    if let Some(r) = feed_gate(&a, &caller, &key) {
        return r;
    }
    a.feeds.clear(&key);
    StatusCode::NO_CONTENT.into_response()
}

/// Every feed, for the panel and the CLI. Clients can't read it: it shows other clients' values.
async fn list_feeds(State(a): State<AppState>, Extension(caller): Extension<Caller>) -> Response {
    if !caller.is_staff() {
        return forbidden();
    }
    let list: Vec<_> = a
        .feeds
        .list_with_age()
        .into_iter()
        .map(|(key, s, age)| {
            json!({
                "key": key,
                "value": s.value,
                "unit": s.unit,
                "state": s.state,
                "stale": s.stale,
                "age_s": age.as_secs(),
            })
        })
        .collect();
    Json(list).into_response()
}

const APPROVAL_BODY_MAX: usize = 8192;

/// A request for a yes or no. The reply is `{id}`; the answer comes by long poll.
async fn create_approval(
    State(a): State<AppState>,
    Extension(caller): Extension<Caller>,
    body: Bytes,
) -> Response {
    use crate::approvals::{CreateError, NewApproval};
    let Ok(req) = serde_json::from_slice::<NewApproval>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            "body is {title, lines?, source?, risk?, details_url?, expires_in_s?}",
        )
            .into_response();
    };
    match a.create_approval(&caller, req) {
        Ok(id) => (StatusCode::CREATED, Json(json!({ "id": id }))).into_response(),
        // Fixed words, never the request's own text.
        Err(CreateError::Invalid(why)) => (StatusCode::BAD_REQUEST, why).into_response(),
        Err(CreateError::Rate) => too_many("one approval request every 5 seconds"),
        Err(CreateError::TooMany) => too_many("too many requests waiting"),
        Err(CreateError::Gone) => unauthorized(),
        Err(CreateError::Internal) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "no randomness").into_response()
        }
    }
}

#[derive(Deserialize)]
struct WaitQuery {
    wait: Option<u64>,
}

fn lookup_error(e: crate::approvals::LookupError) -> Response {
    match e {
        crate::approvals::LookupError::NotFound => {
            (StatusCode::NOT_FOUND, "no such approval").into_response()
        }
        crate::approvals::LookupError::Forbidden => forbidden(),
    }
}

/// `{status}` of the caller's own request; `?wait=0..60` holds the call until it is decided.
async fn get_approval(
    State(a): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Query(q): Query<WaitQuery>,
) -> Response {
    let wait = std::time::Duration::from_secs(q.wait.unwrap_or(0).min(60));
    match a
        .approval_status(&crate::approvals::owner_name(&caller), &id, wait)
        .await
    {
        Ok(status) => Json(json!({ "status": status })).into_response(),
        Err(e) => lookup_error(e),
    }
}

/// The asker takes its own request back.
async fn cancel_approval(
    State(a): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> Response {
    match a.cancel_approval(&crate::approvals::owner_name(&caller), &id) {
        Ok(o) => Json(json!({ "status": o.as_str() })).into_response(),
        Err(e) => lookup_error(e),
    }
}

/// Every waiting request with its text, for the panel and the popover. Clients can't read
/// it: it shows other programs' requests.
async fn list_approvals(
    State(a): State<AppState>,
    Extension(caller): Extension<Caller>,
) -> Response {
    if !caller.is_staff() {
        return forbidden();
    }
    Json(a.pending_approvals()).into_response()
}

/// The panel and the popover can say no. They can't say yes.
async fn deny_approval(
    State(a): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> Response {
    if !caller.is_staff() {
        return forbidden();
    }
    match a.deny_approval(&id) {
        Ok(()) => Json(json!({ "status": "denied" })).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "nothing waiting with that id").into_response(),
    }
}

async fn list_templates(Extension(caller): Extension<Caller>) -> Response {
    if !caller.is_staff() {
        return forbidden();
    }
    let list: Vec<_> = crate::templates::builtin()
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "slug": crate::templates::slug(&t.name),
                "description": t.description,
                "modes": t.modes.iter().map(|m| &m.name).collect::<Vec<_>>(),
                "needs": crate::templates::needs(t),
            })
        })
        .collect();
    Json(list).into_response()
}

#[derive(Deserialize)]
struct ApplyQuery {
    #[serde(default)]
    replace_rules: bool,
}

/// Adds the template's modes next to the user's own. The rules change only on
/// `?replace_rules=true`.
async fn apply_template(
    State(a): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    Query(q): Query<ApplyQuery>,
) -> Response {
    if !caller.is_staff() {
        return forbidden();
    }
    let Some(t) = crate::templates::find(&name) else {
        return (StatusCode::NOT_FOUND, "no such template").into_response();
    };
    match tokio::task::spawn_blocking(move || a.apply_template(&t, q.replace_rules)).await {
        Ok(Ok(report)) => Json(report).into_response(),
        Ok(Err(e)) => err_logged("template apply failed", e),
        Err(e) => err_logged("template apply failed", e.into()),
    }
}

/// Raw image bytes in, `{name}` out. The name is the content's sha256 plus an extension.
pub(crate) async fn put_image(
    State(a): State<AppState>,
    Extension(caller): Extension<Caller>,
    body: Bytes,
) -> Response {
    if !caller.is_staff() {
        return forbidden();
    }
    match tokio::task::spawn_blocking(move || a.store_image(&body)).await {
        Ok(Ok(name)) => Json(json!({ "name": name })).into_response(),
        Ok(Err(e)) => err_logged("image upload failed", e),
        Err(e) => err_logged("image upload failed", e.into()),
    }
}

/// One OBS list call, reduced to the names under `field`. Failures log the kind only
/// (`failure_log`): OBS's own comment and the names themselves stay out of the log.
async fn obs_names(
    a: AppState,
    request_type: &'static str,
    list: &'static str,
    field: &'static str,
) -> anyhow::Result<Vec<String>> {
    let obs = a.config_arc().obs.clone();
    let v = tokio::task::spawn_blocking(move || crate::obs::request(&obs, request_type, json!({})))
        .await??;
    Ok(v[list]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i[field].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default())
}

/// The reply for a failed OBS list call: the kind goes to the log, OBS's own text to the panel.
fn obs_list_failed(e: anyhow::Error) -> Response {
    tracing::warn!("OBS list failed: {}", crate::failure_log(&e));
    err(e)
}

/// The scene names, in the order OBS shows them (its API lists the last one first).
async fn obs_scenes(State(a): State<AppState>, Extension(caller): Extension<Caller>) -> Response {
    if !caller.is_staff() {
        return forbidden();
    }
    match obs_names(a, "GetSceneList", "scenes", "sceneName").await {
        Ok(mut names) => {
            names.reverse();
            Json(names).into_response()
        }
        Err(e) => obs_list_failed(e),
    }
}

async fn obs_inputs(State(a): State<AppState>, Extension(caller): Extension<Caller>) -> Response {
    if !caller.is_staff() {
        return forbidden();
    }
    match obs_names(a, "GetInputList", "inputs", "inputName").await {
        Ok(names) => Json(names).into_response(),
        Err(e) => obs_list_failed(e),
    }
}

async fn list_clients(State(a): State<AppState>, Extension(caller): Extension<Caller>) -> Response {
    if !caller.is_staff() {
        return forbidden();
    }
    Json(crate::lock(&a.clients).list()).into_response()
}

#[derive(Deserialize)]
struct NewClient {
    name: String,
}

/// Makes a client. The token is in this response and nowhere else, ever.
async fn add_client(
    State(a): State<AppState>,
    Extension(caller): Extension<Caller>,
    Json(r): Json<NewClient>,
) -> Response {
    if !caller.is_staff() {
        return forbidden();
    }
    let added = crate::lock(&a.clients).add(&r.name);
    match added {
        Ok(token) => {
            tracing::info!(client = %r.name, "client added");
            (
                StatusCode::CREATED,
                Json(json!({ "name": r.name, "token": token })),
            )
                .into_response()
        }
        Err(e) => err_logged("client add failed", e),
    }
}

async fn remove_client(
    State(a): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
) -> Response {
    if !caller.is_staff() {
        return forbidden();
    }
    match a.remove_client(&name) {
        Ok(true) => {
            tracing::info!(client = %name, "client removed");
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => (StatusCode::NOT_FOUND, "no such client").into_response(),
        Err(e) => err_logged("client remove failed", e),
    }
}

/// The calling client's own named events, as `{name, value}`. Staff have no events here.
async fn client_events(
    State(a): State<AppState>,
    Extension(caller): Extension<Caller>,
) -> Response {
    use tokio::sync::broadcast::error::RecvError;
    let Caller::Client(name) = caller else {
        return forbidden();
    };
    // Subscribed before the response starts, so nothing sent after this returns is missed.
    let Some(rx) = a.client_subscribe(&name) else {
        return unauthorized();
    };
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(s) => return Some((Ok::<_, Infallible>(Event::default().data(s)), rx)),
                Err(RecvError::Lagged(_)) => continue,
                // The client was removed.
                Err(RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

#[derive(Deserialize)]
struct LogsQuery {
    level: Option<String>,
    q: Option<String>,
    limit: Option<usize>,
}

/// Recent log lines, newest first: `?level=warn&q=deck&limit=500`.
async fn logs(Query(q): Query<LogsQuery>) -> Response {
    let Some(dir) = crate::logfile::log_dir() else {
        return Json(Vec::<serde_json::Value>::new()).into_response();
    };
    let limit = q.limit.unwrap_or(500);
    match tokio::task::spawn_blocking(move || {
        crate::logfile::read(dir, q.level.as_deref(), q.q.as_deref(), limit)
    })
    .await
    {
        Ok(lines) => Json(lines).into_response(),
        Err(e) => err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    /// A fake obs-websocket that answers one request with `data`; returns the config for it.
    fn fake_obs(data: serde_json::Value) -> novagear_core::config::ObsConfig {
        use tungstenite::Message;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let cfg = novagear_core::config::ObsConfig {
            url: format!("ws://{}", listener.local_addr().unwrap()),
            password: String::new(),
        };
        std::thread::spawn(move || {
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
            let m: serde_json::Value = serde_json::from_str(t.as_str()).unwrap();
            ws.send(Message::text(
                json!({ "op": 7, "d": {
                    "requestType": m["d"]["requestType"],
                    "requestId": m["d"]["requestId"],
                    "requestStatus": { "result": true, "code": 100 },
                    "responseData": data,
                } })
                .to_string(),
            ))
            .unwrap();
        });
        cfg
    }

    async fn staff_json(agent: Arc<Agent>, scenes: bool) -> (StatusCode, serde_json::Value) {
        let r = if scenes {
            obs_scenes(State(agent), Extension(Caller::Agent)).await
        } else {
            obs_inputs(State(agent), Extension(Caller::Agent)).await
        };
        let code = r.status();
        let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (code, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn obs_lists_answer_names_and_scenes_come_in_display_order() {
        let dir = std::env::temp_dir().join(format!("novagear-obs-names-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let agent = Agent::new(dir.join("config.json"), "t".repeat(48)).unwrap();
        // OBS lists the last scene first.
        let mut cfg = agent.config();
        cfg.obs = fake_obs(json!({ "scenes": [
            { "sceneName": "Last", "sceneIndex": 0 }, { "sceneName": "First", "sceneIndex": 1 }
        ] }));
        agent.set_config(cfg).unwrap();
        let (code, v) = staff_json(agent.clone(), true).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v, json!(["First", "Last"]));

        let mut cfg = agent.config();
        cfg.obs = fake_obs(json!({ "inputs": [
            { "inputName": "Mic/Aux", "inputKind": "x" }, { "inputName": "Desktop", "inputKind": "y" }
        ] }));
        agent.set_config(cfg).unwrap();
        let (code, v) = staff_json(agent, false).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v, json!(["Mic/Aux", "Desktop"]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn panel_edits_the_start_modes_pages_not_top_level_pages() {
        assert!(
            !super::PANEL.contains("config.pages"),
            "saving config.pages would put top-level pages back next to the modes"
        );
        assert!(super::PANEL.contains("config.modes"));
    }

    #[test]
    fn panel_links_details_only_over_https_and_checks_approval_keys() {
        // The deck opens only https links; the panel must not offer more.
        assert!(!super::PANEL.contains("https?:"));
        assert!(super::PANEL.contains("const bad = badSlots(draft)"));
    }

    #[test]
    fn panel_takes_a_fresh_code_over_a_kept_session_and_stops_on_401() {
        let p = super::PANEL;
        // A code in the address is redeemed first, also when only the hash changed.
        assert!(p.contains("addEventListener('hashchange'"));
        assert!(p.contains("The code was used or has expired: do not fall back"));
        // A 401 ends the session: no more calls, the session is forgotten, the message shows.
        assert!(p.contains("if (r.status === 401) { sessionEnded()"));
        assert!(p.contains("sessionStorage.removeItem('novagear-session')"));
        // Nothing polls on a bare timer: the helper waits for each run and backs off.
        assert!(
            !p.contains("setInterval"),
            "use poll(): setInterval piles calls up"
        );
        assert!(p.contains("poll(loadFeeds, 3000"));
        assert!(p.contains("Math.min(delay * 2, 60000)"));
        // A burst of agent events is one reload, and a flapping stream does not reload each time.
        assert!(p.contains("coalesced(load, 1000)"));
        assert!(p.contains("Date.now() - lastOpen < 3000"));
    }

    #[test]
    fn panel_shows_the_ended_session_on_any_tab_and_handles_upload_401_and_cancelled_reloads() {
        let p = super::PANEL;
        assert!(p.contains("$('overview').hidden = false; $('modesView').hidden = true;"));
        assert!(p.contains("$('live').textContent = 'session ended'"));
        assert!(p.contains("Edits you had not saved"));
        assert!(p.contains("if (r.status === 401) { sessionEnded(); throw new Error('This session has ended.'); }\n        const b = await r.json()"));
        // The reload asks before it stores the new session, so a refusal leaves nothing half applied.
        let at = p.find("A new link was opened").unwrap();
        assert!(at < p.find("sessionStorage.setItem('novagear-session', t); sessionStorage.setItem('novagear-next'").unwrap());
        assert!(p.contains("URL.revokeObjectURL"));
    }

    #[test]
    fn panel_draws_what_the_deck_draws() {
        let p = super::PANEL;
        // Lists read as their length, and the dots panel draws its squares.
        assert!(p.contains("Array.isArray(f.value) ? String(f.value.length)"));
        assert!(p.contains("p.kind === 'dots'"));
        // A badge shows only while its feed is truthy, as the deck's does.
        assert!(p.contains("const badgeText"));
        assert!(p.contains("badgeText(f.badge)"));
        assert!(!p.contains("const b = liveText(f.badge)"));
    }

    #[test]
    fn panel_previews_use_the_header_and_blob_urls_not_a_token_in_a_url() {
        let p = super::PANEL;
        assert!(p.contains("'/api/images/' + encodeURIComponent(name)"));
        assert!(p.contains("URL.createObjectURL(new Blob("));
        // The only URL that still carries a token is the event stream, which cannot set headers.
        assert_eq!(
            p.matches("?token=").count(),
            1,
            "only EventSource may put a token in a URL"
        );
        assert!(!p.contains("iconUrl"));
    }

    use super::*;

    #[test]
    fn only_local_hosts_pass() {
        assert!(host_allowed("127.0.0.1:47820"));
        assert!(host_allowed("localhost:47820"));
        assert!(!host_allowed("evil.example:47820"));
        assert!(!host_allowed(""));
    }
}
