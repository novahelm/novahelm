//! Other local programs (Nova's AI and deploy agents, scripts) that may push values to the
//! deck and receive named events. Each holds its own token, scoped to its own feed prefix.
//! Only a sha256 of each token is stored, so a copy of `clients.json` can't be replayed.

use anyhow::{Result, bail};
use novagear_core::proof::{new_nonce, same};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Names that already prefix the agent's own feeds (or a route), so a client can't squat
/// on `obs.recording` by being called "obs".
const RESERVED: [&str; 11] = [
    "obs",
    "clock",
    "system",
    "mic",
    "prompter",
    "lights",
    "hw",
    "deck",
    "mode",
    "me",
    "approvals",
];
/// A bound on `clients.json`, and on the work `who` does per request.
const MAX_CLIENTS: usize = 32;

/// Feed keys one client may hold at once.
pub const MAX_FEED_KEYS: usize = 200;
/// Approvals one client may have waiting at once (used by the approvals routes).
pub const MAX_WAITING_APPROVALS: usize = 20;
const FEEDS_PER_SECOND: f64 = 10.0;
const APPROVAL_EVERY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    pub created: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Stored {
    name: String,
    /// sha256 of the token, hex.
    hash: String,
    created: u64,
}

/// `[a-z][a-z0-9-]{1,31}`, and not a name the agent uses itself.
pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    (2..=32).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && !RESERVED.contains(&name)
}

fn hash(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub struct Clients {
    path: PathBuf,
    list: Vec<Stored>,
}

impl Clients {
    /// Reads `clients.json`; a missing file is an empty list.
    pub fn load(path: PathBuf) -> Result<Clients> {
        let list = match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str::<Vec<Stored>>(&s)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        // The file is the user's to edit by hand, so it gets the same rules `add` applies.
        // Warnings say what kind of problem, never which entry or its hash.
        let mut kept: Vec<Stored> = Vec::new();
        for c in list {
            if !valid_name(&c.name) {
                tracing::warn!("clients.json: skipped an entry with an invalid or reserved name");
            } else if kept.iter().any(|k| k.name == c.name) {
                tracing::warn!("clients.json: skipped a duplicate entry");
            } else if kept.len() >= MAX_CLIENTS {
                tracing::warn!("clients.json: skipped entries past the {MAX_CLIENTS} client limit");
            } else {
                kept.push(c);
            }
        }
        let list = kept;
        Ok(Clients { path, list })
    }

    pub fn empty(path: PathBuf) -> Clients {
        Clients {
            path,
            list: Vec::new(),
        }
    }

    /// Makes a client and returns its token, which is not stored and can't be shown again.
    pub fn add(&mut self, name: &str) -> Result<String> {
        if !valid_name(name) {
            bail!("a client name is 2-32 characters: a-z, 0-9 and -, starting with a letter");
        }
        if self.list.iter().any(|c| c.name == name) {
            bail!("there is already a client called {name}");
        }
        if self.list.len() >= MAX_CLIENTS {
            bail!("too many clients ({MAX_CLIENTS}); remove one first");
        }
        let token = format!("ngc_{}{}", new_nonce()?, new_nonce()?);
        self.list.push(Stored {
            name: name.to_string(),
            hash: hash(&token),
            created: crate::unix_now(),
        });
        if let Err(e) = self.save() {
            self.list.pop();
            return Err(e);
        }
        Ok(token)
    }

    /// False when there was no such client.
    pub fn remove(&mut self, name: &str) -> Result<bool> {
        let Some(i) = self.list.iter().position(|c| c.name == name) else {
            return Ok(false);
        };
        let gone = self.list.remove(i);
        if let Err(e) = self.save() {
            self.list.insert(i, gone);
            return Err(e);
        }
        Ok(true)
    }

    pub fn exists(&self, name: &str) -> bool {
        self.list.iter().any(|c| c.name == name)
    }

    pub fn list(&self) -> Vec<ClientInfo> {
        self.list
            .iter()
            .map(|c| ClientInfo {
                name: c.name.clone(),
                created: c.created,
            })
            .collect()
    }

    /// The client a token belongs to. Every stored hash is compared, so timing doesn't say
    /// which one matched.
    pub fn who(&self, token: &str) -> Option<String> {
        let given = hash(token);
        let mut found = None;
        for c in &self.list {
            if same(&c.hash, &given) {
                found = Some(c.name.clone());
            }
        }
        found
    }

    /// Through a temp file, 0600 like config.json.
    fn save(&self) -> Result<()> {
        use std::io::Write;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        opts.open(&tmp)?
            .write_all(&serde_json::to_vec_pretty(&self.list)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// Who is calling, decided once by the auth middleware and attached to the request. Handlers
/// read this and never look at the credentials again.
#[derive(Debug, Clone, PartialEq)]
pub enum Caller {
    /// Holds the agent token (the CLI, the tray).
    Agent,
    /// A browser panel session.
    Panel,
    Client(String),
}

impl Caller {
    /// The agent's own front ends, which may use any route and any feed key.
    pub fn is_staff(&self) -> bool {
        !matches!(self, Caller::Client(_))
    }
}

/// `prefix` followed by exactly one non-empty path segment.
fn one_segment(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix)
        .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
}

/// What a client token may reach, by method and path under `/api`. Everything else is 403
/// for a client, so a new route is closed to clients until it is listed here.
pub fn client_may(method: &str, path: &str) -> bool {
    match (method, path) {
        ("GET", "/status") | ("GET", "/clients/me/events") => true,
        ("PUT" | "DELETE", p) if one_segment(p, "/feeds/") => true,
        // Create, read and cancel one's own approval. Each handler still checks the approval
        // belongs to the calling client. Listing them all (`GET /approvals`) and denying
        // (`POST /approvals/{id}/deny`) are staff-only, and nothing approves over HTTP.
        ("POST", "/approvals") => true,
        ("GET" | "DELETE", p) => one_segment(p, "/approvals/"),
        _ => false,
    }
}

/// A refill-over-time bucket.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn take(&mut self, now: Instant, capacity: f64, per_second: f64) -> bool {
        let dt = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens = (self.tokens + dt * per_second).min(capacity);
        self.at = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Per-client request rates.
pub struct ClientLimits {
    feed_per_second: f64,
    approval_every: Duration,
    feeds: BTreeMap<String, Bucket>,
    approvals: BTreeMap<String, Bucket>,
}

impl Default for ClientLimits {
    fn default() -> Self {
        Self::new(FEEDS_PER_SECOND, APPROVAL_EVERY)
    }
}

impl ClientLimits {
    /// Other rates exist so tests don't have to wait out the real ones.
    pub fn new(feed_per_second: f64, approval_every: Duration) -> Self {
        Self {
            feed_per_second,
            approval_every,
            feeds: BTreeMap::new(),
            approvals: BTreeMap::new(),
        }
    }

    pub fn allow_feed(&mut self, client: &str) -> bool {
        self.allow_feed_at(Instant::now(), client)
    }

    pub fn allow_feed_at(&mut self, now: Instant, client: &str) -> bool {
        let cap = self.feed_per_second;
        self.feeds
            .entry(client.to_string())
            .or_insert(Bucket {
                tokens: cap,
                at: now,
            })
            .take(now, cap, cap)
    }

    /// One approval per `approval_every`, no bursts.
    pub fn allow_approval(&mut self, client: &str) -> bool {
        self.allow_approval_at(Instant::now(), client)
    }

    pub fn allow_approval_at(&mut self, now: Instant, client: &str) -> bool {
        let rate = 1.0 / self.approval_every.as_secs_f64();
        self.approvals
            .entry(client.to_string())
            .or_insert(Bucket {
                tokens: 1.0,
                at: now,
            })
            .take(now, 1.0, rate)
    }

    /// Forgets a removed client.
    pub fn forget(&mut self, client: &str) {
        self.feeds.remove(client);
        self.approvals.remove(client);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("novagear-clients-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("clients.json")
    }

    #[test]
    fn clients_reach_only_their_routes() {
        assert!(client_may("GET", "/status"));
        assert!(client_may("PUT", "/feeds/a.b"));
        assert!(client_may("DELETE", "/feeds/a.b"));
        assert!(client_may("GET", "/clients/me/events"));
        assert!(client_may("POST", "/approvals"));
        assert!(client_may("GET", "/approvals/abc"));
        assert!(client_may("DELETE", "/approvals/abc"));
        for (m, p) in [
            ("POST", "/action"),
            ("GET", "/config"),
            ("PUT", "/config"),
            ("GET", "/logs"),
            ("GET", "/clients"),
            ("POST", "/clients"),
            ("DELETE", "/clients/other"),
            ("GET", "/feeds"),
            ("GET", "/feeds/a.b"),
            ("POST", "/feeds/a.b"),
            ("GET", "/events"),
            ("POST", "/status"),
            ("GET", "/approvalsx"),
            ("GET", "/devices"),
            ("POST", "/panel-code"),
            ("POST", "/import/streamdeck"),
            ("GET", "/templates"),
            ("POST", "/templates/developer-creator/apply"),
            ("PUT", "/images"),
            ("POST", "/images"),
            ("GET", "/obs/scenes"),
            ("GET", "/obs/inputs"),
            ("GET", ""),
            ("GET", "/"),
            ("POST", "/approvals/x/deny"),
            ("GET", "/approvals"),
            ("DELETE", "/approvals"),
            ("PUT", "/approvals/x"),
            ("GET", "/approvals/x/y"),
            ("GET", "/approvals/"),
            ("POST", "/approvals/x"),
            ("POST", "/approvals/x/approve"),
            ("POST", "/approvals/x/approved"),
            ("PUT", "/approvals"),
            ("PATCH", "/approvals/x"),
            ("PATCH", "/approvals"),
            ("POST", "/approvals/"),
            ("GET", "/approvals/x/deny"),
            ("HEAD", "/status"),
            ("OPTIONS", "/status"),
            ("GET", "/status/"),
            ("PUT", "/feeds"),
            ("PUT", "/feeds/"),
            ("GET", "/icons/x"),
            ("GET", "/prompter"),
            ("POST", "/lights/x/identify"),
        ] {
            assert!(!client_may(m, p), "{m} {p}");
        }
    }

    #[test]
    fn names_follow_the_rule() {
        for ok in ["novaguard", "ab", "a-1", &format!("a{}", "b".repeat(31))] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in [
            "",
            "a",
            "A",
            "1a",
            "-a",
            "a_b",
            "a.b",
            "a b",
            "obs",
            "system",
            "me",
            "é1",
            &format!("a{}", "b".repeat(32)),
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn a_token_names_its_client_and_only_the_hash_is_kept() {
        let path = temp("who");
        let mut c = Clients::load(path.clone()).unwrap();
        let a = c.add("alpha").unwrap();
        let b = c.add("beta").unwrap();
        assert_ne!(a, b);
        assert_eq!(c.who(&a).as_deref(), Some("alpha"));
        assert_eq!(c.who(&b).as_deref(), Some("beta"));
        assert_eq!(c.who("nope"), None);
        assert_eq!(c.who(""), None);
        assert!(c.add("alpha").is_err(), "no duplicates");

        let file = std::fs::read_to_string(&path).unwrap();
        assert!(
            !file.contains(&a) && !file.contains(&b),
            "no plain token on disk"
        );
        assert!(file.contains(&hash(&a)));
        // Survives a restart.
        let again = Clients::load(path.clone()).unwrap();
        assert_eq!(again.who(&a).as_deref(), Some("alpha"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_hand_edited_file_cannot_authorise_a_reserved_name() {
        let path = temp("edited");
        let good = hash("good-token");
        let bad = hash("evil-token");
        let mut entries = vec![
            json!({ "name": "obs", "hash": bad, "created": 1 }),
            json!({ "name": "Bad Name", "hash": bad, "created": 1 }),
            json!({ "name": "good", "hash": good, "created": 1 }),
            json!({ "name": "good", "hash": bad, "created": 1 }),
        ];
        for i in 0..MAX_CLIENTS + 3 {
            entries.push(json!({ "name": format!("extra{i}"), "hash": hash(&format!("x{i}")), "created": 1 }));
        }
        std::fs::write(&path, serde_json::to_string(&entries).unwrap()).unwrap();
        let c = Clients::load(path).unwrap();
        assert_eq!(c.who("evil-token"), None);
        assert_eq!(c.who("good-token").as_deref(), Some("good"));
        assert_eq!(c.list().len(), MAX_CLIENTS);
    }

    #[test]
    fn removing_stops_the_token() {
        let mut c = Clients::load(temp("remove")).unwrap();
        let t = c.add("gone").unwrap();
        assert!(c.remove("gone").unwrap());
        assert!(!c.remove("gone").unwrap());
        assert_eq!(c.who(&t), None);
        assert!(c.list().is_empty());
    }

    #[test]
    fn feed_bucket_allows_ten_a_second_then_refills() {
        let mut l = ClientLimits::default();
        let t0 = Instant::now();
        let ok = (0..15).filter(|_| l.allow_feed_at(t0, "a")).count();
        assert_eq!(ok, 10);
        assert!(l.allow_feed_at(t0, "b"), "buckets are per client");
        assert!(l.allow_feed_at(t0 + Duration::from_millis(150), "a"));
    }

    #[test]
    fn approvals_come_one_per_five_seconds() {
        let mut l = ClientLimits::default();
        let t0 = Instant::now();
        assert!(l.allow_approval_at(t0, "a"));
        assert!(!l.allow_approval_at(t0 + Duration::from_secs(4), "a"));
        assert!(l.allow_approval_at(t0 + Duration::from_secs(6), "a"));
        assert!(!l.allow_approval_at(t0 + Duration::from_secs(7), "a"));
    }
}
