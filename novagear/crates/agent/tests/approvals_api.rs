//! Approvals over HTTP: a real router on an ephemeral port. The point of most of these is
//! what the API cannot do: nothing here (or anywhere) can say yes. Only the deck's hold can,
//! and that is tested beside the code (`approvals.rs`).

use novagear_agent::clients::ClientLimits;
use novagear_agent::{Agent, api};
use novagear_core::API_PORT;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TOKEN: &str = "agent-token-0123456789012345678901234567890123";

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "novagear-approvals-api-{tag}-{}",
        std::process::id()
    ));
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

fn request(method: &str, path: &str, token: Option<&str>, body: &str) -> String {
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    format!(
        "{method} /api{path} HTTP/1.1\r\nHost: 127.0.0.1:{API_PORT}\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
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
        Some(TOKEN),
        &json!({ "name": name }).to_string(),
    )
    .await;
    assert_eq!(code, 201, "{body}");
    serde_json::from_str::<Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string()
}

/// A browser panel session, the way the panel gets one.
async fn panel_session(addr: SocketAddr) -> String {
    let (code, body) = http(addr, "POST", "/panel-code", Some(TOKEN), "").await;
    assert_eq!(code, 200);
    let c = serde_json::from_str::<Value>(&body).unwrap()["code"]
        .as_str()
        .unwrap()
        .to_string();
    let (code, body) = http(
        addr,
        "POST",
        "/panel-session",
        None,
        &json!({ "code": c }).to_string(),
    )
    .await;
    assert_eq!(code, 200);
    serde_json::from_str::<Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Real time is short here: a request every millisecond, so tests needn't wait 5 s.
fn fast_limits(a: &Agent) {
    *a.limits.lock().unwrap() = ClientLimits::new(10.0, Duration::from_millis(1));
}

async fn create(addr: SocketAddr, token: &str, body: Value) -> (u16, Value) {
    let (code, text) = http(addr, "POST", "/approvals", Some(token), &body.to_string()).await;
    (
        code,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

async fn status_of(addr: SocketAddr, token: &str, id: &str) -> String {
    let (code, body) = http(addr, "GET", &format!("/approvals/{id}"), Some(token), "").await;
    assert_eq!(code, 200, "{body}");
    serde_json::from_str::<Value>(&body).unwrap()["status"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn a_client_asks_waits_and_hears_the_answer() {
    let (_a, addr) = serve("flow").await;
    let ct = add_client(addr, "deployer").await;
    let (code, v) = create(
        addr,
        &ct,
        json!({ "title": "Deploy web?", "lines": ["a", "b"], "source": "ci", "risk": "high",
                "details_url": "https://example.com/1", "expires_in_s": 60 }),
    )
    .await;
    assert_eq!(code, 201, "{v}");
    let id = v["id"].as_str().unwrap().to_string();
    assert_eq!(status_of(addr, &ct, &id).await, "waiting");

    // A long poll that the panel's Deny ends.
    let poll = {
        let (ct, id) = (ct.clone(), id.clone());
        tokio::spawn(async move {
            http(
                addr,
                "GET",
                &format!("/approvals/{id}?wait=30"),
                Some(&ct),
                "",
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!poll.is_finished(), "it waits");
    let session = panel_session(addr).await;
    let (code, body) = http(addr, "GET", "/approvals", Some(&session), "").await;
    assert_eq!(code, 200);
    let list: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(list[0]["id"], id.as_str());
    assert_eq!(list[0]["title"], "Deploy web?");
    assert_eq!(list[0]["client"], "deployer");
    assert_eq!(list[0]["risk"], "high");
    let (code, _) = http(
        addr,
        "POST",
        &format!("/approvals/{id}/deny"),
        Some(&session),
        "",
    )
    .await;
    assert_eq!(code, 200);
    let (code, body) = tokio::time::timeout(Duration::from_secs(3), poll)
        .await
        .expect("the poll ended when it was denied")
        .unwrap();
    assert_eq!(code, 200);
    assert!(body.contains("denied"), "{body}");
    // Denying twice, or something that isn't there.
    let (code, _) = http(
        addr,
        "POST",
        &format!("/approvals/{id}/deny"),
        Some(TOKEN),
        "",
    )
    .await;
    assert_eq!(code, 404);
}

/// Nothing over HTTP can approve: not a client token, the agent token or a panel session,
/// by any method, any guessed path, any body or query string.
#[tokio::test]
async fn no_route_can_approve() {
    let (agent, addr) = serve("noapprove").await;
    fast_limits(&agent);
    let ct = add_client(addr, "deployer").await;
    let other = add_client(addr, "other").await;
    let session = panel_session(addr).await;
    let (code, v) = create(addr, &ct, json!({ "title": "Deploy web?" })).await;
    assert_eq!(code, 201);
    let id = v["id"].as_str().unwrap().to_string();

    let tokens = [
        ("client", ct.as_str()),
        ("other client", other.as_str()),
        ("agent", TOKEN),
        ("panel", session.as_str()),
    ];
    // Every method but DELETE (the owner's cancel) and, for staff, POST .../deny.
    let methods = ["GET", "HEAD", "POST", "PUT", "PATCH", "OPTIONS", "TRACE"];
    let paths = [
        format!("/approvals/{id}"),
        format!("/approvals/{id}/"),
        format!("/approvals/{id}/approve"),
        format!("/approvals/{id}/approved"),
        format!("/approvals/{id}/approve/"),
        format!("/approvals/{id}/accept"),
        format!("/approvals/{id}/allow"),
        format!("/approvals/{id}/confirm"),
        format!("/approvals/{id}/grant"),
        format!("/approvals/{id}/yes"),
        format!("/approvals/{id}/ok"),
        format!("/approvals/{id}/status"),
        format!("/approvals/{id}/outcome"),
        format!("/approvals/{id}/deny/approve"),
        format!("/approvals/{id}?status=approved"),
        format!("/approvals/{id}?approve=1&approved=true&outcome=approved"),
        format!("/approvals/{id}?wait=0&status=approved"),
        format!("/approvals?approve={id}"),
        format!("/approvals?id={id}&status=approved"),
        "/approvals/approve".to_string(),
        format!("/approvals/approve/{id}"),
        format!("/approvals/{id}/../approve"),
        format!("/approve/{id}"),
        format!("/approve?id={id}"),
        format!("/approved/{id}"),
        "/approvals".to_string(),
        "/approvals/".to_string(),
    ];
    let bodies = [
        String::new(),
        json!({ "status": "approved" }).to_string(),
        json!({ "approved": true }).to_string(),
        json!({ "outcome": "approved", "id": id }).to_string(),
        json!({ "title": "x", "status": "approved" }).to_string(),
        json!({ "id": id, "approve": true, "do": "approve" }).to_string(),
        json!({ "do": "approval_approve", "id": id }).to_string(),
    ];
    let mut tried = 0;
    for (who, token) in tokens {
        for method in methods {
            for path in &paths {
                for body in &bodies {
                    // The one request that is allowed to do something: a staff POST of a
                    // title-only body would create another approval; none of these is that.
                    let (code, text) = http(addr, method, path, Some(token), body).await;
                    tried += 1;
                    let may_read = matches!(method, "GET" | "HEAD")
                        && code == 200
                        && (path == "/approvals"
                            || path.starts_with("/approvals?")
                            || (path.starts_with(&format!("/approvals/{id}"))
                                && !path.contains("/approve")));
                    assert!(
                        may_read || [400, 403, 404, 405].contains(&code),
                        "{who} {method} {path} {body} -> {code}: {text}"
                    );
                    assert!(
                        !text.contains("approved"),
                        "{who} {method} {path} {body} said something approved: {text}"
                    );
                }
            }
        }
        // And the approval is still waiting, as the asker sees it and as the store has it.
        assert_eq!(status_of(addr, &ct, &id).await, "waiting", "after {who}");
        assert_eq!(
            agent
                .approval_status("deployer", &id, Duration::ZERO)
                .await
                .unwrap(),
            "waiting",
            "after {who}"
        );
    }
    assert!(tried > 4000, "{tried}");

    // No token at all.
    for path in [
        format!("/approvals/{id}"),
        format!("/approvals/{id}/approve"),
    ] {
        for method in ["GET", "POST", "PUT"] {
            let (code, _) = http(addr, method, &path, None, r#"{"status":"approved"}"#).await;
            // 401 where a route exists; a path that matches nothing is just not found.
            assert!([401, 404].contains(&code), "{method} {path} -> {code}");
        }
    }
    // The only things that decide it: its owner cancelling, or staff denying.
    let (code, _) = http(
        addr,
        "POST",
        &format!("/approvals/{id}/deny"),
        Some(&ct),
        "",
    )
    .await;
    assert_eq!(code, 403, "a client can't even deny");
    assert_eq!(status_of(addr, &ct, &id).await, "waiting");
    let (code, body) = http(
        addr,
        "POST",
        &format!("/approvals/{id}/deny"),
        Some(&session),
        "",
    )
    .await;
    assert_eq!(code, 200);
    assert!(body.contains("denied"));
    assert_eq!(status_of(addr, &ct, &id).await, "denied");
}

#[tokio::test]
async fn expiry_and_cancel_and_ownership() {
    let (agent, addr) = serve("expiry").await;
    fast_limits(&agent);
    let a = add_client(addr, "alpha").await;
    let b = add_client(addr, "beta").await;

    // Expiry ends a long poll with `expired`.
    let (_, v) = create(addr, &a, json!({ "title": "short", "expires_in_s": 1 })).await;
    let short = v["id"].as_str().unwrap().to_string();
    let (code, body) = tokio::time::timeout(
        Duration::from_secs(5),
        http(
            addr,
            "GET",
            &format!("/approvals/{short}?wait=30"),
            Some(&a),
            "",
        ),
    )
    .await
    .expect("the long poll ended at expiry");
    assert_eq!(code, 200);
    assert!(body.contains("expired"), "{body}");

    // A client cancels its own, and is refused on another client's.
    let (_, v) = create(addr, &a, json!({ "title": "mine" })).await;
    let mine = v["id"].as_str().unwrap().to_string();
    let (code, _) = http(addr, "DELETE", &format!("/approvals/{mine}"), Some(&b), "").await;
    assert_eq!(code, 403);
    let (code, _) = http(addr, "GET", &format!("/approvals/{mine}"), Some(&b), "").await;
    assert_eq!(code, 403, "nor can it read it");
    assert_eq!(status_of(addr, &a, &mine).await, "waiting");
    let (code, body) = http(addr, "DELETE", &format!("/approvals/{mine}"), Some(&a), "").await;
    assert_eq!(code, 200);
    assert!(body.contains("cancelled"));
    let (code, _) = http(addr, "DELETE", "/approvals/nothing", Some(&a), "").await;
    assert_eq!(code, 404);
    let (code, _) = http(addr, "GET", "/approvals/nothing", Some(&a), "").await;
    assert_eq!(code, 404);

    // Neither staff token reads or cancels a client's request by id; staff list them.
    let (_, v) = create(addr, &a, json!({ "title": "listed" })).await;
    let listed = v["id"].as_str().unwrap().to_string();
    for (m, p) in [
        ("GET", format!("/approvals/{listed}")),
        ("DELETE", format!("/approvals/{listed}")),
    ] {
        let (code, _) = http(addr, m, &p, Some(TOKEN), "").await;
        assert_eq!(code, 403, "{m}");
    }
    let (code, body) = http(addr, "GET", "/approvals", Some(TOKEN), "").await;
    assert_eq!(code, 200);
    assert!(body.contains(&listed));
    let (code, _) = http(addr, "GET", "/approvals", Some(&a), "").await;
    assert_eq!(code, 403, "clients can't list");

    // Staff make and read their own.
    let (code, v) = create(addr, TOKEN, json!({ "title": "from the CLI" })).await;
    assert_eq!(code, 201);
    let own = v["id"].as_str().unwrap();
    assert_eq!(status_of(addr, TOKEN, own).await, "waiting");

    // Removing a client cancels what it left.
    let (code, _) = http(addr, "DELETE", "/clients/alpha", Some(TOKEN), "").await;
    assert_eq!(code, 204);
    let (code, body) = http(addr, "GET", "/approvals", Some(TOKEN), "").await;
    assert_eq!(code, 200);
    assert!(!body.contains(&listed), "{body}");
}

#[tokio::test]
async fn client_limits() {
    let (agent, addr) = serve("limits").await;
    let ct = add_client(addr, "deployer").await;
    // The real rate: one per 5 s.
    let (code, _) = create(addr, &ct, json!({ "title": "one" })).await;
    assert_eq!(code, 201);
    let (code, _) = create(addr, &ct, json!({ "title": "two" })).await;
    assert_eq!(code, 429, "a second within 5 s");
    // A bad request does not use the allowance: validation comes first.
    let (code, _) = create(addr, &ct, json!({ "title": "" })).await;
    assert_eq!(code, 400);

    // At most 20 waiting (the rate sped up so the test needn't wait).
    fast_limits(&agent);
    for n in 0..19 {
        tokio::time::sleep(Duration::from_millis(5)).await;
        let (code, v) = create(addr, &ct, json!({ "title": format!("n{n}") })).await;
        assert_eq!(code, 201, "{v}");
    }
    tokio::time::sleep(Duration::from_millis(5)).await;
    let (code, _) = create(addr, &ct, json!({ "title": "21st" })).await;
    assert_eq!(code, 429, "the 21st waiting request");
    // Another client has its own allowance.
    let other = add_client(addr, "other").await;
    let (code, _) = create(addr, &other, json!({ "title": "fine" })).await;
    assert_eq!(code, 201);
}

#[tokio::test]
async fn bodies_are_checked_and_capped() {
    let (agent, addr) = serve("body").await;
    fast_limits(&agent);
    let ct = add_client(addr, "deployer").await;
    for bad in [
        json!({}),
        json!({ "title": "" }),
        json!({ "title": "x".repeat(121) }),
        json!({ "title": "x", "lines": ["a", "b", "c", "d"] }),
        json!({ "title": "x", "lines": ["y".repeat(161)] }),
        json!({ "title": "x", "expires_in_s": 0 }),
        json!({ "title": "x", "expires_in_s": 3601 }),
        json!({ "title": "x", "risk": "medium" }),
        json!({ "title": "x", "status": "approved" }),
    ] {
        tokio::time::sleep(Duration::from_millis(3)).await;
        let (code, v) = create(addr, &ct, bad.clone()).await;
        assert_eq!(code, 400, "{bad}: {v}");
    }
    tokio::time::sleep(Duration::from_millis(3)).await;
    let (code, _) = http(
        addr,
        "POST",
        "/approvals",
        Some(&ct),
        &json!({ "title": "x", "lines": ["z".repeat(9000)] }).to_string(),
    )
    .await;
    assert_eq!(code, 413, "8 KB at most");
    let (code, _) = create(addr, &ct, json!({ "title": "x".repeat(120), "lines": vec!["y".repeat(160); 3], "expires_in_s": 3600 })).await;
    assert_eq!(code, 201, "the limits themselves are fine");
}

#[tokio::test]
async fn status_shows_the_top_request_to_staff_only() {
    let (_a, addr) = serve("status").await;
    let ct = add_client(addr, "deployer").await;
    let (code, body) = http(addr, "GET", "/status", Some(TOKEN), "").await;
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["approvals"], json!({ "pending": 0, "top": null }));
    let (_, v) = create(addr, &ct, json!({ "title": "Deploy web?", "source": "ci" })).await;
    let id = v["id"].as_str().unwrap();
    let (_, body) = http(addr, "GET", "/status", Some(TOKEN), "").await;
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["approvals"]["pending"], 1);
    assert_eq!(v["approvals"]["top"]["id"], id);
    assert_eq!(v["approvals"]["top"]["title"], "Deploy web?");
    assert_eq!(v["approvals"]["top"]["source"], "deployer: ci");
    let (code, body) = http(addr, "GET", "/status", Some(&ct), "").await;
    assert_eq!(code, 200);
    assert!(
        !body.contains("approvals") && !body.contains("Deploy"),
        "{body}"
    );
}

async fn read_until(s: &mut tokio::net::TcpStream, seen: &mut String, needle: &str) {
    let mut buf = [0u8; 2048];
    while !seen.contains(needle) {
        let n = s.read(&mut buf).await.unwrap();
        assert!(n > 0, "stream closed");
        seen.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
}

/// The panel hears `approval` events with an id and a status: never the title.
#[tokio::test]
async fn events_carry_the_id_and_status_only() {
    let (agent, addr) = serve("events").await;
    fast_limits(&agent);
    let ct = add_client(addr, "deployer").await;
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(request("GET", "/events", Some(TOKEN), "").as_bytes())
        .await
        .unwrap();
    let mut seen = String::new();
    read_until(&mut s, &mut seen, "200 OK").await;
    let (_, v) = create(
        addr,
        &ct,
        json!({ "title": "sekret-event-title-77", "lines": ["sekret-event-line-78"] }),
    )
    .await;
    let id = v["id"].as_str().unwrap().to_string();
    tokio::time::timeout(
        Duration::from_secs(3),
        read_until(&mut s, &mut seen, r#""status":"waiting""#),
    )
    .await
    .expect("the creation event");
    let (code, _) = http(addr, "DELETE", &format!("/approvals/{id}"), Some(&ct), "").await;
    assert_eq!(code, 200);
    tokio::time::timeout(
        Duration::from_secs(3),
        read_until(&mut s, &mut seen, r#""status":"cancelled""#),
    )
    .await
    .expect("the cancel event");
    assert!(seen.contains(r#""type":"approval""#), "{seen}");
    assert!(seen.contains(&id));
    assert!(!seen.contains("sekret-event"), "no text in events: {seen}");
}

/// What `novagear approve-test` does: the agent is verified with the agent's token and the
/// requests are sent with a client's, through the shared client library.
#[tokio::test]
async fn the_client_library_asks_and_waits_with_a_client_token() {
    use novagear_core::client::call_at;
    let (_a, addr) = serve("lib").await;
    let ct = add_client(addr, "approve-test").await;
    let port = addr.port();
    let t = Duration::from_secs(10);
    let made = tokio::task::spawn_blocking({
        let ct = ct.clone();
        move || {
            call_at(
                port,
                TOKEN,
                &ct,
                "POST",
                "/api/approvals",
                Some(&json!({ "title": "Deploy web?", "source": "approve-test" })),
                t,
            )
        }
    })
    .await
    .unwrap()
    .unwrap();
    let id = made["id"].as_str().unwrap().to_string();
    let wait = tokio::task::spawn_blocking({
        let (ct, id) = (ct.clone(), id.clone());
        move || {
            call_at(
                port,
                TOKEN,
                &ct,
                "GET",
                &format!("/api/approvals/{id}?wait=30"),
                None,
                Duration::from_secs(45),
            )
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (code, _) = http(
        addr,
        "POST",
        &format!("/approvals/{id}/deny"),
        Some(TOKEN),
        "",
    )
    .await;
    assert_eq!(code, 200);
    let r = wait.await.unwrap().unwrap();
    assert_eq!(r["status"], "denied");
    // A listener that can't prove it holds the agent token never sees the client token.
    let wrong = tokio::task::spawn_blocking(move || {
        call_at(
            port,
            "some-other-token-xxxxxxxxxxxxxxxxxxxxxxxxxxxx",
            &ct,
            "GET",
            "/api/status",
            None,
            t,
        )
    })
    .await
    .unwrap();
    assert!(wrong.is_err());
}
