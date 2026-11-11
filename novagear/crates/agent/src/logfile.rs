//! The agent's logs: stderr for a terminal, JSON lines in a size-capped rotating file for
//! `/api/logs`, and a live feed of warnings and errors for the event stream.
//!
//! Nothing here formats an event nobody reads: the live layer drops events below warn, and
//! while no panel is listening it drops warn and error too.

use anyhow::Result;
use serde_json::{Map, Value};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

pub const FILE_NAME: &str = "novagear.log";
/// One file's size cap.
pub const MAX_BYTES: u64 = 1_048_576;
/// Rotated files kept beside the current one.
pub const KEEP: usize = 4;
/// Most lines `read` returns.
pub const READ_CAP: usize = 2000;
/// A file that can't be opened is tried again at most this often.
const REOPEN_EVERY: Duration = Duration::from_secs(60);

struct Inner {
    dir: PathBuf,
    max_bytes: u64,
    keep: usize,
    file: Option<File>,
    size: u64,
    retry_at: Option<Instant>,
    /// Earliest next rotation attempt after one failed.
    rotate_retry_at: Option<Instant>,
    warnings: usize,
}

impl Inner {
    fn path(&self) -> PathBuf {
        self.dir.join(FILE_NAME)
    }

    fn rotated(&self, n: usize) -> PathBuf {
        self.dir.join(format!("{FILE_NAME}.{n}"))
    }

    fn open(&mut self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let f = opts.open(self.path())?;
        self.size = f.metadata()?.len();
        self.file = Some(f);
        Ok(())
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        self.file = None;
        if self.keep == 0 {
            let _ = std::fs::remove_file(self.path());
            return Ok(());
        }
        let _ = std::fs::remove_file(self.rotated(self.keep));
        for n in (1..self.keep).rev() {
            let _ = std::fs::rename(self.rotated(n), self.rotated(n + 1));
        }
        std::fs::rename(self.path(), self.rotated(1))
    }

    /// Opens the file when it is closed or has been deleted. Failing is not an error: it
    /// warns once on stderr and the next attempt waits `REOPEN_EVERY`.
    fn ensure_open(&mut self) -> bool {
        if self.file.is_some() && !self.path().exists() {
            self.file = None;
        }
        if self.file.is_some() {
            return true;
        }
        if self.retry_at.is_some_and(|t| Instant::now() < t) {
            return false;
        }
        match self.open() {
            Ok(()) => {
                self.retry_at = None;
                true
            }
            Err(e) => {
                self.retry_at = Some(Instant::now() + REOPEN_EVERY);
                if self.warnings == 0 {
                    eprintln!(
                        "novagear: can't write logs to {} ({e}); file logging is off, retrying every minute",
                        self.dir.display()
                    );
                }
                self.warnings += 1;
                false
            }
        }
    }

    fn write_line(&mut self, buf: &[u8]) {
        if !self.ensure_open() {
            return;
        }
        let over = |me: &Inner| me.size > 0 && me.size + buf.len() as u64 > me.max_bytes;
        if over(self) && self.rotate_retry_at.is_none_or(|t| Instant::now() >= t) {
            match self.rotate() {
                Ok(()) => self.rotate_retry_at = None,
                Err(e) => {
                    // Warn once per attempt window, not once per line.
                    self.rotate_retry_at = Some(Instant::now() + REOPEN_EVERY);
                    if self.warnings == 0 {
                        eprintln!(
                            "novagear: can't rotate the log ({e}); dropping lines while it is full"
                        );
                    }
                    self.warnings += 1;
                }
            }
            if !self.ensure_open() {
                return;
            }
        }
        // Rotation failed or is waiting for its retry: never grow past the cap.
        if over(self) {
            return;
        }
        if let Some(f) = self.file.as_mut() {
            match f.write_all(buf) {
                Ok(()) => self.size += buf.len() as u64,
                // Drop the line, reopen on the next one.
                Err(_) => self.file = None,
            }
        }
    }
}

/// `novagear.log` in a directory, rotating to `.1` … `.keep` at `max_bytes`. Cloning gives
/// another handle on the same file. Writes never fail: a log that can't be written is
/// dropped, so a full or read-only disk can't take the agent down.
#[derive(Clone)]
pub struct RotatingFile(Arc<Mutex<Inner>>);

impl RotatingFile {
    pub fn new(dir: &Path, max_bytes: u64, keep: usize) -> RotatingFile {
        RotatingFile(Arc::new(Mutex::new(Inner {
            dir: dir.to_path_buf(),
            max_bytes,
            keep,
            file: None,
            size: 0,
            retry_at: None,
            rotate_retry_at: None,
            warnings: 0,
        })))
    }

    /// How many times opening the file failed (each failure is one stderr warning at most).
    pub fn warnings(&self) -> usize {
        crate::lock(&self.0).warnings
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        crate::lock(&self.0).write_line(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RotatingFile {
    type Writer = RotatingFile;
    fn make_writer(&'a self) -> RotatingFile {
        self.clone()
    }
}

fn rank(level: &str) -> u8 {
    match level.to_ascii_lowercase().as_str() {
        "trace" => 0,
        "debug" => 1,
        "info" => 2,
        "warn" | "warning" => 3,
        "error" => 4,
        _ => 0,
    }
}

/// Log lines, newest first, from the current file and the rotated ones. `min_level` keeps
/// that level and worse; `q` keeps lines containing it (any case). At most `limit`, and
/// never more than `READ_CAP`.
pub fn read(dir: &Path, min_level: Option<&str>, q: Option<&str>, limit: usize) -> Vec<Value> {
    let limit = limit.min(READ_CAP);
    let min = min_level.map(rank).unwrap_or(0);
    let q = q.filter(|q| !q.is_empty()).map(str::to_lowercase);
    let mut out = Vec::new();
    let mut paths = vec![dir.join(FILE_NAME)];
    paths.extend((1..=KEEP).map(|n| dir.join(format!("{FILE_NAME}.{n}"))));
    for p in paths {
        if out.len() >= limit {
            break;
        }
        let Ok(bytes) = std::fs::read(&p) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        for l in text.lines().rev() {
            if out.len() >= limit {
                break;
            }
            let Ok(v) = serde_json::from_str::<Value>(l) else {
                continue;
            };
            if rank(v["level"].as_str().unwrap_or("")) < min {
                continue;
            }
            if let Some(q) = &q
                && !l.to_lowercase().contains(q)
            {
                continue;
            }
            out.push(v);
        }
    }
    out
}

fn live_sender() -> &'static broadcast::Sender<String> {
    static LIVE: OnceLock<broadcast::Sender<String>> = OnceLock::new();
    LIVE.get_or_init(|| broadcast::channel(64).0)
}

/// Warn and error events as JSON lines, as they happen.
pub fn live() -> broadcast::Receiver<String> {
    live_sender().subscribe()
}

#[derive(Default)]
struct Fields(Map<String, Value>);

impl Visit for Fields {
    fn record_debug(&mut self, f: &Field, v: &dyn std::fmt::Debug) {
        self.0.insert(f.name().into(), format!("{v:?}").into());
    }
    fn record_str(&mut self, f: &Field, v: &str) {
        self.0.insert(f.name().into(), v.into());
    }
    fn record_i64(&mut self, f: &Field, v: i64) {
        self.0.insert(f.name().into(), v.into());
    }
    fn record_u64(&mut self, f: &Field, v: u64) {
        self.0.insert(f.name().into(), v.into());
    }
    fn record_bool(&mut self, f: &Field, v: bool) {
        self.0.insert(f.name().into(), v.into());
    }
}

fn now_rfc3339() -> String {
    use tracing_subscriber::fmt::format::Writer;
    use tracing_subscriber::fmt::time::{FormatTime, SystemTime};
    let mut s = String::new();
    let _ = SystemTime.format_time(&mut Writer::new(&mut s));
    s.trim().to_string()
}

/// Sends warn and error events to `live()`, in the same shape as the file's JSON lines.
struct LiveLayer;

impl<S: tracing::Subscriber> Layer<S> for LiveLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        let meta = event.metadata();
        // Lower is worse in tracing: anything above WARN is too chatty to forward.
        if *meta.level() > tracing::Level::WARN || live_sender().receiver_count() == 0 {
            return;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        let line = serde_json::json!({
            "timestamp": now_rfc3339(),
            "level": meta.level().as_str(),
            "target": meta.target(),
            "fields": fields.0,
        });
        let _ = live_sender().send(line.to_string());
    }
}

static LOG_DIR: OnceLock<PathBuf> = OnceLock::new();

/// The directory `init` logs into, for `/api/logs`. `None` when logging wasn't set up.
pub fn log_dir() -> Option<&'static Path> {
    LOG_DIR.get().map(|p| p.as_path())
}

/// Installs the global subscriber: stderr, the JSON file in `dir`, and the live feed, under
/// `RUST_LOG` (default `info`).
pub fn init(dir: &Path) -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    init_with_filter(dir, filter)
}

pub fn init_with_filter(dir: &Path, filter: EnvFilter) -> Result<()> {
    let file = RotatingFile::new(dir, MAX_BYTES, KEEP);
    let _ = LOG_DIR.set(dir.to_path_buf());
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_ansi(false)
                .with_writer(file),
        )
        .with(LiveLayer)
        .try_init()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::{Path, PathBuf};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("novagear-log-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn line(level: &str, msg: &str) -> String {
        format!(
            "{{\"timestamp\":\"2026-11-01T00:00:00Z\",\"level\":\"{level}\",\"fields\":{{\"message\":\"{msg}\"}},\"target\":\"t\"}}\n"
        )
    }

    fn write_lines(dir: &Path, lines: &[String]) {
        let mut f = RotatingFile::new(dir, 1_048_576, 4);
        for l in lines {
            f.write_all(l.as_bytes()).unwrap();
        }
    }

    #[test]
    fn rotates_at_the_size_cap_and_keeps_four() {
        let dir = temp_dir("rotate");
        let mut f = RotatingFile::new(&dir, 1_048_576, 4);
        let filler = "x".repeat(1000);
        let l = line("INFO", &filler);
        for _ in 0..6_000 {
            f.write_all(l.as_bytes()).unwrap();
        }
        assert!(dir.join("novagear.log").exists());
        for i in 1..=4 {
            assert!(
                dir.join(format!("novagear.log.{i}")).exists(),
                "missing .{i}"
            );
        }
        assert!(!dir.join("novagear.log.5").exists());
        for e in std::fs::read_dir(&dir).unwrap() {
            let len = e.unwrap().metadata().unwrap().len();
            assert!(len <= 1_048_576, "file over the cap: {len}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_filters_by_level_and_text_newest_first() {
        let dir = temp_dir("filter");
        write_lines(
            &dir,
            &[
                line("INFO", "a"),
                line("WARN", "deck gone"),
                line("ERROR", "x"),
            ],
        );
        let got = read(&dir, Some("warn"), Some("deck"), 100);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["fields"]["message"], "deck gone");
        let all = read(&dir, None, None, 100);
        let msgs: Vec<_> = all
            .iter()
            .map(|v| v["fields"]["message"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(msgs, ["x", "deck gone", "a"], "newest first");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_spans_rotated_files_newest_first() {
        let dir = temp_dir("span");
        let mut f = RotatingFile::new(&dir, 300, 4);
        for n in 0..12 {
            f.write_all(line("INFO", &format!("m{n:02}")).as_bytes())
                .unwrap();
        }
        assert!(dir.join("novagear.log.1").exists());
        let got = read(&dir, None, None, 100);
        assert_eq!(got[0]["fields"]["message"], "m11");
        assert!(got.len() > 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_caps_at_2000() {
        let dir = temp_dir("cap");
        let lines: Vec<String> = (0..3000).map(|n| line("INFO", &format!("m{n}"))).collect();
        write_lines(&dir, &lines);
        assert_eq!(read(&dir, None, None, 5000).len(), 2000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unwritable_dir_keeps_running() {
        let dir = temp_dir("unwritable");
        // A directory can't be made under a regular file, so opening always fails.
        let blocker = dir.join("file");
        std::fs::write(&blocker, "x").unwrap();
        let mut f = RotatingFile::new(&blocker.join("logs"), 1_048_576, 4);
        for _ in 0..5 {
            assert_eq!(f.write(b"line\n").unwrap(), 5, "the line is dropped");
        }
        assert_eq!(f.warnings(), 1, "one warning, not one per line");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleted_file_is_recreated() {
        let dir = temp_dir("deleted");
        let mut f = RotatingFile::new(&dir, 1_048_576, 4);
        f.write_all(line("INFO", "one").as_bytes()).unwrap();
        std::fs::remove_file(dir.join("novagear.log")).unwrap();
        f.write_all(line("INFO", "two").as_bytes()).unwrap();
        let got = read(&dir, None, None, 10);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["fields"]["message"], "two");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn live_feed_carries_warn_and_error_only_and_only_when_listened_to() {
        let sub = tracing_subscriber::registry().with(LiveLayer);
        let _g = tracing::subscriber::set_default(sub);
        tracing::warn!("nobody is listening");
        let mut rx = live();
        tracing::info!("too quiet");
        tracing::warn!(device = "deck", "deck gone");
        tracing::error!("boom");
        let a: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(a["level"], "WARN");
        assert_eq!(a["fields"]["message"], "deck gone");
        assert_eq!(a["fields"]["device"], "deck");
        assert!(a["timestamp"].as_str().unwrap().contains('T'));
        let b: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(b["level"], "ERROR");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_failing_rotation_does_not_let_the_file_grow() {
        let dir = temp_dir("rotfail");
        // With keep = 1 the rename onto `.1` fails while `.1` is a non-empty directory.
        std::fs::create_dir_all(dir.join("novagear.log.1")).unwrap();
        std::fs::write(dir.join("novagear.log.1").join("x"), "x").unwrap();
        let mut f = RotatingFile::new(&dir, 2_000, 1);
        let l = line("INFO", &"y".repeat(300));
        for _ in 0..100 {
            f.write_all(l.as_bytes()).unwrap();
        }
        let len = std::fs::metadata(dir.join("novagear.log")).unwrap().len();
        assert!(len <= 2_000, "file grew past the cap: {len}");
        assert_eq!(f.warnings(), 1, "one warning, not one per line");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
