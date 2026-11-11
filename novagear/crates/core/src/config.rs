//! The agent's config file and API token, kept in the OS config dir
//! (`~/Library/Application Support/NovaGear`, `%APPDATA%\NovaGear`, `~/.config/novagear`).

use crate::modes::{Mode, ModeKey, ModeRule};
use crate::{Action, DialBinding, KeyBinding, Page};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub fn config_dir() -> Result<PathBuf> {
    let base = dirs::config_dir().context("no config dir on this OS")?;
    Ok(base.join(if cfg!(target_os = "linux") {
        "novagear"
    } else {
        "NovaGear"
    }))
}

/// Where the agent writes its logs: `~/Library/Logs/NovaGear` on macOS,
/// `%LOCALAPPDATA%\NovaGear\Logs` on Windows, `$XDG_STATE_HOME/novagear` (default
/// `~/.local/state/novagear`) elsewhere.
pub fn log_dir() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        Ok(dirs::home_dir()
            .context("no home dir")?
            .join("Library/Logs/NovaGear"))
    }
    #[cfg(windows)]
    {
        Ok(dirs::data_local_dir()
            .context("no local app data dir")?
            .join("NovaGear")
            .join("Logs"))
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let base = match dirs::state_dir() {
            Some(d) => d,
            None => dirs::home_dir()
                .context("no home dir")?
                .join(".local/state"),
        };
        Ok(base.join("novagear"))
    }
}

pub fn token_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("token"))
}

/// The agent's Unix socket (0600), the only way NovaGuard reaches the API.
pub fn socket_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("agent.sock"))
}

pub fn read_token() -> Result<String> {
    let p = token_path()?;
    let t = std::fs::read_to_string(&p)
        .with_context(|| format!("reading {} (is the agent running?)", p.display()))?;
    Ok(t.trim().to_string())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// When false the agent reads the Stream Deck but never draws on it, so it can sit
    /// beside the Elgato app without flicker.
    pub deck_control: bool,
    pub deck_brightness: u8,
    /// The deck's pages; keys are numbered left to right, then top to bottom.
    pub pages: Vec<Page>,
    /// The page shown when the deck opens.
    pub start_page: String,
    /// Before pages existed the config held one layout here; `normalized` moves it into a
    /// "Main" page.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<Option<KeyBinding>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dials: Vec<Option<DialBinding>>,
    /// DPI stages `MouseDpiStep` walks through.
    pub dpi_stages: Vec<u16>,
    pub prompter: PrompterConfig,
    pub obs: ObsConfig,
    /// Once a day, while a Key Light is around, ask Elgato whether newer light firmware
    /// exists. It reads about 200 KB of Elgato's Control Center download and installs nothing.
    pub firmware_check: bool,
    /// Named sets of pages. `normalized` wraps a pages-only config into one "main" mode and
    /// then clears `pages`.
    pub modes: Vec<Mode>,
    /// The mode shown when the deck opens.
    pub start_mode: String,
    pub mode_rules: Vec<ModeRule>,
    pub mode_key: ModeKey,
    /// Minutes of inactivity before the deck sleeps; 0 never sleeps.
    pub deck_sleep_min: u32,
    /// An image name drawn while the deck sleeps.
    pub deck_sleep_image: Option<String>,
}

/// obs-websocket (built into OBS 28+, Tools › WebSocket Server Settings).
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ObsConfig {
    pub url: String,
    /// Empty when authentication is off. This is why the config file is written 0600.
    pub password: String,
}

/// Hand-written so the password can't reach a log through `{:?}`.
impl std::fmt::Debug for ObsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObsConfig")
            .field("url", &"<hidden>")
            .field("password", &"<hidden>")
            .finish()
    }
}

impl Default for ObsConfig {
    fn default() -> Self {
        Self {
            url: "ws://127.0.0.1:4455".into(),
            password: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrompterConfig {
    pub script: String,
    /// Pixels per second.
    pub speed: u16,
    pub font_size: u16,
    /// Mirror horizontally for beam-splitter glass.
    pub mirror: bool,
}

impl Default for PrompterConfig {
    fn default() -> Self {
        Self {
            script: String::new(),
            speed: default_speed(),
            font_size: default_font(),
            mirror: false,
        }
    }
}

fn default_deck_brightness() -> u8 {
    70
}
fn default_dpi_stages() -> Vec<u16> {
    vec![800, 1600, 3200]
}
fn default_speed() -> u16 {
    60
}
fn default_font() -> u16 {
    64
}

impl Default for Config {
    fn default() -> Self {
        let key = |label: &str, color: &str, action| {
            Some(KeyBinding {
                label: label.into(),
                color: color.into(),
                icon: None,
                face: Default::default(),
                states: None,
                follow: None,
                hold_ms: 0,
                action,
            })
        };
        let dial = |label: &str, turn, press| {
            Some(DialBinding {
                label: label.into(),
                turn: Some(turn),
                turn_left: None,
                turn_right: None,
                press: Some(press),
                alt_turn: None,
            })
        };
        let main = Page {
            name: "Main".into(),
            keys: vec![
                key("LIGHTS", "#c8871e", Action::LightToggle { id: None }),
                key("PROMPT", "#2a6fdb", Action::PrompterPlayPause),
                key("RESTART", "#3a3f55", Action::PrompterRestart),
                key("DPI", "#1b9e5a", Action::MouseDpiStep { step: 1 }),
                None,
                None,
                None,
                None,
            ],
            dials: vec![
                dial(
                    "BRIGHT",
                    Action::LightBrightnessStep { id: None, step: 2 },
                    Action::LightToggle { id: None },
                ),
                dial(
                    "WARMTH",
                    Action::LightKelvinStep {
                        id: None,
                        step: 100,
                    },
                    Action::LightToggle { id: None },
                ),
                dial(
                    "SPEED",
                    Action::PrompterSpeedStep { step: 5 },
                    Action::PrompterPlayPause,
                ),
                dial(
                    "DECK",
                    Action::DeckBrightnessStep { step: 5 },
                    Action::PrompterRestart,
                ),
            ],
        };
        Self {
            deck_control: false,
            deck_brightness: default_deck_brightness(),
            // Already in the normalized shape, so a file with modes but no "pages" key
            // doesn't get a default page that `normalized` would fold over its modes.
            pages: Vec::new(),
            start_page: "Main".into(),
            keys: Vec::new(),
            dials: Vec::new(),
            dpi_stages: default_dpi_stages(),
            prompter: PrompterConfig::default(),
            obs: ObsConfig::default(),
            firmware_check: true,
            modes: vec![Mode {
                id: "main".into(),
                name: "Main".into(),
                pages: vec![main],
                ..Mode::default()
            }],
            start_mode: "main".into(),
            mode_rules: Vec::new(),
            mode_key: ModeKey::default(),
            deck_sleep_min: 0,
            deck_sleep_image: None,
        }
    }
}

impl Config {
    /// Loads the config, or the defaults when there is no file yet.
    pub fn load(path: &Path) -> Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let mut c = serde_json::from_str::<Config>(&s)
                    .map(Config::normalized)
                    .with_context(|| format!("parsing {}", path.display()))?;
                // Pause all moved from the nova client to NovaGuard; the saved file follows
                // so the live Development mode is updated without a hand edit.
                if c.migrate_pause_key()
                    && let Err(e) = c.save(path)
                {
                    tracing::warn!("saving the migrated Pause all key: {e:#}");
                }
                Ok(c)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(Config::default().normalized())
            }
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Writes through a temp file so a crash never leaves half a config. The file is 0600
    /// because it can hold the OBS password.
    pub fn save(&self, path: &Path) -> Result<()> {
        use std::io::Write;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        opts.open(&tmp)?
            .write_all(&serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Moves a pre-pages layout into a "Main" page and makes sure the start page exists.
    pub fn normalized(mut self) -> Config {
        // A file from before pages has no "pages" key, so serde filled in the default Main
        // page; the file's own layout replaces it.
        if !self.keys.is_empty() || !self.dials.is_empty() {
            let keys = std::mem::take(&mut self.keys);
            let dials = std::mem::take(&mut self.dials);
            match self.pages.iter_mut().find(|p| p.name == "Main") {
                Some(p) => {
                    p.keys = keys;
                    p.dials = dials;
                }
                None => self.pages.insert(
                    0,
                    Page {
                        name: "Main".into(),
                        keys,
                        dials,
                    },
                ),
            }
        }
        // A pages-only config becomes one "main" mode. `start_page` stays as it is, so the
        // start mode still opens on that page.
        if self.modes.is_empty() {
            self.modes.push(Mode {
                id: "main".into(),
                name: "Main".into(),
                ..Mode::default()
            });
            self.start_mode = "main".into();
        }
        if self.mode(&self.start_mode).is_none() {
            self.start_mode = self.modes[0].id.clone();
        }
        // Top-level pages never survive next to modes: whoever sent them (an old client, an
        // import) is editing the start mode's pages, so they replace those pages.
        if !self.pages.is_empty() {
            let pages = std::mem::take(&mut self.pages);
            let start = self.start_mode.clone();
            if let Some(m) = self.modes.iter_mut().find(|m| m.id == start) {
                m.pages = pages;
            }
        }
        if self.page(&self.start_page).is_none()
            && let Some(p) = self.modes.iter().find_map(|m| m.home())
        {
            self.start_page = p.name.clone();
        }
        // Approval keys that overlap or fall off the deck are dropped: the mode shows no
        // pop-ups rather than one whose keys say one thing and do another. The saved file
        // loses them too, so this is said once.
        for m in &mut self.modes {
            let keys = m.home().map_or(0, |p| p.keys.len());
            if m.approval_slots
                .as_ref()
                .is_some_and(|s| !s.valid_for(keys))
            {
                tracing::warn!(
                    "mode {}: approval keys must be four different keys on the deck; ignoring them",
                    m.id
                );
                m.approval_slots = None;
            }
        }
        self
    }

    /// Turns the old flat Pause all key (`client_event` client `nova`, name `pause_all`)
    /// into the two-state key that follows `novaguard.paused`, and gives a key already
    /// migrated the values it lacks. Each state's action carries the state it shows (0
    /// "Pause all", 1 "Paused"), so NovaGuard can tell a stale label from a real press.
    /// Touches nothing else, and returns whether anything changed so the caller saves only
    /// then.
    pub fn migrate_pause_key(&mut self) -> bool {
        use crate::modes::KeyState;
        let is_old = |a: &Action| {
            matches!(a, Action::ClientEvent { client, name, value: _ }
                if client == "nova" && name == "pause_all")
        };
        let pause = |value| Action::ClientEvent {
            client: "novaguard".into(),
            name: "pause_all".into(),
            value,
        };
        let shows = |state: u8| Some(serde_json::json!(state));
        let mut changed = false;
        let pages = self
            .pages
            .iter_mut()
            .chain(self.modes.iter_mut().flat_map(|m| m.pages.iter_mut()));
        for key in pages.flat_map(|p| p.keys.iter_mut().flatten()) {
            // Only a key whose look follows the feed: otherwise a value says nothing about
            // the pause.
            if key.follow.as_deref() == Some("novaguard.paused")
                && let Some(states) = key.states.as_mut()
            {
                for (state, s) in (0u8..).zip(states.iter_mut()) {
                    if let Action::ClientEvent {
                        client,
                        name,
                        value,
                    } = &mut s.action
                        && client == "novaguard"
                        && name == "pause_all"
                        && value.is_none()
                    {
                        *value = shows(state);
                        changed = true;
                    }
                }
            }
            if !is_old(&key.action) || key.states.is_some() {
                continue;
            }
            key.states = Some([
                KeyState {
                    label: "Pause all".into(),
                    color: Some("#3f3f46".into()),
                    icon: Some("lucide/pause.svg".into()),
                    face: Default::default(),
                    action: pause(shows(0)),
                },
                KeyState {
                    label: "Paused".into(),
                    color: Some("#d97706".into()),
                    icon: Some("lucide/play.svg".into()),
                    face: Default::default(),
                    action: pause(shows(1)),
                },
            ]);
            key.follow = Some("novaguard.paused".into());
            key.action = pause(None);
            changed = true;
        }
        changed
    }

    /// A page by name: the top-level pages first (a config not yet normalized), then the
    /// start mode's, then any mode's.
    pub fn page(&self, name: &str) -> Option<&Page> {
        self.pages
            .iter()
            .find(|p| p.name == name)
            .or_else(|| self.mode(&self.start_mode).and_then(|m| m.page(name)))
            .or_else(|| self.modes.iter().find_map(|m| m.page(name)))
    }

    pub fn mode(&self, id: &str) -> Option<&Mode> {
        self.modes.iter().find(|m| m.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slotted(slots: crate::modes::ApprovalSlots, keys: usize) -> Config {
        Config {
            modes: vec![Mode {
                id: "dev".into(),
                pages: vec![Page {
                    name: "p".into(),
                    keys: vec![None; keys],
                    dials: Vec::new(),
                }],
                approval_slots: Some(slots),
                ..Mode::default()
            }],
            start_mode: "dev".into(),
            ..Config::default()
        }
        .normalized()
    }

    fn slots(
        approvals: usize,
        details: usize,
        approve: usize,
        deny: usize,
    ) -> crate::modes::ApprovalSlots {
        crate::modes::ApprovalSlots {
            approvals,
            details,
            approve,
            deny,
        }
    }

    #[test]
    fn approval_slots_must_be_four_distinct_keys_on_the_deck() {
        let ok = slotted(slots(0, 1, 2, 3), 0);
        assert_eq!(ok.modes[0].approval_slots, Some(slots(0, 1, 2, 3)));
        // Details and Approve on one key: the label would say Details while holding approves.
        assert_eq!(slotted(slots(0, 6, 6, 3), 0).modes[0].approval_slots, None);
        // Past the eight keys of a Plus, with a home page no bigger.
        assert_eq!(slotted(slots(0, 1, 2, 8), 8).modes[0].approval_slots, None);
        // A home page with more keys (a bigger deck) allows them.
        assert_eq!(
            slotted(slots(0, 1, 2, 11), 15).modes[0].approval_slots,
            Some(slots(0, 1, 2, 11))
        );
    }

    #[test]
    fn firmware_check_is_on_by_default_and_old_configs_get_it() {
        assert!(Config::default().firmware_check);
        let old: Config = serde_json::from_str(r#"{"deck_control":true}"#).unwrap();
        assert!(old.firmware_check);
        let off: Config = serde_json::from_str(r#"{"firmware_check":false}"#).unwrap();
        assert!(!off.firmware_check);
    }

    #[test]
    fn missing_file_gives_defaults_and_save_round_trips() {
        let dir = std::env::temp_dir().join(format!("novagear-cfg-{}", std::process::id()));
        let path = dir.join("config.json");
        let _ = std::fs::remove_dir_all(&dir);
        let c = Config::load(&path).unwrap();
        assert_eq!(c, Config::default());
        assert!(
            !c.deck_control,
            "the deck stays read-only until the user turns control on"
        );
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), c);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_pre_pages_layout_becomes_the_main_page() {
        let old = r#"{"keys":[{"label":"A","action":{"do":"light_toggle"}}],"dials":[]}"#;
        let c = serde_json::from_str::<Config>(old).unwrap().normalized();
        assert!(c.keys.is_empty());
        assert_eq!(c.start_page, "Main");
        let main = c.page("Main").unwrap();
        assert_eq!(main.keys.len(), 1);
        assert_eq!(main.keys[0].as_ref().unwrap().label, "A");
        assert!(
            serde_json::to_value(&c).unwrap()["keys"].is_null(),
            "saved without the old top-level layout"
        );
    }

    #[test]
    fn a_missing_start_page_falls_back_to_the_first() {
        let c = Config {
            start_page: "Gone".into(),
            ..Config::default()
        }
        .normalized();
        assert_eq!(c.start_page, "Main");
    }

    #[test]
    fn partial_file_fills_in_defaults() {
        let c: Config = serde_json::from_str(r#"{"deck_control":true}"#).unwrap();
        assert!(c.deck_control);
        assert_eq!(c.modes, Config::default().modes);
        assert_eq!(c.dpi_stages, vec![800, 1600, 3200]);
        assert_eq!(c.prompter.speed, 60);
    }

    fn page_named(name: &str) -> serde_json::Value {
        serde_json::json!({"name": name, "keys": [], "dials": []})
    }

    #[test]
    fn old_pages_config_becomes_one_main_mode() {
        let old = serde_json::json!({
            "pages": [page_named("Main"), page_named("Lights")],
            "start_page": "Lights"
        });
        let c = serde_json::from_value::<Config>(old).unwrap().normalized();
        assert_eq!(c.modes.len(), 1);
        assert_eq!(c.modes[0].id, "main");
        assert_eq!(c.modes[0].pages.len(), 2);
        assert_eq!(c.start_mode, "main");
        assert!(c.pages.is_empty(), "top-level pages move into the mode");
        // Page order is kept, and start_page still names the page the deck opens on.
        assert_eq!(c.modes[0].pages[0].name, "Main");
        assert_eq!(c.modes[0].pages[1].name, "Lights");
        assert_eq!(c.start_page, "Lights");
        assert!(c.mode("main").unwrap().page(&c.start_page).is_some());
        assert!(c.page("Lights").is_some());
        // Normalizing again changes nothing.
        assert_eq!(c.clone().normalized(), c);
    }

    #[test]
    fn modes_keep_their_pages_and_a_bad_start_mode_falls_back() {
        let c: Config = serde_json::from_value(serde_json::json!({
            "modes": [
                {"id": "a", "name": "A", "pages": [page_named("One")]},
                {"id": "b", "name": "B", "pages": [page_named("Two")]}
            ],
            "start_mode": "gone",
            "start_page": "Nope"
        }))
        .unwrap();
        let c = c.normalized();
        assert_eq!(c.modes.len(), 2);
        assert_eq!(c.start_mode, "a");
        assert_eq!(c.start_page, "One");
        assert_eq!(c.mode("b").unwrap().home().unwrap().name, "Two");
        assert!(c.mode("zzz").is_none());
    }

    #[test]
    fn top_level_pages_next_to_modes_replace_the_start_modes_pages() {
        let c: Config = serde_json::from_value(serde_json::json!({
            "modes": [
                {"id": "main", "name": "Main", "pages": [page_named("Old")]},
                {"id": "dev", "name": "Dev", "pages": [page_named("Dev1")]}
            ],
            "start_mode": "main",
            "pages": [page_named("New1"), page_named("New2")],
            "start_page": "New2"
        }))
        .unwrap();
        let c = c.normalized();
        assert!(c.pages.is_empty());
        assert_eq!(c.modes.len(), 2, "other modes are kept");
        assert_eq!(c.mode("dev").unwrap().pages[0].name, "Dev1");
        let names: Vec<_> = c
            .mode("main")
            .unwrap()
            .pages
            .iter()
            .map(|p| &p.name)
            .collect();
        assert_eq!(names, ["New1", "New2"]);
        assert_eq!(c.start_page, "New2");
        assert_eq!(c.clone().normalized(), c);
    }

    fn old_pause_key() -> serde_json::Value {
        serde_json::json!({"label": "Pause all", "color": "#3f3f46", "icon": "lucide/pause.svg",
            "action": {"do": "client_event", "client": "nova", "name": "pause_all"}})
    }

    fn other_nova_key() -> serde_json::Value {
        serde_json::json!({"label": "Other", "color": "#111111",
            "action": {"do": "client_event", "client": "nova", "name": "something_else"}})
    }

    fn pause_config_file(dir: &Path) -> PathBuf {
        let path = dir.join("config.json");
        std::fs::create_dir_all(dir).unwrap();
        let cfg = serde_json::json!({
            "modes": [{"id": "dev", "name": "Dev", "pages": [
                {"name": "Home", "keys": [other_nova_key(), null, old_pause_key()]},
                {"name": "Two", "keys": [old_pause_key()]}
            ]}],
            "start_mode": "dev",
            "start_page": "Home"
        });
        std::fs::write(&path, serde_json::to_vec_pretty(&cfg).unwrap()).unwrap();
        path
    }

    /// The action a Pause all state sends: the value is the state the key is showing.
    fn pause_showing(value: u8) -> Action {
        Action::ClientEvent {
            client: "novaguard".into(),
            name: "pause_all".into(),
            value: Some(serde_json::json!(value)),
        }
    }

    /// A key as the first migration left it: two states, following the feed, no values.
    fn migrated_pause_key_without_values() -> serde_json::Value {
        let send =
            serde_json::json!({"do": "client_event", "client": "novaguard", "name": "pause_all"});
        serde_json::json!({"label": "Pause all", "color": "#3f3f46", "follow": "novaguard.paused",
            "states": [
                {"label": "Hold it all", "color": "#112233", "icon": "lucide/pause.svg", "action": send},
                {"label": "Held", "color": "#445566", "icon": "lucide/play.svg",
                 "action": {"do": "client_event", "client": "novaguard", "name": "pause_all", "value": null}}
            ],
            "action": send})
    }

    fn config_with_keys(keys: Vec<serde_json::Value>) -> Config {
        serde_json::from_value(serde_json::json!({
            "modes": [{"id": "dev", "name": "Dev", "pages": [{"name": "Home", "keys": keys}]}],
            "start_mode": "dev",
            "start_page": "Home"
        }))
        .unwrap()
    }

    #[test]
    fn pause_migration_adds_values_to_a_key_already_migrated() {
        let mut c = config_with_keys(vec![migrated_pause_key_without_values()]);
        assert!(c.migrate_pause_key());
        let k = c.modes[0].pages[0].keys[0].as_ref().unwrap();
        let st = k.states.as_ref().unwrap();
        assert_eq!(st[0].action, pause_showing(0));
        assert_eq!(st[1].action, pause_showing(1));
        // The person's own look stays.
        assert_eq!(st[0].label, "Hold it all");
        assert_eq!(st[1].color.as_deref(), Some("#445566"));
        assert!(!c.migrate_pause_key(), "a second pass changes nothing");
    }

    #[test]
    fn pause_migration_saves_added_values_once() {
        let dir = std::env::temp_dir().join(format!("novagear-pause3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let cfg = config_with_keys(vec![migrated_pause_key_without_values()]);
        std::fs::write(&path, serde_json::to_vec_pretty(&cfg).unwrap()).unwrap();
        let first = Config::load(&path).unwrap();
        let st = first.modes[0].pages[0].keys[0].as_ref().unwrap();
        assert_eq!(st.states.as_ref().unwrap()[1].action, pause_showing(1));
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), first);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pause_migration_leaves_other_keys_alone() {
        // No follow: its look is a press count, so a value would say nothing about the pause.
        let mut unfollowed = migrated_pause_key_without_values();
        unfollowed.as_object_mut().unwrap().remove("follow");
        // Another client's two-state key on the same feed, and a value someone chose.
        let other = serde_json::json!({"label": "F", "color": "#000000", "follow": "novaguard.paused",
            "states": [
                {"label": "a", "action": {"do": "client_event", "client": "nova", "name": "focus"}},
                {"label": "b", "action": {"do": "client_event", "client": "novaguard", "name": "other"}}
            ],
            "action": {"do": "client_event", "client": "nova", "name": "focus"}});
        let mut c = config_with_keys(vec![unfollowed, other, other_nova_key()]);
        let before = c.clone();
        assert!(!c.migrate_pause_key());
        assert_eq!(c, before);
    }

    #[test]
    fn config_load_moves_old_pause_key() {
        let dir = std::env::temp_dir().join(format!("novagear-pause-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = pause_config_file(&dir);
        let c = Config::load(&path).unwrap();
        let pause = Action::ClientEvent {
            client: "novaguard".into(),
            name: "pause_all".into(),
            value: None,
        };
        let pages = &c.mode("dev").unwrap().pages;
        for k in [&pages[0].keys[2], &pages[1].keys[0]] {
            let k = k.as_ref().unwrap();
            assert_eq!(k.follow.as_deref(), Some("novaguard.paused"));
            assert_eq!(k.action, pause);
            let st = k.states.as_ref().unwrap();
            assert_eq!(st[0].label, "Pause all");
            assert_eq!(st[0].icon.as_deref(), Some("lucide/pause.svg"));
            assert_eq!(st[1].label, "Paused");
            assert_eq!(st[1].icon.as_deref(), Some("lucide/play.svg"));
            assert_eq!(st[0].action, pause_showing(0));
            assert_eq!(st[1].action, pause_showing(1));
        }
        let other: KeyBinding = serde_json::from_value(other_nova_key()).unwrap();
        assert_eq!(pages[0].keys[0].as_ref(), Some(&other));
        assert!(pages[0].keys[1].is_none());
        // The migrated config was saved.
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("novaguard.paused"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn config_load_pause_migration_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("novagear-pause2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = pause_config_file(&dir);
        let first = Config::load(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        let second = Config::load(&path).unwrap();
        assert_eq!(first, second);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), mtime);
        let mut again = second.clone();
        assert!(!again.migrate_pause_key());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
