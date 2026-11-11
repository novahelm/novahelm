//! A blocking, std-only client for the local agent, shared by the CLI and the tray so
//! neither carries an HTTP library.

use crate::config::read_token;
use crate::proof::{agent_proof, new_nonce, same};
use crate::{API_PORT, api_base};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

fn connect(port: u16) -> Result<TcpStream> {
    TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(2))
        .context("the NovaGear agent is not running")
}

fn request_head(method: &str, path: &str, token: &str, body_len: usize) -> String {
    format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{API_PORT}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {body_len}\r\n\r\n"
    )
}

/// Opens a connection and checks, on that same connection, that the listener knows the
/// token, without sending it: a fresh nonce goes to `/api/hello` on a keep-alive
/// connection, and only an HMAC of it keyed by the token is accepted. The caller then
/// sends its real request, token included, down the socket that was verified.
fn verified_connection(port: u16, token: &str) -> Result<BufReader<TcpStream>> {
    let nonce = new_nonce()?;
    let s = connect(port)?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut r = BufReader::new(s);
    r.get_mut().write_all(
        format!(
            "GET /api/hello?nonce={nonce} HTTP/1.1\r\nHost: 127.0.0.1:{API_PORT}\r\nConnection: keep-alive\r\n\r\n"
        )
        .as_bytes(),
    )?;
    let proof = read_hello(&mut r).unwrap_or_default();
    if !same(&proof, &agent_proof(token, &nonce)) {
        bail!(
            "something other than the NovaGear agent is listening on 127.0.0.1:{API_PORT}; not sending it the token"
        );
    }
    Ok(r)
}

/// Reads exactly one hello response (it must carry a Content-Length) and returns its proof,
/// leaving the connection positioned for the next request. Sizes are capped so a squatter
/// can't make the client read forever.
fn read_hello(r: &mut BufReader<TcpStream>) -> Option<String> {
    let mut len = None;
    let mut head_bytes = 0;
    loop {
        let mut line = String::new();
        head_bytes += (&mut *r).take(1024).read_line(&mut line).ok()?;
        if head_bytes > 4096 || line.is_empty() {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse::<usize>().ok();
        }
    }
    let len = len.filter(|&n| n <= 1024)?;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).ok()?;
    let v: Value = serde_json::from_slice(&body).ok()?;
    v["proof"].as_str().map(str::to_string)
}

/// Checks the agent without making a request, for callers that hand the token to someone
/// else (the browser, for the panel).
pub fn verify_agent(token: &str) -> Result<()> {
    verified_connection(API_PORT, token).map(drop)
}

/// One API call. A non-2xx answer becomes an error carrying the agent's message.
pub fn call(method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
    call_as(&read_token()?, method, path, body, Duration::from_secs(10))
}

/// `call` sending a given token (a client's, say) and waiting up to `timeout` for the
/// answer, for long polls. The agent is verified with the agent's own token first: only
/// it can sign `/hello`, and the client token is sent only down the verified connection.
pub fn call_as(
    send_token: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
    timeout: Duration,
) -> Result<Value> {
    call_at(
        API_PORT,
        &read_token()?,
        send_token,
        method,
        path,
        body,
        timeout,
    )
}

/// `call_as` on another port, with the verifying token given (tests serve on an ephemeral one).
pub fn call_at(
    port: u16,
    verify_token: &str,
    send_token: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
    timeout: Duration,
) -> Result<Value> {
    let mut r = verified_connection(port, verify_token)?;
    r.get_ref().set_read_timeout(Some(timeout))?;
    let body = body.map(Value::to_string).unwrap_or_default();
    r.get_mut()
        .write_all(request_head(method, path, send_token, body.len()).as_bytes())?;
    r.get_mut().write_all(body.as_bytes())?;
    let mut raw = String::new();
    r.read_to_string(&mut raw)?;
    let (head, body) = raw
        .split_once("\r\n\r\n")
        .context("bad response from the agent")?;
    let ok = head
        .split_whitespace()
        .nth(1)
        .is_some_and(|c| c.starts_with('2'));
    let v: Value = serde_json::from_str(body).unwrap_or(Value::String(body.to_string()));
    if !ok {
        bail!("{}", v.get("error").and_then(Value::as_str).unwrap_or(body));
    }
    Ok(v)
}

pub fn action(a: &Value) -> Result<()> {
    call("POST", "/api/action", Some(a)).map(drop)
}

/// A feed key goes into the request path, so only the characters the agent accepts do.
fn feed_path(key: &str) -> Result<String> {
    let ok = !key.is_empty()
        && key.len() <= 64
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if !ok {
        bail!("a feed key is letters, digits, . _ - (at most 64), like novaguard.builds.running");
    }
    Ok(format!("/api/feeds/{key}"))
}

/// Sets a feed with the agent token. `state` is ok, warn or bad; the agent checks it.
pub fn feed_set(key: &str, value: Value, state: Option<&str>, ttl_s: Option<u64>) -> Result<()> {
    let mut body = serde_json::json!({ "value": value });
    if let Some(s) = state {
        body["state"] = s.into();
    }
    if let Some(t) = ttl_s {
        body["ttl_s"] = t.into();
    }
    call("PUT", &feed_path(key)?, Some(&body)).map(drop)
}

pub fn feed_clear(key: &str) -> Result<()> {
    call("DELETE", &feed_path(key)?, None).map(drop)
}

/// Makes a client and returns its token, which the agent shows only this once.
pub fn client_add(name: &str) -> Result<String> {
    let r = call(
        "POST",
        "/api/clients",
        Some(&serde_json::json!({ "name": name })),
    )?;
    r["token"]
        .as_str()
        .map(str::to_string)
        .context("agent gave no token")
}

/// Follows the agent's event stream, calling `on_event` with each event's JSON text, until
/// the connection drops. Returns how it ended so the caller can reconnect.
pub fn follow_events(mut on_event: impl FnMut(&str)) -> Result<()> {
    let token = read_token()?;
    let mut r = verified_connection(API_PORT, &token)?;
    // Longer than the agent's 15 s keep-alive, so a silent hang shows up as an error.
    r.get_ref()
        .set_read_timeout(Some(Duration::from_secs(45)))?;
    r.get_mut()
        .write_all(request_head("GET", "/api/events", &token, 0).as_bytes())?;
    let mut in_body = false;
    for line in r.lines() {
        let line = line?;
        if !in_body {
            if line.starts_with("HTTP/")
                && !line.split_whitespace().nth(1).is_some_and(|c| c == "200")
            {
                bail!("agent refused the event stream: {line}");
            }
            in_body = line.is_empty();
            continue;
        }
        if let Some(data) = line
            .strip_prefix("data: ")
            .or_else(|| line.strip_prefix("data:"))
        {
            on_event(data);
        }
    }
    bail!("event stream closed")
}

/// The panel's address. It carries a single-use, one-minute code from the verified agent,
/// never the token, so nothing long-lived lands in browser history; the page trades the code
/// for a session of its own.
pub fn panel_url(page: &str) -> Result<String> {
    let code = call("POST", "/api/panel-code", None)?;
    let code = code["code"].as_str().context("agent gave no panel code")?;
    Ok(format!("{}/{page}#code={code}", api_base()))
}

pub fn open_url(url: &str) -> Result<()> {
    let status = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).status()
    } else if cfg!(windows) {
        // The shell's URL handler, never cmd.exe, which would treat `&` and `|` as commands.
        std::process::Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .status()
    } else {
        std::process::Command::new("xdg-open").arg(url).status()
    };
    if !status?.success() {
        bail!("could not open a browser; visit {}/ yourself", api_base());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_keys_are_checked_before_they_reach_a_path() {
        assert_eq!(feed_path("a.b-c_1").unwrap(), "/api/feeds/a.b-c_1");
        for bad in ["", "a/b", "a b", "a?x=1", "../config", &"k".repeat(65)] {
            assert!(feed_path(bad).is_err(), "{bad}");
        }
    }
}
