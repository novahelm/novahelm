//! No secret ever reaches a log. Runs the real agent API with the global subscriber pointed
//! at a temp dir (its own test binary, so nothing else has installed one), pushes every
//! secret through the paths that touch it, then reads every log file and the log API.

use novagear_agent::{Agent, api, logfile};
use novagear_core::config::Config;
use novagear_core::{API_PORT, Action, KeyBinding};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const OBS_PASSWORD: &str = "obs-pass-7731";
const TYPED: &str = "typed-secret-4410";
const OBS_SALT: &str = "obs-salt-5514";
const OBS_CHALLENGE: &str = "obs-challenge-9981";
const FEED_VALUE: &str = "sekret-feed-991";
const EVENT_NAME: &str = "event-name-secret-4471";
const EVENT_VALUE: &str = "event-value-secret-5582";
const APPROVAL_TITLE: &str = "sekret-approval-552";
const APPROVAL_LINE: &str = "sekret-approval-line-663";
const APPROVAL_SOURCE: &str = "sekret-approval-source-774";
const APPROVAL_URL: &str = "https://example.com/?key=sekret-approval-url-885";

async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (u16, String) {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{API_PORT}\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    let code = out.split(' ').nth(1).unwrap().parse().unwrap();
    let body = out.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (code, body)
}

fn json_body(s: &str) -> Value {
    // Connection: close with no chunking keeps this plain; chunked bodies have size lines.
    let start = s.find(['{', '[']).unwrap();
    let end = s.rfind(['}', ']']).unwrap();
    serde_json::from_str(&s[start..=end]).unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn no_secret_reaches_a_log() {
    let root = std::env::temp_dir().join(format!("novagear-nosecrets-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let logs = root.join("logs");
    std::fs::create_dir_all(&root).unwrap();
    logfile::init_with_filter(&logs, "debug".into()).unwrap();

    // A closed port: bind, note it, drop.
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let cfg = root.join("config.json");
    // The typed text also sits in a key binding, so it is loaded, normalized and saved.
    let mut c = Config::default();
    c.obs.url = format!("ws://127.0.0.1:{closed}");
    c.obs.password = OBS_PASSWORD.into();
    c.keys = vec![Some(KeyBinding {
        label: "t".into(),
        color: "#000000".into(),
        icon: None,
        face: Default::default(),
        states: None,
        follow: None,
        hold_ms: 0,
        action: Action::TypeText {
            text: TYPED.into(),
            enter: false,
        },
    })];
    c.save(&cfg).unwrap();
    let token = "api-token-".to_string() + &"z".repeat(40);
    let agent = Agent::new(cfg, token.clone()).unwrap();
    // Typing and hotkeys run for real below, but must never post a keyboard event: with
    // Accessibility granted they would type into the focused app on this machine.
    agent.set_input_dry_run(true);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = agent.clone();
    tokio::spawn(async move {
        axum::serve(listener, api::router(served)).await.unwrap();
    });

    let nonce = "ab".repeat(16);
    let (code, _) = http(
        addr,
        "GET",
        &format!("/api/hello?nonce={nonce}"),
        None,
        None,
    )
    .await;
    assert_eq!(code, 200);

    let (code, body) = http(addr, "POST", "/api/panel-code", Some(&token), None).await;
    assert_eq!(code, 200);
    let panel_code = json_body(&body)["code"].as_str().unwrap().to_string();
    let (code, body) = http(
        addr,
        "POST",
        "/api/panel-session",
        None,
        Some(json!({ "code": panel_code })),
    )
    .await;
    assert_eq!(code, 200);
    let session = json_body(&body)["token"].as_str().unwrap().to_string();

    // Each fails (no OBS on that port, a link scheme that isn't allowed, a key that doesn't
    // exist), and each failure is logged.
    let (code, _) = http(
        addr,
        "POST",
        "/api/action",
        Some(&token),
        Some(json!({ "do": "obs_scene", "scene": "Main" })),
    )
    .await;
    assert_eq!(code, 400);
    let (code, body) = http(
        addr,
        "POST",
        "/api/action",
        Some(&token),
        Some(json!({ "do": "type_text", "text": TYPED, "enter": false })),
    )
    .await;
    assert_eq!(code, 400);
    assert!(body.contains("dry run"), "the dry run stopped it: {body}");
    // Saving the config re-reads and rewrites the key that holds the typed text.
    let (code, body) = http(addr, "GET", "/api/config", Some(&token), None).await;
    assert_eq!(code, 200);
    let (code, _) = http(
        addr,
        "PUT",
        "/api/config",
        Some(&token),
        Some(json_body(&body)),
    )
    .await;
    assert_eq!(code, 200);
    let _ = http(
        addr,
        "POST",
        "/api/action",
        Some(&token),
        Some(json!({ "do": "open_url", "url": "ftp://x.example/?key=url-secret-5521" })),
    )
    .await;
    let _ = http(
        addr,
        "POST",
        "/api/action",
        Some(&token),
        Some(json!({ "do": "hotkey", "keys": "hotkey-secret-6632" })),
    )
    .await;
    // The persistent link against an OBS that wants a password and refuses ours: the salt,
    // the challenge, the password and the answer built from them stay out of the logs.
    let auth_answer = novagear_agent::obs::auth_string(OBS_PASSWORD, OBS_SALT, OBS_CHALLENGE);
    let fake = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut link_cfg = Config::default().obs;
    link_cfg.url = format!("ws://{}", fake.local_addr().unwrap());
    link_cfg.password = OBS_PASSWORD.into();
    let (seen_tx, seen_rx) = std::sync::mpsc::channel();
    let expected = auth_answer.clone();
    std::thread::spawn(move || {
        let (stream, _) = fake.accept().unwrap();
        let mut ws = tungstenite::accept(stream).unwrap();
        ws.send(tungstenite::Message::text(
            json!({ "op": 0, "d": { "rpcVersion": 1, "authentication": {
                "challenge": OBS_CHALLENGE, "salt": OBS_SALT } } })
            .to_string(),
        ))
        .unwrap();
        let tungstenite::Message::Text(t) = ws.read().unwrap() else {
            panic!("expected identify");
        };
        let m: Value = serde_json::from_str(t.as_str()).unwrap();
        // The client did answer the challenge, so this is the auth failure path.
        let _ = seen_tx.send(m["d"]["authentication"] == json!(expected));
        let _ = ws.close(Some(tungstenite::protocol::CloseFrame {
            code: 4009.into(),
            reason: "Authentication failed.".into(),
        }));
        let _ = ws.flush();
    });
    agent.feeds.set_wanted(["obs.scene".to_string()].into());
    let (obs_tx, _obs_rx) = tokio::sync::mpsc::unbounded_channel();
    let link = novagear_agent::obs::ObsLink::spawn(link_cfg, agent.feeds.clone(), obs_tx);
    assert!(
        seen_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap(),
        "the link answered the challenge"
    );
    // Let the link notice the refusal and log it, then stop it.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    tokio::task::spawn_blocking(move || link.stop())
        .await
        .unwrap();

    // A client: its token, its pushed value and the events sent to it stay out of the logs,
    // including when it is refused, rate limited, or not listening.
    let (code, body) = http(
        addr,
        "POST",
        "/api/clients",
        Some(&token),
        Some(json!({ "name": "novaguard" })),
    )
    .await;
    assert_eq!(code, 201);
    let client_token = json_body(&body)["token"].as_str().unwrap().to_string();
    let (code, _) = http(
        addr,
        "PUT",
        "/api/feeds/novaguard.note",
        Some(&client_token),
        Some(json!({ "value": FEED_VALUE, "unit": "feed-unit-3302" })),
    )
    .await;
    assert_eq!(code, 204);
    let (code, _) = http(
        addr,
        "PUT",
        "/api/feeds/obs.recording",
        Some(&client_token),
        Some(json!({ "value": FEED_VALUE })),
    )
    .await;
    assert_eq!(code, 403);
    let (code, _) = http(addr, "GET", "/api/logs", Some(&client_token), None).await;
    assert_eq!(code, 403);
    let (code, _) = http(
        addr,
        "GET",
        "/api/status",
        Some("wrong-client-token-8820"),
        None,
    )
    .await;
    assert_eq!(code, 401);
    let (code, body) = http(addr, "GET", "/api/feeds", Some(&token), None).await;
    assert_eq!(code, 200);
    assert!(body.contains(FEED_VALUE), "the feed list does show values");
    agent
        .run(Action::ClientEvent {
            client: "novaguard".into(),
            name: EVENT_NAME.into(),
            value: Some(json!(EVENT_VALUE)),
        })
        .await
        .unwrap();

    // Approvals: the request's text (title, lines, source, link) stays out of the logs through
    // every outcome: created, refused, rate limited, denied, cancelled, expired. Only the id,
    // the client's name, the risk and the outcome are logged.
    let ask = |extra: Value| {
        let mut b = json!({
            "title": APPROVAL_TITLE,
            "lines": [APPROVAL_LINE],
            "source": APPROVAL_SOURCE,
            "details_url": APPROVAL_URL,
            "risk": "high",
        });
        b.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        b
    };
    let (code, body) = http(
        addr,
        "POST",
        "/api/approvals",
        Some(&client_token),
        Some(ask(json!({}))),
    )
    .await;
    assert_eq!(code, 201, "{body}");
    let denied_id = json_body(&body)["id"].as_str().unwrap().to_string();
    // Within 5 seconds: rate limited.
    let (code, _) = http(
        addr,
        "POST",
        "/api/approvals",
        Some(&client_token),
        Some(ask(json!({}))),
    )
    .await;
    assert_eq!(code, 429);
    let (code, _) = http(
        addr,
        "POST",
        "/api/approvals",
        Some(&client_token),
        Some(ask(json!({ "lines": ["a", "b", "c", APPROVAL_LINE] }))),
    )
    .await;
    assert_eq!(code, 400);
    let (code, body) = http(addr, "GET", "/api/approvals", Some(&token), None).await;
    assert_eq!(code, 200);
    assert!(
        body.contains(APPROVAL_TITLE),
        "the panel's list does show it"
    );
    let (code, _) = http(
        addr,
        "POST",
        &format!("/api/approvals/{denied_id}/deny"),
        Some(&session),
        None,
    )
    .await;
    assert_eq!(code, 200);
    *agent.limits.lock().unwrap() =
        novagear_agent::clients::ClientLimits::new(10.0, std::time::Duration::from_millis(1));
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let (code, body) = http(
        addr,
        "POST",
        "/api/approvals",
        Some(&client_token),
        Some(ask(json!({}))),
    )
    .await;
    assert_eq!(code, 201, "{body}");
    let cancelled_id = json_body(&body)["id"].as_str().unwrap().to_string();
    let (code, _) = http(
        addr,
        "DELETE",
        &format!("/api/approvals/{cancelled_id}"),
        Some(&client_token),
        None,
    )
    .await;
    assert_eq!(code, 200);
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let (code, body) = http(
        addr,
        "POST",
        "/api/approvals",
        Some(&client_token),
        Some(ask(json!({ "expires_in_s": 1 }))),
    )
    .await;
    assert_eq!(code, 201, "{body}");
    let expiring_id = json_body(&body)["id"].as_str().unwrap().to_string();
    let (code, body) = http(
        addr,
        "GET",
        &format!("/api/approvals/{expiring_id}?wait=10"),
        Some(&client_token),
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert!(body.contains("expired"), "{body}");

    let (code, logs_body) = http(addr, "GET", "/api/logs?limit=2000", Some(&session), None).await;
    assert_eq!(code, 200);

    let secrets = [
        token.as_str(),
        panel_code.as_str(),
        session.as_str(),
        OBS_PASSWORD,
        OBS_SALT,
        OBS_CHALLENGE,
        auth_answer.as_str(),
        TYPED,
        "url-secret-5521",
        "hotkey-secret-6632",
        client_token.as_str(),
        "wrong-client-token-8820",
        FEED_VALUE,
        "feed-unit-3302",
        EVENT_NAME,
        EVENT_VALUE,
        APPROVAL_TITLE,
        APPROVAL_LINE,
        APPROVAL_SOURCE,
        "sekret-approval-url-885",
    ];
    let mut all = logs_body.clone();
    let mut files = 0;
    for e in std::fs::read_dir(&logs).unwrap() {
        all.push_str(&std::fs::read_to_string(e.unwrap().path()).unwrap());
        files += 1;
    }
    assert!(files >= 1, "the agent wrote a log file");
    assert!(all.contains("action"), "the actions were logged");
    assert!(all.contains("OBS link down"), "the refusal was logged");
    assert!(
        all.contains("client_event: novaguard not listening"),
        "the dropped event was logged by client name"
    );
    for (id, outcome) in [
        (&denied_id, "denied"),
        (&cancelled_id, "cancelled"),
        (&expiring_id, "expired"),
    ] {
        assert!(
            all.contains(&format!(
                "approval {id} from novaguard risk=high: {outcome}"
            )),
            "the {outcome} outcome was logged by id and client only"
        );
    }
    for s in secrets {
        assert!(!all.contains(s), "a secret reached the log: {s}");
    }
    let _ = std::fs::remove_dir_all(&root);
}
