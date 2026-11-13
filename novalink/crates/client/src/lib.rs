//! Blocking client for the local agent API, shared by the CLI and the tray.
//!
//! Before the token is sent anywhere, the listener has to prove it knows it (`/v1/hello`).

mod model;

pub use model::*;

use novalink_agent::api::{encode_hex, new_nonce, verify_proof};
use reqwest::blocking::Client as Http;
use reqwest::{Method, header};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::time::Duration;

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

/// How long a request may take. Starting a pairing waits for the session to come up, so
/// it gets longer whoever sends it (the tray, or the CLI through [`Client::request`]).
fn timeout_for(method: &Method, path: &str) -> Duration {
    if *method == Method::POST && path == "/v1/pair" {
        PAIR_TIMEOUT
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

pub struct Client {
    http: Http,
    base: String,
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
        let http = Http::builder()
            .timeout(TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .expect("http client");
        Client {
            http,
            base: base.trim_end_matches('/').to_string(),
            token,
        }
    }

    /// Makes sure the listener knows the token before the token is sent to it. Checked on
    /// every request: a long-lived client must not trust a port whose owner may have changed.
    fn prove(&self) -> Result<(), ClientError> {
        let nonce = new_nonce();
        let hello = self
            .http
            .get(format!("{}/v1/hello", self.base))
            .query(&[("nonce", encode_hex(&nonce))])
            .header(header::HOST, HOST)
            .send()
            .map_err(|e| match () {
                _ if e.is_connect() => ClientError::NotRunning,
                _ if e.is_timeout() => timed_out(),
                // Not speaking HTTP is as good as no proof.
                _ => ClientError::Impostor,
            })?;
        let ok = hello.status().is_success();
        let body = hello.json::<Value>().map_err(|e| {
            if e.is_timeout() {
                timed_out()
            } else {
                ClientError::Impostor
            }
        });
        let proof = match (ok, body) {
            (true, Ok(v)) => v["proof"].as_str().map(str::to_string),
            (_, Err(e @ ClientError::Other(_))) => return Err(e),
            _ => None,
        };
        if !proof.is_some_and(|p| verify_proof(&self.token, &nonce, &p)) {
            return Err(ClientError::Impostor);
        }
        Ok(())
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
        self.prove()?;
        let mut rb = self
            .http
            .request(method, format!("{}{path}", self.base))
            .timeout(timeout)
            .header(header::HOST, HOST)
            .header(header::AUTHORIZATION, format!("Bearer {}", self.token));
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        let resp = rb.send().map_err(|e| {
            if e.is_connect() {
                ClientError::NotRunning
            } else if e.is_timeout() {
                timed_out()
            } else {
                ClientError::Other(e.to_string())
            }
        })?;
        let status = resp.status();
        let v: Value = resp.json().unwrap_or(Value::Null);
        if !status.is_success() {
            let message = v["error"].as_str().unwrap_or("request failed").to_string();
            return Err(ClientError::Api {
                status: status.as_u16(),
                message,
            });
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
    }
}
