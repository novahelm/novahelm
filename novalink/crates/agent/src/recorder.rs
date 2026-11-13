//! Rolling in-memory record of recent events, dumped to disk on anomalies.
//!
//! Dump reasons used by the agent: `reconnect`, `latency_spike`, `safety_release`,
//! `hook_reinstalled`, `tap_reenabled`.

use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use time::macros::format_description;

pub struct FlightRecorder {
    span: Duration,
    max_records: usize,
    records: VecDeque<(Instant, serde_json::Value)>,
}

/// Default cap on retained records, bounding memory under a flood.
pub const DEFAULT_MAX_RECORDS: usize = 20_000;

/// Replace any `usage` field (key identity) with null, at any depth.
fn redact(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(m) => {
            for (k, val) in m.iter_mut() {
                if k == "usage" {
                    *val = serde_json::Value::Null;
                } else {
                    redact(val);
                }
            }
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(redact),
        _ => {}
    }
}

impl FlightRecorder {
    pub fn new(span: Duration) -> Self {
        Self::with_max_records(span, DEFAULT_MAX_RECORDS)
    }

    pub fn with_max_records(span: Duration, max_records: usize) -> Self {
        Self {
            span,
            max_records: max_records.max(1),
            records: VecDeque::new(),
        }
    }

    /// Append a record and evict anything older than `span` relative to `now`.
    pub fn push(&mut self, now: Instant, mut record: serde_json::Value) {
        redact(&mut record);
        self.records.push_back((now, record));
        while self.records.len() > self.max_records {
            self.records.pop_front();
        }
        while let Some((t, _)) = self.records.front() {
            if now.saturating_duration_since(*t) > self.span {
                self.records.pop_front();
            } else {
                break;
            }
        }
    }

    /// The newest `n` records, oldest first.
    pub fn recent(&self, n: usize) -> Vec<serde_json::Value> {
        let skip = self.records.len().saturating_sub(n);
        self.records
            .iter()
            .skip(skip)
            .map(|(_, v)| v.clone())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Write `flight-<UTC yyyymmddThhmmssZ>-<reason>.jsonl` (`-1`, `-2`, ... appended on
    /// a same-second collision) with the records within `span` of `now`, oldest first.
    /// The file is created exclusively with mode 0600 on unix.
    pub fn dump(&self, now: Instant, dir: &Path, reason: &str) -> Result<PathBuf> {
        write_records(&self.snapshot(now), dir, reason)
    }

    /// The records within `span` of `now`, oldest first (cheap to take under a lock; write
    /// them later with [`write_records`]).
    pub fn snapshot(&self, now: Instant) -> Vec<serde_json::Value> {
        self.records
            .iter()
            .filter(|(t, _)| now.saturating_duration_since(*t) <= self.span)
            .map(|(_, v)| v.clone())
            .collect()
    }
}

/// Write `records` as `flight-<stamp>-<reason>.jsonl` (see [`FlightRecorder::dump`]).
pub fn write_records(records: &[serde_json::Value], dir: &Path, reason: &str) -> Result<PathBuf> {
    {
        crate::paths::create_private_dir(dir)?;
        let fmt = format_description!("[year][month][day]T[hour][minute][second]Z");
        let stamp = OffsetDateTime::now_utc().format(&fmt)?;
        let reason: String = reason
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let mut n = 0u32;
        let (path, mut f) = loop {
            let name = if n == 0 {
                format!("flight-{stamp}-{reason}.jsonl")
            } else {
                format!("flight-{stamp}-{reason}-{n}.jsonl")
            };
            let path = dir.join(name);
            match opts.open(&path) {
                Ok(f) => break (path, f),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 10_000 => n += 1,
                Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
            }
        };
        let mut w = std::io::BufWriter::new(&mut f);
        for rec in records {
            serde_json::to_writer(&mut w, rec)?;
            w.write_all(b"\n")?;
        }
        w.flush()?;
        drop(w);
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recorder_evicts_old_and_dumps_in_order() {
        let t = Instant::now();
        let mut r = FlightRecorder::new(Duration::from_secs(30));
        r.push(t, json!({"n":1}));
        r.push(t + Duration::from_secs(31), json!({"n":2}));
        r.push(t + Duration::from_secs(32), json!({"n":3}));
        let dir = tempfile::tempdir().unwrap();
        let p = r
            .dump(t + Duration::from_secs(32), dir.path(), "reconnect")
            .unwrap();
        let name = p.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("flight-") && name.ends_with("-reconnect.jsonl"));
        assert_eq!(name.len(), "flight-".len() + 16 + "-reconnect.jsonl".len());
        assert_eq!(
            std::fs::read_to_string(p).unwrap(),
            "{\"n\":2}\n{\"n\":3}\n"
        );
    }

    #[test]
    fn dump_excludes_records_older_than_span_relative_to_now() {
        let t = Instant::now();
        let mut r = FlightRecorder::new(Duration::from_secs(30));
        r.push(t, json!({"n":1}));
        r.push(t + Duration::from_secs(10), json!({"n":2}));
        let dir = tempfile::tempdir().unwrap();
        let p = r
            .dump(t + Duration::from_secs(60), dir.path(), "x")
            .unwrap();
        assert_eq!(std::fs::read_to_string(p).unwrap(), "");
        let p = r
            .dump(t + Duration::from_secs(35), dir.path(), "y")
            .unwrap();
        assert_eq!(std::fs::read_to_string(p).unwrap(), "{\"n\":2}\n");
    }

    #[test]
    fn recent_returns_newest_n_in_order() {
        let t = Instant::now();
        let mut r = FlightRecorder::new(Duration::from_secs(30));
        for n in 0..5 {
            r.push(t, json!({"n": n}));
        }
        assert_eq!(r.recent(2), vec![json!({"n":3}), json!({"n":4})]);
        assert_eq!(r.recent(99).len(), 5);
    }

    #[test]
    fn max_records_drops_oldest() {
        let t = Instant::now();
        let mut r = FlightRecorder::with_max_records(Duration::from_secs(30), 3);
        for n in 0..10 {
            r.push(t, json!({ "n": n }));
        }
        assert_eq!(r.len(), 3);
        let dir = tempfile::tempdir().unwrap();
        let p = r.dump(t, dir.path(), "flood").unwrap();
        assert_eq!(
            std::fs::read_to_string(p).unwrap(),
            "{\"n\":7}\n{\"n\":8}\n{\"n\":9}\n"
        );
        assert_eq!(
            FlightRecorder::new(Duration::from_secs(30)).max_records,
            20_000
        );
    }

    #[test]
    fn usage_is_redacted() {
        let t = Instant::now();
        let mut r = FlightRecorder::new(Duration::from_secs(30));
        r.push(
            t,
            json!({"kind":"key","usage":4,"nested":{"usage":7,"ok":1}}),
        );
        let dir = tempfile::tempdir().unwrap();
        let p = r.dump(t, dir.path(), "r").unwrap();
        let v: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(p).unwrap().trim()).unwrap();
        assert!(v["usage"].is_null());
        assert!(v["nested"]["usage"].is_null());
        assert_eq!(v["nested"]["ok"], 1);
        assert_eq!(v["kind"], "key");
    }

    #[test]
    fn same_second_collision_gets_suffix_and_never_overwrites() {
        let t = Instant::now();
        let mut r = FlightRecorder::new(Duration::from_secs(30));
        r.push(t, json!({"a":1}));
        let dir = tempfile::tempdir().unwrap();
        let ps: Vec<_> = (0..3)
            .map(|_| r.dump(t, dir.path(), "same").unwrap())
            .collect();
        let mut u = ps.clone();
        u.sort();
        u.dedup();
        assert_eq!(u.len(), 3);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn dump_file_is_private_and_missing_dir_created_0700() {
        use std::os::unix::fs::PermissionsExt;
        let t = Instant::now();
        let mut r = FlightRecorder::new(Duration::from_secs(30));
        r.push(t, json!({"a":1}));
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("x/y");
        let p = r.dump(t, &sub, "tap_reenabled").unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&sub).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}
