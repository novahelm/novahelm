use novalink_agent::api::router;
use novalink_agent::testutil::one_engine;
use novalink_client::{Client, ClientError};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// Serves the real router on an ephemeral port; returns its base URL and the engine id.
async fn serve() -> (String, String) {
    let (h, _p) = one_engine().await;
    let me = h.status().me.to_string();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(l, router(h, "tok".into())).await.unwrap();
    });
    (base, me)
}

#[tokio::test(flavor = "multi_thread")]
async fn client_reads_status_and_pending() {
    let (base, me) = serve().await;
    tokio::task::spawn_blocking(move || {
        let c = Client::with(&base, "tok".into());
        let s = c.status().unwrap();
        assert_eq!(s.me, me);
        assert_eq!(s.capture, "running");
        assert!(c.pending().unwrap().is_empty());
        assert!(c.discovered().unwrap().is_empty());
        assert!(!c.doctor().unwrap().is_empty());
        c.metrics().unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn client_surfaces_api_errors() {
    let (base, _) = serve().await;
    tokio::task::spawn_blocking(move || {
        let c = Client::with(&base, "tok".into());
        match c.layout("sideways", "x") {
            Err(ClientError::Api {
                status: 400,
                message,
            }) => {
                assert!(message.contains("edge must be"), "{message}")
            }
            other => panic!("{other:?}"),
        }
        // Wrong token: the proof fails, so it is an impostor, not a 401.
        let c = Client::with(&base, "nope".into());
        assert!(matches!(c.status(), Err(ClientError::Impostor)));
    })
    .await
    .unwrap();
}

#[test]
fn client_refuses_impostor() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(String::new()));
    let s2 = seen.clone();
    std::thread::spawn(move || {
        for conn in l.incoming() {
            let mut conn = conn.unwrap();
            let mut buf = [0u8; 4096];
            let n = conn.read(&mut buf).unwrap_or(0);
            s2.lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&buf[..n]));
            let body = r#"{"proof":"00"}"#;
            let _ = write!(
                conn,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    let c = Client::with(&base, "tok".into());
    assert!(matches!(c.status(), Err(ClientError::Impostor)));
    assert!(
        !seen
            .lock()
            .unwrap()
            .to_ascii_lowercase()
            .contains("authorization")
    );
}

#[test]
fn client_reports_not_running() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    drop(l);
    let c = Client::with(&base, "tok".into());
    assert!(matches!(c.status(), Err(ClientError::NotRunning)));
}

use novalink_agent::api::{decode_hex, server_proof};
use novalink_agent::engine::PendingPair as AgentPending;
use novalink_agent::testutil::two_engines;
use novalink_proto::{DeviceId, Edge};
use serde_json::{Value, json};

/// Like the agent's API: a device id (32 numbers) becomes 64 hex characters.
fn hexify(v: Value) -> Value {
    match v {
        Value::Array(a)
            if a.len() == 32 && a.iter().all(|x| x.as_u64().is_some_and(|n| n < 256)) =>
        {
            Value::String(
                a.iter()
                    .map(|x| format!("{:02x}", x.as_u64().unwrap()))
                    .collect(),
            )
        }
        Value::Array(a) => Value::Array(a.into_iter().map(hexify).collect()),
        Value::Object(m) => Value::Object(m.into_iter().map(|(k, v)| (k, hexify(v))).collect()),
        other => other,
    }
}

/// Serves engine A of a paired two-engine mesh (B sits to the right of A).
async fn serve_pair() -> String {
    let (a, _pa, _b, _pb) = two_engines().await;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        let _keep = (_pa, _b, _pb);
        axum::serve(l, router(a, "tok".into())).await.unwrap();
    });
    base
}

#[tokio::test(flavor = "multi_thread")]
async fn status_decodes_peers_and_links_from_the_real_agent() {
    let base = serve_pair().await;
    tokio::task::spawn_blocking(move || {
        let c = Client::with(&base, "tok".into());
        let s = c.status().unwrap();
        assert_eq!(s.peers.len(), 1);
        let p = &s.peers[0];
        assert!(p.connected && !p.path.is_empty() && p.rtt_us.is_some());
        assert_eq!(s.layout.links.len(), 1);
        assert!(
            s.layout
                .links
                .iter()
                .any(|l| l.from == s.me && l.from_edge == Edge::Right && l.to == p.id)
        );
        // layout(): move the peer to the left edge by name.
        c.layout("left", &p.name).unwrap();
        let s = c.status().unwrap();
        assert!(
            s.layout
                .links
                .iter()
                .any(|l| l.from == s.me && l.from_edge == Edge::Left)
        );
        // Mutations that need state that isn't there fail with the agent's own error.
        for r in [
            c.pair("nobody"),
            c.answer(&"ab".repeat(32), true),
            c.unpair(&"ab".repeat(32)),
        ] {
            assert!(
                matches!(r, Err(ClientError::Api { status: 404, .. })),
                "{r:?}"
            );
        }
        c.request_permissions().unwrap();
        c.shutdown().unwrap();
    })
    .await
    .unwrap();
}

#[test]
fn pending_discovered_and_check_decode_agent_shapes() {
    let id = DeviceId([0xab; 32]);
    let pending = hexify(
        serde_json::to_value(vec![AgentPending {
            peer: id,
            name: "mac".into(),
            code: "123456".into(),
        }])
        .unwrap(),
    );
    let p: Vec<novalink_client::PendingPair> = serde_json::from_value(pending).unwrap();
    assert_eq!(
        (p[0].peer.as_str(), p[0].code.as_str()),
        (&*"ab".repeat(32), "123456")
    );
    let d: Vec<novalink_client::Discovered> = serde_json::from_value(
        json!([{"id": "ab".repeat(32), "name": "pc", "addrs": ["1.2.3.4:5"]}]),
    )
    .unwrap();
    assert_eq!(d[0].name, "pc");
    let c: novalink_client::Check =
        serde_json::from_value(json!({"name":"a","ok":false,"detail":"bad","advice":"Fix it."}))
            .unwrap();
    assert!(!c.ok && c.advice == "Fix it.");
}

/// Fake agent: answers the first `good` hellos correctly, later ones wrongly; records
/// every other request.
fn fake_agent(good: usize) -> (String, Arc<Mutex<String>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(String::new()));
    let s2 = seen.clone();
    std::thread::spawn(move || {
        let mut hellos = 0;
        for conn in l.incoming() {
            let mut conn = conn.unwrap();
            let mut buf = [0u8; 4096];
            let n = conn.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let body = if let Some(rest) = req.split("nonce=").nth(1) {
                hellos += 1;
                let hex: String = rest.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
                let proof = if hellos <= good {
                    server_proof("tok", &decode_hex(&hex).unwrap())
                } else {
                    "00".into()
                };
                json!({ "proof": proof }).to_string()
            } else {
                s2.lock().unwrap().push_str(&req);
                "{}".to_string()
            };
            let _ = write!(
                conn,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (base, seen)
}

#[test]
fn proof_is_checked_on_every_request() {
    let (base, seen) = fake_agent(1);
    let c = Client::with(&base, "tok".into());
    c.metrics().unwrap();
    let first = seen.lock().unwrap().to_ascii_lowercase();
    assert!(first.contains("authorization"));
    seen.lock().unwrap().clear();
    // The port's owner changed: the second request must fail the proof and send no token.
    assert!(matches!(c.metrics(), Err(ClientError::Impostor)));
    assert!(seen.lock().unwrap().is_empty());
}

#[test]
fn slow_agent_is_a_timeout_not_an_impostor() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    std::thread::spawn(move || {
        let mut held = vec![];
        for conn in l.incoming() {
            held.push(conn.unwrap()); // accept, never answer
        }
    });
    let c = Client::with(&base, "tok".into());
    match c.status() {
        Err(ClientError::Other(m)) => assert!(m.contains("didn't answer in time"), "{m}"),
        other => panic!("{other:?}"),
    }
}

/// Turn Off from the tray: the client's kept-alive connection already carried /v1/hello,
/// and `main` aborts the API once shutdown is asked, while the engine takes a while to stop.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_is_answered_on_a_kept_alive_connection() {
    let (h, _p) = one_engine().await;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let mut api = tokio::spawn(novalink_agent::api::serve_on(l, h.clone(), "tok".into()));
    let main = tokio::spawn(async move {
        h.shutdown_requested().await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), &mut api).await;
        api.abort();
        // A slow engine shutdown keeps the process, and any open socket, alive.
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
    });
    let r = tokio::task::spawn_blocking(move || Client::with(&base, "tok".into()).shutdown())
        .await
        .unwrap();
    assert!(r.is_ok(), "{r:?}");
    main.abort();
}
