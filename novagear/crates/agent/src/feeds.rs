//! Feeds: named live values that key faces and strip panels read. Built-in sources
//! (clock, system, lights, prompter, mic) publish here, and so will clients later.
//!
//! Nothing here polls on its own except the clock/system sampler, which sleeps until a
//! visible face or panel wants one of its keys.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::{Notify, watch};
use tokio::time::Instant;

/// Numbers kept per feed, for sparklines.
const HISTORY_LEN: usize = 60;

/// How long a sampled clock/system value stays fresh; the sampler runs every second.
const SAMPLED_TTL: Duration = Duration::from_secs(5);

/// For values that only change when something happens (lights, prompter, mic): they
/// never go stale just because nothing happened.
pub const FOREVER: Duration = Duration::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FeedState {
    Ok,
    Warn,
    Bad,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeedItem {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<FeedState>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FeedValue {
    Number(f64),
    Text(String),
    List(Vec<FeedItem>),
}

#[derive(Debug, Clone)]
pub struct Feed {
    pub value: FeedValue,
    pub unit: Option<String>,
    pub state: Option<FeedState>,
    pub updated: Instant,
    pub ttl: Duration,
    /// The last 60 numbers set, oldest first.
    pub history: VecDeque<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FeedSnapshot {
    pub value: FeedValue,
    pub unit: Option<String>,
    pub state: Option<FeedState>,
    pub stale: bool,
    pub history: Vec<f64>,
}

/// A non-zero number, text other than "" / "false" / "0", or a non-empty list.
pub fn truthy(v: &FeedValue) -> bool {
    match v {
        FeedValue::Number(n) => *n != 0.0 && !n.is_nan(),
        FeedValue::Text(t) => !t.is_empty() && t != "false" && t != "0",
        FeedValue::List(l) => !l.is_empty(),
    }
}

pub struct FeedStore {
    feeds: Mutex<BTreeMap<String, Feed>>,
    wanted: Mutex<BTreeSet<String>>,
    version: watch::Sender<u64>,
    /// Wakes the sampler when the wanted set changes.
    wanted_changed: Notify,
    /// Counts changes to the wanted set, for tasks that start or stop a source (the OBS
    /// link) instead of sleeping on `wanted_changed`, which only one waiter can use.
    wanted_version: watch::Sender<u64>,
}

impl Default for FeedStore {
    fn default() -> Self {
        Self {
            feeds: Mutex::default(),
            wanted: Mutex::default(),
            version: watch::channel(0).0,
            wanted_changed: Notify::new(),
            wanted_version: watch::channel(0).0,
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl FeedStore {
    pub fn set(
        &self,
        key: &str,
        value: FeedValue,
        unit: Option<String>,
        state: Option<FeedState>,
        ttl: Duration,
    ) {
        self.set_at(Instant::now(), key, value, unit, state, ttl);
    }

    /// `set` with the time passed in, so tests need no real waiting.
    pub fn set_at(
        &self,
        now: Instant,
        key: &str,
        value: FeedValue,
        unit: Option<String>,
        state: Option<FeedState>,
        ttl: Duration,
    ) {
        self.set_inner(now, key, value, unit, state, ttl, None);
    }

    /// `set`, unless `key` is new and `prefix` already has `cap` keys. The count and the
    /// insert share one lock, so concurrent pushes can't overshoot the cap.
    #[allow(clippy::too_many_arguments)]
    pub fn set_if_room(
        &self,
        prefix: &str,
        cap: usize,
        key: &str,
        value: FeedValue,
        unit: Option<String>,
        state: Option<FeedState>,
        ttl: Duration,
    ) -> bool {
        self.set_inner(
            Instant::now(),
            key,
            value,
            unit,
            state,
            ttl,
            Some((prefix, cap)),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn set_inner(
        &self,
        now: Instant,
        key: &str,
        value: FeedValue,
        unit: Option<String>,
        state: Option<FeedState>,
        ttl: Duration,
        room: Option<(&str, usize)>,
    ) -> bool {
        let changed = {
            let mut feeds = lock(&self.feeds);
            let existed = feeds.contains_key(key);
            if let Some((prefix, cap)) = room
                && !existed
                && feeds.keys().filter(|k| k.starts_with(prefix)).count() >= cap
            {
                return false;
            }
            let feed = feeds.entry(key.to_string()).or_insert_with(|| Feed {
                value: value.clone(),
                unit: unit.clone(),
                state,
                updated: now,
                ttl,
                history: VecDeque::new(),
            });
            // A new entry is always a change; for an existing
            // one a repeat of the same value is not, unless it was stale (the face
            // shows stale values differently).
            let changed = !existed
                || feed.value != value
                || feed.unit != unit
                || feed.state != state
                || feed.is_stale_at(now);
            feed.value = value;
            feed.unit = unit;
            feed.state = state;
            feed.updated = now;
            feed.ttl = ttl;
            if let FeedValue::Number(n) = feed.value {
                if feed.history.len() == HISTORY_LEN {
                    feed.history.pop_front();
                }
                feed.history.push_back(n);
            }
            changed
        };
        if changed {
            self.bump_if_wanted(key);
        }
        true
    }

    pub fn clear(&self, key: &str) {
        let removed = lock(&self.feeds).remove(key).is_some();
        if removed {
            self.bump_if_wanted(key);
        }
    }

    pub fn get(&self, key: &str) -> Option<FeedSnapshot> {
        self.get_at(Instant::now(), key)
    }

    pub fn get_at(&self, now: Instant, key: &str) -> Option<FeedSnapshot> {
        lock(&self.feeds).get(key).map(|f| f.snapshot(now))
    }

    /// When a feed goes stale, or `None` for a missing feed or one that never does.
    pub fn expires_at(&self, key: &str) -> Option<Instant> {
        let feeds = lock(&self.feeds);
        let f = feeds.get(key)?;
        f.updated.checked_add(f.ttl)
    }

    pub fn list(&self) -> Vec<(String, FeedSnapshot)> {
        let now = Instant::now();
        lock(&self.feeds)
            .iter()
            .map(|(k, f)| (k.clone(), f.snapshot(now)))
            .collect()
    }

    /// Every feed with how long ago it was last set, for the panel's feed list.
    pub fn list_with_age(&self) -> Vec<(String, FeedSnapshot, Duration)> {
        let now = Instant::now();
        lock(&self.feeds)
            .iter()
            .map(|(k, f)| {
                (
                    k.clone(),
                    f.snapshot(now),
                    now.saturating_duration_since(f.updated),
                )
            })
            .collect()
    }

    pub fn has(&self, key: &str) -> bool {
        lock(&self.feeds).contains_key(key)
    }

    pub fn count_with_prefix(&self, prefix: &str) -> usize {
        lock(&self.feeds)
            .keys()
            .filter(|k| k.starts_with(prefix))
            .count()
    }

    /// Replaces the set of keys something visible is reading.
    pub fn set_wanted(&self, mut keys: BTreeSet<String>) {
        // A template key not yet picked (`obs.meter.<pick:mic>`) names no real feed: nothing
        // can ever set it, so it must not start a source (it would switch OBS meters on).
        keys.retain(|k| !k.contains("<pick:"));
        let changed = {
            let mut wanted = lock(&self.wanted);
            if *wanted == keys {
                false
            } else {
                *wanted = keys;
                true
            }
        };
        if changed {
            self.wanted_changed.notify_one();
            self.wanted_version.send_modify(|v| *v += 1);
        }
    }

    /// A counter bumped whenever the set of wanted keys changes.
    pub fn wanted_changes(&self) -> watch::Receiver<u64> {
        self.wanted_version.subscribe()
    }

    pub fn is_wanted(&self, prefix: &str) -> bool {
        lock(&self.wanted).iter().any(|k| k.starts_with(prefix))
    }

    /// A counter bumped on every change to a wanted key.
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.version.subscribe()
    }

    fn bump_if_wanted(&self, key: &str) {
        if lock(&self.wanted).contains(key) {
            self.version.send_modify(|v| *v += 1);
        }
    }
}

impl Feed {
    fn is_stale_at(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.updated) >= self.ttl
    }

    fn snapshot(&self, now: Instant) -> FeedSnapshot {
        FeedSnapshot {
            value: self.value.clone(),
            unit: self.unit.clone(),
            state: self.state,
            stale: self.is_stale_at(now),
            history: self.history.iter().copied().collect(),
        }
    }
}

// ---- built-in sources: clock and system ----

/// One reading of the machine: the clock and, where the platform allows, load.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub time: String,
    pub date: String,
    pub cpu_pct: Option<f64>,
    pub mem_pct: Option<f64>,
}

/// Where samples come from; tests pass a fake.
pub trait Sampler: Send {
    fn sample(&mut self) -> Sample;
    /// Sampling stops until something wants a value again.
    fn park(&mut self) {}
}

/// Runs the clock/system feeds. Sleeps (no timer at all) until a `clock.` or `system.`
/// key is wanted, samples once a second while one is, and sleeps again when none is.
/// `samples` counts samples taken, for tests.
pub async fn run_sampler(
    store: Arc<FeedStore>,
    mut source: impl Sampler,
    samples: Arc<std::sync::atomic::AtomicUsize>,
) {
    loop {
        if !store.is_wanted("clock.") && !store.is_wanted("system.") {
            source.park();
            // Wakes for each change to the wanted set; only a real one comes back here.
            while !store.is_wanted("clock.") && !store.is_wanted("system.") {
                store.wanted_changed.notified().await;
            }
            continue;
        }
        let s = source.sample();
        samples.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        publish_sample(&store, s);
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn publish_sample(store: &FeedStore, s: Sample) {
    let pct = Some("%".to_string());
    store.set(
        "clock.time",
        FeedValue::Text(s.time),
        None,
        None,
        SAMPLED_TTL,
    );
    store.set(
        "clock.date",
        FeedValue::Text(s.date),
        None,
        None,
        SAMPLED_TTL,
    );
    if let Some(cpu) = s.cpu_pct {
        store.set(
            "system.cpu.pct",
            // Whole percents: the tenths are sampling noise, and each change of the text
            // redraws the deck panel that shows it.
            FeedValue::Number(cpu.round()),
            pct.clone(),
            load_state(cpu),
            SAMPLED_TTL,
        );
    }
    if let Some(mem) = s.mem_pct {
        store.set(
            "system.mem.pct",
            FeedValue::Number(mem.round()),
            pct,
            load_state(mem),
            SAMPLED_TTL,
        );
    }
}

fn load_state(pct: f64) -> Option<FeedState> {
    Some(if pct >= 90.0 {
        FeedState::Bad
    } else if pct >= 75.0 {
        FeedState::Warn
    } else {
        FeedState::Ok
    })
}

/// The real source: libc for the local time, and the kernel's own counters for load
/// (mach `host_statistics64` on macOS, `/proc` on Linux). No extra crate: `libc` is
/// already in the build, so the binary grows by a few KB instead of the ~100+ KB a
/// general system-info crate would add.
#[derive(Default)]
pub struct SystemSampler {
    /// (busy, total) CPU ticks at the previous sample; load needs two readings.
    prev_cpu: Option<(u64, u64)>,
}

impl Sampler for SystemSampler {
    /// The next reading after idle starts load afresh rather than averaging over the gap.
    fn park(&mut self) {
        self.prev_cpu = None;
    }

    fn sample(&mut self) -> Sample {
        let (time, date) = local_clock();
        let cpu_pct = cpu_ticks().and_then(|now| {
            let prev = self.prev_cpu.replace(now)?;
            let busy = now.0.saturating_sub(prev.0) as f64;
            let total = now.1.saturating_sub(prev.1) as f64;
            (total > 0.0).then(|| (busy / total * 100.0).clamp(0.0, 100.0).round())
        });
        Sample {
            time,
            date,
            cpu_pct,
            mem_pct: mem_pct(),
        }
    }
}

#[cfg(unix)]
fn local_clock() -> (String, String) {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    // SAFETY: `tm` is plain data that localtime_r fills; both pointers are valid locals.
    let tm = unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return (String::new(), String::new());
        }
        tm
    };
    (
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min),
        format!(
            "{} {} {}",
            DAYS[(tm.tm_wday as usize) % 7],
            tm.tm_mday,
            MONTHS[(tm.tm_mon as usize) % 12]
        ),
    )
}

#[cfg(not(unix))]
fn local_clock() -> (String, String) {
    (String::new(), String::new())
}

/// The host port, asked for once: each `mach_host_self()` call adds a send right to it that
/// is never given back, so calling it per sample leaks.
// libc marks mach_host_self deprecated in favour of the mach2 crate; one call isn't worth
// another dependency.
#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn host() -> libc::mach_port_t {
    static HOST: std::sync::OnceLock<libc::mach_port_t> = std::sync::OnceLock::new();
    // SAFETY: mach_host_self takes no arguments and returns a port name.
    *HOST.get_or_init(|| unsafe { libc::mach_host_self() })
}

#[cfg(target_os = "macos")]
fn cpu_ticks() -> Option<(u64, u64)> {
    // SAFETY: host_statistics fills the out struct up to the count we pass.
    let ticks = unsafe {
        let mut info: libc::host_cpu_load_info = std::mem::zeroed();
        let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
        let r = libc::host_statistics(
            host(),
            libc::HOST_CPU_LOAD_INFO,
            (&mut info as *mut libc::host_cpu_load_info).cast(),
            &mut count,
        );
        if r != 0 {
            return None;
        }
        info.cpu_ticks
    };
    let t = |i: libc::c_int| u64::from(ticks[i as usize]);
    let idle = t(libc::CPU_STATE_IDLE);
    let total =
        idle + t(libc::CPU_STATE_USER) + t(libc::CPU_STATE_SYSTEM) + t(libc::CPU_STATE_NICE);
    Some((total - idle, total))
}

#[cfg(target_os = "macos")]
fn mem_pct() -> Option<f64> {
    // SAFETY: host_statistics64 fills the out struct up to the count we pass; sysctl
    // writes a u64 into a buffer of that size.
    unsafe {
        let mut vm: libc::vm_statistics64 = std::mem::zeroed();
        let mut count = libc::HOST_VM_INFO64_COUNT;
        let r = libc::host_statistics64(
            host(),
            libc::HOST_VM_INFO64,
            (&mut vm as *mut libc::vm_statistics64).cast(),
            &mut count,
        );
        if r != 0 {
            return None;
        }
        let mut total: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        let name = c"hw.memsize";
        if libc::sysctlbyname(
            name.as_ptr(),
            (&mut total as *mut u64).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        ) != 0
            || total == 0
        {
            return None;
        }
        let page = libc::sysconf(libc::_SC_PAGESIZE).max(1) as u64;
        // Activity Monitor's "memory used": app (active) + wired + compressed.
        let used = (u64::from(vm.active_count)
            + u64::from(vm.wire_count)
            + u64::from(vm.compressor_page_count))
            * page;
        Some(
            (used as f64 / total as f64 * 100.0)
                .clamp(0.0, 100.0)
                .round(),
        )
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn cpu_ticks() -> Option<(u64, u64)> {
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    let line = text.lines().next()?.strip_prefix("cpu ")?;
    let n: Vec<u64> = line
        .split_whitespace()
        .filter_map(|v| v.parse().ok())
        .collect();
    if n.len() < 4 {
        return None;
    }
    // idle + iowait count as not busy.
    let idle = n[3] + n.get(4).copied().unwrap_or(0);
    let total: u64 = n.iter().take(8).sum();
    Some((total.saturating_sub(idle), total))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn mem_pct() -> Option<f64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb = |name: &str| -> Option<f64> {
        text.lines()
            .find_map(|l| l.strip_prefix(name))?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    };
    let total = kb("MemTotal:")?;
    let avail = kb("MemAvailable:")?;
    (total > 0.0).then(|| ((total - avail) / total * 100.0).clamp(0.0, 100.0).round())
}

#[cfg(not(unix))]
fn cpu_ticks() -> Option<(u64, u64)> {
    None
}

#[cfg(not(unix))]
fn mem_pct() -> Option<f64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn num(n: f64) -> FeedValue {
        FeedValue::Number(n)
    }

    fn keys(k: &[&str]) -> BTreeSet<String> {
        k.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn stale_after_ttl() {
        let store = FeedStore::default();
        let t0 = Instant::now();
        store.set_at(t0, "a.b", num(1.0), None, None, Duration::from_secs(2));
        assert!(
            !store
                .get_at(t0 + Duration::from_millis(1999), "a.b")
                .unwrap()
                .stale
        );
        assert!(
            store
                .get_at(t0 + Duration::from_secs(2), "a.b")
                .unwrap()
                .stale
        );
        // Setting again makes it fresh.
        let t1 = t0 + Duration::from_secs(5);
        store.set_at(t1, "a.b", num(1.0), None, None, Duration::from_secs(2));
        assert!(!store.get_at(t1, "a.b").unwrap().stale);
    }

    #[test]
    fn forever_ttl_never_goes_stale() {
        let store = FeedStore::default();
        let t0 = Instant::now();
        store.set_at(t0, "a.b", num(1.0), None, None, FOREVER);
        assert!(
            !store
                .get_at(t0 + Duration::from_secs(10_000_000), "a.b")
                .unwrap()
                .stale
        );
    }

    #[test]
    fn changes_only_bump_for_wanted_keys() {
        let store = FeedStore::default();
        store.set_wanted(keys(&["system.cpu.pct"]));
        let rx = store.changes();
        let v0 = *rx.borrow();
        store.set("foo.bar", num(1.0), None, None, FOREVER);
        assert_eq!(*rx.borrow(), v0);
        store.set("system.cpu.pct", num(5.0), None, None, FOREVER);
        assert_eq!(*rx.borrow(), v0 + 1);
        // Same value again is not a change.
        store.set("system.cpu.pct", num(5.0), None, None, FOREVER);
        assert_eq!(*rx.borrow(), v0 + 1);
        store.set("system.cpu.pct", num(6.0), None, None, FOREVER);
        assert_eq!(*rx.borrow(), v0 + 2);
        store.clear("system.cpu.pct");
        assert_eq!(*rx.borrow(), v0 + 3);
    }

    #[test]
    fn wanted_changes_bump_only_when_the_set_differs() {
        let store = FeedStore::default();
        let rx = store.wanted_changes();
        store.set_wanted(keys(&["obs.scene"]));
        assert_eq!(*rx.borrow(), 1);
        store.set_wanted(keys(&["obs.scene"]));
        assert_eq!(*rx.borrow(), 1);
        store.set_wanted(BTreeSet::new());
        assert_eq!(*rx.borrow(), 2);
    }

    #[test]
    fn history_keeps_last_60_numbers() {
        let store = FeedStore::default();
        for i in 0..100 {
            store.set("a.b", num(f64::from(i)), None, None, FOREVER);
        }
        let h = store.get("a.b").unwrap().history;
        assert_eq!(h.len(), 60);
        assert_eq!(h[0], 40.0);
        assert_eq!(h[59], 99.0);
        // Text feeds keep no history.
        store.set("a.t", FeedValue::Text("x".into()), None, None, FOREVER);
        assert!(store.get("a.t").unwrap().history.is_empty());
    }

    #[test]
    fn truthy_rules() {
        assert!(truthy(&num(1.0)));
        assert!(truthy(&num(-0.5)));
        assert!(!truthy(&num(0.0)));
        assert!(!truthy(&num(f64::NAN)));
        assert!(truthy(&FeedValue::Text("yes".into())));
        for f in ["", "false", "0"] {
            assert!(!truthy(&FeedValue::Text(f.into())), "{f:?}");
        }
        assert!(!truthy(&FeedValue::List(vec![])));
        assert!(truthy(&FeedValue::List(vec![FeedItem {
            label: "x".into(),
            state: None
        }])));
    }

    #[test]
    fn prefix_counts_and_wanted() {
        let store = FeedStore::default();
        store.set("obs.a", num(1.0), None, None, FOREVER);
        store.set("obs.b", num(1.0), None, None, FOREVER);
        store.set("mic.muted", num(0.0), None, None, FOREVER);
        assert_eq!(store.count_with_prefix("obs."), 2);
        assert_eq!(store.list().len(), 3);
        assert!(!store.is_wanted("clock."));
        store.set_wanted(keys(&["clock.time"]));
        assert!(store.is_wanted("clock."));
        assert!(!store.is_wanted("system."));
    }

    #[test]
    fn feed_value_json_is_untagged() {
        assert_eq!(serde_json::to_string(&num(3.0)).unwrap(), "3.0");
        let v: FeedValue = serde_json::from_str(r#""hi""#).unwrap();
        assert_eq!(v, FeedValue::Text("hi".into()));
        let v: FeedValue = serde_json::from_str(r#"[{"label":"a","state":"warn"}]"#).unwrap();
        assert_eq!(
            v,
            FeedValue::List(vec![FeedItem {
                label: "a".into(),
                state: Some(FeedState::Warn)
            }])
        );
    }

    struct Fake;
    impl Sampler for Fake {
        fn sample(&mut self) -> Sample {
            Sample {
                time: "14:05".into(),
                date: "Mon 2 Nov".into(),
                cpu_pct: Some(12.0),
                mem_pct: Some(50.0),
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn system_sampler_idles_when_nothing_is_wanted() {
        let store = Arc::new(FeedStore::default());
        let samples = Arc::new(AtomicUsize::new(0));
        tokio::spawn(run_sampler(store.clone(), Fake, samples.clone()));

        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(samples.load(Ordering::Relaxed), 0);
        assert!(store.get("clock.time").is_none());

        store.set_wanted(keys(&["system.cpu.pct"]));
        // Just under 3 s: the sampler runs at t=0, 1 and 2.
        tokio::time::sleep(Duration::from_millis(2999)).await;
        let n = samples.load(Ordering::Relaxed);
        assert!((1..=3).contains(&n), "samples: {n}");
        assert_eq!(
            store.get("system.cpu.pct").unwrap().value,
            FeedValue::Number(12.0)
        );

        // Nothing wanted again: it stops.
        store.set_wanted(BTreeSet::new());
        tokio::time::sleep(Duration::from_secs(2)).await;
        let after = samples.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert_eq!(samples.load(Ordering::Relaxed), after);
    }

    #[test]
    fn an_unpicked_template_key_is_not_wanted() {
        let store = FeedStore::default();
        store.set_wanted(keys(&["obs.meter.<pick:mic>", "clock.time"]));
        assert!(
            !store.is_wanted("obs."),
            "an unpicked meter must not switch OBS meters on"
        );
        assert!(store.is_wanted("clock."));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn parking_forgets_the_last_cpu_reading() {
        let mut s = SystemSampler::default();
        s.sample();
        assert!(s.prev_cpu.is_some());
        s.park();
        assert!(
            s.prev_cpu.is_none(),
            "the first reading after idle is not averaged over it"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_sampler_parks_its_source_when_nothing_is_wanted() {
        struct Parks(Arc<AtomicUsize>);
        impl Sampler for Parks {
            fn sample(&mut self) -> Sample {
                Fake.sample()
            }
            fn park(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let store = Arc::new(FeedStore::default());
        let parks = Arc::new(AtomicUsize::new(0));
        store.set_wanted(keys(&["system.cpu.pct"]));
        tokio::spawn(run_sampler(
            store.clone(),
            Parks(parks.clone()),
            Arc::default(),
        ));
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(parks.load(Ordering::Relaxed), 0);
        store.set_wanted(BTreeSet::new());
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(parks.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn real_sampler_reads_a_plausible_clock() {
        let mut s = SystemSampler::default();
        let first = s.sample();
        #[cfg(unix)]
        {
            assert_eq!(first.time.len(), 5, "{:?}", first.time);
            assert_eq!(&first.time[2..3], ":");
            assert!(!first.date.is_empty());
            // Load needs a second reading.
            assert!(first.cpu_pct.is_none());
            let mem = first.mem_pct.expect("memory reading");
            assert!((0.0..=100.0).contains(&mem));
        }
        let _ = first;
    }
}

#[cfg(test)]
mod room_tests {
    use super::*;

    #[test]
    fn set_if_room_is_exact_and_lets_existing_keys_update() {
        let s = FeedStore::default();
        let set =
            |k: &str, n: f64| s.set_if_room("c.", 2, k, FeedValue::Number(n), None, None, FOREVER);
        assert!(set("c.a", 1.0));
        assert!(set("c.b", 1.0));
        assert!(!set("c.c", 1.0), "third key refused");
        assert!(set("c.a", 2.0), "existing key still updates");
        s.set("d.x", FeedValue::Number(1.0), None, None, FOREVER);
        assert_eq!(s.count_with_prefix("c."), 2);
    }
}
