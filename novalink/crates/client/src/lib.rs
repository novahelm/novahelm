//! Blocking client for the local agent API, shared by the CLI and the tray.
//!
//! Before the token is sent anywhere, the listener has to prove it knows it (`/v1/hello`).

mod model;

pub use model::*;

use http::Method;
use novalink_agent::api::{encode_hex, new_nonce, verify_proof};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

const BASE: &str = "http://127.0.0.1:7979";
/// The agent only accepts its own address in the Host header.
const HOST: &str = "127.0.0.1:7979";
const TIMEOUT: Duration = Duration::from_secs(2);
/// The agent is on this machine, so a connection opens in milliseconds or not at all. Windows takes about two seconds
/// to refuse a closed local port, which used to lose the race with TIMEOUT and report "didn't answer in time" when the
/// agent simply wasn't running. Giving up on the dial sooner keeps that case a connect error on every system.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(750);
/// Pairing waits for the session to come up, and each dead address costs a dial timeout.
const PAIR_TIMEOUT: Duration = Duration::from_secs(10);
/// On Windows the doctor asks PowerShell and powercfg about the network adapter's wake settings, each allowed 5 s,
/// after the network and firewall probe. A cold PowerShell alone can take longer than [`TIMEOUT`].
const DOCTOR_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a request may take. Starting a pairing waits for the session to come up, so
/// it gets longer whoever sends it (the tray, or the CLI through [`Client::request`]).
fn timeout_for(method: &Method, path: &str) -> Duration {
    if *method == Method::POST && path == "/v1/pair" {
        PAIR_TIMEOUT
    } else if *method == Method::GET && path == "/v1/doctor" {
        DOCTOR_TIMEOUT
    } else {
        TIMEOUT
    }
}

fn timed_out() -> ClientError {
    ClientError::Other(
        "The NovaLink agent didn't answer in time. Try again; if it keeps happening, run novalink doctor."
            .into(),
    )
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("NovaLink agent isn't running. Start it with: novalink-agent")]
    NotRunning,
    #[error("Something else is listening on 127.0.0.1:7979 — refusing to send the NovaLink token.")]
    Impostor,
    #[error("The NovaLink API token is missing. Start the agent once to create it.")]
    TokenMissing,
    #[error("{message} (HTTP {status})")]
    Api { status: u16, message: String },
    #[error("{0}")]
    Other(String),
}

/// Why an exchange on a [`Conn`] stopped.
enum Broken {
    TimedOut,
    /// The listener hung up before a whole reply arrived.
    Closed,
    /// What came back wasn't an HTTP reply this client reads.
    Malformed,
}

/// The largest reply this client reads. The agent's own replies are a few kilobytes.
const MAX_REPLY: usize = 16 * 1024 * 1024;
const MAX_HEAD: usize = 64 * 1024;

/// One TCP connection to the agent. The proof and the request that carries the token both
/// travel on it, so the token can only reach the listener that proved itself. An HTTP
/// client with a connection pool can't promise that: when the first connection is gone it
/// quietly dials again, and by then the port may belong to someone else.
struct Conn {
    stream: TcpStream,
    /// Bytes read past the end of the last reply.
    buf: Vec<u8>,
}

impl Conn {
    /// Sends one request and reads its reply: the status and the body. `close` asks the
    /// agent to end the connection afterwards, which also lets a reply without a length
    /// run to the end of the stream.
    fn exchange(
        &mut self,
        head: &str,
        body: &[u8],
        close: bool,
        deadline: Instant,
    ) -> Result<(u16, Vec<u8>), Broken> {
        self.arm(deadline)?;
        self.stream
            .write_all(head.as_bytes())
            .and_then(|_| self.stream.write_all(body))
            .map_err(broken)?;
        let end = loop {
            if let Some(i) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            if self.buf.len() > MAX_HEAD {
                return Err(Broken::Malformed);
            }
            if self.fill(deadline)? == 0 {
                // Nothing at all is a hang-up; half a head is not HTTP.
                return Err(if self.buf.is_empty() {
                    Broken::Closed
                } else {
                    Broken::Malformed
                });
            }
        };
        let head: Vec<u8> = self.buf.drain(..end).collect();
        let head = std::str::from_utf8(&head).map_err(|_| Broken::Malformed)?;
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .and_then(|l| l.strip_prefix("HTTP/1.1 "))
            .and_then(|l| l.get(..3))
            .and_then(|code| code.parse::<u16>().ok())
            .ok_or(Broken::Malformed)?;
        let mut length = None;
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if name.eq_ignore_ascii_case("content-length") {
                length = Some(
                    value
                        .trim()
                        .parse::<usize>()
                        .map_err(|_| Broken::Malformed)?,
                );
            } else if name.eq_ignore_ascii_case("transfer-encoding") {
                // The agent always sends a length.
                return Err(Broken::Malformed);
            }
        }
        match length {
            Some(n) if n > MAX_REPLY => Err(Broken::Malformed),
            Some(n) => {
                while self.buf.len() < n {
                    if self.fill(deadline)? == 0 {
                        return Err(Broken::Closed);
                    }
                }
                Ok((status, self.buf.drain(..n).collect()))
            }
            None if close => {
                while self.fill(deadline)? > 0 {
                    if self.buf.len() > MAX_REPLY {
                        return Err(Broken::Malformed);
                    }
                }
                Ok((status, std::mem::take(&mut self.buf)))
            }
            // Without a length there is no telling where this reply ends and the next begins.
            None => Err(Broken::Malformed),
        }
    }

    /// Lets the next read or write wait until `deadline` and no longer.
    fn arm(&self, deadline: Instant) -> Result<(), Broken> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Broken::TimedOut);
        }
        self.stream
            .set_read_timeout(Some(left))
            .and_then(|_| self.stream.set_write_timeout(Some(left)))
            .map_err(broken)
    }

    /// Reads more of the reply into `buf`; 0 means the agent closed the connection.
    fn fill(&mut self, deadline: Instant) -> Result<usize, Broken> {
        self.arm(deadline)?;
        let mut chunk = [0u8; 8192];
        let n = self.stream.read(&mut chunk).map_err(broken)?;
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(n)
    }
}

fn broken(e: std::io::Error) -> Broken {
    match e.kind() {
        // A socket timeout is WouldBlock on Unix and TimedOut on Windows.
        ErrorKind::WouldBlock | ErrorKind::TimedOut => Broken::TimedOut,
        _ => Broken::Closed,
    }
}

pub struct Client {
    /// `None` when the base address made no sense; every request then fails as not running.
    addr: Option<SocketAddr>,
    token: String,
}

impl Client {
    /// The agent on this machine, with the token from the config directory.
    pub fn local() -> Result<Client, ClientError> {
        let path = novalink_agent::paths::config_dir().join("api-token");
        let token = std::fs::read_to_string(path).map_err(|_| ClientError::TokenMissing)?;
        Ok(Client::with(BASE, token.trim().to_string()))
    }

    pub fn with(base: &str, token: String) -> Client {
        let host = base.trim_end_matches('/').trim_start_matches("http://");
        Client {
            addr: host.to_socket_addrs().ok().and_then(|mut a| a.next()),
            token,
        }
    }

    /// Opens a connection and makes sure the listener knows the token before the token is
    /// sent to it. Done for every request: a long-lived client must not trust a port whose
    /// owner may have changed. The proven connection is returned so the token can follow
    /// on it, and only on it.
    fn prove(&self) -> Result<Conn, ClientError> {
        let addr = self.addr.ok_or(ClientError::NotRunning)?;
        let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
            .map_err(|_| ClientError::NotRunning)?;
        let _ = stream.set_nodelay(true);
        let mut conn = Conn {
            stream,
            buf: Vec::new(),
        };
        let nonce = new_nonce();
        let head = format!(
            "GET /v1/hello?nonce={} HTTP/1.1\r\nHost: {HOST}\r\nAccept: application/json\r\n\r\n",
            encode_hex(&nonce)
        );
        let (status, body) = conn
            .exchange(&head, &[], false, Instant::now() + TIMEOUT)
            .map_err(|e| match e {
                Broken::TimedOut => timed_out(),
                // Not speaking HTTP is as good as no proof.
                Broken::Closed | Broken::Malformed => ClientError::Impostor,
            })?;
        let proof = (200..300)
            .contains(&status)
            .then(|| serde_json::from_slice::<Value>(&body).ok())
            .flatten()
            .and_then(|v| v["proof"].as_str().map(str::to_string));
        if !proof.is_some_and(|p| verify_proof(&self.token, &nonce, &p)) {
            return Err(ClientError::Impostor);
        }
        Ok(conn)
    }

    /// One authenticated request, returning the JSON body. The CLI uses this directly.
    pub fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, ClientError> {
        let timeout = timeout_for(&method, path);
        self.request_within(method, path, body, timeout)
    }

    fn request_within(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, ClientError> {
        let mut conn = self.prove()?;
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {HOST}\r\nAuthorization: Bearer {}\r\nAccept: application/json\r\nConnection: close\r\nContent-Length: {}\r\n",
            self.token,
            body.len()
        );
        if !body.is_empty() {
            head.push_str("Content-Type: application/json\r\n");
        }
        head.push_str("\r\n");
        let (status, reply) = conn
            .exchange(&head, body.as_bytes(), true, Instant::now() + timeout)
            .map_err(|e| match e {
                Broken::TimedOut => timed_out(),
                // No second connection: whoever answers a new one hasn't proved anything.
                Broken::Closed => ClientError::Other(
                    "The NovaLink agent closed the connection before answering. Try again; if it keeps happening, run novalink doctor."
                        .into(),
                ),
                Broken::Malformed => {
                    ClientError::Other("The NovaLink agent sent a reply that isn't HTTP.".into())
                }
            })?;
        let v: Value = serde_json::from_slice(&reply).unwrap_or(Value::Null);
        if !(200..300).contains(&status) {
            let message = v["error"].as_str().unwrap_or("request failed").to_string();
            return Err(ClientError::Api { status, message });
        }
        Ok(v)
    }

    fn typed<T: DeserializeOwned>(&self, v: Value) -> Result<T, ClientError> {
        serde_json::from_value(v).map_err(|e| ClientError::Other(format!("unexpected reply: {e}")))
    }

    pub fn status(&self) -> Result<Status, ClientError> {
        self.typed(self.request(Method::GET, "/v1/status", None)?)
    }

    pub fn pending(&self) -> Result<Vec<PendingPair>, ClientError> {
        self.typed(self.request(Method::GET, "/v1/pair/pending", None)?)
    }

    pub fn discovered(&self) -> Result<Vec<Discovered>, ClientError> {
        self.typed(self.request(Method::GET, "/v1/discovered", None)?)
    }

    pub fn doctor(&self) -> Result<Vec<Check>, ClientError> {
        let v = self.request(Method::GET, "/v1/doctor", None)?;
        self.typed(v["checks"].clone())
    }

    pub fn metrics(&self) -> Result<Value, ClientError> {
        self.request(Method::GET, "/v1/metrics", None)
    }

    /// Starts pairing with a discovered device, by name or id.
    pub fn pair(&self, target: &str) -> Result<(), ClientError> {
        let body = json!({ "name": target });
        self.request(Method::POST, "/v1/pair", Some(body)).map(drop)
    }

    pub fn answer(&self, id: &str, accept: bool) -> Result<(), ClientError> {
        let body = json!({ "id": id, "accept": accept });
        self.request(Method::POST, "/v1/pair/answer", Some(body))
            .map(drop)
    }

    pub fn unpair(&self, id: &str) -> Result<(), ClientError> {
        self.request(Method::DELETE, &format!("/v1/devices/{id}"), None)
            .map(drop)
    }

    pub fn layout(&self, edge: &str, peer: &str) -> Result<(), ClientError> {
        let body = json!({ "edge": edge, "peer": peer });
        self.request(Method::PUT, "/v1/layout", Some(body))
            .map(drop)
    }

    pub fn request_permissions(&self) -> Result<(), ClientError> {
        self.request(Method::POST, "/v1/permissions/request", None)
            .map(drop)
    }

    pub fn shutdown(&self) -> Result<(), ClientError> {
        self.request(Method::POST, "/v1/shutdown", None).map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starting_a_pairing_gets_the_long_timeout_whoever_asks() {
        assert_eq!(timeout_for(&Method::POST, "/v1/pair"), PAIR_TIMEOUT);
        assert_eq!(timeout_for(&Method::POST, "/v1/pair/answer"), TIMEOUT);
        assert_eq!(timeout_for(&Method::GET, "/v1/pair/pending"), TIMEOUT);
        assert_eq!(timeout_for(&Method::GET, "/v1/status"), TIMEOUT);
        assert_eq!(timeout_for(&Method::GET, "/v1/doctor"), DOCTOR_TIMEOUT);
    }
}
