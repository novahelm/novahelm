//! The NovaGear agent: owns every device, runs actions, and pushes changes to the panel.
//! One current-thread tokio runtime; blocking HID work goes to the blocking pool or the
//! Stream Deck's own thread.

pub mod api;
pub mod approvals;
pub mod clients;
pub mod deck;
pub mod elgato_fw;
pub mod feeds;
pub mod import;
pub mod logfile;
pub mod macos_events;
pub mod modes;
pub mod obs;
pub mod panel_auth;
pub mod sleep;
pub mod system;
pub mod templates;

use anyhow::{Context, Result, bail};
use novagear_core::config::Config;
use novagear_core::{Action, DeviceInfo, DeviceKind, FirmwareInfo, Status, UsbId};
use novagear_drivers::catalog::{self, HidDevice, UsbDevice};
use novagear_drivers::devices::{Family, Record, Records};
use novagear_drivers::hotplug::{self, Watch};
use novagear_drivers::keylight::{self, LightState};
use novagear_drivers::razer;
use novagear_drivers::streamdeck::{self, Command, Input};
use serde_json::json;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;

/// Quiet time after the last hotplug event before re-listing (a plug makes a burst).
const DEBOUNCE: Duration = Duration::from_millis(300);
/// A burst of hotplug events is listed at most this long after its first event.
const BURST_MAX: Duration = Duration::from_secs(2);
/// A full re-list now and then in case an event was missed.
const SAFETY: Duration = Duration::from_secs(60);
/// While a Bluetooth device was around recently, HID is polled this often: Bluetooth
/// connects send no USB event.
const BT_POLL: Duration = Duration::from_secs(10);
/// How long after the last Bluetooth sighting the polling goes on.
const BT_LINGER: Duration = Duration::from_secs(60);
/// Re-list period when the OS gives no hotplug events.
const FALLBACK_SCAN: Duration = Duration::from_secs(2);
/// How often light state is re-read, to pick up changes made from elsewhere.
const LIGHT_REFRESH: Duration = Duration::from_secs(20);
/// Wait before retrying a device that failed to open, so a held device isn't hammered.
const RETRY: Duration = Duration::from_secs(15);
/// How long after the last knob-turned setting the config file is written.
const CONFIG_DEBOUNCE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct Light {
    pub addr: SocketAddr,
    pub name: String,
    pub serial: String,
    pub firmware: String,
    /// accessory-info `hardwareBoardType`; picks the firmware image to compare with.
    pub board: u8,
    pub build: u32,
    pub state: Option<LightState>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct PrompterRun {
    pub playing: bool,
    /// Bumped on restart so every open prompter window jumps to the top.
    pub restarts: u32,
}

/// What the deck screen shows, as a hash of the JPEG last sent to each key and strip
/// region. A picture the screen already has is not sent again: every report is a USB
/// write, and the same picture comes round whenever a feed changes to the same text.
/// It lives in `DeckSlot`, so a deck that closes and opens again starts empty.
#[derive(Default)]
struct OnScreen {
    keys: std::collections::HashMap<usize, u64>,
    /// (x, width, hash) of each strip image.
    strip: Vec<(u16, u16, u64)>,
}

fn hash_bytes(b: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

impl OnScreen {
    /// Whether `cmd` changes the screen, remembering what it will show.
    fn changes(&mut self, cmd: &Command) -> bool {
        match cmd {
            Command::KeyImage { index, jpeg } => {
                let h = hash_bytes(jpeg);
                self.keys.insert(*index, h) != Some(h)
            }
            Command::StripImage { x, width, jpeg } => {
                let h = hash_bytes(jpeg);
                if self.strip.contains(&(*x, *width, h)) {
                    return false;
                }
                // Whatever this one covers is no longer what was recorded there.
                let (lo, hi) = (u32::from(*x), u32::from(*x) + u32::from(*width));
                self.strip.retain(|&(sx, sw, _)| {
                    let (a, b) = (u32::from(sx), u32::from(sx) + u32::from(sw));
                    b <= lo || a >= hi
                });
                self.strip.push((*x, *width, h));
                true
            }
            Command::Reset => {
                self.keys.clear();
                self.strip.clear();
                true
            }
            Command::Brightness(_) => true,
        }
    }
}

#[derive(Default)]
struct DeckSlot {
    on_screen: OnScreen,
    commands: Option<std::sync::mpsc::Sender<Command>>,
    info: Option<streamdeck::DeckInfo>,
    error: Option<String>,
    retry_at: Option<Instant>,
}

#[derive(Default)]
struct MouseSlot {
    pid: Option<u16>,
    dpi: Option<u16>,
    /// Read once per connection, with the first DPI read.
    firmware: Option<String>,
    error: Option<String>,
    retry_at: Option<Instant>,
}

/// The firmware check's memory: what Elgato last shipped, and when a check last failed.
#[derive(Default)]
struct FirmwareState {
    cache: Option<elgato_fw::Cache>,
    last_failure: Option<u64>,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Why a client's feed write was refused.
#[derive(Debug, PartialEq)]
pub enum ClientPutError {
    Gone,
    Full,
}

/// How many presses and releases are kept per key. A hold lasts a second and a key can't
/// change state that often, so this covers far more than one hold's window.
const KEY_LOG_LEN: usize = 8;

/// What the deck read of a key after a hold began.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeySince {
    /// Nothing after the press: as far as the deck has said, the key is still down.
    StillDown,
    /// The key's first release after the press, as the deck stamped it.
    Released(Instant),
    /// The key was pressed again, or the history lost events after the press: the hold is
    /// not one continuous press, whatever the stamps say.
    Broken,
}

/// One key's recent presses and releases, oldest first.
#[derive(Default)]
pub(crate) struct KeyLog {
    events: std::collections::VecDeque<(Instant, bool)>,
    /// The latest stamp dropped to stay within `KEY_LOG_LEN`.
    lost: Option<Instant>,
}

impl KeyLog {
    fn note(&mut self, at: Instant, down: bool) {
        self.events.push_back((at, down));
        while self.events.len() > KEY_LOG_LEN {
            self.lost = self.events.pop_front().map(|(t, _)| t);
        }
    }

    /// Only the first event after `since` counts: a release that follows a later press
    /// belongs to that press, never to the hold that began at `since`.
    fn since(&self, since: Instant) -> KeySince {
        if self.lost.is_some_and(|t| t > since) {
            return KeySince::Broken;
        }
        match self.events.iter().find(|(t, _)| *t > since) {
            None => KeySince::StillDown,
            Some(&(t, false)) => KeySince::Released(t),
            Some(&(_, true)) => KeySince::Broken,
        }
    }
}

pub struct Agent {
    cfg_path: PathBuf,
    /// Shared, not copied: a redraw pass or key press reads it, and a copy of an imported
    /// config is hundreds of KB of small allocations.
    cfg: Mutex<Arc<Config>>,
    records: Arc<Records>,
    /// Problems found in the user's devices.toml, shown by `/api/status`.
    problems: Mutex<Vec<String>>,
    lights: Mutex<BTreeMap<String, Light>>,
    usb: Mutex<Vec<UsbDevice>>,
    hid: Mutex<Vec<HidDevice>>,
    deck: Mutex<DeckSlot>,
    mouse: Mutex<MouseSlot>,
    prompter: Mutex<PrompterRun>,
    nav: Mutex<modes::ModeState>,
    ui: Mutex<modes::DeckUi>,
    /// Per key: the last few presses and releases the deck's thread read, with their stamps.
    /// Noted as the report is read, before the input waits in the queue, so a hold's timer
    /// sees what the input loop has not got to yet. A leaf lock.
    key_log: Mutex<[KeyLog; streamdeck::KEYS]>,
    /// Decoded key images by name; cleared when the config or the image files change.
    images: Arc<deck::FsImages>,
    /// Wakes the redraw task when the deck was redrawn, so it re-reads what is on screen.
    redraw_wake: tokio::sync::Notify,
    /// Counts changes to the current mode or page, so only the last one is written.
    save_gen: Arc<std::sync::atomic::AtomicU64>,
    /// The same for knob-turned settings (`set_scalar`).
    cfg_save_gen: std::sync::atomic::AtomicU64,
    /// Set while a `set_scalar` change is not on disk yet (see `flush_saves`).
    scalar_dirty: std::sync::atomic::AtomicBool,
    /// The mode and page `save_soon` has not written yet (see `flush_saves`). A leaf lock.
    state_pending: Arc<Mutex<Option<(String, String)>>>,
    /// Every action `run` was asked to do; for tests.
    #[cfg(test)]
    ran: Mutex<Vec<Action>>,
    events: broadcast::Sender<String>,
    /// Device id -> name as last logged, so connects and drops are logged once.
    logged_devices: Mutex<BTreeMap<String, String>>,
    /// When set, typing and hotkeys fail before posting any event; for tests.
    input_dry_run: Arc<std::sync::atomic::AtomicBool>,
    fw: Mutex<FirmwareState>,
    /// Wakes the daily check when a light appears or the setting changes.
    fw_wake: tokio::sync::Notify,
    /// Set while lights or mDNS fail with unreachable-host errors (macOS Local Network
    /// privacy); cleared by the next light call that works.
    network_blocked: std::sync::atomic::AtomicBool,
    /// The block came from mDNS alone, so a resolved advert clears it.
    mdns_blocked: std::sync::atomic::AtomicBool,
    /// When a light call last worked; an unreachable error right after one is a dead
    /// light, not a blocked network.
    last_light_success: Mutex<Option<Instant>>,
    pub token: String,
    pub panel: Mutex<panel_auth::PanelAuth>,
    pub feeds: Arc<feeds::FeedStore>,
    /// Waiting and decided approval requests, in memory only. A leaf lock: taken after
    /// `clients` and `ui`, and nothing else is taken while it is held.
    approvals: Mutex<approvals::Approvals>,
    /// The links the deck's Details key opened; tests read it instead of opening a browser.
    #[cfg(test)]
    opened: Mutex<Vec<String>>,
    /// Makes the pop-up strip fail to draw; for tests.
    #[cfg(test)]
    popup_draw_fails: std::sync::atomic::AtomicBool,
    /// Programs allowed to push feeds and receive named events (`clients.json`).
    pub clients: Mutex<clients::Clients>,
    pub limits: Mutex<clients::ClientLimits>,
    /// One channel per client that has ever listened; `ClientEvent` actions go here.
    client_events: Mutex<BTreeMap<String, broadcast::Sender<String>>>,
    /// The persistent OBS connection and the config it was made with; present only while
    /// something needs OBS.
    obs_link: Mutex<Option<(novagear_core::config::ObsConfig, obs::ObsLink)>>,
    /// Links told to stop whose threads may still be cleaning up; the next link waits for
    /// them on its own thread. Taken only inside `obs_link`.
    obs_retiring: Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// Whether anything needs OBS right now; the link thread reads it to know when to quit.
    obs_needed: Arc<std::sync::atomic::AtomicBool>,
    obs_events_tx: mpsc::UnboundedSender<obs::ObsEvent>,
    /// Where the link's events wait for the mode rules (`take_obs_events`).
    obs_events_rx: Mutex<Option<mpsc::UnboundedReceiver<obs::ObsEvent>>>,
    /// What the mode rules look at: OBS and the frontmost app.
    facts: Mutex<modes::Facts>,
    /// Idle time and sleep state of the deck. A leaf lock.
    sleep: Mutex<sleep::SleepTimer>,
    /// Wakes the events loop when the sleep setting changes, so it re-reads its deadline.
    sleep_wake: tokio::sync::Notify,
    os_events_tx: mpsc::UnboundedSender<macos_events::OsEvent>,
    os_events_rx: Mutex<Option<mpsc::UnboundedReceiver<macos_events::OsEvent>>>,
    /// `Some` while the OS observer is wanted; inside, its subscriptions (none off macOS).
    os_observer: Mutex<Option<Option<macos_events::Handle>>>,
    /// How many times the observer was started; for tests.
    #[cfg(test)]
    os_spawns: std::sync::atomic::AtomicUsize,
}

/// What a light shows while the OS refuses it local network access.
const BLOCKED_REASON: &str = if cfg!(target_os = "macos") {
    "macOS is blocking local network access for NovaGear"
} else {
    "local network access is blocked"
};

/// Runs a closure when dropped.
struct OnDrop<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> OnDrop<F> {
    fn new(f: F) -> Self {
        Self(Some(f))
    }
}

impl<F: FnOnce()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

/// What an unreachable light shows while the network as a whole looks fine.
const UNREACHABLE_REASON: &str = "can't reach the light (is it on and on this network?)";

/// How long after a working light call an unreachable error is not read as a block.
const BLOCK_WINDOW: Duration = Duration::from_secs(60);

/// Wait before light discovery restarts after it stops with an error.
/// Sends one small mDNS query (a PTR question for the Key Light service) to the mDNS
/// group and reports only whether the send worked.
fn probe_mdns() -> std::io::Result<()> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0")?;
    sock.set_nonblocking(true)?;
    // Header: id 0, flags 0, one question; then "_elg._tcp.local" PTR IN.
    let mut q = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in ["_elg", "_tcp", "local"] {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.extend_from_slice(&[0, 0, 12, 0, 1]);
    sock.send_to(&q, "224.0.0.251:5353").map(|_| ())
}

const DISCOVERY_RETRY: Duration = Duration::from_secs(30);

/// An action's failure as it may be logged. OBS's own comment can name an input or a scene,
/// so only the request type and code are kept; the panel still gets the full text.
fn failure_log(e: &anyhow::Error) -> String {
    match e.downcast_ref::<obs::RequestFailed>() {
        Some(f) => format!("OBS {} failed (code {})", f.request_type, f.code),
        None => format!("{e:#}"),
    }
}

/// The most a multi-action may wait between its steps, in all.
const SEQUENCE_MAX_DELAY: Duration = Duration::from_secs(60);

/// How deep the back stack goes before the oldest entry is dropped.
const BACK_DEPTH: usize = 16;

/// Runs platform input (keystrokes) unless dry-run is on, in which case it errors before
/// anything is posted. Tests that push real `TypeText` / `Hotkey` actions turn it on.
/// Where the deck's thread queues inputs for the agent, each with the moment it was read.
pub(crate) type DeckInputs = mpsc::UnboundedSender<(Input, Instant)>;

fn guarded_input<T>(
    dry_run: &std::sync::atomic::AtomicBool,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
    if dry_run.load(std::sync::atomic::Ordering::Relaxed) {
        bail!("keyboard input is switched off (dry run)");
    }
    f()
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while holding one of these short locks leaves plain data behind; keep going.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What "Copy device details" copies for an unlisted device. No serial and no path.
fn unknown_details(u: &UsbDevice) -> serde_json::Value {
    json!({
        "vid": format!("{:04x}", u.usb.vid),
        "pid": format!("{:04x}", u.usb.pid),
        "product": u.product,
        "manufacturer": u.vendor,
        "interface_classes": u.interfaces,
        "usage_pages": u.usage_pages.iter().map(|p| format!("{p:#06x}")).collect::<Vec<_>>(),
        "device_version": u.device_version,
    })
}

fn light_id(serial: &str, fullname: &str) -> String {
    if serial.is_empty() {
        format!("light-{fullname}")
    } else {
        format!("light-{serial}")
    }
}

impl Agent {
    pub fn new(cfg_path: PathBuf, token: String) -> Result<Arc<Agent>> {
        let cfg = Config::load(&cfg_path)?;
        // The mode and page the deck was last on; `resolve` fixes up what no longer exists.
        let (mode, page) = cfg_path
            .parent()
            .and_then(modes::load_state)
            .unwrap_or_else(|| (cfg.start_mode.clone(), cfg.start_page.clone()));
        let mut nav = modes::ModeState {
            mode,
            page,
            ..Default::default()
        };
        (nav.mode, nav.page) = modes::resolve(&cfg, &nav);
        let (obs_events_tx, obs_events_rx) = mpsc::unbounded_channel();
        let (os_events_tx, os_events_rx) = mpsc::unbounded_channel();
        let mut sleep = sleep::SleepTimer::default();
        sleep.configure(cfg.deck_sleep_min, tokio::time::Instant::now());
        let config_dir = cfg_path
            .parent()
            .map_or_else(|| PathBuf::from("."), PathBuf::from);
        let images = Arc::new(deck::FsImages::new(
            config_dir.join("images"),
            config_dir.join("icons"),
        ));
        let (events, _) = broadcast::channel(64);
        // The user's records sit beside the config file.
        let user_records = cfg_path.with_file_name("devices.toml");
        let (records, problems) = match std::fs::read_to_string(&user_records) {
            Ok(text) => Records::with_overrides(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Records::builtin(), Vec::new()),
            Err(e) => (
                Records::builtin(),
                vec![format!("devices.toml: could not be read: {}", e.kind())],
            ),
        };
        for p in &problems {
            tracing::warn!("{p}");
        }
        let fw = FirmwareState {
            cache: elgato_fw::load_cache(&cfg_path.with_file_name("firmware-cache.json")),
            last_failure: None,
        };
        // A clients file that can't be read leaves no clients: nobody gets in by accident.
        let clients = clients::Clients::load(cfg_path.with_file_name("clients.json"))
            .unwrap_or_else(|e| {
                tracing::warn!(kind = %api::error_kind(&e), "clients.json unreadable; no clients");
                clients::Clients::empty(cfg_path.with_file_name("clients.json"))
            });
        let agent = Arc::new(Agent {
            cfg_path,
            cfg: Mutex::new(Arc::new(cfg)),
            records: Arc::new(records),
            problems: Mutex::new(problems),
            lights: Mutex::default(),
            usb: Mutex::default(),
            hid: Mutex::default(),
            deck: Mutex::default(),
            mouse: Mutex::default(),
            prompter: Mutex::default(),
            nav: Mutex::new(nav),
            ui: Mutex::default(),
            key_log: Mutex::default(),
            images,
            redraw_wake: tokio::sync::Notify::new(),
            save_gen: Arc::default(),
            cfg_save_gen: Default::default(),
            scalar_dirty: Default::default(),
            state_pending: Arc::default(),
            #[cfg(test)]
            ran: Mutex::default(),
            events,
            logged_devices: Mutex::default(),
            input_dry_run: Arc::default(),
            fw: Mutex::new(fw),
            fw_wake: tokio::sync::Notify::new(),
            network_blocked: Default::default(),
            mdns_blocked: Default::default(),
            last_light_success: Mutex::new(None),
            token,
            panel: Mutex::default(),
            feeds: Arc::default(),
            approvals: Mutex::default(),
            #[cfg(test)]
            opened: Mutex::default(),
            #[cfg(test)]
            popup_draw_fails: Default::default(),
            clients: Mutex::new(clients),
            limits: Mutex::default(),
            client_events: Mutex::default(),
            obs_link: Mutex::new(None),
            obs_retiring: Mutex::default(),
            obs_needed: Arc::default(),
            obs_events_tx,
            obs_events_rx: Mutex::new(Some(obs_events_rx)),
            facts: Mutex::default(),
            sleep: Mutex::new(sleep),
            sleep_wake: tokio::sync::Notify::new(),
            os_events_tx,
            os_events_rx: Mutex::new(Some(os_events_rx)),
            os_observer: Mutex::new(None),
            #[cfg(test)]
            os_spawns: Default::default(),
        });
        agent.publish_prompter_speed();
        // So the Approvals key and badge read 0 rather than a missing feed.
        agent.feeds.set(
            "approvals.pending",
            feeds::FeedValue::Number(0.0),
            None,
            None,
            feeds::FOREVER,
        );
        Ok(agent)
    }

    /// Makes `TypeText` and `Hotkey` error before any keyboard event is posted. For tests
    /// that must run those actions without typing into the machine's focused app.
    pub fn set_input_dry_run(&self, on: bool) {
        self.input_dry_run
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.events.subscribe()
    }

    /// A receiver for one client's named events; `None` if the client no longer exists.
    /// Subscribing is what "listening" means. The check and the channel creation happen
    /// under the clients lock, so a request authenticated before `remove_client` can't
    /// bring the channel back.
    pub fn client_subscribe(&self, client: &str) -> Option<broadcast::Receiver<String>> {
        let clients = lock(&self.clients);
        if !clients.exists(client) {
            return None;
        }
        let rx = lock(&self.client_events)
            .entry(client.to_string())
            .or_insert_with(|| broadcast::channel(32).0)
            .subscribe();
        drop(clients);
        Some(rx)
    }

    /// A client's feed write, with the same removal guard. `Gone` means the client was
    /// removed after its request was authenticated; `Full` that it holds its key cap.
    pub fn client_put_feed(
        &self,
        client: &str,
        key: &str,
        value: feeds::FeedValue,
        unit: Option<String>,
        state: Option<feeds::FeedState>,
        ttl: Duration,
    ) -> Result<(), ClientPutError> {
        let clients = lock(&self.clients);
        if !clients.exists(client) {
            return Err(ClientPutError::Gone);
        }
        let stored = self.feeds.set_if_room(
            &format!("{client}."),
            clients::MAX_FEED_KEYS,
            key,
            value,
            unit,
            state,
            ttl,
        );
        drop(clients);
        if stored {
            Ok(())
        } else {
            Err(ClientPutError::Full)
        }
    }

    /// Sends a named event to one client, if it is listening. Only the client's name is
    /// logged: the event's name and value are the client's business.
    fn send_client_event(&self, client: &str, name: &str, value: Option<serde_json::Value>) {
        let data = json!({ "name": name, "value": value }).to_string();
        let delivered = lock(&self.client_events)
            .get(client)
            .is_some_and(|tx| tx.send(data).is_ok());
        if !delivered {
            tracing::info!("client_event: {client} not listening");
        }
    }

    /// Removes a client and everything it left behind: its token, its feeds and its
    /// event channel (which ends any stream it has open).
    pub fn remove_client(&self, name: &str) -> Result<bool> {
        // Held to the end: the guarded creators above take this lock first.
        let mut clients = lock(&self.clients);
        let removed = clients.remove(name)?;
        let mut cancelled = Vec::new();
        if removed {
            // A removed program's requests must not wait on for someone to approve.
            cancelled = self.cancel_client_approvals(name);
            let prefix = format!("{name}.");
            for (key, _) in self.feeds.list() {
                if key.starts_with(&prefix) {
                    self.feeds.clear(&key);
                }
            }
            lock(&self.client_events).remove(name);
            lock(&self.limits).forget(name);
        }
        drop(clients);
        self.approvals_gone(cancelled);
        Ok(removed)
    }

    fn emit(&self, kind: &str, data: serde_json::Value) {
        // No subscribers is the normal idle case: don't build the message for nobody.
        if self.events.receiver_count() == 0 {
            return;
        }
        let _ = self
            .events
            .send(json!({ "type": kind, "data": data }).to_string());
    }

    // ---- OBS link ----

    /// The receiving end of the OBS link's events (recording, streaming, connected), for the
    /// mode rules. There is one; the first caller gets it.
    pub fn take_obs_events(&self) -> Option<mpsc::UnboundedReceiver<obs::ObsEvent>> {
        lock(&self.obs_events_rx).take()
    }

    /// OBS is needed while a mode rule watches recording or streaming, or a visible face
    /// or panel reads an `obs.` feed.
    fn obs_needed(&self) -> bool {
        use novagear_core::modes::RuleWhen;
        self.config_arc()
            .mode_rules
            .iter()
            .any(|r| matches!(r.when, RuleWhen::ObsRecording | RuleWhen::ObsStreaming))
            || self.feeds.is_wanted("obs.")
    }

    /// Starts, restarts or stops the OBS link to match what needs it. Called when the
    /// wanted feeds change and when the config does.
    fn reconcile_obs(&self) {
        let needed = self.obs_needed();
        self.obs_needed
            .store(needed, std::sync::atomic::Ordering::Relaxed);
        let obs_cfg = self.config_arc().obs.clone();
        let mut slot = lock(&self.obs_link);
        let old = slot.take();
        let keep = match old {
            Some((c, link)) if needed && c == obs_cfg && !link.is_finished() => Some((c, link)),
            // Not joined here: a connect attempt (slow DNS) can hold the old thread up for
            // seconds, and this runs on the agent's one runtime thread, which also times
            // the deck's holds. The next link joins it on its own thread before it sets
            // any feed, so a dying link's cleanup still can't land after its replacement's.
            Some((_, link)) => {
                let mut retiring = lock(&self.obs_retiring);
                // Finished threads have nothing left to wait for.
                retiring.retain(|t| !t.is_finished());
                retiring.extend(link.retire());
                None
            }
            None => None,
        };
        *slot = match keep {
            Some(k) => Some(k),
            None if needed => {
                let flag = self.obs_needed.clone();
                let after = std::mem::take(&mut *lock(&self.obs_retiring));
                let link = obs::ObsLink::spawn_after(
                    obs_cfg.clone(),
                    self.feeds.clone(),
                    self.obs_events_tx.clone(),
                    move || flag.load(std::sync::atomic::Ordering::Relaxed),
                    after,
                );
                Some((obs_cfg, link))
            }
            None => None,
        };
    }

    /// Follows the wanted-feed set so the OBS link runs only while something reads it.
    async fn obs_loop(self: Arc<Self>) {
        let mut changes = self.feeds.wanted_changes();
        loop {
            self.reconcile_obs();
            if changes.changed().await.is_err() {
                break;
            }
        }
    }

    // ---- feeds ----

    fn publish_prompter_speed(&self) {
        let speed = f64::from(lock(&self.cfg).prompter.speed);
        self.feeds.set(
            "prompter.speed",
            feeds::FeedValue::Number(speed),
            None,
            None,
            feeds::FOREVER,
        );
    }

    fn publish_prompter_playing(&self, playing: bool) {
        self.feeds.set(
            "prompter.playing",
            feeds::FeedValue::Number(f64::from(u8::from(playing))),
            None,
            None,
            feeds::FOREVER,
        );
    }

    fn set_mic_muted(&self, muted: bool) {
        self.feeds.set(
            "mic.muted",
            feeds::FeedValue::Number(f64::from(u8::from(muted))),
            None,
            muted.then_some(feeds::FeedState::Warn),
            feeds::FOREVER,
        );
    }

    /// Mirrors the lights' state into feeds. Brightness and kelvin are averages over the
    /// lights that reported, like the strip dials; with none reporting the feeds go away.
    fn publish_light_feeds(&self) {
        let states: Vec<LightState> = lock(&self.lights)
            .values()
            .filter_map(|l| l.state)
            .collect();
        if states.is_empty() {
            for k in ["lights.on", "lights.brightness", "lights.kelvin"] {
                self.feeds.clear(k);
            }
            return;
        }
        let avg = |f: fn(&LightState) -> u32| {
            f64::from(states.iter().map(f).sum::<u32>()) / states.len() as f64
        };
        let any_on = states.iter().any(|s| s.on == 1);
        self.feeds.set(
            "lights.on",
            feeds::FeedValue::Number(f64::from(u8::from(any_on))),
            None,
            None,
            feeds::FOREVER,
        );
        self.feeds.set(
            "lights.brightness",
            feeds::FeedValue::Number(avg(|s| u32::from(s.brightness)).round()),
            Some("%".into()),
            None,
            feeds::FOREVER,
        );
        self.feeds.set(
            "lights.kelvin",
            feeds::FeedValue::Number(
                avg(|s| u32::from(keylight::mired_to_kelvin(s.temperature))).round(),
            ),
            Some("K".into()),
            None,
            feeds::FOREVER,
        );
    }

    /// A copy to edit and hand back to `set_config`. To only read, use `config_arc`.
    pub fn config(&self) -> Config {
        (**lock(&self.cfg)).clone()
    }

    /// The current config without copying it.
    pub fn config_arc(&self) -> Arc<Config> {
        lock(&self.cfg).clone()
    }

    pub fn set_config(&self, cfg: Config) -> Result<()> {
        let cfg = cfg.normalized();
        cfg.save(&self.cfg_path)?;
        {
            let mut nav = lock(&self.nav);
            let before = nav.mode.clone();
            let (mode, page) = modes::resolve(&cfg, &nav);
            if mode != before {
                nav.back.clear();
            }
            nav.mode = mode;
            nav.page = page;
            let mode = cfg.mode(&nav.mode);
            let back = std::mem::take(&mut nav.back);
            nav.back = back
                .into_iter()
                .filter(|p| mode.is_some_and(|m| m.page(p).is_some()))
                .collect();
            // A rule or a hand-picked mode that no longer exists stops counting.
            if nav.manual.as_ref().is_some_and(|m| cfg.mode(m).is_none()) {
                nav.manual = None;
            }
            if nav
                .rule_active
                .as_ref()
                .is_some_and(|(_, m)| cfg.mode(m).is_none())
            {
                nav.rule_active = None;
            }
        }
        // Image names may now point at different files, and a failed lookup is cached.
        self.images.invalidate();
        {
            let mut ui = lock(&self.ui);
            ui.reset_knobs();
            // The keys may have changed under a hold or a remembered state.
            ui.cancel_holds();
            ui.prune_states(&cfg);
        }
        let released = std::mem::replace(&mut *lock(&self.cfg), Arc::new(cfg)).deck_control
            && !self.config_arc().deck_control;
        self.publish_prompter_speed();
        self.emit("config", json!({}));
        self.fw_wake.notify_one();
        if released {
            self.release_deck();
        }
        // Turning sleep off wakes a sleeping deck; the redraw below draws it.
        modes::forget_missing_warnings();
        let sleep_min = self.config_arc().deck_sleep_min;
        lock(&self.sleep).configure(sleep_min, tokio::time::Instant::now());
        self.sleep_wake.notify_one();
        self.reconcile_os_events();
        // The rules may have changed, and a rule that holds now applies at once.
        self.eval_rules();
        self.reconcile_obs();
        self.redraw_deck();
        self.devices_changed();
        Ok(())
    }

    /// A number turned on the deck (prompter speed, deck brightness). Each detent applies at
    /// once, without `set_config`'s resets: the image cache, the knobs' alternate roles and
    /// any hold stay as they are, since nothing they depend on changed. The file is written
    /// once the turning has stopped for `CONFIG_DEBOUNCE`.
    fn set_scalar(self: &Arc<Self>, change: impl FnOnce(&mut Config)) {
        change(Arc::make_mut(&mut lock(&self.cfg)));
        self.publish_prompter_speed();
        self.redraw_strip();
        let gen_now = self
            .cfg_save_gen
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        self.scalar_dirty
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let me = self.clone();
        // The deadline is fixed now, not when the task first runs.
        let due = Instant::now() + CONFIG_DEBOUNCE;
        tokio::spawn(async move {
            tokio::time::sleep_until(due).await;
            if me.cfg_save_gen.load(std::sync::atomic::Ordering::Relaxed) != gen_now {
                return;
            }
            // Whatever is current: a `PUT /config` meanwhile already holds the latest.
            if let Err(e) = me.config().save(&me.cfg_path) {
                tracing::warn!("saving the config: {e:#}");
                return;
            }
            me.scalar_dirty
                .store(false, std::sync::atomic::Ordering::SeqCst);
            me.emit("config", json!({}));
        });
    }

    /// Writes what the debounced saves (`set_scalar`, `save_soon`) are still waiting to
    /// write. Called on shutdown, where the process would otherwise drop a change made in
    /// the last second. Their timers find nothing left to do.
    pub fn flush_saves(&self) {
        use std::sync::atomic::Ordering::SeqCst;
        if self.scalar_dirty.swap(false, SeqCst) {
            self.cfg_save_gen.fetch_add(1, SeqCst);
            if let Err(e) = self.config_arc().save(&self.cfg_path) {
                tracing::warn!("saving the config: {e:#}");
            }
        }
        if let Some((mode, page)) = lock(&self.state_pending).take() {
            self.save_gen.fetch_add(1, SeqCst);
            modes::save_state(&self.state_dir(), &mode, &page);
        }
    }

    fn state_dir(&self) -> PathBuf {
        self.cfg_path
            .parent()
            .map_or_else(|| PathBuf::from("."), std::path::Path::to_path_buf)
    }

    pub fn icons_dir(&self) -> PathBuf {
        self.cfg_path
            .parent()
            .map_or_else(|| PathBuf::from("icons"), |d| d.join("icons"))
    }

    /// An image the deck can draw, by name, as stored (for the panel's previews).
    pub fn image_bytes(&self, name: &str) -> Option<std::borrow::Cow<'static, [u8]>> {
        self.images.raw(name)
    }

    pub fn images_dir(&self) -> PathBuf {
        self.cfg_path
            .parent()
            .map_or_else(|| PathBuf::from("images"), |d| d.join("images"))
    }

    /// Call after an image file is added, replaced or removed: forgets what was decoded
    /// (a missing name is remembered too) and redraws the deck.
    pub fn images_changed(&self) {
        self.images.invalidate();
        // As `set_config` does: the redraw below decodes images with `ui` held, which can
        // take a while, and a key's face may change under its hold. A hold is never carried
        // across that: cancelling only ever loses a hold, it can't approve one.
        lock(&self.ui).cancel_holds();
        self.redraw_deck();
    }

    /// Saves an uploaded image as `images/<sha256>.<ext>` and returns that name. The same
    /// bytes always give the same name; the deck picks the file up at once.
    pub fn store_image(&self, bytes: &[u8]) -> Result<String> {
        let name = templates::content_name(bytes);
        templates::write_image(&self.images_dir(), &name, bytes)?;
        self.images_changed();
        Ok(name)
    }

    /// Adds a template's modes to the saved config (see `templates::apply`).
    pub fn apply_template(
        &self,
        t: &novagear_core::modes::Template,
        replace_rules: bool,
    ) -> Result<templates::ApplyReport> {
        let mut cfg = self.config();
        let report = templates::apply(&mut cfg, t, replace_rules, &self.images_dir())?;
        // Also drops what was cached for the new images' names and redraws the deck.
        self.set_config(cfg)?;
        Ok(report)
    }

    pub fn current_page(&self) -> String {
        lock(&self.nav).page.clone()
    }

    /// Goes to a page of the current mode.
    fn show_page(&self, page: String, push: bool) -> Result<()> {
        let cfg = self.config_arc();
        let mode;
        {
            let mut nav = lock(&self.nav);
            if cfg.mode(&nav.mode).and_then(|m| m.page(&page)).is_none() {
                bail!("there's no page called {page:?} in this mode");
            }
            if nav.page == page {
                return Ok(());
            }
            if push {
                let prev = std::mem::replace(&mut nav.page, page.clone());
                nav.back.push(prev);
                if nav.back.len() > BACK_DEPTH {
                    nav.back.remove(0);
                }
            } else {
                nav.page = page.clone();
            }
            mode = nav.mode.clone();
        }
        // After the guard is gone: `save_soon` reads `nav` itself.
        self.save_soon(&mode, &page);
        lock(&self.ui).cancel_holds();
        self.emit("page", json!({ "page": page }));
        self.redraw_deck();
        Ok(())
    }

    /// Replaces the deck's pages with an import of the Elgato Stream Deck profiles in `dir`
    /// (default: the Stream Deck app's own folder). The old config is kept as `.bak`.
    pub fn import_streamdeck(&self, dir: Option<PathBuf>) -> Result<serde_json::Value> {
        let dir = dir
            .or_else(import::default_profiles_dir)
            .context("no Stream Deck profiles found on this computer")?;
        let r = import::import(&dir, &self.icons_dir())?;
        if r.pages.is_empty() {
            bail!("no Stream Deck + profiles in {}", dir.display());
        }
        let mut cfg = self.config();
        if self.cfg_path.exists() {
            std::fs::copy(&self.cfg_path, self.cfg_path.with_extension("json.bak"))?;
        }
        // The profiles become their own mode, so the user's modes are left alone. A second
        // import replaces the first.
        let pages = r.pages.len();
        let imported = novagear_core::modes::Mode {
            id: "imported".into(),
            name: "Imported".into(),
            pages: r.pages,
            ..Default::default()
        };
        match cfg.modes.iter_mut().find(|m| m.id == imported.id) {
            Some(m) => *m = imported,
            None => cfg.modes.push(imported),
        }
        // Point the deck at the new mode before `set_config`, which resolves it against the
        // new config and does the one redraw.
        *lock(&self.nav) = modes::ModeState {
            mode: "imported".into(),
            page: r.start_page.clone(),
            ..Default::default()
        };
        self.save_soon("imported", &r.start_page);
        self.set_config(cfg)?;
        Ok(json!({
            "mode": "imported",
            "pages": pages,
            "start_page": r.start_page,
            "icons": r.icons,
            "placeholders": r.placeholders,
            "skipped_profiles": r.skipped_profiles,
        }))
    }

    /// Problems found while loading the user's device records.
    pub fn problems(&self) -> Vec<String> {
        lock(&self.problems).clone()
    }

    pub fn prompter(&self) -> PrompterRun {
        lock(&self.prompter).clone()
    }

    /// Whether the OS is refusing the agent local network access.
    pub fn network_blocked(&self) -> bool {
        self.network_blocked
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether some light call worked within `BLOCK_WINDOW`.
    fn light_worked_recently(&self) -> bool {
        lock(&self.last_light_success).is_some_and(|t| t.elapsed() < BLOCK_WINDOW)
    }

    /// Marks the network blocked unless a light worked recently. `from_mdns` records that
    /// nothing but mDNS has shown the block.
    fn mark_blocked(&self, from_mdns: bool, why: &dyn std::fmt::Display) {
        use std::sync::atomic::Ordering::Relaxed;
        if self.light_worked_recently() {
            return;
        }
        if !self.network_blocked.swap(true, Relaxed) {
            self.mdns_blocked.store(from_mdns, Relaxed);
            tracing::warn!("local network looks blocked: {why}");
        } else if !from_mdns {
            self.mdns_blocked.store(false, Relaxed);
        }
    }

    /// Folds one light call's result into the blocked flag: a success clears it and
    /// records the time, an unreachable-host error sets it unless a light worked in the
    /// last `BLOCK_WINDOW`. Returns the error text to show for a failure. Callers with
    /// several lights fold successes first so the flag does not depend on light order.
    fn light_outcome<T>(&self, r: &Result<T>) -> Option<String> {
        use std::sync::atomic::Ordering::Relaxed;
        match r {
            Ok(_) => {
                *lock(&self.last_light_success) = Some(Instant::now());
                self.network_blocked.store(false, Relaxed);
                self.mdns_blocked.store(false, Relaxed);
                None
            }
            Err(e) if keylight::is_network_blocked(e) => {
                self.mark_blocked(false, &format_args!("{e:#}"));
                Some(if self.network_blocked() {
                    BLOCKED_REASON.into()
                } else {
                    UNREACHABLE_REASON.into()
                })
            }
            Err(e) => Some(format!("{e:#}")),
        }
    }

    /// Runs `light_outcome` over several results, successes first.
    fn fold_outcomes<T>(&self, results: &[Result<T>]) -> Vec<Option<String>> {
        for r in results.iter().filter(|r| r.is_ok()) {
            self.light_outcome(r);
        }
        results.iter().map(|r| self.light_outcome(r)).collect()
    }

    /// mDNS daemon events: its socket errors mean the network is not usable.
    fn on_daemon_event(&self, ev: &mdns_sd::DaemonEvent) {
        if let mdns_sd::DaemonEvent::Error(e) = ev {
            self.mark_blocked(true, e);
        }
    }

    /// The result of the one-packet mDNS probe sent when discovery starts. Only an
    /// unreachable-host error counts, and it feeds the same path as an mDNS daemon error
    /// (so a resolved advert clears it). Success never clears a block a light call set,
    /// and other errors say nothing about Local Network access.
    fn on_probe_result(&self, r: &std::io::Result<()>) {
        if let Err(e) = r
            && keylight::is_unreachable_io(e)
        {
            self.mark_blocked(true, &"mDNS probe could not reach the network");
        }
    }

    /// An advert resolved, so mDNS works: a block only mDNS showed is over.
    fn on_service_resolved(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        if self.mdns_blocked.swap(false, Relaxed) {
            self.network_blocked.store(false, Relaxed);
        }
    }

    pub fn devices(&self) -> Vec<DeviceInfo> {
        let mut out = Vec::new();
        for (id, l) in lock(&self.lights).iter() {
            out.push(DeviceInfo {
                id: id.clone(),
                kind: DeviceKind::Light,
                name: l.name.clone(),
                vendor: "Elgato".into(),
                usb: None,
                firmware: Some(self.light_firmware(l)),
                status: match &l.error {
                    Some(e) => Status::Busy(e.clone()),
                    None => Status::Ready,
                },
                state: match l.state {
                    Some(s) => json!({
                        "on": s.on == 1,
                        "brightness": s.brightness,
                        "kelvin": keylight::mired_to_kelvin(s.temperature),
                        "address": l.addr.to_string(),
                        "firmware": l.firmware,
                    }),
                    None => json!({ "address": l.addr.to_string() }),
                },
            });
        }
        let deck = lock(&self.deck);
        let mouse = lock(&self.mouse);
        let deck_control = lock(&self.cfg).deck_control;
        for u in lock(&self.usb).iter() {
            let (status, state) = match u.kind {
                _ if u.unknown => (
                    Status::Listed,
                    json!({
                        "unknown": true,
                        "details": unknown_details(u),
                        "draft": novagear_drivers::devices::draft_record(u),
                    }),
                ),
                DeviceKind::StreamDeck
                    if u.family == Family::StreamDeck && u.usb == catalog::STREAM_DECK_PLUS =>
                {
                    match (&deck.info, &deck.error) {
                        (Some(i), _) => (
                            Status::Ready,
                            json!({ "serial": i.serial, "firmware": i.firmware, "control": deck_control }),
                        ),
                        (None, Some(e)) => (Status::Busy(e.clone()), json!({})),
                        (None, None) => (Status::Listed, json!({})),
                    }
                }
                DeviceKind::Mouse if u.family == Family::Razer90 => match (mouse.dpi, &mouse.error)
                {
                    (Some(dpi), _) => (Status::Ready, json!({ "dpi": dpi })),
                    (None, Some(e)) => (Status::Busy(e.clone()), json!({})),
                    (None, None) => (Status::Listed, json!({})),
                },
                DeviceKind::Prompter => (Status::Ready, json!({ "display": true })),
                DeviceKind::Capture => (Status::Ready, json!({ "preview": "browser" })),
                _ => (Status::Listed, json!({})),
            };
            out.push(DeviceInfo {
                firmware: Some(self.usb_firmware(u, &deck, &mouse)),
                id: u.key.clone(),
                kind: u.kind,
                name: if u.name.is_empty() {
                    format!("{:04x}:{:04x}", u.usb.vid, u.usb.pid)
                } else {
                    u.name.clone()
                },
                vendor: u.vendor.clone(),
                usb: Some(u.usb),
                status,
                state,
            });
        }
        let hid_from = out.len();
        out.extend(hid_infos(&lock(&self.hid), &lock(&self.usb)));
        for d in &mut out[hid_from..] {
            let rec = d.usb.and_then(|u| self.records.by_usb(u));
            d.firmware = Some(firmware_info(rec, None, false));
        }
        out
    }

    fn light_firmware(&self, l: &Light) -> FirmwareInfo {
        let rec = (l.board != 0)
            .then(|| self.records.by_elgato_board(l.board))
            .flatten();
        let installed = (!l.firmware.is_empty()).then(|| match l.build {
            0 => l.firmware.clone(),
            b => format!("{} ({b})", l.firmware),
        });
        let mut f = firmware_info(rec, installed, false);
        let fw = lock(&self.fw);
        if let Some(c) = &fw.cache
            && let Some(latest) = c.by_board.get(&l.board)
        {
            f.latest = Some(latest.display());
            f.source = Some(format!(
                "the newest Elgato ships in Control Center {}",
                c.app_version
            ));
            // A light that gave no build number (0) is not known to be behind an image of
            // its own version; only a higher version counts then.
            f.update_available = elgato_fw::parse_version(&l.firmware).is_some_and(|v| {
                let same_version = v == (latest.major, latest.minor, latest.patch);
                !(l.build == 0 && same_version)
                    && elgato_fw::newer((v.0, v.1, v.2, l.build), latest)
            });
        }
        f
    }

    /// A USB device's firmware: the deck's and the Razer mouse's own strings once read,
    /// otherwise the bcdDevice.
    fn usb_firmware(&self, u: &UsbDevice, deck: &DeckSlot, mouse: &MouseSlot) -> FirmwareInfo {
        let rec = self.records.by_usb(u.usb);
        let own = match u.kind {
            DeviceKind::StreamDeck
                if u.family == Family::StreamDeck && u.usb == catalog::STREAM_DECK_PLUS =>
            {
                deck.info.as_ref().map(|i| i.firmware.clone())
            }
            // The slot holds the one Razer mouse that was opened; match it by product id.
            DeviceKind::Mouse if u.family == Family::Razer90 && mouse.pid == Some(u.usb.pid) => {
                mouse.firmware.clone()
            }
            _ => None,
        };
        match own.filter(|s| !s.is_empty()) {
            Some(s) => firmware_info(rec, Some(s), false),
            None => firmware_info(rec, u.device_version.clone(), true),
        }
    }

    /// How many devices have a newer firmware out.
    pub fn firmware_updates(&self) -> usize {
        self.devices()
            .iter()
            .filter(|d| d.firmware.as_ref().is_some_and(|f| f.update_available))
            .count()
    }

    /// Replaces what the check last learned and clears the failure backoff.
    fn set_firmware_cache(&self, cache: Option<elgato_fw::Cache>) {
        let mut fw = lock(&self.fw);
        fw.cache = cache;
        fw.last_failure = None;
    }

    fn firmware_cache_path(&self) -> PathBuf {
        self.cfg_path.with_file_name("firmware-cache.json")
    }

    /// How long until the firmware check is due, or `None` while it must not run: the
    /// setting is off or no Key Light is around.
    fn firmware_wait(&self) -> Option<Duration> {
        if !lock(&self.cfg).firmware_check || lock(&self.lights).is_empty() {
            return None;
        }
        let fw = lock(&self.fw);
        Some(elgato_fw::check_delay(
            unix_now(),
            fw.cache.as_ref().map(|c| c.checked_at),
            fw.last_failure,
        ))
    }

    /// The once-a-day firmware check. It sleeps until due, and a light appearing or a
    /// settings change only makes it look again; it never polls.
    async fn firmware_loop(self: Arc<Self>) {
        loop {
            match self.firmware_wait() {
                None => self.fw_wake.notified().await,
                Some(d) if !d.is_zero() => {
                    tokio::select! {
                        _ = tokio::time::sleep(d) => {}
                        _ = self.fw_wake.notified() => {}
                    }
                }
                Some(_) => self.run_firmware_check().await,
            }
        }
    }

    async fn run_firmware_check(&self) {
        let prev = lock(&self.fw).cache.as_ref().and_then(|c| c.etag.clone());
        tracing::info!("firmware check: asking Elgato for the newest Key Light firmware");
        // Tests never start a copy of the test binary.
        let r = tokio::task::spawn_blocking(move || {
            if cfg!(test) {
                elgato_fw::check(prev.as_deref())
            } else {
                elgato_fw::check_isolated(prev.as_deref())
            }
        })
        .await;
        let now = unix_now();
        match r {
            Ok(Ok(Some(l))) => {
                tracing::info!(
                    "firmware check: Control Center {} carries {} light images, read {} bytes",
                    l.app_version,
                    l.by_board.len(),
                    l.bytes_fetched
                );
                let cache = elgato_fw::Cache {
                    checked_at: now,
                    etag: l.etag,
                    app_version: l.app_version,
                    by_board: l.by_board,
                };
                if let Err(e) = elgato_fw::save_cache(&self.firmware_cache_path(), &cache) {
                    tracing::warn!("firmware check: could not save its cache: {e:#}");
                }
                self.set_firmware_cache(Some(cache));
            }
            Ok(Ok(None)) => {
                tracing::info!("firmware check: nothing changed since the last check");
                let mut fw = lock(&self.fw);
                fw.last_failure = None;
                if let Some(c) = fw.cache.as_mut() {
                    c.checked_at = now;
                    if let Err(e) = elgato_fw::save_cache(&self.firmware_cache_path(), c) {
                        tracing::warn!("firmware check: could not save its cache: {e:#}");
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::warn!("firmware check failed, trying again in an hour: {e:#}");
                lock(&self.fw).last_failure = Some(now);
            }
            Err(e) => {
                tracing::warn!("firmware check stopped: {e}");
                lock(&self.fw).last_failure = Some(now);
            }
        }
        self.devices_changed();
    }

    fn devices_changed(&self) {
        self.publish_light_feeds();
        self.log_device_changes();
        self.emit("devices", json!({}));
    }

    /// Logs devices that appeared or went away since the last time, by name and kind only
    /// (no serials or paths).
    fn log_device_changes(&self) {
        let now: BTreeMap<String, (String, DeviceKind, bool, bool)> = self
            .devices()
            .into_iter()
            .map(|d| {
                let matched = d.usb.is_some() && d.state.get("unknown").is_none();
                let listed_only = d.state.get("unknown").is_some();
                (d.id, (d.name, d.kind, matched, listed_only))
            })
            .collect();
        let mut seen = lock(&self.logged_devices);
        for (id, (name, kind, matched, unlisted)) in &now {
            if !seen.contains_key(id) {
                if *unlisted {
                    tracing::info!("device connected: {name} ({kind:?}), no record matches it");
                } else if *matched {
                    tracing::info!("device connected: {name} ({kind:?}), record matched");
                } else {
                    tracing::info!("device connected: {name} ({kind:?})");
                }
            }
        }
        for (id, name) in seen.iter() {
            if !now.contains_key(id) {
                tracing::info!("device disconnected: {name}");
            }
        }
        *seen = now.into_iter().map(|(id, v)| (id, v.0)).collect();
    }

    // ---- actions ----

    /// Runs an action. Logs only its `log_label`, never its fields, which can hold typed text.
    pub async fn run(self: &Arc<Self>, action: Action) -> Result<()> {
        let label = action.log_label();
        tracing::info!(action = %label, "action");
        #[cfg(test)]
        lock(&self.ran).push(action.clone());
        let result = self.run_action(action).await;
        if let Err(e) = &result {
            tracing::warn!(action = %label, "action failed: {}", failure_log(e));
        }
        result
    }

    async fn run_action(self: &Arc<Self>, action: Action) -> Result<()> {
        match action {
            Action::LightToggle { id } => {
                // Toggle as a group: if any target light is on, turn them all off.
                let any_on = self
                    .light_targets(id.as_deref())
                    .iter()
                    .any(|(_, l)| l.state.is_some_and(|s| s.on == 1));
                self.update_lights(id.as_deref(), |s| s.on = u8::from(!any_on))
                    .await
            }
            Action::LightSet {
                id,
                on,
                brightness,
                kelvin,
            } => {
                self.update_lights(id.as_deref(), |s| {
                    if let Some(on) = on {
                        s.on = u8::from(on);
                    }
                    if let Some(b) = brightness {
                        s.brightness = b.clamp(3, 100);
                    }
                    if let Some(k) = kelvin {
                        s.temperature = keylight::kelvin_to_mired(k);
                    }
                })
                .await
            }
            Action::LightBrightnessStep { id, step } => {
                self.update_lights(id.as_deref(), |s| {
                    s.brightness = (i16::from(s.brightness) + step).clamp(3, 100) as u8
                })
                .await
            }
            Action::LightKelvinStep { id, step } => {
                self.update_lights(id.as_deref(), |s| {
                    let k = keylight::mired_to_kelvin(s.temperature) as i32 + i32::from(step);
                    s.temperature = keylight::kelvin_to_mired(k.clamp(2900, 7000) as u16);
                })
                .await
            }
            Action::PrompterPlayPause => {
                let p = {
                    let mut p = lock(&self.prompter);
                    p.playing = !p.playing;
                    p.clone()
                };
                self.publish_prompter_playing(p.playing);
                self.emit("prompter", json!(p));
                self.redraw_strip();
                Ok(())
            }
            Action::PrompterRestart => {
                let p = {
                    let mut p = lock(&self.prompter);
                    p.restarts += 1;
                    p.playing = false;
                    p.clone()
                };
                self.publish_prompter_playing(false);
                self.emit("prompter", json!(p));
                Ok(())
            }
            Action::PrompterSpeedStep { step } => {
                self.set_scalar(|cfg| {
                    cfg.prompter.speed =
                        (i32::from(cfg.prompter.speed) + i32::from(step)).clamp(5, 600) as u16;
                });
                Ok(())
            }
            Action::DeckBrightnessStep { step } => {
                self.set_scalar(|cfg| {
                    cfg.deck_brightness =
                        (i16::from(cfg.deck_brightness) + step).clamp(5, 100) as u8;
                });
                self.deck_send(vec![Command::Brightness(self.config_arc().deck_brightness)]);
                Ok(())
            }
            Action::MouseDpi { dpi } => self.set_mouse_dpi(dpi).await,
            Action::MouseDpiStep { step } => {
                let stages = self.config_arc().dpi_stages.clone();
                if stages.is_empty() {
                    bail!("no DPI stages configured");
                }
                let current = lock(&self.mouse).dpi.unwrap_or(stages[0]);
                // Nearest stage to the current DPI, then walk `step` stages, wrapping.
                let at = stages
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, s)| s.abs_diff(current))
                    .map_or(0, |(i, _)| i);
                let next = (at as i32 + i32::from(step)).rem_euclid(stages.len() as i32) as usize;
                self.set_mouse_dpi(stages[next]).await
            }
            Action::MouseColor { color } => {
                let rgb = novagear_drivers::draw::parse_color(&color);
                self.with_mouse(move |m| m.set_static_color(rgb)).await
            }
            Action::PageGo { page } => self.show_page(page, true),
            Action::PageBack => {
                let prev = lock(&self.nav).back.pop();
                match prev {
                    Some(p) => self.show_page(p, false),
                    None => {
                        // Nothing to go back to: the mode's first page.
                        let cfg = self.config_arc();
                        let home = lock(&self.nav).mode.clone();
                        let first = cfg
                            .mode(&home)
                            .and_then(|m| m.home())
                            .map(|p| p.name.clone());
                        match first {
                            Some(p) => self.show_page(p, false),
                            None => Ok(()),
                        }
                    }
                }
            }
            Action::OpenApp { path } => blocking(move || system::open_app(&path)).await,
            Action::OpenUrl { url } => blocking(move || system::open_url(&url)).await,
            Action::TypeText { text, enter } => {
                let dry = self.input_dry_run.clone();
                blocking(move || guarded_input(&dry, || system::type_text(&text, enter))).await
            }
            Action::Hotkey { keys } => {
                let dry = self.input_dry_run.clone();
                blocking(move || guarded_input(&dry, || system::hotkey(&keys))).await
            }
            Action::VolumeStep { step } => blocking(move || system::volume_step(step, false)).await,
            Action::MicVolumeStep { step } => {
                blocking(move || system::volume_step(step, true)).await
            }
            Action::MuteToggle => blocking(|| system::mute_toggle(false).map(drop)).await,
            Action::MicMuteToggle => {
                let muted = tokio::task::spawn_blocking(|| system::mute_toggle(true)).await??;
                // Nothing reads the mic back, so the feed follows what NovaGear did.
                let was = self
                    .feeds
                    .get("mic.muted")
                    .is_some_and(|f| feeds::truthy(&f.value));
                self.set_mic_muted(muted.unwrap_or(!was));
                Ok(())
            }
            Action::ObsScene { scene } => {
                let obs = self.config_arc().obs.clone();
                blocking(move || obs::set_scene(&obs, &scene)).await
            }
            Action::ObsRecordToggle => {
                let obs = self.config_arc().obs.clone();
                blocking(move || obs::toggle_record(&obs)).await
            }
            Action::ObsStreamToggle => {
                let obs = self.config_arc().obs.clone();
                blocking(move || obs::toggle_stream(&obs)).await
            }
            Action::Placeholder { note } => {
                bail!("\"{note}\" came from the Stream Deck app and NovaGear can't run it yet")
            }
            Action::ModeGo { .. } | Action::ModeNext | Action::ModePicker => {
                self.run_mode_action(&action)
            }
            Action::Sequence { steps } => self.run_sequence(steps).await,
            Action::ObsChapterMarker => {
                let obs = self.config_arc().obs.clone();
                blocking(move || obs::chapter_marker(&obs)).await
            }
            Action::ObsReplaySave => {
                let obs = self.config_arc().obs.clone();
                blocking(move || obs::replay_save(&obs)).await
            }
            Action::ObsInputVolumeStep { input, step } => {
                let obs = self.config_arc().obs.clone();
                blocking(move || obs::input_volume_step(&obs, &input, step)).await
            }
            Action::ObsInputMuteToggle { input } => {
                let obs = self.config_arc().obs.clone();
                blocking(move || obs::input_mute_toggle(&obs, &input)).await
            }
            Action::ClientEvent {
                client,
                name,
                value,
            } => {
                self.send_client_event(&client, &name, value);
                Ok(())
            }
            Action::ApprovalStep { step } => self.approval_step(step),
        }
    }

    /// Runs a sequence's steps in order. A failed step is logged and the rest still run; the
    /// result says how many failed. The waits add up to at most `SEQUENCE_MAX_DELAY`, and
    /// steps past that are skipped. A step that is itself a sequence is skipped too, so a
    /// config cannot make this recurse.
    async fn run_sequence(self: &Arc<Self>, steps: Vec<novagear_core::Step>) -> Result<()> {
        let mut waited = Duration::ZERO;
        let mut failed = 0usize;
        for (n, step) in steps.into_iter().enumerate() {
            let delay = Duration::from_millis(u64::from(step.delay_ms));
            if waited + delay > SEQUENCE_MAX_DELAY {
                tracing::warn!(
                    "sequence waits more than {}s in all; skipping step {} and the rest",
                    SEQUENCE_MAX_DELAY.as_secs(),
                    n + 1
                );
                failed += 1;
                break;
            }
            waited += delay;
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            if matches!(step.action, Action::Sequence { .. }) {
                tracing::warn!("sequence step {} is a sequence; skipped", n + 1);
                failed += 1;
                continue;
            }
            // `run` logs a failure with the step's label.
            if Box::pin(self.run(step.action)).await.is_err() {
                failed += 1;
            }
        }
        if failed > 0 {
            // The steps that ran have had their effect, so the multi-action counts as done.
            tracing::warn!("multi-action: {failed} step(s) failed or were skipped");
        }
        Ok(())
    }

    fn light_targets(&self, id: Option<&str>) -> Vec<(String, Light)> {
        lock(&self.lights)
            .iter()
            .filter(|(k, _)| id.is_none_or(|id| id == k.as_str()))
            .map(|(k, l)| (k.clone(), l.clone()))
            .collect()
    }

    async fn update_lights(
        self: &Arc<Self>,
        id: Option<&str>,
        f: impl Fn(&mut LightState),
    ) -> Result<()> {
        let targets = self.light_targets(id);
        if targets.is_empty() {
            bail!("no lights found");
        }
        let mut results = Vec::new();
        for (_, l) in &targets {
            let result = async {
                let mut s = match l.state {
                    Some(s) => s,
                    None => keylight::get(l.addr).await?,
                };
                f(&mut s);
                keylight::set(l.addr, s).await
            }
            .await;
            results.push(result);
        }
        let texts = self.fold_outcomes(&results);
        let mut errors = Vec::new();
        for (((key, l), result), text) in targets.iter().zip(&results).zip(texts) {
            if let Some(slot) = lock(&self.lights).get_mut(key) {
                if let Ok(back) = result {
                    slot.state = Some(*back);
                }
                slot.error = text.clone();
            }
            if let Some(text) = text {
                errors.push(format!("{}: {text}", l.name));
            }
        }
        self.devices_changed();
        self.redraw_strip();
        if errors.is_empty() {
            Ok(())
        } else {
            bail!(errors.join("; "))
        }
    }

    pub async fn identify_light(&self, id: &str) -> Result<()> {
        let addr = lock(&self.lights)
            .get(id)
            .map(|l| l.addr)
            .context("no such light")?;
        let r = keylight::identify(addr).await;
        match self.light_outcome(&r) {
            Some(text) => bail!(text),
            None => Ok(()),
        }
    }

    async fn with_mouse<T: Send + 'static>(
        &self,
        f: impl FnOnce(&razer::Mouse) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let pid = lock(&self.mouse).pid.context("no Razer mouse connected")?;
        let quirks = self
            .records
            .by_usb(UsbId {
                vid: catalog::RAZER,
                pid,
            })
            .map(|r| r.quirks.clone())
            .unwrap_or_default();
        tokio::task::spawn_blocking(move || {
            // Opened on the hid thread (the only owner of hidapi), used on this one.
            let m = novagear_drivers::hid::with(move |api| razer::Mouse::open(api, pid, &quirks))??;
            f(&m)
        })
        .await?
    }

    async fn set_mouse_dpi(&self, dpi: u16) -> Result<()> {
        let r = self
            .with_mouse(move |m| m.set_dpi(dpi).and_then(|_| m.dpi()))
            .await;
        {
            let mut slot = lock(&self.mouse);
            match &r {
                Ok((x, _)) => {
                    slot.dpi = Some(*x);
                    slot.error = None;
                }
                Err(e) => slot.error = Some(format!("{e:#}")),
            }
        }
        self.devices_changed();
        r.map(drop)
    }

    // ---- discovery ----

    /// Starts the background tasks: device detection, light discovery and Stream Deck input.
    pub fn start(self: &Arc<Self>) {
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<(Input, Instant)>();
        let me = self.clone();
        let lister = Arc::new(Hardware(self.records.clone()));
        tokio::spawn(me.detect(hotplug::watch(), lister, input_tx));
        let me = self.clone();
        tokio::spawn(async move {
            while let Some((input, at)) = input_rx.recv().await {
                me.on_deck_input_at(input, at).await;
            }
        });
        let me = self.clone();
        tokio::spawn(async move {
            me.run_discovery(|| {
                let me = me.clone();
                async move { me.discover_lights().await }
            })
            .await;
        });
        tokio::spawn(self.clone().firmware_loop());
        tokio::spawn(self.clone().redraw_loop());
        tokio::spawn(self.clone().obs_loop());
        self.reconcile_os_events();
        tokio::spawn(self.clone().events_loop());
        tokio::spawn(feeds::run_sampler(
            self.feeds.clone(),
            feeds::SystemSampler::default(),
            Arc::default(),
        ));
        let me = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(LIGHT_REFRESH);
            tick.tick().await;
            loop {
                tick.tick().await;
                me.refresh_lights().await;
            }
        });
    }

    /// Finds devices: one listing at start, then again after hotplug events (debounced),
    /// on a slow safety timer, and while Bluetooth gear was recently around. Without an
    /// event feed it falls back to listing every `FALLBACK_SCAN`.
    async fn detect(self: Arc<Self>, watch: Watch, lister: Arc<dyn Lister>, input_tx: DeckInputs) {
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<()>();
        // The watcher hands over a std channel; one small thread moves it into tokio's.
        let mut polling = match watch {
            Watch::Events(rx) => {
                let spawned =
                    std::thread::Builder::new()
                        .name("usb-events".into())
                        .spawn(move || {
                            while rx.recv().is_ok() {
                                if ev_tx.send(()).is_err() {
                                    break;
                                }
                            }
                        });
                match spawned {
                    Ok(_) => false,
                    Err(e) => {
                        tracing::warn!("USB hotplug thread failed ({e}); listing every 2 s");
                        true
                    }
                }
            }
            Watch::Unavailable(why) => {
                tracing::warn!("USB hotplug unavailable ({why}); listing every 2 s");
                true
            }
        };
        let both = Rescan {
            usb: true,
            hid: true,
        };
        let mut last_event: Option<Instant> = None;
        let mut first_event: Option<Instant> = None;
        let mut last_bt: Option<Instant> = None;
        self.scan(&lister, both, &input_tx, &mut last_bt).await;
        let mut last_safety = Instant::now();
        loop {
            let (mut wait, rescan) = if polling {
                (FALLBACK_SCAN, both)
            } else {
                next_rescan(Instant::now(), last_event, last_bt, last_safety)
                    .unwrap_or((SAFETY, both))
            };
            // A deck or mouse that failed to open is retried on its own timer.
            let retry = self
                .next_retry()
                .map(|t| t.saturating_duration_since(Instant::now()))
                .filter(|r| *r < wait);
            if let Some(r) = retry {
                wait = r;
            }
            tokio::select! {
                ev = ev_rx.recv(), if !polling => match ev {
                    Some(()) => {
                        // Debounce, but never push a burst's listing past BURST_MAX.
                        let now = Instant::now();
                        let first = *first_event.get_or_insert(now);
                        last_event = Some(now.min(first + BURST_MAX - DEBOUNCE));
                    }
                    None => {
                        tracing::warn!("USB hotplug watcher stopped; listing every 2 s");
                        polling = true;
                    }
                },
                _ = tokio::time::sleep(wait) => {
                    if retry.is_some() {
                        self.retry_devices(&input_tx).await;
                    } else {
                        self.scan(&lister, rescan, &input_tx, &mut last_bt).await;
                        if rescan.usb {
                            last_event = None;
                            first_event = None;
                            last_safety = Instant::now();
                        }
                    }
                }
            }
        }
    }

    async fn scan(
        self: &Arc<Self>,
        lister: &Arc<dyn Lister>,
        what: Rescan,
        input_tx: &DeckInputs,
        last_bt: &mut Option<Instant>,
    ) {
        let l = lister.clone();
        let Ok((usb, hid)) = tokio::task::spawn_blocking(move || {
            (what.usb.then(|| l.usb()), what.hid.then(|| l.hid()))
        })
        .await
        else {
            return;
        };
        let mut changed = false;
        if let Some(h) = hid {
            if h.iter().any(|d| d.bluetooth) {
                *last_bt = Some(Instant::now());
            }
            let mut slot = lock(&self.hid);
            changed |= *slot != h;
            *slot = h;
        }
        if let Some(now) = usb {
            changed |= self.apply_usb(now, input_tx).await;
        }
        if changed {
            self.devices_changed();
        }
    }

    /// Stores a fresh USB list and opens or releases the deck and mouse to match.
    /// Returns whether the list changed.
    async fn apply_usb(self: &Arc<Self>, now: Vec<UsbDevice>, input_tx: &DeckInputs) -> bool {
        let changed = {
            let mut usb = lock(&self.usb);
            let changed = *usb != now;
            *usb = now.clone();
            changed
        };
        self.open_devices(&now, input_tx).await;
        changed
    }

    /// Opens or releases the deck and the mouse to match a USB list.
    async fn open_devices(self: &Arc<Self>, now: &[UsbDevice], input_tx: &DeckInputs) {
        let has_deck = now
            .iter()
            .any(|u| u.family == Family::StreamDeck && u.usb == catalog::STREAM_DECK_PLUS);
        let razer_pid = now
            .iter()
            .find(|u| u.family == Family::Razer90 && u.kind == DeviceKind::Mouse)
            .map(|u| u.usb.pid);

        if has_deck {
            self.maybe_open_deck(input_tx).await;
        } else {
            let mut d = lock(&self.deck);
            let gone = d.commands.is_some();
            if gone || d.error.is_some() {
                *d = DeckSlot::default();
            }
            drop(d);
            if gone {
                self.on_deck_closed();
            }
        }
        self.maybe_read_mouse(razer_pid).await;
    }

    /// The soonest time a failed deck open or mouse read is due for another try.
    fn next_retry(&self) -> Option<Instant> {
        let deck = lock(&self.deck);
        let mouse = lock(&self.mouse);
        let d = deck.commands.is_none().then_some(deck.retry_at).flatten();
        let m = (mouse.pid.is_some() && mouse.dpi.is_none())
            .then_some(mouse.retry_at)
            .flatten();
        d.into_iter().chain(m).min()
    }

    async fn retry_devices(self: &Arc<Self>, input_tx: &DeckInputs) {
        let usb = lock(&self.usb).clone();
        self.open_devices(&usb, input_tx).await;
    }

    async fn maybe_open_deck(self: &Arc<Self>, input_tx: &DeckInputs) {
        {
            let d = lock(&self.deck);
            if d.commands.is_some() || d.retry_at.is_some_and(|t| Instant::now() < t) {
                return;
            }
        }
        let tx = input_tx.clone();
        let me = Arc::downgrade(self);
        let reader = me.clone();
        // Off the async workers: opening waits on the hid thread's enumeration.
        let opened = tokio::task::spawn_blocking(move || {
            streamdeck::open(
                move |i, at| {
                    let at = Instant::from_std(at);
                    if let Some(me) = reader.upgrade() {
                        me.deck_read(&i, at);
                    }
                    let _ = tx.send((i, at));
                },
                move |reason| {
                    if let Some(me) = me.upgrade() {
                        tracing::info!("Stream Deck closed: {reason}");
                        *lock(&me.deck) = DeckSlot {
                            error: Some(reason),
                            retry_at: Some(Instant::now() + RETRY),
                            ..Default::default()
                        };
                        me.on_deck_closed();
                        me.devices_changed();
                    }
                },
            )
        })
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("opening the deck: {e}")));
        match opened {
            Ok(deck) => {
                tracing::info!("Stream Deck + open");
                *lock(&self.deck) = DeckSlot {
                    commands: Some(deck.commands),
                    info: Some(deck.info),
                    ..Default::default()
                };
                self.redraw_deck();
                self.devices_changed();
            }
            Err(e) => {
                let reason = format!("{e:#}");
                let mut deck = lock(&self.deck);
                // Retries come every few seconds; say it once per distinct reason.
                if deck.error.as_deref() != Some(reason.as_str()) {
                    tracing::warn!("Stream Deck could not be opened: {reason}");
                }
                *deck = DeckSlot {
                    error: Some(reason),
                    retry_at: Some(Instant::now() + RETRY),
                    ..Default::default()
                };
                drop(deck);
                self.devices_changed();
            }
        }
    }

    async fn maybe_read_mouse(&self, pid: Option<u16>) {
        {
            let mut m = lock(&self.mouse);
            if m.pid != pid {
                *m = MouseSlot {
                    pid,
                    ..Default::default()
                };
            }
            if pid.is_none() || m.dpi.is_some() || m.retry_at.is_some_and(|t| Instant::now() < t) {
                return;
            }
        }
        let r = self.with_mouse(|m| Ok((m.dpi()?, m.firmware()))).await;
        {
            let mut m = lock(&self.mouse);
            match r {
                Ok(((x, _), fw)) => {
                    match fw {
                        Ok(f) => {
                            tracing::info!("mouse firmware read: {f}");
                            m.firmware = Some(f);
                        }
                        Err(e) => tracing::warn!("mouse firmware could not be read: {e:#}"),
                    }
                    m.dpi = Some(x);
                    m.error = None;
                    m.retry_at = None;
                }
                Err(e) => {
                    let reason = format!("{e:#}");
                    if m.error.as_deref() != Some(reason.as_str()) {
                        tracing::warn!("mouse could not be read: {reason}");
                    }
                    m.error = Some(reason);
                    m.retry_at = Some(Instant::now() + RETRY);
                }
            }
        }
        self.devices_changed();
    }

    /// Runs light discovery forever: when it stops it starts again after
    /// `DISCOVERY_RETRY`, so lights come back once Local Network access is granted. Each
    /// distinct error is logged once.
    async fn run_discovery<F, Fut>(&self, mut once: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let mut last = String::new();
        loop {
            if let Err(e) = once().await {
                let text = format!("{e:#}");
                if text != last {
                    tracing::warn!("light discovery stopped, retrying in 30 s: {text}");
                    last = text;
                }
            }
            tokio::time::sleep(DISCOVERY_RETRY).await;
        }
    }

    async fn discover_lights(self: &Arc<Self>) -> Result<()> {
        // mdns-sd only reports a blocked Local Network from `register_service`, so send one
        // query ourselves: macOS fails the send with "no route to host" when access is off.
        let probe = tokio::task::spawn_blocking(probe_mdns).await;
        if let Ok(r) = probe {
            self.on_probe_result(&r);
        }
        // mdns-sd has no Drop: its thread runs until `shutdown()`. The guard shuts the
        // daemon down on every exit path once it exists, so restarts do not leak threads.
        let mut guard = None;
        let started = mdns_sd::ServiceDaemon::new().and_then(|d| {
            let for_guard = d.clone();
            guard = Some(OnDrop::new(move || {
                // The reply receiver is dropped unread: never wait on the daemon here.
                let _ = for_guard.shutdown();
            }));
            let events = d.monitor()?;
            let rx = d.browse(keylight::SERVICE)?;
            Ok((events, rx))
        });
        let (events, rx) = match started {
            Ok(v) => v,
            Err(e) => {
                self.on_daemon_event(&mdns_sd::DaemonEvent::Error(mdns_sd::Error::Msg(
                    e.to_string(),
                )));
                bail!("starting mDNS: {e}");
            }
        };
        let _guard = guard;
        loop {
            let ev = tokio::select! {
                Ok(d) = events.recv_async() => {
                    self.on_daemon_event(&d);
                    continue;
                }
                ev = rx.recv_async() => match ev {
                    Ok(ev) => ev,
                    Err(_) => break,
                },
            };
            let mdns_sd::ServiceEvent::ServiceResolved(info) = ev else {
                continue;
            };
            self.on_service_resolved();
            // mDNS is unauthenticated: anything on the network can advertise a "light".
            // Only talk to private LAN IPv4 addresses on the lights' port, so a fake
            // advertisement can't point the agent at this Mac or the internet.
            let Some(addr) = info
                .get_addresses()
                .iter()
                .map(|ip| SocketAddr::new(*ip, info.get_port()))
                .find(|a| keylight::is_lan_light(*a))
            else {
                tracing::warn!(
                    "ignoring light advert {} with no LAN address",
                    info.get_fullname()
                );
                continue;
            };
            let fullname = info.get_fullname().to_string();
            let me = self.clone();
            tokio::spawn(async move { me.add_light(addr, &fullname).await });
        }
        Ok(())
    }

    async fn add_light(&self, addr: SocketAddr, fullname: &str) {
        let instance = fullname
            .split("._elg")
            .next()
            .unwrap_or(fullname)
            .replace("\\032", " ");
        let (info, state) = tokio::join!(keylight::info(addr), keylight::get(addr));
        let (serial, firmware, name, board, build) = match &info {
            Ok(i) => (
                i.serial_number.clone(),
                i.firmware_version.clone(),
                if i.display_name.is_empty() {
                    instance.clone()
                } else {
                    i.display_name.clone()
                },
                i.hardware_board_type,
                i.firmware_build_number,
            ),
            Err(_) => (String::new(), String::new(), instance.clone(), 0, 0),
        };
        let id = light_id(&serial, &instance);
        // A second advert for a light we already know must not take it over while the
        // known address still answers; a real address change (DHCP) passes once it doesn't.
        let known = lock(&self.lights)
            .get(&id)
            .map(|l| l.addr)
            .filter(|a| *a != addr);
        if let Some(old) = known
            && keylight::get(old).await.is_ok()
        {
            tracing::warn!(
                "ignoring advert moving light {name} from {old} to {addr}: {old} still answers"
            );
            return;
        }
        let error = self.light_outcome(&state);
        lock(&self.lights).insert(
            id,
            Light {
                addr,
                name,
                serial,
                firmware,
                board,
                build,
                state: state.ok(),
                error,
            },
        );
        self.fw_wake.notify_one();
        self.devices_changed();
        self.redraw_strip();
    }

    async fn refresh_lights(&self) {
        let lights: Vec<(String, SocketAddr)> = lock(&self.lights)
            .iter()
            .map(|(k, l)| (k.clone(), l.addr))
            .collect();
        let mut results = Vec::new();
        for (_, addr) in &lights {
            results.push(keylight::get(*addr).await);
        }
        let texts = self.fold_outcomes(&results);
        let mut changed = false;
        for (((key, _), r), error) in lights.iter().zip(results).zip(texts) {
            if let Some(l) = lock(&self.lights).get_mut(key) {
                let state = r.ok().or(l.state);
                changed |= l.state != state || l.error != error;
                l.state = state;
                l.error = error;
            }
        }
        if changed {
            self.devices_changed();
            self.redraw_strip();
        }
    }

    // ---- Stream Deck ----

    /// The deck's thread read `input` at `at`. Runs on that thread, before the input is
    /// queued: only a quick leaf-lock note.
    fn deck_read(&self, input: &Input, at: Instant) {
        if let Input::Key { index, down } = *input
            && let Some(log) = lock(&self.key_log).get_mut(index)
        {
            log.note(at, down);
        }
    }

    /// What the deck has read of key `index` since a hold began at `since` (its key-down's
    /// stamp): the first release after it, or that the press was broken, or nothing yet.
    pub(crate) fn key_since(&self, index: usize, since: Instant) -> KeySince {
        lock(&self.key_log)
            .get(index)
            .map_or(KeySince::StillDown, |l| l.since(since))
    }

    async fn on_deck_input_at(self: &Arc<Self>, input: Input, at: Instant) {
        self.emit("input", json!(input));
        self.handle_input(input, at).await;
    }

    /// Tests: the deck reads `input` now and the agent handles it at once.
    #[cfg(test)]
    async fn on_deck_input(self: &Arc<Self>, input: Input) {
        let at = Instant::now();
        self.deck_read(&input, at);
        self.on_deck_input_at(input, at).await;
    }

    /// Sends to the deck, unless it sleeps: a sleeping deck stays dark (`draw_sleep` and
    /// `release_deck` use `send_to_deck`).
    fn deck_send(&self, cmds: Vec<Command>) {
        if !self.asleep() {
            self.send_to_deck(cmds);
        }
    }

    fn send_to_deck(&self, cmds: Vec<Command>) {
        let mut d = lock(&self.deck);
        let DeckSlot {
            commands: Some(tx),
            on_screen,
            ..
        } = &mut *d
        else {
            return;
        };
        for c in cmds {
            if on_screen.changes(&c) {
                let _ = tx.send(c);
            }
        }
    }

    /// The strip for a mode with no panels: each dial's label and live value, as before
    /// modes existed.
    fn legacy_strip(&self, cfg: &Config, page: &novagear_core::Page) -> Option<Command> {
        let values = deck::DialValues {
            brightness: self
                .avg_light(|s| u32::from(s.brightness))
                .map(|b| format!("{b}%")),
            kelvin: self
                .avg_light(|s| u32::from(keylight::mired_to_kelvin(s.temperature)))
                .map(|k| format!("{k}K")),
            lights_on: lock(&self.lights)
                .values()
                .any(|l| l.state.is_some_and(|s| s.on == 1)),
            prompter_speed: cfg.prompter.speed,
            prompter_playing: lock(&self.prompter).playing,
            deck_brightness: cfg.deck_brightness,
            dpi: lock(&self.mouse).dpi,
        };
        match deck::strip_image(&page.dials, &values) {
            Ok(jpeg) => Some(Command::StripImage {
                x: 0,
                width: streamdeck::STRIP_W as u16,
                jpeg,
            }),
            Err(e) => {
                tracing::warn!("drawing touch strip: {e:#}");
                None
            }
        }
    }

    fn avg_light(&self, f: impl Fn(&LightState) -> u32) -> Option<u32> {
        let lights = lock(&self.lights);
        let vals: Vec<u32> = lights
            .values()
            .filter_map(|l| l.state.as_ref().map(&f))
            .collect();
        (!vals.is_empty()).then(|| vals.iter().sum::<u32>() / vals.len() as u32)
    }

    /// Clears the deck's screen when NovaGear gives control back.
    pub fn release_deck(&self) {
        self.send_to_deck(vec![Command::Reset]);
    }
}

/// A device's firmware status from its record. `latest` is only ever filled for lights.
fn firmware_info(
    rec: Option<&Record>,
    installed: Option<String>,
    installed_is_device_version: bool,
) -> FirmwareInfo {
    let fw = rec.map(|r| &r.firmware);
    FirmwareInfo {
        installed,
        installed_is_device_version,
        policy: fw.map_or("none", |f| f.policy.as_str()).into(),
        tool: fw.and_then(|f| f.tool.clone()),
        url: fw.and_then(|f| f.url.clone()),
        ..Default::default()
    }
}

/// Runs a blocking system or OBS call on the blocking pool.
async fn blocking(f: impl FnOnce() -> Result<()> + Send + 'static) -> Result<()> {
    tokio::task::spawn_blocking(f).await?
}

/// What a rescan covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rescan {
    pub usb: bool,
    pub hid: bool,
}

/// When to list next and what to list. Pure, so the timings are testable.
/// A pending hotplug event wins (debounced, USB and HID both); otherwise the sooner of
/// the safety re-list and, while Bluetooth gear was recently seen, a HID-only poll.
pub(crate) fn next_rescan(
    now: Instant,
    last_event: Option<Instant>,
    last_bt_seen: Option<Instant>,
    last_safety: Instant,
) -> Option<(Duration, Rescan)> {
    let both = Rescan {
        usb: true,
        hid: true,
    };
    if let Some(t) = last_event {
        return Some(((t + DEBOUNCE).saturating_duration_since(now), both));
    }
    let safety = (last_safety + SAFETY).saturating_duration_since(now);
    let bt_active = last_bt_seen.is_some_and(|t| now.saturating_duration_since(t) <= BT_LINGER);
    if bt_active && BT_POLL < safety {
        Some((
            BT_POLL,
            Rescan {
                usb: false,
                hid: true,
            },
        ))
    } else {
        Some((safety, both))
    }
}

/// Where device lists come from; a fake in tests.
pub(crate) trait Lister: Send + Sync + 'static {
    fn usb(&self) -> Vec<UsbDevice>;
    fn hid(&self) -> Vec<HidDevice>;
}

struct Hardware(Arc<Records>);

impl Lister for Hardware {
    fn usb(&self) -> Vec<UsbDevice> {
        catalog::list(&self.0)
    }
    fn hid(&self) -> Vec<HidDevice> {
        catalog::list_hid(&self.0)
    }
}

/// HID devices as API devices. USB ones the USB list already has are skipped, and a
/// controller seen over both USB and Bluetooth with the same serial is one device. Without
/// a serial the two can't be told apart from two controllers, so both are listed with
/// "(USB)" and "(Bluetooth)" after the name.
pub(crate) fn hid_infos(hid: &[HidDevice], usb: &[UsbDevice]) -> Vec<DeviceInfo> {
    let listed = |h: &HidDevice| {
        usb.iter().any(|u| {
            u.usb == h.usb && (!h.bluetooth || (h.serial.is_some() && u.serial == h.serial))
        })
    };
    // (device, over usb, over bluetooth)
    let mut merged: Vec<(&HidDevice, bool, bool)> = Vec::new();
    for h in hid.iter().filter(|h| !listed(h)) {
        match merged.iter_mut().find(|m| m.0.key == h.key) {
            Some(m) => {
                m.1 |= !h.bluetooth;
                m.2 |= h.bluetooth;
            }
            None => merged.push((h, !h.bluetooth, h.bluetooth)),
        }
    }
    merged
        .iter()
        .map(|&(h, by_usb, by_bt)| {
            let twin = merged
                .iter()
                .any(|m| m.0.usb == h.usb && m.0.key != h.key && m.0.bluetooth != h.bluetooth);
            let name = match (twin && h.serial.is_none(), h.bluetooth) {
                (true, true) => format!("{} (Bluetooth)", h.name),
                (true, false) => format!("{} (USB)", h.name),
                (false, _) => h.name.clone(),
            };
            let connections: Vec<&str> = [(by_usb, "usb"), (by_bt, "bluetooth")]
                .into_iter()
                .filter_map(|(on, n)| on.then_some(n))
                .collect();
            DeviceInfo {
                id: h.key.clone(),
                kind: h.kind,
                name,
                vendor: h.vendor.clone(),
                usb: Some(h.usb),
                status: Status::Listed,
                state: json!({ "connections": connections }),
                firmware: None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use novagear_drivers::catalog::HidDevice;
    use novagear_drivers::hotplug::Watch;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }
    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn events_debounce_to_one_usb_and_hid_rescan() {
        let now = Instant::now();
        let r = next_rescan(now, Some(now - ms(100)), None, now);
        assert_eq!(
            r,
            Some((
                ms(200),
                Rescan {
                    usb: true,
                    hid: true
                }
            ))
        );
    }

    #[test]
    fn no_bluetooth_means_no_hid_poll() {
        let now = Instant::now();
        let r = next_rescan(now, None, None, now - secs(5));
        assert_eq!(
            r,
            Some((
                secs(55),
                Rescan {
                    usb: true,
                    hid: true
                }
            ))
        );
    }

    #[test]
    fn recent_bluetooth_polls_hid_every_10s() {
        let now = Instant::now();
        let r = next_rescan(now, None, Some(now - secs(30)), now);
        assert_eq!(
            r,
            Some((
                secs(10),
                Rescan {
                    usb: false,
                    hid: true
                }
            ))
        );
    }

    #[test]
    fn bluetooth_poll_stops_after_linger() {
        let now = Instant::now();
        let r = next_rescan(now, None, Some(now - secs(61)), now - secs(5));
        assert_eq!(
            r,
            Some((
                secs(55),
                Rescan {
                    usb: true,
                    hid: true
                }
            ))
        );
    }

    #[test]
    fn safety_wins_when_it_is_sooner_than_a_bluetooth_poll() {
        let now = Instant::now();
        let r = next_rescan(now, None, Some(now - secs(5)), now - secs(57));
        assert_eq!(
            r,
            Some((
                secs(3),
                Rescan {
                    usb: true,
                    hid: true
                }
            ))
        );
    }

    fn hid(bluetooth: bool, serial: Option<&str>, path: &str) -> HidDevice {
        let usb = UsbId {
            vid: 0x054c,
            pid: 0x0ce6,
        };
        HidDevice {
            usb,
            key: catalog::hid_key(usb, serial, bluetooth, path),
            name: "DualSense Controller".into(),
            vendor: "Sony".into(),
            serial: serial.map(str::to_string),
            bluetooth,
            kind: DeviceKind::Controller,
        }
    }

    #[test]
    fn same_serial_over_usb_and_bluetooth_is_one_device() {
        let both = [
            hid(false, Some("ab:cd"), "usb-path"),
            hid(true, Some("ab:cd"), "bt-path"),
        ];
        let out = hid_infos(&both, &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "hid-054c-0ce6-ab:cd");
        assert_eq!(out[0].name, "DualSense Controller");
        assert_eq!(out[0].kind, DeviceKind::Controller);
        assert_eq!(out[0].state["connections"], json!(["usb", "bluetooth"]));
    }

    #[test]
    fn without_a_serial_usb_and_bluetooth_are_two_devices() {
        let both = [hid(false, None, "usb-path"), hid(true, None, "bt-path")];
        let out = hid_infos(&both, &[]);
        let names: Vec<_> = out.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "DualSense Controller (USB)",
                "DualSense Controller (Bluetooth)"
            ]
        );
        assert_ne!(out[0].id, out[1].id);
        assert!(out[1].id.starts_with("hid-054c-0ce6-bt-"));
    }

    #[test]
    fn a_lone_controller_keeps_its_plain_name() {
        let out = hid_infos(&[hid(true, None, "bt-path")], &[]);
        assert_eq!(out[0].name, "DualSense Controller");
    }

    fn usb_dev(
        vid: u16,
        pid: u16,
        serial: Option<&str>,
        kind: DeviceKind,
        family: Family,
    ) -> UsbDevice {
        UsbDevice {
            usb: UsbId { vid, pid },
            key: format!("usb-{vid:04x}-{pid:04x}"),
            vendor: String::new(),
            name: "x".into(),
            serial: serial.map(str::to_string),
            kind,
            family,
            device_version: None,
            unknown: false,
            product: String::new(),
            interfaces: Vec::new(),
            usage_pages: Vec::new(),
        }
    }

    /// What `catalog::list` returns on a desk with a deck and a USB DualSense: the
    /// DualSense is not in it, because controllers are HID's.
    fn desk() -> Vec<UsbDevice> {
        vec![usb_dev(
            0x0fd9,
            0x0084,
            Some("EL51"),
            DeviceKind::StreamDeck,
            Family::StreamDeck,
        )]
    }

    #[test]
    fn merge_works_beside_other_usb_gear() {
        let both = [
            hid(false, Some("ab:cd"), "usb-path"),
            hid(true, Some("ab:cd"), "bt-path"),
        ];
        let out = hid_infos(&both, &desk());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "hid-054c-0ce6-ab:cd");
        let none = [hid(false, None, "u"), hid(true, None, "b")];
        let out = hid_infos(&none, &desk());
        assert_eq!(out[0].name, "DualSense Controller (USB)");
        assert_eq!(out[1].name, "DualSense Controller (Bluetooth)");
    }

    #[test]
    fn the_id_is_the_same_over_usb_and_over_bluetooth() {
        let usb_only = hid_infos(&[hid(false, Some("ab:cd"), "u")], &desk());
        let bt_only = hid_infos(&[hid(true, Some("ab:cd"), "b")], &desk());
        assert_eq!(usb_only[0].id, bt_only[0].id);
    }

    #[test]
    fn an_unknown_controller_the_usb_list_has_is_not_listed_twice() {
        let mut u = usb_dev(
            0x054c,
            0x0ce6,
            Some("ab:cd"),
            DeviceKind::Controller,
            Family::None,
        );
        u.unknown = true;
        let both = [
            hid(false, Some("ab:cd"), "usb-path"),
            hid(true, Some("ab:cd"), "bt-path"),
        ];
        assert!(hid_infos(&both, &[u]).is_empty());
    }

    struct Counting {
        usb: AtomicUsize,
        hid: AtomicUsize,
    }
    impl Lister for Counting {
        fn usb(&self) -> Vec<UsbDevice> {
            self.usb.fetch_add(1, Ordering::SeqCst);
            Vec::new()
        }
        fn hid(&self) -> Vec<HidDevice> {
            self.hid.fetch_add(1, Ordering::SeqCst);
            Vec::new()
        }
    }

    fn counting() -> Arc<Counting> {
        Arc::new(Counting {
            usb: AtomicUsize::new(0),
            hid: AtomicUsize::new(0),
        })
    }

    /// Lets the detection task and the blocking pool run until `n` USB listings happened.
    async fn until_usb(l: &Counting, n: usize) {
        for _ in 0..2000 {
            if l.usb.load(Ordering::SeqCst) >= n {
                return;
            }
            std::thread::sleep(ms(1));
            tokio::task::yield_now().await;
        }
        panic!(
            "only {} USB listings, wanted {n}",
            l.usb.load(Ordering::SeqCst)
        );
    }

    /// A fake light on loopback that answers every request with one fixed state.
    fn fake_light() -> SocketAddr {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for mut c in l.incoming().flatten() {
                let mut buf = [0u8; 1024];
                let _ = c.read(&mut buf);
                let body =
                    r#"{"numberOfLights":1,"lights":[{"on":1,"brightness":20,"temperature":200}]}"#;
                let _ = write!(
                    c,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        addr
    }

    fn unreachable_error() -> anyhow::Error {
        // 65 is EHOSTUNREACH on macOS; use whatever the driver treats as blocked.
        let probe = [65, 113, 10065]
            .into_iter()
            .map(|c| anyhow::Error::new(std::io::Error::from_raw_os_error(c)))
            .find(keylight::is_network_blocked)
            .unwrap();
        probe.context("connect")
    }

    #[tokio::test]
    async fn success_clears_blocked() {
        let agent = test_agent("net-clear");
        let mut l = light(53, 200);
        l.addr = fake_light();
        l.error = Some("old".into());
        lock(&agent.lights).insert("light-s".into(), l);
        agent
            .network_blocked
            .store(true, std::sync::atomic::Ordering::Relaxed);
        agent.refresh_lights().await;
        assert!(!agent.network_blocked());
        assert!(lock(&agent.lights)["light-s"].error.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn blocked_light_error_sets_flag_and_explains() {
        let agent = test_agent("net-set");
        assert!(!agent.network_blocked());
        let text = agent
            .light_outcome::<()>(&Err(unreachable_error()))
            .unwrap();
        assert!(agent.network_blocked());
        let want = if cfg!(target_os = "macos") {
            "macOS is blocking local network access for NovaGear"
        } else {
            "local network access is blocked"
        };
        assert_eq!(text, want);
        // An ordinary failure explains itself and leaves the flag alone.
        let other = agent.light_outcome::<()>(&Err(anyhow::anyhow!("light timed out")));
        assert_eq!(other.unwrap(), "light timed out");
        assert!(agent.network_blocked());
        // Success after blocked clears it.
        assert!(agent.light_outcome(&Ok(())).is_none());
        assert!(!agent.network_blocked());
    }

    #[tokio::test(start_paused = true)]
    async fn one_reachable_light_keeps_the_network_unblocked_in_any_order() {
        let ok = || -> Result<()> { Ok(()) };
        let dead = || -> Result<()> { Err(unreachable_error()) };
        for results in [[ok(), dead()], [dead(), ok()]] {
            let agent = test_agent("net-order");
            let texts = agent.fold_outcomes(&results);
            assert!(!agent.network_blocked());
            assert_eq!(texts[0].is_none(), results[0].is_ok());
            let dead_text = texts.iter().flatten().next().unwrap();
            assert_eq!(dead_text, UNREACHABLE_REASON);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn unreachable_after_a_recent_success_is_a_dead_light_until_the_window_passes() {
        let agent = test_agent("net-window");
        agent.light_outcome(&Ok(()));
        let text = agent.light_outcome::<()>(&Err(unreachable_error()));
        assert_eq!(text.unwrap(), UNREACHABLE_REASON);
        assert!(!agent.network_blocked());
        tokio::time::advance(BLOCK_WINDOW + Duration::from_secs(1)).await;
        agent.light_outcome::<()>(&Err(unreachable_error()));
        assert!(agent.network_blocked());
    }

    #[tokio::test(start_paused = true)]
    async fn probe_result_sets_only_an_mdns_block() {
        let agent = test_agent("net-probe");
        // Other errors and success leave the state alone.
        agent.on_probe_result(&Err(std::io::Error::other("boom")));
        agent.on_probe_result(&Ok(()));
        assert!(!agent.network_blocked());
        // Unreachable sets the block, as an mDNS block.
        let code = [65, 113, 10065]
            .into_iter()
            .map(std::io::Error::from_raw_os_error)
            .find(keylight::is_unreachable_io)
            .unwrap();
        agent.on_probe_result(&Err(code));
        assert!(agent.network_blocked());
        // A successful probe does not clear it, a resolved advert does.
        agent.on_probe_result(&Ok(()));
        assert!(agent.network_blocked());
        agent.on_service_resolved();
        assert!(!agent.network_blocked());
    }

    #[tokio::test(start_paused = true)]
    async fn probe_does_not_override_a_recent_light_success() {
        let agent = test_agent("net-probe-ok");
        agent.light_outcome(&Ok(()));
        let code =
            std::io::Error::from_raw_os_error(if cfg!(target_os = "macos") { 65 } else { 113 });
        agent.on_probe_result(&Err(code));
        assert!(!agent.network_blocked());
    }

    #[tokio::test(start_paused = true)]
    async fn a_resolved_advert_clears_only_an_mdns_block() {
        let agent = test_agent("net-resolved");
        agent.on_daemon_event(&mdns_sd::DaemonEvent::Error(mdns_sd::Error::Msg(
            "x".into(),
        )));
        assert!(agent.network_blocked());
        agent.on_service_resolved();
        assert!(!agent.network_blocked());
        // A block seen on a light call is not cleared by mDNS.
        agent.light_outcome::<()>(&Err(unreachable_error()));
        assert!(agent.network_blocked());
        agent.on_service_resolved();
        assert!(agent.network_blocked());
    }

    #[test]
    fn on_drop_guard_runs_once_when_dropped() {
        let n = std::cell::Cell::new(0);
        {
            let _g = OnDrop::new(|| n.set(n.get() + 1));
            assert_eq!(n.get(), 0);
        }
        assert_eq!(n.get(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_restarts_every_30_seconds_after_an_error() {
        let agent = test_agent("net-retry");
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = calls.clone();
        let run = agent.run_discovery(move || {
            let c = c.clone();
            async move {
                c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                bail!("no route")
            }
        });
        let _ = tokio::time::timeout(Duration::from_secs(100), run).await;
        // Starts at 0 s, 30 s, 60 s and 90 s.
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn identify_explains_a_blocked_network_like_the_light_does() {
        let agent = test_agent("net-identify");
        let r = agent.light_outcome::<()>(&Err(unreachable_error()));
        assert_eq!(r.unwrap(), BLOCKED_REASON);
    }

    #[tokio::test(start_paused = true)]
    async fn mdns_daemon_error_sets_blocked() {
        let agent = test_agent("net-mdns");
        agent.on_daemon_event(&mdns_sd::DaemonEvent::IpAdd("10.0.0.2".parse().unwrap()));
        assert!(!agent.network_blocked());
        agent.on_daemon_event(&mdns_sd::DaemonEvent::Error(mdns_sd::Error::Msg(
            "send: No route to host (os error 65)".into(),
        )));
        assert!(agent.network_blocked());
    }

    fn test_agent(tag: &str) -> Arc<Agent> {
        let dir = std::env::temp_dir().join(format!("novagear-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Agent::new(dir.join("config.json"), "t".repeat(48)).unwrap()
    }

    fn key_cmd_of(index: usize, byte: u8) -> Command {
        Command::KeyImage {
            index,
            jpeg: vec![byte; 64],
        }
    }

    fn strip_cmd_of(x: u16, width: u16, byte: u8) -> Command {
        Command::StripImage {
            x,
            width,
            jpeg: vec![byte; 64],
        }
    }

    /// How many commands reached the fake deck.
    fn delivered(rx: &std::sync::mpsc::Receiver<Command>) -> usize {
        let mut n = 0;
        while rx.try_recv().is_ok() {
            n += 1;
        }
        n
    }

    #[test]
    fn a_picture_the_deck_already_shows_is_not_sent_again() {
        let agent = test_agent("on-screen");
        let (tx, rx) = std::sync::mpsc::channel();
        lock(&agent.deck).commands = Some(tx);
        agent.send_to_deck(vec![
            key_cmd_of(0, 1),
            key_cmd_of(1, 1),
            strip_cmd_of(0, 200, 1),
        ]);
        assert_eq!(delivered(&rx), 3);
        // The same pictures again: nothing to write.
        agent.send_to_deck(vec![
            key_cmd_of(0, 1),
            key_cmd_of(1, 1),
            strip_cmd_of(0, 200, 1),
        ]);
        assert_eq!(delivered(&rx), 0);
        // A changed key goes through, and only that one.
        agent.send_to_deck(vec![key_cmd_of(0, 2), key_cmd_of(1, 1)]);
        assert_eq!(delivered(&rx), 1);
        // The key that changed and changed back is a change too.
        agent.send_to_deck(vec![key_cmd_of(0, 1)]);
        assert_eq!(delivered(&rx), 1);
    }

    #[test]
    fn a_reset_or_a_new_deck_forgets_what_was_on_screen() {
        let agent = test_agent("on-screen-reset");
        let (tx, rx) = std::sync::mpsc::channel();
        lock(&agent.deck).commands = Some(tx);
        agent.send_to_deck(vec![key_cmd_of(0, 1), strip_cmd_of(200, 200, 1)]);
        assert_eq!(delivered(&rx), 2);
        agent.send_to_deck(vec![Command::Reset]);
        assert_eq!(delivered(&rx), 1);
        agent.send_to_deck(vec![key_cmd_of(0, 1), strip_cmd_of(200, 200, 1)]);
        assert_eq!(delivered(&rx), 2, "the screen was cleared");
        // A deck that closes and opens again is a new slot with nothing recorded.
        let (tx, rx) = std::sync::mpsc::channel();
        *lock(&agent.deck) = DeckSlot {
            commands: Some(tx),
            ..Default::default()
        };
        agent.send_to_deck(vec![key_cmd_of(0, 1), strip_cmd_of(200, 200, 1)]);
        assert_eq!(delivered(&rx), 2);
    }

    #[test]
    fn a_wide_strip_image_covers_the_segments_under_it() {
        let agent = test_agent("on-screen-strip");
        let (tx, rx) = std::sync::mpsc::channel();
        lock(&agent.deck).commands = Some(tx);
        agent.send_to_deck(vec![strip_cmd_of(0, 200, 1), strip_cmd_of(200, 200, 1)]);
        assert_eq!(delivered(&rx), 2);
        // A picture across the whole strip (the pop-up) replaces both.
        agent.send_to_deck(vec![strip_cmd_of(0, 800, 9)]);
        assert_eq!(delivered(&rx), 1);
        // So the segment pictures that were there before must be sent again.
        agent.send_to_deck(vec![strip_cmd_of(0, 200, 1), strip_cmd_of(200, 200, 1)]);
        assert_eq!(delivered(&rx), 2);
        // A neighbour's change leaves the others as they were.
        agent.send_to_deck(vec![strip_cmd_of(0, 200, 1), strip_cmd_of(200, 200, 2)]);
        assert_eq!(delivered(&rx), 1);
    }

    #[tokio::test]
    async fn reading_the_config_shares_it_and_a_turned_knob_does_not_disturb_readers() {
        let agent = test_agent("cfg-shared");
        let (a, b) = (agent.config_arc(), agent.config_arc());
        assert!(Arc::ptr_eq(&a, &b), "a read must not copy the config");
        let before = a.deck_brightness;
        agent.set_scalar(|c| c.deck_brightness = before.saturating_sub(10).max(1));
        // The reader still holds the config it started with; new readers see the change.
        assert_eq!(a.deck_brightness, before);
        assert_ne!(agent.config_arc().deck_brightness, before);
        // An owned copy for editing is still available and independent.
        let mut owned = agent.config();
        owned.deck_brightness = 7;
        assert_ne!(agent.config_arc().deck_brightness, 7);
    }

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            lock(&self.0).extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn watcher_unavailable_falls_back_to_scanning() {
        let sink = Sink::default();
        let w = sink.clone();
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(move || w.clone())
                .with_ansi(false)
                .finish(),
        );
        let agent = test_agent("fallback");
        let lister = counting();
        let (tx, _rx) = mpsc::unbounded_channel();
        tokio::spawn(agent.clone().detect(
            Watch::Unavailable("no udev".into()),
            lister.clone(),
            tx,
        ));
        until_usb(&lister, 1).await;
        for n in 2..=4 {
            // Let the loop start its next wait before moving the clock.
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }
            tokio::time::advance(FALLBACK_SCAN).await;
            until_usb(&lister, n).await;
        }
        assert_eq!(lister.usb.load(Ordering::SeqCst), 4);
        let log = String::from_utf8(lock(&sink.0).clone()).unwrap();
        let warnings: Vec<_> = log.lines().filter(|l| l.contains("WARN")).collect();
        assert_eq!(warnings.len(), 1, "{log}");
        assert!(warnings[0].contains("no udev"));
    }

    #[tokio::test(start_paused = true)]
    async fn events_trigger_one_debounced_rescan_and_idle_does_not_poll() {
        let agent = test_agent("events");
        let lister = counting();
        let (tx, _rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = std::sync::mpsc::channel();
        tokio::spawn(
            agent
                .clone()
                .detect(Watch::Events(ev_rx), lister.clone(), tx),
        );
        until_usb(&lister, 1).await;
        for _ in 0..3 {
            ev_tx.send(()).unwrap();
        }
        // Let the forwarding thread deliver, then wait out the debounce.
        std::thread::sleep(ms(50));
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(DEBOUNCE - ms(1)).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        std::thread::sleep(ms(20));
        assert_eq!(
            lister.usb.load(Ordering::SeqCst),
            1,
            "listed before the debounce"
        );
        tokio::time::advance(ms(1)).await;
        until_usb(&lister, 2).await;
        // Half a minute of nothing: no 2 s polling.
        for _ in 0..30 {
            tokio::time::advance(secs(1)).await;
            for _ in 0..5 {
                tokio::task::yield_now().await;
            }
        }
        std::thread::sleep(ms(50));
        assert_eq!(lister.usb.load(Ordering::SeqCst), 2);
        assert_eq!(lister.hid.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_steady_stream_of_events_is_still_listed_within_two_seconds() {
        let agent = test_agent("burst");
        let lister = counting();
        let (tx, _rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = std::sync::mpsc::channel();
        tokio::spawn(
            agent
                .clone()
                .detect(Watch::Events(ev_rx), lister.clone(), tx),
        );
        until_usb(&lister, 1).await;
        for step in 1..=21 {
            ev_tx.send(()).unwrap();
            std::thread::sleep(ms(20));
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            tokio::time::advance(ms(100)).await;
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            std::thread::sleep(ms(5));
            let n = lister.usb.load(Ordering::SeqCst);
            if step < 19 {
                assert_eq!(n, 1, "listed too early at step {step}");
            }
        }
        until_usb(&lister, 2).await;
    }

    struct FakeMouse;
    impl Lister for FakeMouse {
        fn usb(&self) -> Vec<UsbDevice> {
            // A Razer mouse the records don't know: opening it fails without touching hardware.
            vec![usb_dev(
                catalog::RAZER,
                0xffff,
                None,
                DeviceKind::Mouse,
                Family::Razer90,
            )]
        }
        fn hid(&self) -> Vec<HidDevice> {
            Vec::new()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_mouse_read_is_retried_after_15_seconds_without_an_event() {
        let agent = test_agent("retry");
        let (tx, _rx) = mpsc::unbounded_channel();
        let (_ev_tx, ev_rx) = std::sync::mpsc::channel::<()>();
        tokio::spawn(
            agent
                .clone()
                .detect(Watch::Events(ev_rx), Arc::new(FakeMouse), tx),
        );
        let first = loop {
            if let Some(t) = lock(&agent.mouse).retry_at {
                break t;
            }
            std::thread::sleep(ms(1));
            tokio::task::yield_now().await;
        };
        assert!(lock(&agent.mouse).error.is_some());
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(RETRY - ms(1)).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        std::thread::sleep(ms(30));
        assert_eq!(lock(&agent.mouse).retry_at, Some(first), "retried early");
        tokio::time::advance(ms(1)).await;
        for _ in 0..2000 {
            if lock(&agent.mouse).retry_at != Some(first) {
                break;
            }
            std::thread::sleep(ms(1));
            tokio::task::yield_now().await;
        }
        assert_eq!(lock(&agent.mouse).retry_at, Some(first + RETRY));
    }

    #[test]
    fn dry_run_blocks_input_before_the_platform_code() {
        let dry = std::sync::atomic::AtomicBool::new(true);
        let called = std::cell::Cell::new(false);
        let r = guarded_input(&dry, || {
            called.set(true);
            Ok(())
        });
        assert!(r.is_err());
        assert!(!called.get(), "platform input ran during a dry run");
        dry.store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(guarded_input(&dry, || Ok(())).is_ok());
    }

    #[tokio::test]
    async fn typing_and_hotkeys_error_in_dry_run() {
        let agent = test_agent("dryrun");
        agent.set_input_dry_run(true);
        let e = agent
            .run(Action::TypeText {
                text: "never-typed".into(),
                enter: true,
            })
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("dry run"));
        let e = agent
            .run(Action::Hotkey {
                keys: "cmd+a".into(),
            })
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("dry run"));
    }

    fn latest_cache(board: u8, build: u16) -> elgato_fw::Cache {
        elgato_fw::Cache {
            checked_at: 1,
            etag: None,
            app_version: "1.9.2".into(),
            by_board: BTreeMap::from([(
                board,
                elgato_fw::FwHeader {
                    board,
                    major: 1,
                    minor: 0,
                    patch: 3,
                    build,
                },
            )]),
        }
    }

    fn light(board: u8, build: u32) -> Light {
        Light {
            addr: "192.168.1.5:9123".parse().unwrap(),
            name: "Key Light".into(),
            serial: "S".into(),
            firmware: "1.0.3".into(),
            board,
            build,
            state: None,
            error: None,
        }
    }

    #[test]
    fn builtin_feeds_follow_lights_prompter_and_mic() {
        let agent = test_agent("feeds");
        let num = |k: &str| match agent.feeds.get(k).map(|f| f.value) {
            Some(feeds::FeedValue::Number(n)) => Some(n),
            _ => None,
        };
        assert_eq!(
            num("prompter.speed"),
            Some(f64::from(agent.config().prompter.speed))
        );
        assert_eq!(num("lights.on"), None);

        let mut l = light(53, 1);
        l.state = Some(LightState {
            on: 1,
            brightness: 40,
            temperature: keylight::kelvin_to_mired(5000),
        });
        lock(&agent.lights).insert("light-s".into(), l);
        agent.devices_changed();
        assert_eq!(num("lights.on"), Some(1.0));
        assert_eq!(num("lights.brightness"), Some(40.0));
        assert!((num("lights.kelvin").unwrap() - 5000.0).abs() < 60.0);

        lock(&agent.lights).clear();
        agent.devices_changed();
        assert_eq!(num("lights.on"), None);

        agent.publish_prompter_playing(true);
        assert_eq!(num("prompter.playing"), Some(1.0));
        agent.set_mic_muted(true);
        assert_eq!(num("mic.muted"), Some(1.0));
        agent.set_mic_muted(false);
        assert_eq!(num("mic.muted"), Some(0.0));
    }

    #[test]
    fn light_shows_update_when_latest_build_is_higher() {
        let agent = test_agent("fw-light");
        lock(&agent.lights).insert("light-s".into(), light(53, 200));
        agent.set_firmware_cache(Some(latest_cache(53, 222)));
        let d = agent.devices();
        let f = d[0].firmware.as_ref().unwrap();
        assert_eq!(f.installed.as_deref(), Some("1.0.3 (200)"));
        assert_eq!(f.latest.as_deref(), Some("1.0.3 (222)"));
        assert!(f.update_available);
        assert!(!f.installed_is_device_version);
        assert_eq!(f.policy, "native");
        assert_eq!(
            f.source.as_deref(),
            Some("the newest Elgato ships in Control Center 1.9.2")
        );
        assert_eq!(agent.firmware_updates(), 1);

        // Up to date, and a board the zip does not carry.
        lock(&agent.lights).insert("light-s".into(), light(53, 222));
        assert!(
            !agent.devices()[0]
                .firmware
                .as_ref()
                .unwrap()
                .update_available
        );
        lock(&agent.lights).insert("light-s".into(), light(205, 1));
        let f = agent.devices()[0].firmware.clone().unwrap();
        assert!(!f.update_available && f.latest.is_none());
        assert_eq!(agent.firmware_updates(), 0);
    }

    #[test]
    fn unknown_build_with_the_same_version_claims_no_update() {
        let agent = test_agent("fw-build0");
        agent.set_firmware_cache(Some(latest_cache(53, 222)));
        lock(&agent.lights).insert("light-s".into(), light(53, 0));
        let f = agent.devices()[0].firmware.clone().unwrap();
        assert!(!f.update_available, "build 0 is unknown, not older");
        assert_eq!(f.installed.as_deref(), Some("1.0.3"));
        assert_eq!(f.latest.as_deref(), Some("1.0.3 (222)"));
        // A higher version still counts without a build number.
        let mut l = light(53, 0);
        l.firmware = "1.0.2".into();
        lock(&agent.lights).insert("light-s".into(), l);
        assert!(
            agent.devices()[0]
                .firmware
                .as_ref()
                .unwrap()
                .update_available
        );
    }

    #[test]
    fn each_razer_mouse_shows_only_its_own_firmware() {
        let agent = test_agent("fw-two-razer");
        let mouse = |pid| {
            let mut m = usb_dev(
                catalog::RAZER,
                pid,
                None,
                DeviceKind::Mouse,
                Family::Razer90,
            );
            m.device_version = Some("2.00".into());
            m
        };
        *lock(&agent.usb) = vec![mouse(0x0099), mouse(0x00aa)];
        {
            let mut m = lock(&agent.mouse);
            m.pid = Some(0x0099);
            m.firmware = Some("1.4".into());
        }
        let d = agent.devices();
        let f0 = d[0].firmware.as_ref().unwrap();
        let f1 = d[1].firmware.as_ref().unwrap();
        assert_eq!(f0.installed.as_deref(), Some("1.4"));
        assert_eq!(f1.installed.as_deref(), Some("2.00"));
        assert!(f1.installed_is_device_version);
    }

    #[test]
    fn vendor_tool_device_never_claims_an_update() {
        let agent = test_agent("fw-deck");
        // Even a cache with a "newer" image for every board must not touch the deck.
        agent.set_firmware_cache(Some(latest_cache(53, 9999)));
        *lock(&agent.usb) = vec![usb_dev(
            catalog::STREAM_DECK_PLUS.vid,
            catalog::STREAM_DECK_PLUS.pid,
            Some("D1"),
            DeviceKind::StreamDeck,
            Family::StreamDeck,
        )];
        lock(&agent.deck).info = Some(streamdeck::DeckInfo {
            serial: "D1".into(),
            firmware: "1.05.008".into(),
        });
        let d = agent.devices();
        let f = d[0].firmware.as_ref().unwrap();
        assert_eq!(f.policy, "vendor_tool");
        assert_eq!(f.installed.as_deref(), Some("1.05.008"));
        assert!(!f.installed_is_device_version);
        assert!(!f.update_available);
        assert!(f.latest.is_none());
        assert_eq!(f.tool.as_deref(), Some("Elgato Stream Deck app"));
        assert_eq!(f.url.as_deref(), Some("https://www.elgato.com/downloads"));
    }

    #[test]
    fn other_devices_show_bcd_device_and_the_mouse_its_read_firmware() {
        let agent = test_agent("fw-other");
        let mut mouse = usb_dev(
            catalog::RAZER,
            0x0099,
            None,
            DeviceKind::Mouse,
            Family::Razer90,
        );
        mouse.device_version = Some("2.00".into());
        let mut cam = usb_dev(0x0fd9, 0x0066, None, DeviceKind::Capture, Family::Uvc);
        cam.device_version = Some("1.02".into());
        *lock(&agent.usb) = vec![mouse, cam];
        // Before the read the mouse shows its bcdDevice; after, the firmware the mouse said.
        let f = agent.devices()[0].firmware.clone().unwrap();
        assert_eq!(f.installed.as_deref(), Some("2.00"));
        assert!(f.installed_is_device_version);
        {
            let mut m = lock(&agent.mouse);
            m.pid = Some(0x0099);
            m.firmware = Some("1.4".into());
        }
        let d = agent.devices();
        let f = d[0].firmware.as_ref().unwrap();
        assert_eq!(f.installed.as_deref(), Some("1.4"));
        assert!(!f.installed_is_device_version);
        assert_eq!(f.policy, "vendor_tool");
        let f = d[1].firmware.as_ref().unwrap();
        assert_eq!(f.installed.as_deref(), Some("1.02"));
        assert!(f.installed_is_device_version);
    }

    #[test]
    fn a_light_without_firmware_info_has_none_installed() {
        let agent = test_agent("fw-nolight");
        let mut l = light(0, 0);
        l.firmware = String::new();
        lock(&agent.lights).insert("light-s".into(), l);
        let f = agent.devices()[0].firmware.clone().unwrap();
        assert!(f.installed.is_none() && !f.update_available);
    }

    #[test]
    fn obs_failure_logs_leave_out_obs_comment() {
        let e: anyhow::Error = obs::RequestFailed {
            request_type: "ToggleInputMute".into(),
            code: 601,
            comment: "No input called Mic/Aux-sekret".into(),
        }
        .into();
        // The panel text keeps the comment; the log line does not.
        assert!(format!("{e:#}").contains("Mic/Aux-sekret"));
        let logged = failure_log(&e);
        assert!(!logged.contains("sekret"), "{logged}");
        assert!(logged.contains("ToggleInputMute") && logged.contains("601"));
        assert_eq!(failure_log(&anyhow::anyhow!("plain")), "plain");
    }

    /// Waits for something to connect to `listener`.
    fn connected_to(listener: &std::net::TcpListener) -> bool {
        listener.set_nonblocking(true).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if listener.accept().is_ok() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn obs_link_runs_only_while_something_needs_obs() {
        use novagear_core::modes::{ModeRule, RuleWhen};
        let agent = test_agent("obs-link");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut cfg = agent.config();
        cfg.obs.url = format!("ws://{}", listener.local_addr().unwrap());
        agent.set_config(cfg.clone()).unwrap();
        // No rule and no obs feed on screen: no thread, no connection.
        assert!(lock(&agent.obs_link).is_none());

        // A rule on recording needs it.
        cfg.mode_rules = vec![ModeRule {
            when: RuleWhen::ObsRecording,
            mode: "x".into(),
        }];
        agent.set_config(cfg.clone()).unwrap();
        assert!(lock(&agent.obs_link).is_some());
        assert!(connected_to(&listener), "the link tried OBS");
        cfg.mode_rules.clear();
        agent.set_config(cfg).unwrap();
        assert!(lock(&agent.obs_link).is_none());

        // So does a visible face reading an obs feed, and only while it is visible.
        agent.feeds.set_wanted(["obs.scene".to_string()].into());
        agent.reconcile_obs();
        assert!(lock(&agent.obs_link).is_some());
        agent.feeds.set_wanted(["clock.time".to_string()].into());
        agent.reconcile_obs();
        assert!(lock(&agent.obs_link).is_none());
        // Events wait for whoever handles the rules, once.
        assert!(agent.take_obs_events().is_some());
        assert!(agent.take_obs_events().is_none());
    }

    /// Replacing a link that is slow to give up (a connect stuck on DNS) returns at once,
    /// and the old link's cleanup still lands before the new link sets its feeds.
    #[test]
    fn replacing_a_slow_obs_link_does_not_block_the_caller() {
        let agent = test_agent("obs-slow");
        // Never accepted: the new link's handshake just waits.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut cfg = agent.config();
        cfg.obs.url = format!("ws://{}", listener.local_addr().unwrap());
        agent.set_config(cfg.clone()).unwrap();
        agent.feeds.set_wanted(["obs.scene".to_string()].into());
        agent.feeds.set(
            "obs.connected",
            feeds::FeedValue::Number(1.0),
            None,
            None,
            feeds::FOREVER,
        );
        let mut old = cfg.obs.clone();
        old.url = "ws://127.0.0.1:1".into();
        *lock(&agent.obs_link) = Some((
            old,
            obs::ObsLink::slow_to_stop(agent.feeds.clone(), Duration::from_millis(1500)),
        ));
        let t = std::time::Instant::now();
        agent.reconcile_obs();
        assert!(
            t.elapsed() < Duration::from_millis(500),
            "returned after {:?}",
            t.elapsed()
        );
        assert!(lock(&agent.obs_link).is_some(), "the new link is in place");
        std::thread::sleep(Duration::from_millis(2500));
        assert_eq!(
            agent.feeds.get("obs.connected").map(|f| f.value),
            Some(feeds::FeedValue::Number(0.0)),
            "the new link's feed, set after the old one cleared"
        );
    }
}
