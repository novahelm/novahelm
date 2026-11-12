//! Client tokens end to end: a real router on an ephemeral port, plain HTTP over a socket.

use novagear_agent::clients::ClientLimits;
use novagear_agent::{Agent, api};
use novagear_core::{API_PORT, Action};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TOKEN: &str = "agent-token-0123456789012345678901234567890123";

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("novagear-clients-api-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn serve(tag: &str) -> (Arc<Agent>, SocketAddr) {
    let dir = temp_dir(tag);
    let agent = Agent::new(dir.join("config.json"), TOKEN.to_string()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = agent.clone();
    tokio::spawn(async move {
        axum::serve(listener, api::router(served)).await.unwrap();
    });
    (agent, addr)
}

fn request(method: &str, path: &str, token: &str, body: &str) -> String {
    format!(
        "{method} /api{path} HTTP/1.1\r\nHost: 127.0.0.1:{API_PORT}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: &str,
    body: &str,
) -> (u16, String) {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(request(method, path, token, body).as_bytes())
        .await
        .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    let code = out.split(' ').nth(1).unwrap().parse().unwrap();
    (code, out.split("\r\n\r\n").nth(1).unwrap_or("").to_string())
}

async fn add_client(addr: SocketAddr, name: &str) -> String {
    let (code, body) = http(
        addr,
        "POST",
        "/clients",
        TOKEN,
        &json!({ "name": name }).to_string(),
    )
    .await;
    assert_eq!(code, 201, "{body}");
    serde_json::from_str::<Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn push(addr: SocketAddr, token: &str, key: &str, value: Value) -> u16 {
    http(
        addr,
        "PUT",
        &format!("/feeds/{key}"),
        token,
        &json!({ "value": value }).to_string(),
    )
    .await
    .0
}

/// An open `/clients/me/events` stream, read until its first byte has arrived (so the
/// subscription exists).
struct Stream(tokio::net::TcpStream, String);

impl Stream {
    async fn open(addr: SocketAddr, token: &str) -> Stream {
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(request("GET", "/clients/me/events", token, "").as_bytes())
            .await
            .unwrap();
        let mut st = Stream(s, String::new());
        st.read_until("\r\n\r\n", Duration::from_secs(3))
            .await
            .unwrap();
        st
    }

    async fn read_until(&mut self, needle: &str, within: Duration) -> Option<String> {
        let deadline = tokio::time::Instant::now() + within;
        while !self.1.contains(needle) {
            let mut buf = [0u8; 1024];
            let n = tokio::time::timeout_at(deadline, self.0.read(&mut buf))
                .await
                .ok()?
                .ok()?;
            if n == 0 {
                return None;
            }
            self.1.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        Some(self.1.clone())
    }
}

#[tokio::test]
async fn client_token_cannot_reach_other_routes() {
    let (_agent, addr) = serve("routes").await;
    let t = add_client(addr, "novaguard").await;
    for (m, p, body) in [
        ("POST", "/action", r#"{"do":"light_toggle"}"#),
        ("GET", "/config", ""),
        ("PUT", "/config", "{}"),
        ("GET", "/logs", ""),
        ("GET", "/clients", ""),
        ("POST", "/clients", r#"{"name":"evil"}"#),
        ("DELETE", "/clients/novaguard", ""),
        ("GET", "/feeds", ""),
        ("GET", "/events", ""),
        ("GET", "/devices", ""),
        ("POST", "/panel-code", ""),
        ("POST", "/import/streamdeck", ""),
        ("GET", "/deck", ""),
    ] {
        let (code, _) = http(addr, m, p, &t, body).await;
        assert_eq!(code, 403, "{m} {p}");
    }
    let (code, _) = http(addr, "GET", "/status", &t, "").await;
    assert_eq!(code, 200);
    // The staff routes still answer staff.
    assert_eq!(http(addr, "GET", "/clients", TOKEN, "").await.0, 200);
    assert_eq!(http(addr, "GET", "/feeds", TOKEN, "").await.0, 200);
}

#[tokio::test]
async fn client_status_is_a_reduced_view() {
    let (_agent, addr) = serve("status").await;
    let t = add_client(addr, "novaguard").await;
    let (_, body) = http(addr, "GET", "/status", &t, "").await;
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
    for hidden in ["problems", "mode", "modes", "firmware_updates"] {
        assert!(v.get(hidden).is_none(), "{hidden} leaked to a client");
    }
    let (_, body) = http(addr, "GET", "/status", TOKEN, "").await;
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(v.get("problems").is_some() && v.get("modes").is_some());
}

#[tokio::test]
async fn client_can_only_push_its_own_prefix() {
    let (agent, addr) = serve("prefix").await;
    let t = add_client(addr, "novaguard").await;
    assert_eq!(
        push(addr, &t, "novaguard.builds.running", json!(3)).await,
        204
    );
    assert_eq!(
        agent.feeds.get("novaguard.builds.running").unwrap().value,
        novagear_agent::feeds::FeedValue::Number(3.0)
    );
    for key in [
        "obs.recording",
        "other.x",
        "novaguard",
        "novaguard.",
        "novaguardx.a",
        "novaguard-2.a",
    ] {
        assert_eq!(push(addr, &t, key, json!(1)).await, 403, "{key}");
        let (code, _) = http(addr, "DELETE", &format!("/feeds/{key}"), &t, "").await;
        assert_eq!(code, 403, "delete {key}");
    }
    assert!(agent.feeds.get("obs.recording").is_none());
    // Staff may set any key; a client can't clear it.
    assert_eq!(push(addr, TOKEN, "obs.recording", json!(1)).await, 204);
    assert_eq!(
        http(addr, "DELETE", "/feeds/obs.recording", &t, "").await.0,
        403
    );
    assert!(agent.feeds.get("obs.recording").is_some());
    // Own key: delete works.
    assert_eq!(
        http(addr, "DELETE", "/feeds/novaguard.builds.running", &t, "")
            .await
            .0,
        204
    );
    assert!(agent.feeds.get("novaguard.builds.running").is_none());
}

#[tokio::test]
async fn feed_bodies_are_checked() {
    let (agent, addr) = serve("body").await;
    let t = add_client(addr, "novaguard").await;
    let put = |body: String| {
        let t = t.clone();
        async move { http(addr, "PUT", "/feeds/novaguard.x", &t, &body).await.0 }
    };
    assert_eq!(
        put(json!({ "value": 1, "unit": "%", "state": "warn", "ttl_s": 30 }).to_string()).await,
        204
    );
    let f = agent.feeds.get("novaguard.x").unwrap();
    assert_eq!(f.unit.as_deref(), Some("%"));
    assert_eq!(f.state, Some(novagear_agent::feeds::FeedState::Warn));
    assert_eq!(put("not json".into()).await, 400);
    assert_eq!(
        put(json!({ "value": 1, "extra": 1 }).to_string()).await,
        400
    );
    assert_eq!(
        put(json!({ "value": 1, "ttl_s": 0 }).to_string()).await,
        400
    );
    assert_eq!(
        put(json!({ "value": 1, "ttl_s": 86401 }).to_string()).await,
        400
    );
    assert_eq!(
        put(json!({ "value": 1, "state": "purple" }).to_string()).await,
        400
    );
    assert_eq!(
        put(json!({ "value": [{ "label": "a", "state": "ok" }] }).to_string()).await,
        204
    );
    // A key with odd characters never gets in.
    assert_eq!(push(addr, &t, "novaguard.a%20b", json!(1)).await, 400);
}

#[tokio::test]
async fn feed_body_cap_and_rate_cap() {
    let (_agent, addr) = serve("rate").await;
    let t = add_client(addr, "novaguard").await;
    let big = json!({ "value": "x".repeat(5000) }).to_string();
    assert_eq!(
        http(addr, "PUT", "/feeds/novaguard.big", &t, &big).await.0,
        413
    );
    // Staff get the same cap.
    assert_eq!(
        http(addr, "PUT", "/feeds/novaguard.big", TOKEN, &big)
            .await
            .0,
        413
    );

    // Fired together so the bucket has no time to refill.
    let sends: Vec<_> = (0..15)
        .map(|i| {
            let t = t.clone();
            tokio::spawn(async move { push(addr, &t, &format!("novaguard.k{i}"), json!(i)).await })
        })
        .collect();
    let mut codes = Vec::new();
    for s in sends {
        codes.push(s.await.unwrap());
    }
    assert!(
        codes.iter().filter(|c| **c == 429).count() >= 5,
        "{codes:?}"
    );
    assert!(codes.iter().all(|c| *c == 204 || *c == 429), "{codes:?}");
    // The cap is per client, and staff are not limited.
    let other = add_client(addr, "other").await;
    assert_eq!(push(addr, &other, "other.a", json!(1)).await, 204);
    for i in 0..15 {
        assert_eq!(push(addr, TOKEN, &format!("obs.k{i}"), json!(i)).await, 204);
    }
}

#[tokio::test]
async fn feed_key_cap_per_client() {
    let (agent, addr) = serve("keycap").await;
    // The real rate (10/s) would make 200 pushes take 20 s.
    *agent.limits.lock().unwrap() = ClientLimits::new(100_000.0, Duration::from_secs(5));
    let t = add_client(addr, "novaguard").await;
    for i in 0..200 {
        assert_eq!(
            push(addr, &t, &format!("novaguard.k{i}"), json!(i)).await,
            204,
            "key {i}"
        );
    }
    assert_eq!(agent.feeds.count_with_prefix("novaguard."), 200);
    let over = push(addr, &t, "novaguard.k200", json!(1)).await;
    assert!(over == 429 || over == 403, "{over}");
    assert!(
        agent.feeds.get("novaguard.k200").is_none(),
        "memory stays bounded"
    );
    // Updating a key it already has is fine at the cap.
    assert_eq!(push(addr, &t, "novaguard.k7", json!(99)).await, 204);
    // Another client has its own 200.
    let other = add_client(addr, "other").await;
    assert_eq!(push(addr, &other, "other.a", json!(1)).await, 204);
    // After a delete there is room again.
    assert_eq!(
        http(addr, "DELETE", "/feeds/novaguard.k0", &t, "").await.0,
        204
    );
    assert_eq!(push(addr, &t, "novaguard.k200", json!(1)).await, 204);
}

#[tokio::test]
async fn client_event_reaches_only_its_client() {
    let (agent, addr) = serve("events").await;
    let ta = add_client(addr, "aa").await;
    let tb = add_client(addr, "bb").await;
    let mut a = Stream::open(addr, &ta).await;
    let mut b = Stream::open(addr, &tb).await;

    agent
        .run(Action::ClientEvent {
            client: "aa".into(),
            name: "deploy".into(),
            value: Some(json!({ "env": "prod" })),
        })
        .await
        .unwrap();
    let got = a
        .read_until("\n\n", Duration::from_secs(3))
        .await
        .expect("a gets it");
    let data = got.lines().find_map(|l| l.strip_prefix("data:")).unwrap();
    let v: Value = serde_json::from_str(data.trim()).unwrap();
    assert_eq!(v, json!({ "name": "deploy", "value": { "env": "prod" } }));
    assert!(
        b.read_until("deploy", Duration::from_millis(300))
            .await
            .is_none(),
        "b sees nothing"
    );
    // No value: still delivered, with null.
    agent
        .run(Action::ClientEvent {
            client: "bb".into(),
            name: "ping".into(),
            value: None,
        })
        .await
        .unwrap();
    assert!(b.read_until("ping", Duration::from_secs(3)).await.is_some());
    // The Pause all key's value reaches the client as a bare number, which NovaGuard reads strictly.
    agent
        .run(Action::ClientEvent {
            client: "aa".into(),
            name: "pause_all".into(),
            value: Some(json!(1)),
        })
        .await
        .unwrap();
    assert!(
        a.read_until(r#""name":"pause_all","value":1"#, Duration::from_secs(3))
            .await
            .is_some(),
        "a gets the press"
    );

    // Nobody listening, or no such client: dropped, and the action still succeeds.
    agent
        .run(Action::ClientEvent {
            client: "ghost".into(),
            name: "x".into(),
            value: None,
        })
        .await
        .unwrap();
    // Staff have no personal stream.
    assert_eq!(
        http(addr, "GET", "/clients/me/events", TOKEN, "").await.0,
        403
    );
}

#[tokio::test]
async fn removed_client_token_stops_working() {
    let (agent, addr) = serve("removed").await;
    let t = add_client(addr, "novaguard").await;
    assert_eq!(push(addr, &t, "novaguard.a", json!(1)).await, 204);
    let mut s = Stream::open(addr, &t).await;
    // Duplicates and bad names are refused, and the list never shows a token.
    assert_eq!(
        http(addr, "POST", "/clients", TOKEN, r#"{"name":"novaguard"}"#)
            .await
            .0,
        400
    );
    assert_eq!(
        http(addr, "POST", "/clients", TOKEN, r#"{"name":"obs"}"#)
            .await
            .0,
        400
    );
    assert_eq!(
        http(addr, "POST", "/clients", TOKEN, r#"{"name":"Bad Name"}"#)
            .await
            .0,
        400
    );
    let (_, list) = http(addr, "GET", "/clients", TOKEN, "").await;
    assert!(list.contains("novaguard") && !list.contains(&t));

    assert_eq!(
        http(addr, "DELETE", "/clients/novaguard", TOKEN, "")
            .await
            .0,
        204
    );
    assert_eq!(
        http(addr, "DELETE", "/clients/novaguard", TOKEN, "")
            .await
            .0,
        404
    );
    assert_eq!(push(addr, &t, "novaguard.a", json!(2)).await, 401);
    assert_eq!(http(addr, "GET", "/status", &t, "").await.0, 401);
    assert!(
        agent.feeds.get("novaguard.a").is_none(),
        "its feeds went with it"
    );
    // The open stream ends.
    assert!(
        s.read_until("never", Duration::from_secs(3))
            .await
            .is_none()
    );
    // A restarted agent still doesn't know the token.
    let again = Agent::new(
        std::env::temp_dir()
            .join(format!(
                "novagear-clients-api-removed-{}",
                std::process::id()
            ))
            .join("config.json"),
        TOKEN.to_string(),
    )
    .unwrap();
    assert!(again.clients.lock().unwrap().who(&t).is_none());
}

#[tokio::test]
async fn staff_lists_feeds_with_values_and_age() {
    let (_agent, addr) = serve("list").await;
    let t = add_client(addr, "novaguard").await;
    push(addr, &t, "novaguard.n", json!(7)).await;
    let (code, body) = http(addr, "GET", "/feeds", TOKEN, "").await;
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    let f = v
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["key"] == "novaguard.n")
        .unwrap();
    assert_eq!(f["value"], 7.0);
    assert!(f["age_s"].is_number());
}

#[tokio::test]
async fn a_request_authenticated_before_removal_creates_nothing_after_it() {
    use novagear_agent::ClientPutError;
    use novagear_agent::feeds::FeedValue;
    let (agent, addr) = serve("race").await;
    let _t = add_client(addr, "novaguard").await;
    assert!(agent.client_subscribe("novaguard").is_some());
    assert!(agent.remove_client("novaguard").unwrap());
    // The stale `Caller::Client("novaguard")` paths, as a request that passed auth earlier
    // would reach them.
    assert!(agent.client_subscribe("novaguard").is_none());
    let put = agent.client_put_feed(
        "novaguard",
        "novaguard.late",
        FeedValue::Number(1.0),
        None,
        None,
        Duration::from_secs(60),
    );
    assert_eq!(put, Err(ClientPutError::Gone));
    assert!(agent.feeds.get("novaguard.late").is_none());
    // A client added later under the same name starts with no listener, so an event
    // is dropped rather than queued on a channel left over from before.
    let again = add_client(addr, "novaguard").await;
    let mut s = Stream::open(addr, &again).await;
    assert!(
        s.read_until("late", Duration::from_millis(200))
            .await
            .is_none()
    );
}

#[tokio::test]
async fn obs_scene_and_input_routes_need_agent_or_panel() {
    let (agent, addr) = serve("obs-lists").await;
    // OBS is pointed at a port nothing listens on, so no test here ever reaches a real OBS.
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let mut cfg = agent.config();
    cfg.obs.url = format!("ws://127.0.0.1:{closed}");
    agent.set_config(cfg).unwrap();
    let t = add_client(addr, "novaguard").await;
    for p in ["/obs/scenes", "/obs/inputs"] {
        // A client token is turned away before the handler runs.
        assert_eq!(http(addr, "GET", p, &t, "").await.0, 403, "{p}");
        // No token at all is not let in either.
        assert_eq!(http(addr, "GET", p, "wrong", "").await.0, 401, "{p}");
        // Staff reach the handler, which can't reach OBS.
        let (code, body) = http(addr, "GET", p, TOKEN, "").await;
        assert_eq!(code, 400, "{p}: {body}");
    }
}
