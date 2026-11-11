//! `/api/status` end to end: a real router on an ephemeral port, plain HTTP over a socket.

use novagear_agent::{Agent, api};
use novagear_core::API_PORT;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("novagear-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn get(addr: std::net::SocketAddr, path: &str, token: &str) -> (u16, String) {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{API_PORT}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    let code = out.split(' ').nth(1).unwrap().parse().unwrap();
    let body = out.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (code, body)
}

async fn serve(dir: &std::path::Path, token: &str) -> std::net::SocketAddr {
    let agent = Agent::new(dir.join("config.json"), token.to_string()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, api::router(agent)).await.unwrap();
    });
    addr
}

#[tokio::test]
async fn status_reports_bad_records() {
    let dir = temp_dir("status-bad");
    std::fs::write(
        dir.join("devices.toml"),
        "[[device]]\nusb = \"1532:0099\"\nname = \"Broken\"\nkind = \"mouse\"\nfamily = \"warpdrive\"\n",
    )
    .unwrap();
    let token = "t".repeat(48);
    let addr = serve(&dir, &token).await;

    let (code, body) = get(addr, "/api/status", &token).await;
    assert_eq!(code, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["problems"].as_array().unwrap().len(), 1);
    assert_eq!(v["network_blocked"], false);
    assert_eq!(v["firmware_updates"], 0);
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));

    let (code, _) = get(addr, "/api/status", "wrong").await;
    assert_eq!(code, 401);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn status_reports_mode() {
    let dir = temp_dir("status-mode");
    let token = "t".repeat(48);
    let addr = serve(&dir, &token).await;

    let (code, body) = get(addr, "/api/status", &token).await;
    assert_eq!(code, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["mode"]["id"], "main");
    assert_eq!(v["mode"]["name"], "Main");
    assert!(v["mode"]["accent"].as_str().unwrap().starts_with('#'));
    assert_eq!(v["modes"][0]["id"], "main");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn panel_has_modes_view() {
    let dir = temp_dir("panel-modes");
    let token = "t".repeat(48);
    let addr = serve(&dir, &token).await;

    let (code, body) = get(addr, "/", "").await;
    assert_eq!(code, 200);
    assert!(body.contains(r#"id="modes""#));
    assert!(body.contains(r#"id="approvals""#));
    assert!(body.contains("Deny"));
    // Only the deck approves: the panel never offers it.
    assert!(!body.contains("approve("));
    let _ = std::fs::remove_dir_all(&dir);
}
