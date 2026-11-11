//! Elgato Key Light, Key Light Air and Ring Light: a tiny JSON API on port 9123, found by
//! mDNS as `_elg._tcp`. Spoken over a raw HTTP/1.1 socket so the agent carries no HTTP
//! client library.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub const SERVICE: &str = "_elg._tcp.local.";
pub const PORT: u16 = 9123;
const TIMEOUT: Duration = Duration::from_secs(3);
/// Real answers are a few hundred bytes; anything bigger is not a light.
const MAX_RESPONSE: usize = 64 * 1024;

/// Whether a discovered address is somewhere a Key Light can be: a private or link-local
/// IPv4 address on the lights' port. Loopback, public and IPv6 addresses are refused.
pub fn is_lan_light(addr: SocketAddr) -> bool {
    match addr {
        SocketAddr::V4(a) => a.port() == PORT && (a.ip().is_private() || a.ip().is_link_local()),
        SocketAddr::V6(_) => false,
    }
}

/// `EHOSTUNREACH` / `ENETUNREACH` on this OS. macOS reports these ("No route to host") when
/// Local Network privacy blocks an app that the shell can still reach the light from.
#[cfg(target_os = "macos")]
const UNREACHABLE: [i32; 2] = [65, 51];
#[cfg(target_os = "windows")]
const UNREACHABLE: [i32; 2] = [10065, 10051];
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const UNREACHABLE: [i32; 2] = [113, 101];

/// Whether `e` (or anything in its cause chain) is a host- or network-unreachable socket
/// error, the signature of a blocked Local Network. Refused connections and timeouts are
/// not: the light is reachable then, just not answering.
pub fn is_network_blocked(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(is_unreachable_io)
    })
}

/// Whether one socket error is host- or network-unreachable (see `is_network_blocked`).
pub fn is_unreachable_io(e: &std::io::Error) -> bool {
    e.raw_os_error()
        .is_some_and(|code| UNREACHABLE.contains(&code))
}

/// The light's own units: `temperature` is in mireds, 143 (7000 K) to 344 (2900 K).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LightState {
    pub on: u8,
    pub brightness: u8,
    pub temperature: u16,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Lights {
    number_of_lights: u8,
    lights: Vec<LightState>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessoryInfo {
    pub product_name: String,
    #[serde(default)]
    pub display_name: String,
    pub serial_number: String,
    pub firmware_version: String,
    /// Which Key Light model this is; picks the matching firmware image.
    #[serde(default)]
    pub hardware_board_type: u8,
    #[serde(default)]
    pub firmware_build_number: u32,
}

pub fn kelvin_to_mired(k: u16) -> u16 {
    (1_000_000 / u32::from(k.clamp(2900, 7000))).clamp(143, 344) as u16
}

pub fn mired_to_kelvin(m: u16) -> u16 {
    // Round to the nearest 50 K, as Control Center shows it.
    let k = 1_000_000 / u32::from(m.clamp(143, 344));
    ((k + 25) / 50 * 50) as u16
}

async fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let run = async {
        let mut s = TcpStream::connect(addr).await?;
        let body = body.unwrap_or_default();
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        s.write_all(head.as_bytes()).await?;
        s.write_all(body).await?;
        // The lights answer `Connection: keep-alive` whatever we ask for, so stop at the
        // end of the body instead of waiting for the socket to close.
        let mut buf = Vec::with_capacity(512);
        let mut chunk = [0u8; 1024];
        while !complete(&buf) {
            let n = s.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.len() > MAX_RESPONSE {
                bail!("light response over {MAX_RESPONSE} bytes");
            }
        }
        anyhow::Ok(buf)
    };
    let raw = tokio::time::timeout(TIMEOUT, run)
        .await
        .context("light timed out")??;
    parse_response(&raw)
}

/// Whether `raw` holds a whole response: headers plus `Content-Length` bytes, or a
/// chunked body through its final empty chunk.
fn complete(raw: &[u8]) -> bool {
    let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
    let body = &raw[split + 4..];
    if head.contains("transfer-encoding: chunked") {
        return body.ends_with(b"0\r\n\r\n");
    }
    let len = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok());
    // No length and not chunked: only the socket closing ends the body.
    len.is_some_and(|n| body.len() >= n)
}

/// Body of a complete response, failing on a non-2xx status.
fn parse_response(raw: &[u8]) -> Result<Vec<u8>> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("bad HTTP response")?;
    let head = std::str::from_utf8(&raw[..split])?;
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .context("bad status line")?;
    if !(200..300).contains(&status) {
        bail!("light answered HTTP {status}");
    }
    let body = &raw[split + 4..];
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        return Ok(dechunk(body));
    }
    Ok(body.to_vec())
}

fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(nl) = b.windows(2).position(|w| w == b"\r\n") {
        let size = std::str::from_utf8(&b[..nl])
            .ok()
            .and_then(|s| usize::from_str_radix(s.trim(), 16).ok());
        let Some(size) = size.filter(|&n| n > 0 && nl + 2 + n <= b.len()) else {
            break;
        };
        out.extend_from_slice(&b[nl + 2..nl + 2 + size]);
        b = &b[(nl + 4 + size).min(b.len())..];
    }
    out
}

pub async fn get(addr: SocketAddr) -> Result<LightState> {
    let body = request(addr, "GET", "/elgato/lights", None).await?;
    let l: Lights = serde_json::from_slice(&body)?;
    l.lights
        .into_iter()
        .next()
        .context("light reported no lights")
}

pub async fn set(addr: SocketAddr, state: LightState) -> Result<LightState> {
    let body = serde_json::to_vec(&Lights {
        number_of_lights: 1,
        lights: vec![state],
    })?;
    let back = request(addr, "PUT", "/elgato/lights", Some(&body)).await?;
    let l: Lights = serde_json::from_slice(&back)?;
    l.lights
        .into_iter()
        .next()
        .context("light reported no lights")
}

pub async fn info(addr: SocketAddr) -> Result<AccessoryInfo> {
    let body = request(addr, "GET", "/elgato/accessory-info", None).await?;
    Ok(serde_json::from_slice(&body)?)
}

/// Blinks the light so you can tell which one it is.
pub async fn identify(addr: SocketAddr) -> Result<()> {
    request(addr, "POST", "/elgato/identify", None)
        .await
        .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accessory_info_reads_board_and_build_and_tolerates_their_absence() {
        let i: AccessoryInfo = serde_json::from_str(
            r#"{"productName":"Elgato Key Light","hardwareBoardType":53,"firmwareBuildNumber":200,"firmwareVersion":"1.0.3","serialNumber":"S","displayName":"L"}"#,
        )
        .unwrap();
        assert_eq!((i.hardware_board_type, i.firmware_build_number), (53, 200));
        let old: AccessoryInfo = serde_json::from_str(
            r#"{"productName":"x","firmwareVersion":"1.0.0","serialNumber":"S"}"#,
        )
        .unwrap();
        assert_eq!((old.hardware_board_type, old.firmware_build_number), (0, 0));
    }

    #[test]
    fn kelvin_and_mired_convert_within_range() {
        assert_eq!(kelvin_to_mired(5000), 200);
        assert_eq!(
            kelvin_to_mired(1000),
            344,
            "clamped to the warmest the light does"
        );
        assert_eq!(mired_to_kelvin(188), 5300);
        assert_eq!(mired_to_kelvin(143), 7000);
    }

    #[test]
    fn parses_plain_and_chunked_bodies() {
        let plain = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
        assert_eq!(parse_response(plain).unwrap(), b"{}");
        let chunked =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{\"\r\n1\r\n}\r\n0\r\n\r\n";
        assert_eq!(parse_response(chunked).unwrap(), b"{\"}");
        assert!(parse_response(b"HTTP/1.1 404 Not Found\r\n\r\n").is_err());
    }

    #[test]
    fn only_lan_ipv4_on_the_light_port_is_trusted() {
        let ok = |s: &str| is_lan_light(s.parse().unwrap());
        assert!(ok("192.168.0.125:9123"));
        assert!(ok("10.1.2.3:9123"));
        assert!(ok("169.254.10.20:9123"));
        assert!(
            !ok("127.0.0.1:9123"),
            "loopback would reach services on this Mac"
        );
        assert!(!ok("127.0.0.1:47820"));
        assert!(!ok("192.168.0.125:22"), "wrong port");
        assert!(!ok("8.8.8.8:9123"), "public address");
        assert!(!ok("[::1]:9123"));
    }

    #[test]
    fn host_and_net_unreachable_count_as_blocked() {
        for code in UNREACHABLE {
            let io = std::io::Error::from_raw_os_error(code);
            assert!(is_network_blocked(&anyhow::Error::new(io)));
            let wrapped = anyhow::Error::new(std::io::Error::from_raw_os_error(code))
                .context("connecting to light");
            assert!(is_network_blocked(&wrapped));
        }
        let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert!(!is_network_blocked(&anyhow::Error::new(refused)));
        assert!(!is_network_blocked(&anyhow::anyhow!("light timed out")));
        let timed = std::io::Error::from(std::io::ErrorKind::TimedOut);
        assert!(!is_network_blocked(&anyhow::Error::new(timed)));
    }

    #[test]
    fn knows_when_a_keep_alive_response_is_done() {
        // What a Key Light really sends: keep-alive with a Content-Length.
        let head = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: keep-alive\r\n\r\n";
        assert!(!complete(&head[..20]));
        assert!(!complete(&[&head[..], b"{\"a"].concat()));
        assert!(complete(&[&head[..], b"{\"a}"].concat()));
        assert!(complete(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n"
        ));
    }
}
