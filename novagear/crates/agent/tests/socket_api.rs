//! The Unix socket: the same routes and token rules as TCP, no Host check, mode 0600.
#![cfg(unix)]

use novagear_agent::{Agent, api};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TOKEN: &str = "agent-token-0123456789012345678901234567890123";

fn temp_dir(tag: &str) -> PathBuf {
    // Short: a socket path must fit in sun_path (104 bytes on macOS).
    let dir = std::env::temp_dir().join(format!("ng-sock-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn serve(tag: &str) -> PathBuf {
    let dir = temp_dir(tag);
    let agent = Agent::new(dir.join("config.json"), TOKEN.to_string()).unwrap();
    let sock = dir.join("a.sock");
    let listener = api::bind_socket(&sock).unwrap();
    tokio::spawn(async move {
        axum::serve(listener, api::socket_router(agent))
            .await
            .unwrap();
    });
    sock
}

async fn http(
    sock: &PathBuf,
    method: &str,
    path: &str,
    host: &str,
    token: Option<&str>,
    body: &str,
) -> (u16, String) {
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "{method} /api{path} HTTP/1.1\r\nHost: {host}\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut s = tokio::net::UnixStream::connect(sock).await.unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    let code = out.split(' ').nth(1).unwrap().parse().unwrap();
    (code, out.split("\r\n\r\n").nth(1).unwrap_or("").to_string())
}

#[tokio::test]
async fn socket_requires_token() {
    let sock = serve("tok").await;
    let (code, _) = http(&sock, "GET", "/status", "localhost", None, "").await;
    assert_eq!(code, 401);
}

#[tokio::test]
async fn socket_ignores_host() {
    let sock = serve("host").await;
    let (code, body) = http(&sock, "GET", "/status", "anything", Some(TOKEN), "").await;
    assert_eq!(code, 200, "{body}");
}

#[tokio::test]
async fn socket_client_rules_match_tcp() {
    let sock = serve("cli").await;
    let (code, body) = http(
        &sock,
        "POST",
        "/clients",
        "x",
        Some(TOKEN),
        &json!({ "name": "novaguard" }).to_string(),
    )
    .await;
    assert_eq!(code, 201, "{body}");
    let client = serde_json::from_str::<Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    let (code, body) = http(
        &sock,
        "PUT",
        "/feeds/novaguard.x",
        "x",
        Some(&client),
        &json!({ "value": 1 }).to_string(),
    )
    .await;
    assert!((200..300).contains(&code), "{code} {body}");
    let (code, _) = http(&sock, "GET", "/config", "x", Some(&client), "").await;
    assert_eq!(code, 403);
}

#[tokio::test]
async fn socket_file_is_0600() {
    let sock = serve("mode").await;
    let mode = std::fs::metadata(&sock).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[tokio::test]
async fn stale_socket_is_replaced() {
    let dir = temp_dir("stale");
    let sock = dir.join("a.sock");
    std::fs::write(&sock, b"leftover").unwrap();
    api::bind_socket(&sock).unwrap();
    let first = api::bind_socket(&sock);
    assert!(first.is_ok(), "rebinding over a live or stale file works");
}
