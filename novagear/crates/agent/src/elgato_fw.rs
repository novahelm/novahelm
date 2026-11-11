//! Is a newer Key Light firmware out? Elgato publishes the lights' firmware only inside
//! Control Center's download, so this reads the version out of the zip without
//! downloading it: the catalog names the zip, HTTP Range reads fetch its central
//! directory, and the first 128 bytes of each `Firmware_*.bin` hold the version. The
//! whole check reads well under 2 MB. Nothing here writes firmware to any device or
//! downloads a firmware image.

use anyhow::{Context, Result, anyhow, bail};
use miniz_oxide::inflate::core::{DecompressorOxide, decompress, inflate_flags};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// Elgato's update catalog.
pub const CATALOG_URL: &str = "https://gc-updates.elgato.com/";
/// Every request gets this long.
pub const TIMEOUT: Duration = Duration::from_secs(15);
/// How old the cache may get before the next check.
const MAX_AGE_SECS: u64 = 24 * 3600;
/// After a failed check, wait at least this long.
const BACKOFF_SECS: u64 = 3600;
/// The tail read that must hold the end of the central directory.
const TAIL: u64 = 64 * 1024;
/// Zip bytes the check may read in total (the ceiling is 2 MB with the catalog).
const ZIP_BUDGET: usize = 1_500_000;
const MAX_CATALOG: u64 = 256 * 1024;
const MAX_CENTRAL_DIR: u64 = 1024 * 1024;
const MAX_FIRMWARE_ENTRIES: usize = 32;
const HEADER_LEN: usize = 128;
/// Compressed bytes read per firmware file; 128 bytes of output come out of far fewer.
const DATA_WINDOW: u64 = 4096;
/// Room for a local header's extra field, which can differ from the central one.
const LOCAL_EXTRA_ROOM: u64 = 1024;

/// A firmware image's version, from its header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FwHeader {
    pub board: u8,
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
    pub build: u16,
}

impl FwHeader {
    /// "1.0.3 (222)", as the lights' own version is shown.
    pub fn display(&self) -> String {
        format!(
            "{}.{}.{} ({})",
            self.major, self.minor, self.patch, self.build
        )
    }
}

/// Magic `CE 00` at bytes 0-1, board `u8` at 45, then major, minor, patch and build as
/// little-endian `u16` at 46, 48, 50 and 52. Confirmed against Control Center 1.9.2.
pub fn parse_header(h: &[u8]) -> Option<FwHeader> {
    if h.len() < 54 || h[0] != 0xCE || h[1] != 0x00 {
        return None;
    }
    let u16_at = |i: usize| u16::from_le_bytes([h[i], h[i + 1]]);
    Some(FwHeader {
        board: h[45],
        major: u16_at(46),
        minor: u16_at(48),
        patch: u16_at(50),
        build: u16_at(52),
    })
}

/// "1.0.3" as (major, minor, patch).
pub fn parse_version(s: &str) -> Option<(u16, u16, u16)> {
    let mut it = s.split('.').map(|p| p.parse::<u16>().ok());
    let v = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(v)
}

/// Whether `latest` is ahead of what is installed; the build number breaks ties.
pub fn newer(installed: (u16, u16, u16, u32), latest: &FwHeader) -> bool {
    let l = (
        latest.major,
        latest.minor,
        latest.patch,
        u32::from(latest.build),
    );
    l > installed
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Latest {
    /// Control Center's version, whose zip held these images.
    pub app_version: String,
    pub by_board: BTreeMap<u8, FwHeader>,
    /// The zip's ETag.
    pub etag: Option<String>,
    /// Zip bytes the check read (the catalog is not counted).
    pub bytes_fetched: usize,
}

/// What the agent keeps between runs in `firmware-cache.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cache {
    /// Unix seconds.
    pub checked_at: u64,
    pub etag: Option<String>,
    pub app_version: String,
    pub by_board: BTreeMap<u8, FwHeader>,
}

pub fn load_cache(path: &Path) -> Option<Cache> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

pub fn save_cache(path: &Path, c: &Cache) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(c)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// How long until a check is due: a day after the last one, and an hour after a failure.
pub fn check_delay(now: u64, checked_at: Option<u64>, last_failure: Option<u64>) -> Duration {
    let due = checked_at.map_or(0, |t| t.saturating_add(MAX_AGE_SECS));
    let retry = last_failure.map_or(0, |t| t.saturating_add(BACKOFF_SECS));
    Duration::from_secs(due.max(retry).saturating_sub(now))
}

pub fn should_check(now: u64, cache: Option<&Cache>, last_failure: Option<u64>) -> bool {
    check_delay(now, cache.map(|c| c.checked_at), last_failure).is_zero()
}

/// The newest firmware Elgato ships, or `None` when the zip's ETag is `prev_etag`.
/// Blocking; run it off the async thread.
pub fn check(prev_etag: Option<&str>) -> Result<Option<Latest>> {
    check_with(CATALOG_URL, TIMEOUT, prev_etag)
}

/// The flag that makes the agent binary run one check, print its [`Outcome`] and exit
/// (`main.rs`).
pub const CHILD_FLAG: &str = "--firmware-check";
/// The previous ETag, passed to the child.
pub const CHILD_ETAG_FLAG: &str = "--firmware-etag";
/// A child that has not finished by now is killed (35 requests of 15 s would be far
/// longer than any real check).
const CHILD_DEADLINE: Duration = Duration::from_secs(120);

/// What the child prints on stdout, one JSON line.
#[derive(Debug, Serialize, Deserialize)]
// Externally tagged on purpose: an internal tag buffers the content first, and the
// buffered form can't turn the board map's string keys back into numbers.
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Unchanged,
    Latest(Latest),
    Failed { message: String },
}

/// The child's side: runs [`check`] and says what happened. Never fails; a failure is an
/// [`Outcome`] too. Debug builds (the test binaries) read the catalog URL from
/// `NOVAGEAR_TEST_CATALOG_URL`, so a test can point the real binary at a dead port.
pub fn child_outcome(prev_etag: Option<&str>) -> Outcome {
    #[cfg(debug_assertions)]
    let r = match std::env::var("NOVAGEAR_TEST_CATALOG_URL") {
        Ok(u) => check_with(&u, Duration::from_secs(3), prev_etag),
        Err(_) => check(prev_etag),
    };
    #[cfg(not(debug_assertions))]
    let r = check(prev_etag);
    match r {
        Ok(None) => Outcome::Unchanged,
        Ok(Some(l)) => Outcome::Latest(l),
        Err(e) => Outcome::Failed {
            message: format!("{e:#}"),
        },
    }
}

/// [`check`] in a short-lived copy of this program. The TLS client and its buffers cost
/// well over a megabyte of the long-running agent's footprint, which the allocator keeps
/// after the check ends; in a child that memory goes back to the system when it exits.
/// Blocking; run it off the async thread.
pub fn check_isolated(prev_etag: Option<&str>) -> Result<Option<Latest>> {
    let exe = std::env::current_exe().context("finding myself")?;
    run_child(child_command(&exe, prev_etag), CHILD_DEADLINE)
}

/// The ETag is joined to its flag with `=`: it comes from a server, and clap rejects a
/// separate value that starts with `-`.
fn child_command(exe: &Path, prev_etag: Option<&str>) -> std::process::Command {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg(CHILD_FLAG);
    if let Some(e) = prev_etag {
        cmd.arg(format!("{CHILD_ETAG_FLAG}={e}"));
    }
    cmd
}

fn run_child(mut cmd: std::process::Command, deadline: Duration) -> Result<Option<Latest>> {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("starting the check")?;
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if start.elapsed() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("the check did not finish in {} s", deadline.as_secs());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // One short line, so it fits the pipe and is all there once the child has exited.
    let mut out = Vec::new();
    if let Some(p) = child.stdout.take() {
        p.take(64 * 1024).read_to_end(&mut out)?;
    }
    if !status.success() {
        bail!("the check ended with {status}");
    }
    match serde_json::from_slice::<Outcome>(&out).context("the check's answer was unreadable")? {
        Outcome::Unchanged => Ok(None),
        Outcome::Latest(l) => Ok(Some(l)),
        Outcome::Failed { message } => Err(anyhow!("{message}")),
    }
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        // A redirect could lead anywhere; the CDN answers directly.
        .max_redirects(0)
        .https_only(false)
        .user_agent("NovaGear")
        // ureq's default is 128 KB in and 128 KB out per connection; this check moves a
        // few KB per request, and the agent keeps the pages those buffers touched for
        // good (measured: about 1.5 MB of footprint).
        .input_buffer_size(16 * 1024)
        .output_buffer_size(16 * 1024)
        .build()
        .into()
}

/// The download must come from Elgato over TLS, wherever the catalog says it is.
fn check_download_url(url: &str) -> Result<()> {
    let rest = url
        .strip_prefix("https://")
        .context("the catalog's download is not https")?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.contains('@') || host.contains(':') {
        bail!("the catalog's download URL is not a plain host");
    }
    if host == "elgato.com" || host.ends_with(".elgato.com") {
        Ok(())
    } else {
        bail!("the catalog's download is not on elgato.com")
    }
}

/// The version and zip URL of `cc-mac`. The entry's `enabled` flag is ignored: Elgato
/// switched it off when Control Center left the Mac, but the zip is still served.
fn parse_catalog(body: &[u8]) -> Result<(String, String)> {
    let v: serde_json::Value = serde_json::from_slice(body).context("the catalog is not JSON")?;
    let entry = v.get("cc-mac").context("the catalog has no cc-mac entry")?;
    let field = |k: &str| {
        entry
            .get(k)
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .with_context(|| format!("cc-mac has no {k}"))
    };
    Ok((field("version")?, field("downloadURL")?))
}

/// A Range request must be answered 206. A server that answers 200 would send the whole
/// file, so the body is never read.
fn require_partial(status: u16) -> Result<()> {
    if status == 206 {
        Ok(())
    } else {
        bail!("the server ignored the Range request (HTTP {status})")
    }
}

/// A 206's `Content-Range` must be exactly the bytes asked for out of the length HEAD
/// gave; anything else means the file changed or the server is confused.
fn check_content_range(header: &str, start: u64, end: u64, total: u64) -> Result<()> {
    let want = format!("bytes {start}-{end}/{total}");
    if header.trim() == want {
        Ok(())
    } else {
        bail!("the server answered a different range than asked ({header:?}, wanted {want:?})")
    }
}

struct Ranged<'a> {
    agent: &'a ureq::Agent,
    url: &'a str,
    /// The zip's length, from HEAD.
    total: u64,
    used: usize,
}

impl Ranged<'_> {
    /// Bytes `start..=end` of the zip, counted against the budget.
    fn read(&mut self, start: u64, end: u64) -> Result<Vec<u8>> {
        let len = (end - start + 1) as usize;
        if self.used + len > ZIP_BUDGET {
            bail!("the check would read more than {ZIP_BUDGET} bytes");
        }
        let resp = self
            .agent
            .get(self.url)
            .header("Range", format!("bytes={start}-{end}"))
            .call()?;
        require_partial(resp.status().as_u16())?;
        let range = resp
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        check_content_range(&range, start, end, self.total)?;
        let bytes = resp
            .into_body()
            .into_with_config()
            // ureq refuses a body that reaches its limit, so allow one byte more and
            // check for it below.
            .limit(len as u64 + 1)
            .read_to_vec()?;
        if bytes.len() != len {
            bail!(
                "the server sent {} bytes, not the {len} asked for",
                bytes.len()
            );
        }
        self.used += bytes.len();
        Ok(bytes)
    }
}

fn le16(b: &[u8], i: usize) -> usize {
    usize::from(u16::from_le_bytes([b[i], b[i + 1]]))
}
fn le32(b: &[u8], i: usize) -> u64 {
    u64::from(u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]))
}

/// One `Firmware_*.bin` in the zip's central directory.
struct Entry {
    name: String,
    method: usize,
    comp_size: u64,
    name_len: usize,
    local_offset: u64,
}

fn is_firmware_name(name: &str) -> bool {
    name.rsplit_once('/').is_some_and(|(dir, file)| {
        dir.ends_with("/Resources") && file.starts_with("Firmware_") && file.ends_with(".bin")
    })
}

fn firmware_entries(cd: &[u8]) -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    let mut p = 0;
    while p + 46 <= cd.len() {
        if le32(cd, p) != 0x0201_4b50 {
            bail!("the zip's central directory is damaged");
        }
        let (n, m, k) = (le16(cd, p + 28), le16(cd, p + 30), le16(cd, p + 32));
        let name = cd
            .get(p + 46..p + 46 + n)
            .context("the zip's central directory is cut short")?;
        let name = String::from_utf8_lossy(name).to_string();
        if is_firmware_name(&name) && out.len() < MAX_FIRMWARE_ENTRIES {
            out.push(Entry {
                name,
                method: le16(cd, p + 10),
                comp_size: le32(cd, p + 20),
                name_len: n,
                local_offset: le32(cd, p + 42),
            });
        }
        p += 46 + n + m + k;
    }
    Ok(out)
}

/// The first 128 bytes of a deflate stream (raw, no zlib header).
fn inflate_head(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = [0u8; HEADER_LEN];
    let mut d = DecompressorOxide::new();
    let (status, _, written) = decompress(
        &mut d,
        data,
        &mut out,
        0,
        inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
    );
    if written < HEADER_LEN {
        bail!("deflate stopped after {written} bytes ({status:?})");
    }
    Ok(out.to_vec())
}

fn read_header(r: &mut Ranged, e: &Entry, zip_len: u64) -> Result<FwHeader> {
    let start = e.local_offset;
    if start >= zip_len {
        bail!("local header past the end of the zip");
    }
    let end = (start + 30 + e.name_len as u64 + LOCAL_EXTRA_ROOM + DATA_WINDOW).min(zip_len) - 1;
    let b = r.read(start, end)?;
    if b.len() < 30 || le32(&b, 0) != 0x0403_4b50 {
        bail!("no local header where the directory says");
    }
    let data_at = 30 + le16(&b, 26) + le16(&b, 28);
    let avail = e.comp_size.min(DATA_WINDOW) as usize;
    let data = b
        .get(data_at..)
        .context("the local header is longer than expected")?;
    let data = &data[..data.len().min(avail)];
    let head = match e.method {
        0 if data.len() >= HEADER_LEN => data[..HEADER_LEN].to_vec(),
        0 => bail!("stored file shorter than a header"),
        8 => inflate_head(data)?,
        m => bail!("compression method {m} is not handled"),
    };
    parse_header(&head).context("not a firmware header")
}

/// [`check`] against another catalog URL and timeout; the tests point it at a local
/// server. A catalog that is not https is trusted to name any download.
fn check_with(
    catalog_url: &str,
    timeout: Duration,
    prev_etag: Option<&str>,
) -> Result<Option<Latest>> {
    let agent = agent(timeout);
    let resp = agent.get(catalog_url).call()?;
    if resp.status().as_u16() != 200 {
        bail!("the catalog answered HTTP {}", resp.status().as_u16());
    }
    let body = resp
        .into_body()
        .into_with_config()
        .limit(MAX_CATALOG)
        .read_to_vec()?;
    let (app_version, url) = parse_catalog(&body)?;
    if catalog_url.starts_with("https://") {
        check_download_url(&url)?;
    }

    let head = agent.head(&url).call()?;
    if head.status().as_u16() != 200 {
        bail!("the zip answered HTTP {}", head.status().as_u16());
    }
    let etag = head
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if etag.is_some() && etag.as_deref() == prev_etag {
        return Ok(None);
    }
    let zip_len: u64 = head
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .context("the zip has no length")?;
    if zip_len < 22 {
        bail!("the zip is too small to be one");
    }

    let mut r = Ranged {
        agent: &agent,
        url: &url,
        total: zip_len,
        used: 0,
    };
    let tail_start = zip_len.saturating_sub(TAIL);
    let tail = r.read(tail_start, zip_len - 1)?;
    if tail.len() < 22 {
        bail!("the zip's tail came back short");
    }
    let eocd = (0..=tail.len() - 22)
        .rev()
        .find(|&i| le32(&tail, i) == 0x0605_4b50)
        .context("no end of central directory in the zip")?;
    let (cd_size, cd_off) = (le32(&tail, eocd + 12), le32(&tail, eocd + 16));
    if cd_off == 0xFFFF_FFFF || cd_size == 0xFFFF_FFFF {
        bail!("zip64 archives are not handled");
    }
    if cd_size == 0 || cd_size > MAX_CENTRAL_DIR || cd_off + cd_size > zip_len {
        bail!("the zip's central directory is not where it should be");
    }
    let cd = if cd_off >= tail_start {
        let from = (cd_off - tail_start) as usize;
        tail.get(from..from + cd_size as usize)
            .context("the zip's central directory runs past its tail")?
            .to_vec()
    } else {
        r.read(cd_off, cd_off + cd_size - 1)?
    };

    let mut by_board = BTreeMap::new();
    for e in firmware_entries(&cd)? {
        match read_header(&mut r, &e, zip_len) {
            Ok(h) => {
                by_board.insert(h.board, h);
            }
            Err(err) => tracing::warn!("firmware check skipped {}: {err:#}", e.name),
        }
    }
    if by_board.is_empty() {
        return Err(anyhow!("no firmware headers could be read from the zip"));
    }
    Ok(Some(Latest {
        app_version,
        by_board,
        etag,
        bytes_fetched: r.used,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    const KEYLIGHT: &[u8] = include_bytes!("../tests/fixtures/fw-header-keylight.bin");
    const MK2: &[u8] = include_bytes!("../tests/fixtures/fw-header-keylight-mk2.bin");
    const NEO: &[u8] = include_bytes!("../tests/fixtures/fw-header-keylight-neo.bin");
    const CATALOG: &str = include_str!("../tests/fixtures/gc-updates.json");

    #[test]
    fn parses_the_three_known_headers() {
        let h = |b| parse_header(b).unwrap();
        assert_eq!(
            h(KEYLIGHT),
            FwHeader {
                board: 53,
                major: 1,
                minor: 0,
                patch: 3,
                build: 222
            }
        );
        assert_eq!(
            h(MK2),
            FwHeader {
                board: 205,
                major: 1,
                minor: 0,
                patch: 4,
                build: 199
            }
        );
        assert_eq!(
            h(NEO),
            FwHeader {
                board: 210,
                major: 1,
                minor: 0,
                patch: 6,
                build: 218
            }
        );
    }

    #[test]
    fn rejects_wrong_magic_and_short_input() {
        assert!(parse_header(&[0u8; 128]).is_none());
        assert!(parse_header(&[0xCE, 0]).is_none());
        assert!(parse_header(&KEYLIGHT[..53]).is_none());
        assert!(parse_header(&KEYLIGHT[..54]).is_some());
    }

    fn fw(build: u16) -> FwHeader {
        FwHeader {
            board: 53,
            major: 1,
            minor: 0,
            patch: 3,
            build,
        }
    }

    #[test]
    fn build_counts_in_version_compare() {
        assert!(newer((1, 0, 3, 200), &fw(222)));
        assert!(!newer((1, 0, 3, 222), &fw(222)));
        assert!(!newer((1, 0, 3, 300), &fw(222)));
        assert!(!newer((1, 0, 4, 0), &fw(222)));
        assert!(newer((0, 9, 9, 999), &fw(222)));
    }

    #[test]
    fn parses_installed_version_strings() {
        assert_eq!(parse_version("1.0.3"), Some((1, 0, 3)));
        assert_eq!(parse_version("1.0"), None);
        assert_eq!(parse_version("a.b.c"), None);
    }

    #[test]
    fn catalog_gives_version_and_url() {
        let (v, u) = parse_catalog(CATALOG.as_bytes()).unwrap();
        assert_eq!(v, "1.9.2");
        assert!(u.starts_with("https://edge.elgato.com/") && u.ends_with(".app.zip"));
    }

    #[test]
    fn catalog_without_cc_mac_is_an_error_not_a_panic() {
        let mut v: serde_json::Value = serde_json::from_str(CATALOG).unwrap();
        v.as_object_mut().unwrap().remove("cc-mac");
        assert!(parse_catalog(v.to_string().as_bytes()).is_err());
        assert!(parse_catalog(b"not json").is_err());
        assert!(parse_catalog(br#"{"cc-mac":{"version":"1"}}"#).is_err());
        assert!(parse_catalog(br#"{"cc-mac":7}"#).is_err());
    }

    #[test]
    fn download_url_must_be_https_on_elgato() {
        assert!(check_download_url("https://edge.elgato.com/a.zip").is_ok());
        assert!(check_download_url("http://edge.elgato.com/a.zip").is_err());
        assert!(check_download_url("https://evil.example/a.zip").is_err());
        assert!(check_download_url("https://elgato.com.evil.example/a.zip").is_err());
    }

    const DAY: u64 = 24 * 3600;

    fn cache(checked_at: u64) -> Cache {
        Cache {
            checked_at,
            etag: None,
            app_version: "1.9.2".into(),
            by_board: Default::default(),
        }
    }

    #[test]
    fn schedule_runs_when_never_checked_or_a_day_old() {
        let now = 10 * DAY;
        assert!(should_check(now, None, None));
        assert!(!should_check(now, Some(&cache(now - DAY + 60)), None));
        assert!(should_check(now, Some(&cache(now - DAY)), None));
    }

    #[test]
    fn failed_check_backs_off_an_hour() {
        let now = 10 * DAY;
        // Never checked, but it failed 59 minutes ago: wait.
        assert!(!should_check(now, None, Some(now - 59 * 60)));
        assert!(should_check(now, None, Some(now - 3600)));
        // A stale cache and a recent failure: still wait.
        assert!(!should_check(
            now,
            Some(&cache(now - 2 * DAY)),
            Some(now - 600)
        ));
        assert_eq!(
            check_delay(now, None, Some(now - 600)),
            Duration::from_secs(3000)
        );
        assert_eq!(
            check_delay(now, Some(now - DAY + 100), None),
            Duration::from_secs(100)
        );
    }

    #[test]
    fn cache_round_trips_and_a_bad_file_is_no_cache() {
        let dir = std::env::temp_dir().join(format!("novagear-fwcache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("firmware-cache.json");
        assert!(load_cache(&path).is_none());
        let mut c = cache(1234);
        c.etag = Some("\"abc\"".into());
        c.by_board.insert(53, fw(222));
        save_cache(&path, &c).unwrap();
        assert_eq!(load_cache(&path).unwrap(), c);
        std::fs::write(&path, "{ nope").unwrap();
        assert!(load_cache(&path).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- a local server that speaks just enough of Elgato's CDN ----

    fn header_bytes(board: u8, v: (u16, u16, u16), build: u16) -> Vec<u8> {
        let mut h = vec![0u8; 128];
        h[0] = 0xCE;
        h[45] = board;
        for (i, n) in [v.0, v.1, v.2, build].iter().enumerate() {
            h[46 + 2 * i..48 + 2 * i].copy_from_slice(&n.to_le_bytes());
        }
        h
    }

    fn noise(n: usize) -> Vec<u8> {
        let mut x = 0x2545_F491u32;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect()
    }

    /// (name, data, deflate it?)
    fn zip(entries: &[(String, Vec<u8>, bool)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data, deflate) in entries {
            let packed = if *deflate {
                miniz_oxide::deflate::compress_to_vec(data, 6)
            } else {
                data.clone()
            };
            let method: u16 = if *deflate { 8 } else { 0 };
            let off = out.len() as u32;
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&[20, 0, 0, 0]);
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&[0; 4]); // time, date
            out.extend_from_slice(&0u32.to_le_bytes()); // crc, unchecked
            out.extend_from_slice(&(packed.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&packed);

            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&[20, 0, 20, 0, 0, 0]);
            central.extend_from_slice(&method.to_le_bytes());
            central.extend_from_slice(&[0; 4]);
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&(packed.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0; 2 + 2 + 2 + 2 + 4]); // extra, comment, disk, attrs
            central.extend_from_slice(&off.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_off = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_off.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    struct Server {
        base: String,
        body_bytes: Arc<AtomicUsize>,
        full_gets: Arc<AtomicUsize>,
    }

    const FW_DIR: &str = "ControlCenter.app/Contents/Resources/ControlCenterSDK_ControlCenterSDK.bundle/Contents/Resources/";

    fn serve_zip(zip: Vec<u8>, version: &str) -> Server {
        serve_zip_with(zip, version, Quirk::None)
    }

    /// Ways a misbehaving server can answer a Range request.
    #[derive(Clone, Copy, PartialEq)]
    enum Quirk {
        None,
        /// A valid 206 that carries 10 bytes fewer than asked, with a matching Content-Range.
        Short,
        /// Right bytes, but a Content-Range that says another start.
        WrongRange,
        /// Right bytes, but the total is not the length HEAD gave.
        WrongTotal,
    }

    fn serve_zip_with(zip: Vec<u8>, version: &str, quirk: Quirk) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{port}");
        let catalog = format!(
            r#"{{"cc-mac":{{"enabled":false,"version":"{version}","downloadURL":"{base}/fw.zip"}}}}"#
        );
        let body_bytes = Arc::new(AtomicUsize::new(0));
        let full_gets = Arc::new(AtomicUsize::new(0));
        let (bb, fg) = (body_bytes.clone(), full_gets.clone());
        let zip = Arc::new(zip);
        std::thread::spawn(move || {
            for s in listener.incoming() {
                let (Ok(mut s), zip, catalog, bb, fg) =
                    (s, zip.clone(), catalog.clone(), bb.clone(), fg.clone())
                else {
                    continue;
                };
                std::thread::spawn(move || {
                    let mut req = Vec::new();
                    let mut b = [0u8; 1];
                    while !req.ends_with(b"\r\n\r\n") {
                        if s.read(&mut b).unwrap_or(0) == 0 {
                            return;
                        }
                        req.push(b[0]);
                    }
                    let req = String::from_utf8_lossy(&req).to_string();
                    let first = req.lines().next().unwrap_or("").to_string();
                    let range = req.lines().find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("range: bytes=")
                            .map(str::to_string)
                    });
                    let (status, extra, body): (&str, String, Vec<u8>) = if first
                        .starts_with("GET / ")
                    {
                        (
                            "200 OK",
                            "Content-Type: application/json\r\n".into(),
                            catalog.into_bytes(),
                        )
                    } else if first.starts_with("HEAD /fw.zip") {
                        (
                            "200 OK",
                            format!(
                                "ETag: \"zip-1\"\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\n",
                                zip.len()
                            ),
                            Vec::new(),
                        )
                    } else if let Some(r) = range {
                        let (a, z) = r.split_once('-').unwrap();
                        let a: usize = a.parse().unwrap();
                        let z: usize = z.trim().parse::<usize>().unwrap().min(zip.len() - 1);
                        let (shown_a, shown_z, total) = match quirk {
                            Quirk::Short => (a, z - 10, zip.len()),
                            Quirk::WrongRange => (a + 1, z + 1, zip.len()),
                            Quirk::WrongTotal => (a, z, zip.len() + 5),
                            Quirk::None => (a, z, zip.len()),
                        };
                        let end = if quirk == Quirk::Short { z - 10 } else { z };
                        (
                            "206 Partial Content",
                            format!("Content-Range: bytes {shown_a}-{shown_z}/{total}\r\n"),
                            zip[a..=end].to_vec(),
                        )
                    } else {
                        fg.fetch_add(1, Ordering::SeqCst);
                        ("200 OK", String::new(), zip.to_vec())
                    };
                    bb.fetch_add(body.len(), Ordering::SeqCst);
                    let head = if first.starts_with("HEAD") {
                        format!("HTTP/1.1 {status}\r\n{extra}Connection: close\r\n\r\n")
                    } else {
                        format!(
                            "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                    };
                    let _ = s.write_all(head.as_bytes());
                    let _ = s.write_all(&body);
                });
            }
        });
        Server {
            base,
            body_bytes,
            full_gets,
        }
    }

    fn big_zip() -> Vec<u8> {
        let mut entries = vec![
            (
                "ControlCenter.app/Contents/MacOS/ControlCenter".to_string(),
                noise(3_000_000),
                false,
            ),
            (
                format!("{FW_DIR}Firmware_Key_Light.bin"),
                [header_bytes(53, (1, 0, 3), 222), noise(30_000)].concat(),
                true,
            ),
            (
                format!("{FW_DIR}Firmware_Key_Light_Mk2.bin"),
                [header_bytes(205, (1, 0, 4), 199), noise(30_000)].concat(),
                true,
            ),
            (
                format!("{FW_DIR}Firmware_Key_Light_Neo.bin"),
                [header_bytes(210, (1, 0, 6), 218), noise(30_000)].concat(),
                true,
            ),
            (format!("{FW_DIR}Firmware_Odd.bin"), vec![7u8; 500], true),
        ];
        entries.push((
            "ControlCenter.app/Contents/Resources/Other.bin".into(),
            noise(1000),
            false,
        ));
        zip(&entries)
    }

    #[test]
    fn check_reads_headers_with_ranges_only() {
        let srv = serve_zip(big_zip(), "1.9.2");
        let latest = check_with(&format!("{}/", srv.base), Duration::from_secs(5), None)
            .unwrap()
            .expect("first check has no cache, so it reports");
        assert_eq!(latest.app_version, "1.9.2");
        assert_eq!(latest.etag.as_deref(), Some("\"zip-1\""));
        assert_eq!(
            latest.by_board.len(),
            3,
            "the unreadable Firmware_Odd.bin is skipped"
        );
        assert_eq!(
            latest.by_board[&53],
            FwHeader {
                board: 53,
                major: 1,
                minor: 0,
                patch: 3,
                build: 222
            }
        );
        assert_eq!(latest.by_board[&205].build, 199);
        assert_eq!(latest.by_board[&210].build, 218);
        assert_eq!(
            srv.full_gets.load(Ordering::SeqCst),
            0,
            "never a plain GET of the zip"
        );
        let got = srv.body_bytes.load(Ordering::SeqCst);
        assert!(got < 200_000, "fetched {got} bytes of a 3 MB zip");
        assert_eq!(latest.bytes_fetched, got - catalog_len(&srv));
    }

    fn catalog_len(srv: &Server) -> usize {
        // The catalog body is not part of the zip budget.
        format!(
            r#"{{"cc-mac":{{"enabled":false,"version":"1.9.2","downloadURL":"{}/fw.zip"}}}}"#,
            srv.base
        )
        .len()
    }

    #[test]
    fn same_etag_means_unchanged_and_reads_no_zip_bytes() {
        let srv = serve_zip(big_zip(), "1.9.2");
        let r = check_with(
            &format!("{}/", srv.base),
            Duration::from_secs(5),
            Some("\"zip-1\""),
        )
        .unwrap();
        assert!(r.is_none());
        let got = srv.body_bytes.load(Ordering::SeqCst);
        assert_eq!(got, catalog_len(&srv), "only the catalog was read");
    }

    #[test]
    fn short_or_mismatched_range_answers_are_errors_not_panics() {
        for quirk in [Quirk::Short, Quirk::WrongRange, Quirk::WrongTotal] {
            let srv = serve_zip_with(big_zip(), "1.9.2", quirk);
            let r = check_with(&format!("{}/", srv.base), Duration::from_secs(5), None);
            assert!(r.is_err(), "a misbehaving server must give Err");
        }
    }

    #[test]
    fn content_range_is_checked_against_the_request() {
        assert!(check_content_range("bytes 10-19/100", 10, 19, 100).is_ok());
        assert!(check_content_range("bytes 11-19/100", 10, 19, 100).is_err());
        assert!(check_content_range("bytes 10-18/100", 10, 19, 100).is_err());
        assert!(check_content_range("bytes 10-19/101", 10, 19, 100).is_err());
        assert!(check_content_range("bytes 10-19/*", 10, 19, 100).is_err());
        assert!(check_content_range("items 10-19/100", 10, 19, 100).is_err());
        assert!(check_content_range("", 10, 19, 100).is_err());
    }

    #[test]
    fn a_server_that_ignores_range_is_refused_before_the_body() {
        assert!(require_partial(206).is_ok());
        assert!(require_partial(200).is_err());
    }

    #[test]
    fn stalled_server_times_out() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", l.local_addr().unwrap());
        let _hold = std::thread::spawn(move || {
            let mut keep = Vec::new();
            for s in l.incoming().take(2) {
                keep.push(s);
            }
            std::thread::sleep(Duration::from_secs(20));
        });
        let t = Instant::now();
        let r = check_with(&url, Duration::from_millis(400), None);
        assert!(r.is_err());
        assert!(
            t.elapsed() < Duration::from_secs(16),
            "took {:?}",
            t.elapsed()
        );
        assert!(t.elapsed() >= Duration::from_millis(300));
    }

    #[test]
    fn a_server_that_stalls_mid_body_times_out() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", l.local_addr().unwrap());
        let _hold = std::thread::spawn(move || {
            for s in l.incoming().take(1) {
                let mut s = s.unwrap();
                let mut b = [0u8; 1024];
                let _ = s.read(&mut b);
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5000\r\n\r\n{\"cc");
                std::thread::sleep(Duration::from_secs(20));
            }
        });
        let t = Instant::now();
        assert!(check_with(&url, Duration::from_millis(400), None).is_err());
        assert!(
            t.elapsed() < Duration::from_secs(16),
            "took {:?}",
            t.elapsed()
        );
    }
    fn sh(script: &str) -> std::process::Command {
        let mut c = std::process::Command::new("/bin/sh");
        c.args(["-c", script]);
        c
    }

    #[test]
    fn a_child_that_found_firmware_hands_it_back() {
        let out = Outcome::Latest(Latest {
            app_version: "1.9.2".into(),
            by_board: BTreeMap::from([(
                53,
                FwHeader {
                    board: 53,
                    major: 1,
                    minor: 0,
                    patch: 3,
                    build: 222,
                },
            )]),
            etag: Some("\"abc\"".into()),
            bytes_fetched: 1234,
        });
        let json = serde_json::to_string(&out).unwrap();
        let got = run_child(sh(&format!("echo '{json}'")), Duration::from_secs(5))
            .unwrap()
            .expect("a Latest");
        assert_eq!(got.app_version, "1.9.2");
        assert_eq!(got.by_board[&53].build, 222);
        assert_eq!(got.etag.as_deref(), Some("\"abc\""));
        assert_eq!(got.bytes_fetched, 1234);
    }

    #[test]
    fn a_child_that_saw_no_change_says_so() {
        let json = serde_json::to_string(&Outcome::Unchanged).unwrap();
        let r = run_child(sh(&format!("echo '{json}'")), Duration::from_secs(5)).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn a_child_failure_keeps_its_message() {
        let json = serde_json::to_string(&Outcome::Failed {
            message: "the catalog answered HTTP 503".into(),
        })
        .unwrap();
        let e = run_child(sh(&format!("echo '{json}'")), Duration::from_secs(5)).unwrap_err();
        assert_eq!(e.to_string(), "the catalog answered HTTP 503");
    }

    #[test]
    fn a_child_that_dies_or_babbles_is_an_error_not_a_panic() {
        assert!(run_child(sh("exit 3"), Duration::from_secs(5)).is_err());
        assert!(run_child(sh("echo not json"), Duration::from_secs(5)).is_err());
        assert!(run_child(sh("true"), Duration::from_secs(5)).is_err());
    }

    #[test]
    fn a_child_that_hangs_is_killed_at_the_deadline() {
        let t = Instant::now();
        let e = run_child(sh("sleep 30"), Duration::from_millis(300)).unwrap_err();
        assert!(e.to_string().contains("did not finish"), "{e}");
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    }

    #[test]
    fn an_etag_that_starts_with_a_dash_is_one_argument_with_its_flag() {
        let cmd = child_command(Path::new("/x/agent"), Some("-W/\"abc\""));
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args, ["--firmware-check", "--firmware-etag=-W/\"abc\""]);
    }

    #[test]
    fn the_http_buffers_are_small() {
        // ureq's 128 KB defaults cost about 1.5 MB of footprint that is never given back.
        let a = agent(Duration::from_secs(1));
        assert!(a.config().input_buffer_size() <= 16 * 1024);
        assert!(a.config().output_buffer_size() <= 16 * 1024);
    }
}
