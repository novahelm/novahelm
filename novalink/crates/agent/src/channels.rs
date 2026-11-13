//! Service channels: forwards TCP connections on `127.0.0.1:7982` to a service on a paired
//! device, and incoming channels to a service registered on this device.
//!
//! Outgoing: a client sends `NOVALINK-CONNECT <peer-hex> <service> <api-token>\n`; the agent
//! answers `OK\n` or `ERR <reason>\n`, then pipes bytes both ways. The peer's agent confirms
//! the channel with a single `K` byte once its local service accepted (that byte is consumed
//! here, never shown to the client); a reset instead means the peer refused it.
//!
//! Incoming: the agent connects to `127.0.0.1:<port>` of the registered service, writes
//! `NOVALINK-FROM <peer-hex> <secret>\n`, then pipes. Channels need no licence.
//!
//! The registry is in memory only; services re-register on every start. Neither the API
//! token nor a service secret is ever logged.

use crate::engine::EngineHandle;
use anyhow::{Context, Result};
use novalink_proto::{DeviceId, valid_service_name};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const FORWARD_PORT: u16 = 7982;
pub const FORWARD_ADDR: &str = "127.0.0.1:7982";
/// The agent's own API port (`api::API_ADDR`).
const API_PORT: u16 = 7979;
/// A preface line longer than this (newline included) is refused.
const MAX_PREFACE: usize = 512;
const PREFACE_TIMEOUT: Duration = Duration::from_secs(5);
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
const LOCAL_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_SECRET: usize = 256;
/// Sent by the accepting agent once the local service is connected.
const CONFIRM: u8 = b'K';

struct Service {
    port: u16,
    secret: String,
}

/// The services this device offers to its paired devices. Cheap to clone.
#[derive(Clone, Default)]
pub struct Services(Arc<Mutex<HashMap<String, Service>>>);

impl Services {
    /// Registers (or replaces) `name` -> `127.0.0.1:port`. The secret is sent to the service
    /// in the `NOVALINK-FROM` line, so it must be one printable token.
    pub fn set(&self, name: &str, port: u16, secret: &str) -> Result<()> {
        anyhow::ensure!(valid_service_name(name), "bad service name");
        // Never the forwarder itself or the agent's own API: peers must not reach either through a service.
        anyhow::ensure!(
            port != 0 && port != FORWARD_PORT && port != API_PORT,
            "bad port"
        );
        anyhow::ensure!(
            !secret.is_empty()
                && secret.len() <= MAX_SECRET
                && secret.bytes().all(|b| b.is_ascii_graphic()),
            "bad secret"
        );
        self.0.lock().unwrap_or_else(|e| e.into_inner()).insert(
            name.to_string(),
            Service {
                port,
                secret: secret.to_string(),
            },
        );
        Ok(())
    }

    /// Returns whether it was registered.
    pub fn remove(&self, name: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name)
            .is_some()
    }

    fn get(&self, name: &str) -> Option<(u16, String)> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .map(|s| (s.port, s.secret.clone()))
    }
}

/// Handles a channel opened by a paired peer. Never blocks the caller.
pub fn accept_incoming(
    services: Services,
    peer: DeviceId,
    service: String,
    send: quinn::SendStream,
    recv: quinn::RecvStream,
) {
    tokio::spawn(async move {
        let Some((port, secret)) = services.get(&service) else {
            novalink_net::session::refuse_channel(send, recv);
            return;
        };
        let connect = TcpStream::connect(("127.0.0.1", port));
        let tcp = match tokio::time::timeout(LOCAL_CONNECT_TIMEOUT, connect).await {
            Ok(Ok(t)) => t,
            _ => {
                tracing::debug!(%service, "local service did not accept");
                novalink_net::session::refuse_channel(send, recv);
                return;
            }
        };
        if let Err(e) = pipe_incoming(tcp, peer, &secret, send, recv).await {
            tracing::debug!(%service, "channel ended: {e}");
        }
    });
}

async fn pipe_incoming(
    mut tcp: TcpStream,
    peer: DeviceId,
    secret: &str,
    mut send: quinn::SendStream,
    recv: quinn::RecvStream,
) -> std::io::Result<()> {
    tcp.write_all(format!("NOVALINK-FROM {peer} {secret}\n").as_bytes())
        .await?;
    send.write_all(&[CONFIRM])
        .await
        .map_err(std::io::Error::other)?;
    let mut quic = tokio::io::join(recv, send);
    tokio::io::copy_bidirectional(&mut tcp, &mut quic).await?;
    Ok(())
}

/// Binds the forwarder to loopback only; port 0 picks a free one (tests).
pub async fn bind_on(port: u16) -> Result<TcpListener> {
    TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("bind 127.0.0.1:{port}"))
}

/// Binds `127.0.0.1:7982`.
pub async fn bind() -> Result<TcpListener> {
    bind_on(FORWARD_PORT).await
}

/// Serves the forwarder until the task is dropped.
pub async fn serve_on(listener: TcpListener, handle: EngineHandle, token: String) -> Result<()> {
    let token: Arc<str> = token.into();
    loop {
        let (sock, _) = listener.accept().await?;
        let (handle, token) = (handle.clone(), token.clone());
        tokio::spawn(async move {
            if let Err(e) = client(sock, handle, &token).await {
                tracing::debug!("forward client ended: {e}");
            }
        });
    }
}

/// Reads one `\n`-terminated line byte by byte (so nothing after it is consumed).
async fn read_preface(sock: &mut TcpStream) -> Option<String> {
    let read = async {
        let mut line = Vec::with_capacity(128);
        loop {
            let b = sock.read_u8().await.ok()?;
            if b == b'\n' {
                return String::from_utf8(line).ok();
            }
            if line.len() + 1 >= MAX_PREFACE {
                return None;
            }
            line.push(b);
        }
    };
    tokio::time::timeout(PREFACE_TIMEOUT, read)
        .await
        .ok()
        .flatten()
}

async fn reply_err(sock: &mut TcpStream, why: &str) -> std::io::Result<()> {
    sock.write_all(format!("ERR {why}\n").as_bytes()).await?;
    sock.shutdown().await?;
    // Drain what the client already sent: closing with unread bytes sends an RST that can
    // discard the ERR line before the client reads it.
    let mut sink = [0u8; 512];
    let drain = async { while matches!(sock.read(&mut sink).await, Ok(n) if n > 0) {} };
    let _ = tokio::time::timeout(Duration::from_secs(1), drain).await;
    Ok(())
}

fn parse_peer(s: &str) -> Option<DeviceId> {
    crate::api::parse_id(s)
}

async fn client(mut sock: TcpStream, handle: EngineHandle, token: &str) -> std::io::Result<()> {
    let Some(line) = read_preface(&mut sock).await else {
        return reply_err(&mut sock, "bad preface").await;
    };
    let parts: Vec<&str> = line.split(' ').collect();
    let [cmd, peer, service, given] = parts[..] else {
        return reply_err(&mut sock, "bad preface").await;
    };
    if cmd != "NOVALINK-CONNECT" {
        return reply_err(&mut sock, "bad preface").await;
    }
    let ok: bool = given.as_bytes().ct_eq(token.as_bytes()).into();
    if !ok {
        return reply_err(&mut sock, "unauthorised").await;
    }
    let Some(peer) = parse_peer(peer) else {
        return reply_err(&mut sock, "bad peer").await;
    };
    if !valid_service_name(service) {
        return reply_err(&mut sock, "bad service").await;
    }
    let opened = tokio::time::timeout(OPEN_TIMEOUT, handle.manager().open_channel(peer, service));
    let (send, mut recv) = match opened.await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::debug!("open channel failed: {e:#}");
            return reply_err(&mut sock, "peer unavailable").await;
        }
        Err(_) => return reply_err(&mut sock, "timeout").await,
    };
    // The peer confirms once its local service accepted; a reset means it refused.
    let mut first = [0u8; 1];
    let confirmed = matches!(
        tokio::time::timeout(OPEN_TIMEOUT, recv.read_exact(&mut first)).await,
        Ok(Ok(())) if first[0] == CONFIRM
    );
    if !confirmed {
        return reply_err(&mut sock, "no such service").await;
    }
    sock.write_all(b"OK\n").await?;
    let mut quic = tokio::io::join(recv, send);
    tokio::io::copy_bidirectional(&mut sock, &mut quic).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_validates_and_replaces() {
        let s = Services::default();
        assert!(s.set("compute", 1234, "sec").is_ok());
        assert!(s.set("Bad Name", 1234, "sec").is_err());
        assert!(s.set("compute", 0, "sec").is_err());
        assert!(s.set("compute", FORWARD_PORT, "sec").is_err());
        assert!(s.set("compute", 7979, "sec").is_err());
        assert!(s.set("compute", 1, "has space").is_err());
        assert!(s.set("compute", 1, "new\nline").is_err());
        assert!(s.set("compute", 1, "").is_err());
        assert!(s.set("compute", 4321, "other").is_ok());
        assert_eq!(s.get("compute"), Some((4321, "other".to_string())));
        assert!(s.remove("compute"));
        assert!(!s.remove("compute"));
        assert_eq!(s.get("compute"), None);
    }

    #[tokio::test]
    async fn listener_is_loopback_only() {
        let addr: std::net::SocketAddr = FORWARD_ADDR.parse().unwrap();
        assert!(addr.ip().is_loopback());
        assert_eq!(addr.port(), 7982);
        let l = bind_on(0).await.unwrap();
        assert!(l.local_addr().unwrap().ip().is_loopback());
    }
}
