//! Modes on the Stream Deck: which mode and page show, the mode picker, knob and strip
//! behaviours, and the feed-driven redraw. Drawing itself lives in `deck.rs`.

use crate::approvals::Pop;
use crate::deck::Images;
use crate::macos_events::{self, OsEvent};
use crate::obs::ObsEvent;
use crate::sleep::Verdict;
use crate::{Agent, KeySince, deck, lock};
use anyhow::{Result, bail};
use novagear_core::config::Config;
use novagear_core::modes::{ApprovalSlots, Commit, Mode, PanelKind, RuleWhen, StripPanel};
use novagear_core::{Action, DialBinding, KeyBinding, Page};
use novagear_drivers::streamdeck::{Command, DIALS, Input, KEYS, STRIP_W};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::time::Instant;

/// How long the picker, an alternate knob role and a choice preview wait for input.
const IDLE: Duration = Duration::from_secs(5);
/// How long after a change `state.json` is written, so a burst of changes is one write.
const SAVE_DEBOUNCE: Duration = Duration::from_secs(2);
/// A panel or key redraws at most this often for feed changes.
const PANEL_INTERVAL: Duration = Duration::from_secs(1);
/// Meters are the one exception: levels move fast.
const METER_INTERVAL: Duration = Duration::from_millis(100);
/// GIFs play at most 15 frames a second.
const MIN_FRAME: Duration = Duration::from_millis(67);
const SEG_W: u16 = (STRIP_W as usize / DIALS) as u16;

/// Where the deck is: the mode and page showing, and how it got there.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModeState {
    pub mode: String,
    pub page: String,
    /// Pages to go back to, inside the current mode.
    pub back: Vec<String>,
    /// A mode picked by hand while a rule holds; the rule does not override it.
    pub manual: Option<String>,
    /// The rule index and the mode it switched to, while one holds.
    pub rule_active: Option<(usize, String)>,
    /// The mode to go back to when the rules let go: where the deck was before the first.
    pub rule_prev: Option<String>,
    /// The page the deck was on in `rule_prev`, saved with it.
    pub rule_prev_page: String,
}

/// What the mode rules look at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    pub obs_recording: bool,
    pub obs_streaming: bool,
    /// The bundle id of the app in front, once known.
    pub frontmost: Option<String>,
}

fn holds(when: &RuleWhen, facts: &Facts) -> bool {
    match when {
        RuleWhen::ObsRecording => facts.obs_recording,
        RuleWhen::ObsStreaming => facts.obs_streaming,
        RuleWhen::AppFrontmost { bundle_id } => facts.frontmost.as_deref() == Some(bundle_id),
    }
}

/// Rules naming a mode that is not there, logged once each per config: `set_config`
/// clears this, so a mode that breaks again later is logged again.
static MISSING_SEEN: std::sync::Mutex<BTreeSet<String>> = std::sync::Mutex::new(BTreeSet::new());

pub(crate) fn forget_missing_warnings() {
    lock(&MISSING_SEEN).clear();
}

fn warn_missing_mode(mode: &str) {
    if lock(&MISSING_SEEN).insert(mode.to_string()) {
        tracing::warn!("a mode rule names the mode {mode:?}, which does not exist; it is skipped");
    }
}

/// Moves the rules along for the facts as they are now. The first rule that holds (and
/// names a mode that exists) wins; the mode before the first rule is remembered and comes
/// back when none holds, unless a mode was picked by hand meanwhile: that one stays, and
/// the pick is spent. Returns the mode to show now, when it changes; the caller shows it.
pub fn apply_rules(cfg: &Config, st: &mut ModeState, facts: &Facts) -> Option<String> {
    let hit = cfg.mode_rules.iter().enumerate().find_map(|(i, r)| {
        if !holds(&r.when, facts) {
            return None;
        }
        if cfg.mode(&r.mode).is_none() {
            warn_missing_mode(&r.mode);
            return None;
        }
        Some((i, r.mode.clone()))
    });
    match (hit, st.rule_active.clone()) {
        (None, None) => None,
        (Some(hit), None) => {
            st.rule_prev = Some(st.mode.clone());
            st.rule_prev_page = st.page.clone();
            st.manual = None;
            let target = hit.1.clone();
            st.rule_active = Some(hit);
            (st.mode != target).then_some(target)
        }
        (Some(hit), Some(cur)) if hit == cur => None,
        (Some(hit), Some(_)) => {
            let target = hit.1.clone();
            st.rule_active = Some(hit);
            // A hand-picked mode outlasts a change of rule too.
            (st.manual.is_none() && st.mode != target).then_some(target)
        }
        (None, Some((_, was))) => {
            st.rule_active = None;
            let prev = st.rule_prev.take();
            if st.manual.take().is_some() {
                return None;
            }
            prev.filter(|p| *p != was && cfg.mode(p).is_some())
        }
    }
}

/// Whether the config needs the OS's app and display events: an `app_frontmost` rule
/// wants the first, deck sleep the second.
pub fn os_events_wanted(cfg: &Config) -> bool {
    cfg.deck_sleep_min > 0
        || cfg
            .mode_rules
            .iter()
            .any(|r| matches!(r.when, RuleWhen::AppFrontmost { .. }))
}

/// The next message from a channel that may not exist; waits forever if it does not.
async fn next<T>(rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<T>>) -> T {
    match rx {
        Some(r) => match r.recv().await {
            Some(v) => v,
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}

/// The mode id and page name to show. A missing mode falls back to `start_mode`, then the
/// first mode; a missing page to the start page (in the start mode), then the mode's first
/// page. With no modes at all both are empty.
pub fn resolve(cfg: &Config, st: &ModeState) -> (String, String) {
    let Some(mode) = cfg
        .mode(&st.mode)
        .or_else(|| cfg.mode(&cfg.start_mode))
        .or_else(|| cfg.modes.first())
    else {
        return (String::new(), String::new());
    };
    let page = mode
        .page(&st.page)
        .or_else(|| {
            (mode.id == cfg.start_mode)
                .then(|| mode.page(&cfg.start_page))
                .flatten()
        })
        .or_else(|| mode.home())
        .map(|p| p.name.clone())
        .unwrap_or_default();
    (mode.id.clone(), page)
}

pub fn state_path(cfg_dir: &Path) -> PathBuf {
    cfg_dir.join("state.json")
}

#[derive(Serialize, Deserialize)]
struct Saved {
    mode: String,
    page: String,
}

pub fn load_state(cfg_dir: &Path) -> Option<(String, String)> {
    let text = std::fs::read_to_string(state_path(cfg_dir)).ok()?;
    let s: Saved = serde_json::from_str(&text).ok()?;
    Some((s.mode, s.page))
}

pub(crate) fn save_state(cfg_dir: &Path, mode: &str, page: &str) {
    let path = state_path(cfg_dir);
    let tmp = path.with_extension("json.tmp");
    let body = json!({ "mode": mode, "page": page }).to_string();
    if let Err(e) = std::fs::write(&tmp, body).and_then(|_| std::fs::rename(&tmp, &path)) {
        tracing::warn!("saving the deck's mode: {e}");
    }
}

/// What a knob does when its panel names nothing. A meter has mic volume and mute, a value
/// panel's press runs the panel's press; a choice panel moves its own highlight instead.
fn default_dial(p: &StripPanel) -> Option<DialBinding> {
    let bind = |turn, press| DialBinding {
        label: p.label.clone(),
        turn,
        turn_left: None,
        turn_right: None,
        press,
        alt_turn: None,
    };
    match p.kind {
        PanelKind::Meter => Some(bind(
            Some(Action::MicVolumeStep { step: 2 }),
            Some(Action::MicMuteToggle),
        )),
        PanelKind::Value => p.press.clone().map(|a| bind(None, Some(a))),
        _ => None,
    }
}

fn panel_at(mode: Option<&Mode>, i: usize) -> Option<&StripPanel> {
    mode.and_then(|m| m.strip.get(i)).and_then(Option::as_ref)
}

/// The dial's binding: its panel's, the default for the panel's kind, or the page's own
/// (older configs).
fn dial_for(mode: Option<&Mode>, page: &Page, i: usize) -> Option<DialBinding> {
    panel_at(mode, i)
        .and_then(|p| p.dial.clone().or_else(|| default_dial(p)))
        .or_else(|| page.dials.get(i).cloned().flatten())
}

/// A choice panel the dial steers itself: it has options and no dial of its own.
fn steered_choice(p: Option<&StripPanel>) -> Option<&StripPanel> {
    p.filter(|p| p.kind == PanelKind::Choice && !p.options.is_empty() && p.dial.is_none())
}

/// A two-state key's own state (0 when it has none recorded).
fn local_state(ui: &DeckUi, mode: &str, page: &str, i: usize) -> u8 {
    ui.two_state
        .get(&(mode.to_string(), page.to_string(), i))
        .copied()
        .unwrap_or(0)
}

fn zone_of(x: u16) -> usize {
    (usize::from(x) / usize::from(SEG_W)).min(DIALS - 1)
}

/// One drawn key or panel: what it read when last drawn, so a redraw happens only when
/// something it reads changed.
struct Elem {
    reads: Vec<String>,
    sig: Vec<Option<deck::Seen>>,
    /// Whether the history of the feeds it reads is drawn (a sparkline).
    history: bool,
    last: Instant,
    interval: Duration,
    /// The frame period of an animated image on this key.
    anim: Option<Duration>,
}

/// How often a held key redraws its filling bar.
const HOLD_TICK: Duration = Duration::from_millis(100);
/// A hold timer that wakes later than this after it asked to was stalled behind other work,
/// so it does not complete the hold on that wake (see `complete_hold`).
const LATE_WAKE: Duration = Duration::from_millis(150);

/// A key held down toward a hold-to-fire action. Only state: the task that waits for it
/// lives in `start_hold`. Reusable by anything that fires after a hold (G2 task 9's Approve).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Hold {
    /// Tells this hold from a later one on the same key, so a stale task cannot cancel it.
    id: u64,
    started: Instant,
    needs: Duration,
}

impl Hold {
    pub(crate) fn start(id: u64, needs: Duration, now: Instant) -> Hold {
        Hold {
            id,
            started: now,
            needs,
        }
    }

    pub(crate) fn deadline(&self) -> Instant {
        self.started + self.needs
    }

    /// How full the bar is, 0 to 1.
    pub(crate) fn progress(&self, now: Instant) -> f32 {
        if self.needs.is_zero() {
            return 1.0;
        }
        (now.saturating_duration_since(self.started).as_secs_f32() / self.needs.as_secs_f32())
            .min(1.0)
    }

    /// Whether the key went down before `t`; lets an action refuse a hold that began too
    /// recently (a guard against a press that was already down when the screen changed).
    /// The Approve hold asks it against the moment the pop-up appeared plus 300 ms.
    pub(crate) fn started_before(&self, t: Instant) -> bool {
        self.started < t
    }
}

/// What runs when a hold completes.
pub(crate) type HoldDone =
    Box<dyn FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send>;
/// Whether a hold is still wanted; asked at every tick and again just before it completes
/// (the deadline task and a release after the deadline both go through that check).
pub(crate) type HoldValid = Arc<dyn Fn() -> bool + Send + Sync>;

/// What a key draws besides its binding.
#[derive(Clone, Copy, Default)]
struct KeyExtra {
    held: Option<f32>,
    state: u8,
    pop: Pop,
}

fn slots_of(mode: Option<&Mode>) -> Option<&ApprovalSlots> {
    mode.and_then(|m| m.approval_slots.as_ref())
}

/// A two-state key's place: mode id, page name and key index.
type KeySlot = (String, String, usize);

/// Deck state that is not "where we are": the picker, knob roles and what is drawn.
#[derive(Default)]
pub(crate) struct DeckUi {
    /// The picker's page of modes, while open.
    pub(crate) picker: Option<usize>,
    picker_gen: u64,
    /// Per dial: when it last did something, while its alternate role is on.
    alt: [Option<Instant>; DIALS],
    /// Per dial: the choice a `commit: press` panel is previewing.
    highlight: [Option<usize>; DIALS],
    keys: Vec<Option<Elem>>,
    panels: Vec<Option<Elem>>,
    frame: usize,
    next_frame: Option<Instant>,
    /// Per key: the hold in progress.
    holds: [Option<Hold>; KEYS],
    hold_seq: u64,
    /// Per hold: what runs when it completes.
    hold_done: [Option<HoldDone>; KEYS],
    /// Per hold: the check that it is still wanted, kept so completing re-asks it.
    hold_valid: [Option<HoldValid>; KEYS],
    /// The approval pop-up (`approvals.rs`).
    pub(crate) popup: crate::approvals::Popup,
    /// Per key: 0 idle, 1 its multi-action is running, 2 running and a press was noted.
    running: [u8; KEYS],
    /// The same per dial, which shares it with its strip segment.
    dial_running: [u8; DIALS],
    /// Which look each two-state key without a `follow` feed shows (0 or 1).
    two_state: BTreeMap<KeySlot, u8>,
}

/// Where a multi-action was started: a key, or a dial (and the strip segment above it).
#[derive(Debug, Clone, Copy)]
enum RunSlot {
    Key(usize),
    Dial(usize),
}

impl DeckUi {
    fn running_mut(&mut self, s: RunSlot) -> Option<&mut u8> {
        match s {
            RunSlot::Key(i) => self.running.get_mut(i),
            RunSlot::Dial(i) => self.dial_running.get_mut(i),
        }
    }

    /// Marks a multi-action as running on `s`. False when one already is: the press is
    /// ignored, and said so once per run.
    fn claim_run(&mut self, s: RunSlot) -> bool {
        let Some(r) = self.running_mut(s) else {
            return false;
        };
        match *r {
            0 => {
                *r = 1;
                true
            }
            n => {
                if n == 1 {
                    *r = 2;
                    tracing::info!("{s:?}: its multi-action is still running; press ignored");
                }
                false
            }
        }
    }

    fn end_run(&mut self, s: RunSlot) {
        if let Some(r) = self.running_mut(s) {
            *r = 0;
        }
    }

    pub(crate) fn cancel_holds(&mut self) {
        self.holds = Default::default();
        self.hold_done = Default::default();
        self.hold_valid = Default::default();
    }

    /// Whether the hold on `index` exists and began before `t`.
    pub(crate) fn hold_began_before(&self, index: usize, t: Instant) -> bool {
        self.holds
            .get(index)
            .copied()
            .flatten()
            .is_some_and(|h| h.started_before(t))
    }

    pub(crate) fn holding(&self, index: usize) -> bool {
        self.holds.get(index).is_some_and(Option::is_some)
    }

    /// Forgets the state of keys that are no longer two-state keys in `cfg`.
    pub(crate) fn prune_states(&mut self, cfg: &Config) {
        self.two_state.retain(|(mode, page, i), _| {
            cfg.mode(mode)
                .and_then(|m| m.page(page))
                .and_then(|p| p.keys.get(*i))
                .and_then(Option::as_ref)
                .is_some_and(|k| k.states.is_some() && k.follow.is_none())
        });
    }

    pub(crate) fn reset_knobs(&mut self) {
        self.alt = Default::default();
        self.highlight = Default::default();
    }
}

fn earliest(a: Option<Instant>, b: Instant) -> Option<Instant> {
    Some(a.map_or(b, |a| a.min(b)))
}

fn earliest_opt(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    b.map_or(a, |b| earliest(a, b))
}

impl Agent {
    // ---- where we are ----

    pub fn current_mode(&self) -> String {
        lock(&self.nav).mode.clone()
    }

    /// `/status`: the current mode and the list to pick from.
    pub fn mode_status(&self) -> serde_json::Value {
        let cfg = self.config_arc();
        let id = self.current_mode();
        let mode = cfg
            .mode(&id)
            .map(|m| json!({ "id": m.id, "name": m.name, "accent": m.accent }));
        let modes: Vec<_> = cfg
            .modes
            .iter()
            .map(|m| json!({ "id": m.id, "name": m.name }))
            .collect();
        json!({ "mode": mode, "modes": modes })
    }

    /// The mode and page showing now, borrowed from `cfg` (an owned page only when the
    /// current one no longer exists).
    pub(crate) fn view<'a>(
        &self,
        cfg: &'a Config,
    ) -> (Option<&'a Mode>, std::borrow::Cow<'a, Page>) {
        let st = lock(&self.nav).clone();
        let (mode_id, page_name) = resolve(cfg, &st);
        let mode = cfg.mode(&mode_id);
        // Borrowed from the config: copying a mode copies every page it has.
        let page = match mode.and_then(|m| m.page(&page_name)) {
            Some(p) => std::borrow::Cow::Borrowed(p),
            None => std::borrow::Cow::Owned(Page {
                name: page_name,
                keys: Vec::new(),
                dials: Vec::new(),
            }),
        };
        (mode, page)
    }

    /// Switches mode. `by_hand` marks a pick made while a rule holds, so the rule does not
    /// switch it straight back.
    pub fn show_mode(&self, id: &str, by_hand: bool) -> Result<()> {
        let cfg = self.config_arc();
        if cfg.mode(id).is_none() {
            bail!("there's no mode called {id:?}");
        }
        let (mode, page) = {
            let mut nav = lock(&self.nav);
            let unchanged = nav.mode == id;
            if by_hand && nav.rule_active.is_some() {
                nav.manual = Some(id.to_string());
            }
            if !unchanged {
                nav.mode = id.to_string();
                nav.page = String::new();
                nav.back.clear();
                let (m, p) = resolve(&cfg, &nav);
                nav.mode = m;
                nav.page = p;
            }
            (nav.mode.clone(), nav.page.clone())
        };
        {
            let mut ui = lock(&self.ui);
            ui.picker = None;
            ui.picker_gen += 1;
            ui.reset_knobs();
            ui.cancel_holds();
        }
        self.save_soon(&mode, &page);
        self.emit("mode", json!({ "mode": mode, "page": page }));
        self.redraw_deck();
        Ok(())
    }

    /// Writes `state.json` after `SAVE_DEBOUNCE` with no further change.
    pub(crate) fn save_soon(&self, mode: &str, page: &str) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        // While a rule holds (and nothing was picked by hand), what survives a restart is
        // the user's own mode and page, not the rule's: the rules then apply again on
        // their own. Every save path comes through here.
        let (mode, page) = {
            let nav = lock(&self.nav);
            match (&nav.rule_active, &nav.manual, &nav.rule_prev) {
                (Some(_), None, Some(prev)) => (prev.clone(), nav.rule_prev_page.clone()),
                _ => (mode.to_string(), page.to_string()),
            }
        };
        let gen_now = self.save_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let gen_cell = self.save_gen.clone();
        let dir = self.state_dir();
        *lock(&self.state_pending) = Some((mode.clone(), page.clone()));
        let pending = self.state_pending.clone();
        let deadline = Instant::now() + SAVE_DEBOUNCE;
        rt.spawn(async move {
            tokio::time::sleep_until(deadline).await;
            if gen_cell.load(Ordering::SeqCst) == gen_now {
                save_state(&dir, &mode, &page);
                *lock(&pending) = None;
            }
        });
    }

    /// Cycles to the next mode in config order.
    fn mode_next(&self) -> Result<()> {
        let cfg = self.config_arc();
        if cfg.modes.is_empty() {
            bail!("there are no modes");
        }
        let at = cfg
            .modes
            .iter()
            .position(|m| m.id == self.current_mode())
            .unwrap_or(0);
        let next = &cfg.modes[(at + 1) % cfg.modes.len()].id;
        self.show_mode(next, true)
    }

    pub(crate) fn run_mode_action(self: &Arc<Self>, action: &Action) -> Result<()> {
        match action {
            Action::ModeGo { mode } => self.show_mode(mode, true),
            Action::ModeNext => self.mode_next(),
            Action::ModePicker => {
                if self.config_arc().mode_key == novagear_core::modes::ModeKey::Cycle {
                    self.mode_next()
                } else {
                    self.open_picker();
                    Ok(())
                }
            }
            _ => bail!("not a mode action"),
        }
    }

    // ---- rules and sleep ----

    pub(crate) fn asleep(&self) -> bool {
        lock(&self.sleep).asleep()
    }

    /// Starts or stops the OS observer to match the config. It runs only while an
    /// `app_frontmost` rule or deck sleep needs it.
    pub(crate) fn reconcile_os_events(&self) {
        let wanted = os_events_wanted(&self.config_arc());
        let mut slot = lock(&self.os_observer);
        match (wanted, slot.is_some()) {
            (true, false) => {
                #[cfg(test)]
                self.os_spawns.fetch_add(1, Ordering::SeqCst);
                *slot = Some(macos_events::spawn(self.os_events_tx.clone()));
            }
            (false, true) => {
                *slot = None;
                drop(slot);
                lock(&self.facts).frontmost = None;
            }
            _ => {}
        }
    }

    /// Re-reads the rules against the facts and shows the mode they ask for.
    pub(crate) fn eval_rules(&self) {
        let cfg = self.config_arc();
        let facts = lock(&self.facts).clone();
        let switch = apply_rules(&cfg, &mut lock(&self.nav), &facts);
        if let Some(mode) = switch
            && let Err(e) = self.show_mode(&mode, false)
        {
            tracing::warn!("a mode rule could not show its mode: {e:#}");
        }
    }

    pub(crate) fn obs_event(&self, ev: ObsEvent) {
        {
            let mut facts = lock(&self.facts);
            match ev {
                ObsEvent::Recording(on) => facts.obs_recording = on,
                ObsEvent::Streaming(on) => facts.obs_streaming = on,
                // A lost connection already said recording and streaming stopped.
                ObsEvent::Connected(_) => return,
            }
        }
        self.eval_rules();
    }

    pub(crate) fn os_event(&self, ev: OsEvent) {
        match ev {
            OsEvent::Frontmost(id) => {
                lock(&self.facts).frontmost = Some(id);
                self.eval_rules();
            }
            OsEvent::DisplaysAsleep => self.fall_asleep(),
            OsEvent::DisplaysAwake => {
                if self.asleep() {
                    self.wake_deck();
                }
            }
        }
    }

    /// Puts the deck to sleep if it is time.
    pub(crate) fn check_sleep(&self) {
        if lock(&self.sleep).due(Instant::now()) {
            self.fall_asleep();
        }
    }

    /// The deck goes dark (a no-op with sleep off, or already asleep).
    fn fall_asleep(&self) {
        let slept = {
            let mut s = lock(&self.sleep);
            let was = s.asleep();
            s.sleep();
            s.asleep() && !was
        };
        if !slept {
            return;
        }
        // The loop's deadline is None while asleep; it re-reads it on waking.
        self.sleep_wake.notify_one();
        {
            let mut ui = lock(&self.ui);
            ui.cancel_holds();
            ui.picker = None;
            ui.picker_gen += 1;
            ui.reset_knobs();
        }
        self.redraw_deck();
    }

    pub(crate) fn wake_deck(&self) {
        lock(&self.sleep).wake(Instant::now());
        // The idle deadline is back: the events loop must re-read it.
        self.sleep_wake.notify_one();
        // The pop-up may show again, with a fresh guard.
        lock(&self.ui).popup.dismissed = false;
        self.redraw_deck();
    }

    /// What the sleeping deck shows: the sleep image on every key and a black strip, or
    /// black with the backlight off.
    fn draw_sleep(&self, cfg: &Config) {
        let image = cfg
            .deck_sleep_image
            .as_deref()
            .and_then(|n| self.images.get(n))
            .filter(|i| !i.frames.is_empty());
        let (key, strip) = match (
            deck::sleep_key_jpeg(image.as_deref()),
            deck::sleep_strip_jpeg(),
        ) {
            (Ok(k), Ok(s)) => (k, s),
            _ => {
                tracing::warn!("drawing the sleeping deck failed");
                return;
            }
        };
        let mut cmds = Vec::new();
        if image.is_some() {
            cmds.push(Command::Brightness(cfg.deck_brightness));
        }
        cmds.extend((0..KEYS).map(|index| Command::KeyImage {
            index,
            jpeg: key.clone(),
        }));
        cmds.push(Command::StripImage {
            x: 0,
            width: STRIP_W as u16,
            jpeg: strip,
        });
        if image.is_none() {
            cmds.push(Command::Brightness(0));
        }
        self.send_to_deck(cmds);
    }

    /// Reacts to what OBS and the OS report, and to the idle timer, for the life of the
    /// agent. It does nothing until something arrives or the deck is due to sleep.
    pub(crate) async fn events_loop(self: Arc<Self>) {
        let mut obs = self.take_obs_events();
        let mut os = lock(&self.os_events_rx).take();
        loop {
            let deadline = lock(&self.sleep).deadline();
            tokio::select! {
                e = next(&mut obs) => self.obs_event(e),
                e = next(&mut os) => self.os_event(e),
                _ = self.sleep_wake.notified() => {}
                _ = async {
                    match deadline {
                        Some(t) => tokio::time::sleep_until(t).await,
                        None => std::future::pending().await,
                    }
                } => self.check_sleep(),
            }
        }
    }

    // ---- picker ----

    pub fn open_picker(self: &Arc<Self>) {
        let cfg = self.config_arc();
        let at = cfg
            .modes
            .iter()
            .position(|m| m.id == self.current_mode())
            .unwrap_or(0);
        lock(&self.ui).picker = Some(at / DIALS);
        self.arm_picker();
        // An open picker hides the approval pop-up, which dims the keys too.
        if self.sync_popup() {
            self.redraw_deck();
        } else {
            self.redraw_strip_now();
        }
    }

    /// The deck went away (unplugged, or it closed): a hold on it can't finish, and the
    /// pop-up shows again, with a fresh guard, when it is back.
    pub(crate) fn on_deck_closed(&self) {
        {
            let mut ui = lock(&self.ui);
            ui.cancel_holds();
            ui.popup.dismissed = false;
        }
        // A release lost with the deck must not swallow the next real one.
        lock(&self.sleep).clear_swallowed();
        self.sync_popup();
    }

    #[cfg(test)]
    pub(crate) fn picker_page(&self) -> Option<usize> {
        lock(&self.ui).picker
    }

    /// Closes the picker after `IDLE` with no input; any input arms it again.
    fn arm_picker(self: &Arc<Self>) {
        let gen_now = {
            let mut ui = lock(&self.ui);
            ui.picker_gen += 1;
            ui.picker_gen
        };
        let me = self.clone();
        // The deadline is fixed now, not when the task first runs.
        let deadline = Instant::now() + IDLE;
        tokio::spawn(async move {
            tokio::time::sleep_until(deadline).await;
            let closed = {
                let mut ui = lock(&me.ui);
                let stale = ui.picker_gen != gen_now || ui.picker.is_none();
                if !stale {
                    ui.picker = None;
                }
                !stale
            };
            if closed {
                // The pop-up comes back if requests are still waiting.
                if me.sync_popup() {
                    me.redraw_deck();
                } else {
                    me.redraw_strip_now();
                }
            }
        });
    }

    /// Handles a strip or dial input while the picker is open. Returns whether it was used;
    /// keys always keep their normal actions.
    fn picker_input(self: &Arc<Self>, cfg: &Config, input: &Input) -> bool {
        enum Step {
            Pick(usize),
            Page(usize),
            Keep,
        }
        let step = {
            let ui = lock(&self.ui);
            let Some(page) = ui.picker else {
                return false;
            };
            let pages = cfg.modes.len().div_ceil(DIALS).max(1);
            match input {
                Input::Touch { x, .. } => Step::Pick(page * DIALS + zone_of(*x)),
                Input::DialPress { index, down: true } => Step::Pick(page * DIALS + index),
                Input::DialTurn { detents, .. } => Step::Page(
                    (page as i64 + i64::from(detents.signum())).rem_euclid(pages as i64) as usize,
                ),
                Input::DialPress { .. } | Input::Swipe { .. } => Step::Keep,
                Input::Key { .. } => return false,
            }
        };
        match step {
            Step::Pick(at) => match cfg.modes.get(at) {
                Some(m) => {
                    if let Err(e) = self.show_mode(&m.id, true) {
                        tracing::warn!("picking a mode: {e:#}");
                    }
                }
                None => self.arm_picker(),
            },
            Step::Page(p) => {
                lock(&self.ui).picker = Some(p);
                self.arm_picker();
                self.redraw_strip_now();
            }
            Step::Keep => self.arm_picker(),
        }
        true
    }

    #[cfg(test)]
    pub(crate) fn two_state_count(&self) -> usize {
        lock(&self.ui).two_state.len()
    }

    // ---- input ----

    /// Handles one deck input, read by the deck's thread at `at`. Holds are timed from `at`,
    /// not from now: the input may have waited behind other work.
    pub(crate) async fn handle_input(self: &Arc<Self>, input: Input, at: Instant) {
        let cfg = self.config_arc();
        if !cfg.deck_control {
            return;
        }
        // The first input after sleep only wakes the deck, release included.
        // Not inlined in the `match`: its guard would live through `wake_deck`.
        let verdict = lock(&self.sleep).input(&input, Instant::now());
        match verdict {
            Verdict::Pass => {}
            Verdict::Swallow => return,
            Verdict::Wake => {
                self.wake_deck();
                return;
            }
        }
        if self.popup_input(&cfg, &input, at).await || self.picker_input(&cfg, &input) {
            return;
        }
        let (mode, page) = self.view(&cfg);
        // The dial the input belongs to; a strip touch counts as its segment's dial.
        let dial = match &input {
            Input::DialTurn { index, .. } | Input::DialPress { index, .. } => *index,
            Input::Touch { x, .. } => zone_of(*x),
            _ => 0,
        };
        let actions: Vec<Action> = match &input {
            Input::Key { index, down } => {
                self.key_input(mode, &page, *index, *down, at).await;
                return;
            }
            Input::DialTurn { index, detents } => {
                self.turn_actions(mode, &page, *index, i16::from(*detents))
            }
            Input::DialPress { index, down: true } => self.press_actions(mode, &page, *index),
            Input::Touch { x, .. } => {
                let zone = zone_of(*x);
                panel_at(mode, zone)
                    .and_then(|p| p.press.clone())
                    .or_else(|| {
                        page.dials
                            .get(zone)
                            .cloned()
                            .flatten()
                            .and_then(|d| d.press)
                    })
                    .into_iter()
                    .collect()
            }
            _ => Vec::new(),
        };
        // A `Sequence` can take a minute: like a key's, it runs on its own task so the deck
        // keeps taking input, and the same dial is ignored until it is done.
        if actions.iter().any(|a| matches!(a, Action::Sequence { .. })) {
            let slot = RunSlot::Dial(dial);
            if !lock(&self.ui).claim_run(slot) {
                return;
            }
            let me = self.clone();
            tokio::spawn(async move {
                me.run_all(actions).await;
                lock(&me.ui).end_run(slot);
            });
        } else {
            self.run_all(actions).await;
        }
    }

    /// Runs actions in order, stopping at the first failure, which goes to the panel.
    async fn run_all(self: &Arc<Self>, actions: Vec<Action>) {
        for a in actions {
            if let Err(e) = self.run(a).await {
                self.emit("error", json!(format!("{e:#}")));
                break;
            }
        }
    }

    /// Runs one action; a failure goes to the panel's event stream. Returns whether it worked.
    async fn run_reported(self: &Arc<Self>, action: Action) -> bool {
        match self.run(action).await {
            Ok(()) => true,
            Err(e) => {
                self.emit("error", json!(format!("{e:#}")));
                false
            }
        }
    }

    /// Which look a two-state key shows: its feed's, or its own press count.
    fn key_state(&self, slot: &KeySlot, b: &KeyBinding) -> u8 {
        match &b.follow {
            Some(f) => u8::from(deck::feed_truthy(&self.feeds, f)),
            None => lock(&self.ui).two_state.get(slot).copied().unwrap_or(0),
        }
    }

    /// A key goes down or up. Down runs the key's action (the current state's, for a
    /// two-state key) or starts its hold; up completes a hold that is already due, or
    /// cancels one that is not.
    async fn key_input(
        self: &Arc<Self>,
        mode: Option<&Mode>,
        page: &Page,
        index: usize,
        down: bool,
        at: Instant,
    ) {
        if index >= KEYS {
            return;
        }
        if !down {
            self.release_hold(index, at).await;
            return;
        }
        let Some(b) = page.keys.get(index).cloned().flatten() else {
            return;
        };
        let slot: KeySlot = (
            mode.map_or_else(String::new, |m| m.id.clone()),
            page.name.clone(),
            index,
        );
        let action = match &b.states {
            Some(s) => s[usize::from(self.key_state(&slot, &b)).min(1)]
                .action
                .clone(),
            None => b.action.clone(),
        };
        // A followed key's state comes from its feed, so there is nothing to flip.
        let flips = b.states.is_some() && b.follow.is_none();
        if b.hold_ms > 0 {
            let (me, here, then) = (self.clone(), slot.clone(), self.clone());
            self.start_hold(
                index,
                Duration::from_millis(u64::from(b.hold_ms)),
                // Still the same mode and page.
                Arc::new(move || {
                    let nav = lock(&me.nav);
                    nav.mode == here.0 && nav.page == here.1
                }),
                Box::new(move || Box::pin(async move { then.fire_key(slot, action, flips).await })),
                at,
            );
        } else {
            self.fire_key(slot, action, flips).await;
        }
    }

    /// Runs a key's action, then flips a two-state key if it ran. A `Sequence` can take a
    /// minute, so it runs on its own task and the deck keeps taking input (a release,
    /// another key); a press on a key whose sequence is still running is ignored.
    async fn fire_key(self: &Arc<Self>, slot: KeySlot, action: Action, flips: bool) {
        let me = self.clone();
        let sequence = matches!(action, Action::Sequence { .. });
        let index = slot.2;
        if sequence && !lock(&self.ui).claim_run(RunSlot::Key(index)) {
            return;
        }
        let job = async move {
            let ok = me.run_reported(action).await;
            if sequence {
                lock(&me.ui).end_run(RunSlot::Key(index));
            }
            if ok && flips {
                {
                    let mut ui = lock(&me.ui);
                    let e = ui.two_state.entry(slot.clone()).or_insert(0);
                    *e ^= 1;
                }
                me.redraw_key(index);
            }
        };
        if sequence {
            tokio::spawn(job);
        } else {
            job.await;
        }
    }

    // ---- hold to fire ----

    /// Starts a hold on slot `index` (a key), begun when the key went down at `down_at`;
    /// `done` runs once it has been held for `needs`. The key redraws with its bar every
    /// `HOLD_TICK`. Releasing early, the deck closing, a config change, or `valid` turning
    /// false cancels it (so does a mode or page change, through `cancel_holds`). Anything that
    /// fires after a hold can use this: Approve passes its own `done` and a `valid` that asks
    /// whether the pop-up still shows the same request. See `complete_hold` for how a hold
    /// is judged when the agent was too busy to watch it.
    pub(crate) fn start_hold(
        self: &Arc<Self>,
        index: usize,
        needs: Duration,
        valid: HoldValid,
        done: HoldDone,
        down_at: Instant,
    ) {
        let hold = {
            let mut ui = lock(&self.ui);
            ui.hold_seq += 1;
            let h = Hold::start(ui.hold_seq, needs, down_at);
            ui.holds[index] = Some(h);
            ui.hold_done[index] = Some(done);
            ui.hold_valid[index] = Some(valid.clone());
            h
        };
        self.redraw_key(index);
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                let now = Instant::now();
                // Past the deadline (a late start, or a late wake) the next look is a tick on.
                let wake = if now < hold.deadline() {
                    (now + HOLD_TICK).min(hold.deadline())
                } else {
                    now + HOLD_TICK
                };
                tokio::time::sleep_until(wake).await;
                let current = lock(&me.ui).holds[index].is_some_and(|h| h.id == hold.id);
                if !current {
                    return;
                }
                if !valid() || lock(&me.deck).commands.is_none() {
                    me.cancel_hold(index);
                    return;
                }
                let late = Instant::now().saturating_duration_since(wake) > LATE_WAKE;
                match me.key_since(index, hold.started) {
                    // Pressed again, or the history is incomplete: not one press.
                    KeySince::Broken => {
                        me.cancel_hold_if(index, hold.id);
                        return;
                    }
                    // The deck read the key coming up: its stamp decides, now, whatever the
                    // wake time was.
                    KeySince::Released(up) => {
                        if up >= hold.deadline() {
                            me.complete_hold(index, hold.id).await;
                        } else {
                            me.cancel_hold_if(index, hold.id);
                        }
                        return;
                    }
                    // Still down as far as the deck has said, and this wake came on time.
                    KeySince::StillDown if !late && Instant::now() >= hold.deadline() => {
                        me.complete_hold(index, hold.id).await;
                        return;
                    }
                    // Not yet due, or the wake was late: look again next tick.
                    KeySince::StillDown => {}
                }
                me.redraw_key(index);
            }
        });
    }

    /// Runs the hold's completion, once, whoever gets here first (the waiting task or a
    /// release at or after the deadline). Both come through here, so the hold's `valid`
    /// check is asked here, just before it fires: a state that changed between the deadline
    /// and the key coming up (a request cancelled, the pop-up moving on) must not complete it.
    ///
    /// Only a physical hold completes. Every time is the deck thread's stamp, taken as the
    /// report was read: the hold starts at the key-down's stamp, and a key-up stamped before
    /// `start + needs` cancels it whenever it is processed. The timer completes a hold only
    /// when the deck has not read the key coming up and the timer itself woke within
    /// `LATE_WAKE` of when it asked to: a later wake means the agent was stalled, inputs may
    /// be waiting unseen, so it looks again a tick later and the stamped key-up (if any)
    /// decides.
    ///
    /// The deck thread keeps each key's last few presses and releases, so a release is
    /// always paired with its own press: only the first release after the hold's key-down
    /// counts, and a press read after it (a tap, then a second press, whose release lands
    /// before the deadline or after) breaks the hold, whenever the timer or the input loop
    /// gets to it. History that overflowed past the key-down breaks it too. The late-wake
    /// rule applies only to the no-information case; a stamped release decides on its own
    /// time, never on the processing time.
    async fn complete_hold(self: &Arc<Self>, index: usize, id: u64) {
        let valid = {
            let ui = lock(&self.ui);
            if ui.holds[index].is_none_or(|h| h.id != id) {
                return;
            }
            ui.hold_valid[index].clone()
        };
        // Asked with no lock held: the check may read the deck's state itself.
        if valid.is_some_and(|v| !v()) {
            self.cancel_hold_if(index, id);
            return;
        }
        let done = {
            let mut ui = lock(&self.ui);
            if ui.holds[index].is_none_or(|h| h.id != id) {
                return;
            }
            ui.holds[index] = None;
            ui.hold_valid[index] = None;
            ui.hold_done[index].take()
        };
        self.redraw_key(index);
        if let Some(done) = done {
            done().await;
        }
    }

    /// The key came up at `up_at` (the deck's stamp): a hold held for its full time fires,
    /// even if this is processed late or between ticks; one released early is cancelled,
    /// however late this runs. The release must be the hold's own: the first one the deck
    /// read after the press. A later press's release says nothing about this hold.
    pub(crate) async fn release_hold(self: &Arc<Self>, index: usize, up_at: Instant) {
        let due = lock(&self.ui).holds[index].filter(|h| up_at >= h.deadline());
        let due = due.filter(|h| self.key_since(index, h.started) == KeySince::Released(up_at));
        match due {
            Some(h) => self.complete_hold(index, h.id).await,
            None => self.cancel_hold(index),
        }
    }

    pub(crate) fn cancel_hold(&self, index: usize) {
        let had = {
            let mut ui = lock(&self.ui);
            ui.hold_done[index] = None;
            ui.hold_valid[index] = None;
            ui.holds[index].take().is_some()
        };
        if had {
            self.redraw_key(index);
        }
    }

    /// Cancels the hold only if it is still the one with this id.
    fn cancel_hold_if(&self, index: usize, id: u64) {
        let same = lock(&self.ui).holds[index].is_some_and(|h| h.id == id);
        if same {
            self.cancel_hold(index);
        }
    }

    /// Redraws one key (hold progress, a state flip).
    pub(crate) fn redraw_key(&self, i: usize) {
        let cfg = self.config_arc();
        if !cfg.deck_control || lock(&self.deck).commands.is_none() {
            return;
        }
        let (mode, page) = self.view(&cfg);
        let cmd = {
            let ui = lock(&self.ui);
            let b = page.keys.get(i).and_then(Option::as_ref);
            let mode_id = mode.map_or("", |m| m.id.as_str());
            let extra = KeyExtra {
                held: ui.holds[i].map(|h| h.progress(Instant::now())),
                state: local_state(&ui, mode_id, &page.name, i),
                pop: ui.popup.pop_for(slots_of(mode), i),
            };
            self.key_cmd(mode, b, i, ui.frame, extra)
        };
        self.deck_send(cmd.into_iter().collect());
    }

    /// The choice a preview points at: the dial's own highlight, else the feed's current.
    fn choice_start(&self, i: usize, p: &StripPanel) -> Option<usize> {
        lock(&self.ui).highlight[i].or_else(|| deck::choice_current(p, &self.feeds))
    }

    fn turn_actions(
        &self,
        mode: Option<&Mode>,
        page: &Page,
        i: usize,
        detents: i16,
    ) -> Vec<Action> {
        if i >= DIALS {
            return Vec::new();
        }
        if let Some(p) = steered_choice(panel_at(mode, i)) {
            let last = p.options.len() as i32 - 1;
            let start = self.choice_start(i, p);
            let new = (start.unwrap_or(0) as i32 + i32::from(detents)).clamp(0, last) as usize;
            lock(&self.ui).highlight[i] = Some(new);
            self.redraw_segment(i);
            // Turn commits as the dial moves; press only previews.
            return if p.commit == Commit::Turn && start != Some(new) {
                vec![p.options[new].action.clone()]
            } else {
                Vec::new()
            };
        }
        let Some(d) = dial_for(mode, page, i) else {
            return Vec::new();
        };
        let now = Instant::now();
        let mut ui = lock(&self.ui);
        let alt_on = ui.alt[i].is_some_and(|t| now.duration_since(t) < IDLE);
        match (&d.alt_turn, alt_on) {
            (Some(alt), true) => {
                ui.alt[i] = Some(now);
                vec![alt.scaled(detents)]
            }
            _ => {
                ui.alt[i] = None;
                d.turn_actions(detents)
            }
        }
    }

    fn press_actions(&self, mode: Option<&Mode>, page: &Page, i: usize) -> Vec<Action> {
        if i >= DIALS {
            return Vec::new();
        }
        if let Some(p) = steered_choice(panel_at(mode, i))
            && p.commit == Commit::Press
        {
            let at = self.choice_start(i, p);
            lock(&self.ui).highlight[i] = None;
            self.redraw_segment(i);
            return at
                .and_then(|h| p.options.get(h))
                .map(|o| o.action.clone())
                .into_iter()
                .collect();
        }
        let Some(d) = dial_for(mode, page, i) else {
            return Vec::new();
        };
        if d.alt_turn.is_some() {
            // A press flips the knob's role: on after one press, off after the next.
            let now = Instant::now();
            let mut ui = lock(&self.ui);
            let on = ui.alt[i].is_some_and(|t| now.duration_since(t) < IDLE);
            ui.alt[i] = (!on).then_some(now);
        }
        d.press.into_iter().collect()
    }

    // ---- drawing ----

    fn anim_period(&self, b: &KeyBinding) -> Option<Duration> {
        let mut names: Vec<&str> = Vec::new();
        names.extend(b.icon.as_deref());
        names.extend(b.face.background_image.as_deref());
        for s in b.states.iter().flatten() {
            names.extend(s.icon.as_deref());
            names.extend(s.face.background_image.as_deref());
        }
        names
            .into_iter()
            .filter_map(|n| self.images.get(n))
            .filter(|i| i.frames.len() > 1)
            .map(|i| Duration::from_millis(u64::from(i.delay_ms)).max(MIN_FRAME))
            .min()
    }

    fn sig_of(&self, reads: &[String], history: bool) -> Vec<Option<deck::Seen>> {
        reads
            .iter()
            .map(|k| self.feeds.get(k).map(|s| deck::seen(&s, history)))
            .collect()
    }

    fn key_cmd(
        &self,
        mode: Option<&Mode>,
        b: Option<&KeyBinding>,
        i: usize,
        frame: usize,
        extra: KeyExtra,
    ) -> Option<Command> {
        let accent = mode.map_or("#2563eb", |m| m.accent.as_str());
        // A key the pop-up has borrowed shows its pop-up face; the others dim.
        let borrowed = crate::approvals::borrowed_key(extra.pop);
        let look = deck::KeyLook {
            binding: borrowed.as_ref().or(b),
            accent,
            held: extra.held,
            state: if borrowed.is_some() { 0 } else { extra.state },
            dimmed: extra.pop == Pop::Dim,
            frame,
        };
        match deck::key_jpeg(&look, &self.feeds, &*self.images) {
            Ok(jpeg) => Some(Command::KeyImage { index: i, jpeg }),
            Err(e) => {
                tracing::warn!("drawing key {i}: {e:#}");
                None
            }
        }
    }

    fn panel_cmd(
        &self,
        mode: Option<&Mode>,
        i: usize,
        highlight: Option<usize>,
    ) -> Option<Command> {
        let accent = mode.map_or("#2563eb", |m| m.accent.as_str());
        match deck::panel_jpeg(panel_at(mode, i), &self.feeds, highlight, accent) {
            Ok(jpeg) => Some(Command::StripImage {
                x: i as u16 * SEG_W,
                width: SEG_W,
                jpeg,
            }),
            Err(e) => {
                tracing::warn!("drawing strip segment {i}: {e:#}");
                None
            }
        }
    }

    /// Redraws one strip segment (a choice preview moving).
    fn redraw_segment(&self, i: usize) {
        let cfg = self.config_arc();
        if !cfg.deck_control || lock(&self.deck).commands.is_none() {
            return;
        }
        let (mode, _) = self.view(&cfg);
        let highlight = lock(&self.ui).highlight[i];
        self.deck_send(self.panel_cmd(mode, i, highlight).into_iter().collect());
    }

    fn track_key(&self, b: &KeyBinding, now: Instant) -> Elem {
        let reads = deck::feeds_read(b);
        Elem {
            sig: self.sig_of(&reads, false),
            history: false,
            reads,
            last: now,
            interval: PANEL_INTERVAL,
            anim: self.anim_period(b),
        }
    }

    fn track_panel(&self, p: &StripPanel, now: Instant) -> Elem {
        let reads = deck::panel_feeds(p);
        let history = p.kind == PanelKind::Sparkline;
        Elem {
            sig: self.sig_of(&reads, history),
            history,
            reads,
            last: now,
            interval: if p.kind == PanelKind::Meter {
                METER_INTERVAL
            } else {
                PANEL_INTERVAL
            },
            anim: None,
        }
    }

    /// The strip's commands: the picker, the mode's panels, or (a config with no panels) the
    /// page's dials drawn the old way. Also rebuilds what the panels read.
    fn strip_cmds(
        &self,
        cfg: &Config,
        mode: Option<&Mode>,
        page: &Page,
        ui: &mut DeckUi,
        now: Instant,
    ) -> Vec<Command> {
        let has_panels = mode.is_some_and(|m| m.strip.iter().any(Option::is_some));
        ui.panels = (0..DIALS)
            .map(|i| {
                panel_at(mode, i)
                    .filter(|_| has_panels)
                    .map(|p| self.track_panel(p, now))
            })
            .collect();
        // The pop-up covers the whole strip, so no panel is on screen to redraw under it.
        if ui.popup.showing.is_some() {
            if let Some(cmd) = self.popup_strip(&ui.popup) {
                ui.panels = (0..DIALS).map(|_| None).collect();
                return vec![cmd];
            }
            // It can't be drawn, so it isn't showing: no borrowed keys without the request
            // they answer.
            ui.popup.showing = None;
            ui.popup.idle_at = None;
            ui.cancel_holds();
        }
        if let Some(at) = ui.picker {
            let modes: Vec<&Mode> = cfg.modes.iter().collect();
            let current = mode.map_or("", |m| m.id.as_str());
            return match deck::picker_jpeg(&modes, current, at, &*self.images) {
                Ok(jpeg) => vec![Command::StripImage {
                    x: 0,
                    width: STRIP_W as u16,
                    jpeg,
                }],
                Err(e) => {
                    tracing::warn!("drawing the mode picker: {e:#}");
                    Vec::new()
                }
            };
        }
        if has_panels {
            (0..DIALS)
                .filter_map(|i| self.panel_cmd(mode, i, ui.highlight[i]))
                .collect()
        } else {
            self.legacy_strip(cfg, page).into_iter().collect()
        }
    }

    /// Redraws only the strip (the picker opening, paging or closing).
    // Lock order: `ui` is taken before `lights`, `prompter`, `mouse`, the feeds and the image
    // cache, never after. Callers must not hold any of those when they redraw.
    fn redraw_strip_now(&self) {
        let cfg = self.config_arc();
        if !cfg.deck_control || lock(&self.deck).commands.is_none() {
            return;
        }
        let (mode, page) = self.view(&cfg);
        let (cmds, dropped) = {
            let mut ui = lock(&self.ui);
            let was = ui.popup.showing.is_some();
            let cmds = self.strip_cmds(&cfg, mode, &page, &mut ui, Instant::now());
            (cmds, was && ui.popup.showing.is_none())
        };
        if dropped {
            // Its borrowed keys are still on screen.
            self.redraw_deck();
            return;
        }
        self.deck_send(cmds);
        self.redraw_wake.notify_one();
    }

    /// Redraws every key and the strip, if NovaGear controls the deck, and tells the feeds
    /// what is now being read.
    // Lock order: `ui` is taken before `lights`, `prompter`, `mouse`, the feeds and the image
    // cache, never after. Callers must not hold any of those when they redraw.
    pub fn redraw_deck(&self) {
        // Whatever changed (mode, picker, deck, config), the pop-up is decided first.
        self.sync_popup();
        let cfg = self.config_arc();
        if !cfg.deck_control || lock(&self.deck).commands.is_none() {
            return;
        }
        // Everything on screen is drawn below; images nothing asks for are then let go.
        self.images.begin_sweep();
        if self.asleep() {
            // Nothing is read while asleep: no feeds wanted, nothing remembered on screen.
            {
                let mut ui = lock(&self.ui);
                ui.keys.clear();
                ui.panels.clear();
            }
            self.feeds.set_wanted(BTreeSet::new());
            self.draw_sleep(&cfg);
            self.images.evict_unused();
            return;
        }
        let (mode, page) = self.view(&cfg);
        let now = Instant::now();
        let mut cmds = vec![Command::Brightness(cfg.deck_brightness)];
        {
            let mut ui = lock(&self.ui);
            let frame = ui.frame;
            let mode_id = mode.map_or("", |m| m.id.as_str());
            // The strip first: a pop-up that fails to draw must not leave its keys borrowed.
            let strip = self.strip_cmds(&cfg, mode, &page, &mut ui, now);
            let looks: Vec<KeyExtra> = (0..KEYS)
                .map(|i| KeyExtra {
                    held: ui.holds[i].map(|h| h.progress(now)),
                    state: local_state(&ui, mode_id, &page.name, i),
                    pop: ui.popup.pop_for(slots_of(mode), i),
                })
                .collect();
            ui.keys = (0..KEYS)
                .map(|i| {
                    let b = page.keys.get(i).and_then(Option::as_ref);
                    cmds.extend(self.key_cmd(mode, b, i, frame, looks[i]));
                    b.map(|b| self.track_key(b, now))
                })
                .collect();
            cmds.extend(strip);
            let wanted: BTreeSet<String> = ui
                .keys
                .iter()
                .chain(ui.panels.iter())
                .flatten()
                .flat_map(|e| e.reads.iter().cloned())
                .collect();
            self.feeds.set_wanted(wanted);
        }
        self.images.evict_unused();
        self.deck_send(cmds);
        self.redraw_wake.notify_one();
    }

    /// Redraws the strip for configs whose mode has no panels; with panels, feeds drive it.
    pub(crate) fn redraw_strip(&self) {
        let cfg = self.config_arc();
        if !cfg.deck_control || lock(&self.deck).commands.is_none() || self.asleep() {
            return;
        }
        let (mode, page) = self.view(&cfg);
        let has_panels = mode.is_some_and(|m| m.strip.iter().any(Option::is_some));
        if has_panels || lock(&self.ui).picker.is_some() {
            return;
        }
        self.deck_send(self.legacy_strip(&cfg, &page).into_iter().collect());
    }

    /// Redraws what feeds changed since it was drawn, at most once a second per key or
    /// panel (meters ten times), and steps animated keys. Returns when it next has work
    /// that is not driven by a feed.
    fn redraw_pass(&self) -> Option<Instant> {
        let cfg = self.config_arc();
        if !cfg.deck_control || lock(&self.deck).commands.is_none() || self.asleep() {
            return None;
        }
        let (mode, page) = self.view(&cfg);
        let now = Instant::now();
        let mut cmds = Vec::new();
        let mut due = None;
        {
            let mut guard = lock(&self.ui);
            // 30 s with no deck input sends the deck back to normal; the request stays.
            if guard.popup.idle_expired(now) {
                guard.popup.dismissed = true;
                guard.popup.showing = None;
                guard.popup.idle_at = None;
                guard.cancel_holds();
                drop(guard);
                self.redraw_deck();
                return None;
            }
            due = earliest_opt(
                due,
                guard
                    .popup
                    .idle_at
                    .filter(|_| guard.popup.showing.is_some()),
            );
            let DeckUi {
                keys,
                panels,
                frame,
                next_frame,
                highlight,
                picker,
                holds,
                two_state,
                popup,
                ..
            } = &mut *guard;
            let mode_id = mode.map_or("", |m| m.id.as_str());
            let mut ticked = false;
            match keys.iter().flatten().filter_map(|e| e.anim).min() {
                None => *next_frame = None,
                Some(period) => {
                    let at = *next_frame.get_or_insert(now + period);
                    if now >= at {
                        *frame = frame.wrapping_add(1);
                        ticked = true;
                        *next_frame = Some(now + period);
                    }
                    due = earliest(due, next_frame.unwrap_or(now + period));
                }
            }
            // A feed that goes stale draws differently ("—", dimmed) with no `set` to say so,
            // so wake for the earliest expiry among fresh feeds on screen.
            let pending = |e: &Elem| -> Option<Instant> {
                e.reads
                    .iter()
                    .zip(&e.sig)
                    .filter(|(_, s)| s.as_ref().is_some_and(|s| !s.stale()))
                    .filter_map(|(k, _)| self.feeds.expires_at(k))
                    .min()
            };
            for (i, slot) in keys.iter_mut().enumerate() {
                let Some(e) = slot else { continue };
                let b = page.keys.get(i).and_then(Option::as_ref);
                let sig = self.sig_of(&e.reads, e.history);
                let animate = ticked && e.anim.is_some();
                if sig == e.sig && !animate {
                    due = pending(e).map_or(due, |t| earliest(due, t));
                    continue;
                }
                if !animate && now.duration_since(e.last) < e.interval {
                    due = earliest(due, e.last + e.interval);
                    continue;
                }
                let held = holds[i].map(|h| h.progress(now));
                let state = two_state
                    .get(&(mode_id.to_string(), page.name.clone(), i))
                    .copied()
                    .unwrap_or(0);
                let extra = KeyExtra {
                    held,
                    state,
                    pop: popup.pop_for(slots_of(mode), i),
                };
                cmds.extend(self.key_cmd(mode, b, i, *frame, extra));
                (e.sig, e.last) = (sig, now);
                due = pending(e).map_or(due, |t| earliest(due, t));
            }
            if picker.is_none() && popup.showing.is_none() {
                for (i, slot) in panels.iter_mut().enumerate() {
                    let Some(e) = slot else { continue };
                    let sig = self.sig_of(&e.reads, e.history);
                    if sig == e.sig {
                        due = pending(e).map_or(due, |t| earliest(due, t));
                        continue;
                    }
                    if now.duration_since(e.last) < e.interval {
                        due = earliest(due, e.last + e.interval);
                        continue;
                    }
                    cmds.extend(self.panel_cmd(mode, i, highlight[i]));
                    (e.sig, e.last) = (sig, now);
                    due = pending(e).map_or(due, |t| earliest(due, t));
                }
            }
        }
        self.deck_send(cmds);
        due
    }

    /// Redraws the deck as feeds change. Runs for the life of the agent; it does nothing
    /// until a feed that is on screen changes.
    pub(crate) async fn redraw_loop(self: Arc<Self>) {
        let mut changes = self.feeds.changes();
        loop {
            let due = self.redraw_pass();
            tokio::select! {
                r = changes.changed() => {
                    if r.is_err() {
                        return;
                    }
                }
                _ = self.redraw_wake.notified() => {}
                _ = async {
                    match due {
                        Some(t) => tokio::time::sleep_until(t).await,
                        None => std::future::pending().await,
                    }
                } => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feeds::{FOREVER, FeedValue};
    use novagear_core::modes::ChoiceOption;
    use std::sync::mpsc;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("novagear-modes-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn page(name: &str) -> Page {
        Page {
            name: name.into(),
            keys: Vec::new(),
            dials: Vec::new(),
        }
    }

    fn mode(id: &str, pages: &[&str]) -> Mode {
        Mode {
            id: id.into(),
            name: id.to_uppercase(),
            pages: pages.iter().map(|p| page(p)).collect(),
            ..Mode::default()
        }
    }

    fn cfg_with(modes: Vec<Mode>) -> Config {
        let start = modes.first().map(|m| m.id.clone()).unwrap_or_default();
        Config {
            modes,
            start_mode: start,
            deck_control: true,
            ..Config::default()
        }
    }

    fn agent_with(tag: &str, cfg: Config) -> (Arc<Agent>, PathBuf) {
        let d = dir(tag);
        cfg.save(&d.join("config.json")).unwrap();
        (
            Agent::new(d.join("config.json"), "t".repeat(48)).unwrap(),
            d,
        )
    }

    fn note(n: &str) -> Action {
        Action::Placeholder { note: n.into() }
    }

    fn ran(a: &Agent) -> Vec<String> {
        lock(&a.ran)
            .iter()
            .map(|a| match a {
                Action::Placeholder { note } => note.clone(),
                other => other.log_label(),
            })
            .collect()
    }

    fn dial(label: &str, turn: &str, alt: Option<&str>) -> DialBinding {
        DialBinding {
            label: label.into(),
            turn: Some(note(turn)),
            turn_left: None,
            turn_right: None,
            press: None,
            alt_turn: alt.map(note),
        }
    }

    /// An agent with a fake deck: the commands it would send arrive on the receiver.
    fn with_deck(a: &Arc<Agent>) -> mpsc::Receiver<Command> {
        let (tx, rx) = mpsc::channel();
        lock(&a.deck).commands = Some(tx);
        rx
    }

    fn drain(rx: &mpsc::Receiver<Command>) -> (usize, usize) {
        let (mut keys, mut strips) = (0, 0);
        while let Ok(c) = rx.try_recv() {
            match c {
                Command::KeyImage { .. } => keys += 1,
                Command::StripImage { .. } => strips += 1,
                _ => {}
            }
        }
        (keys, strips)
    }

    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    #[test]
    fn missing_current_mode_falls_back_to_first() {
        let mut cfg = cfg_with(vec![mode("a", &["one", "two"]), mode("b", &["x"])]);
        cfg.start_mode = "b".into();
        cfg.start_page = "x".into();
        let st = |m: &str, p: &str| ModeState {
            mode: m.into(),
            page: p.into(),
            ..ModeState::default()
        };
        assert_eq!(resolve(&cfg, &st("gone", "gone")), ("b".into(), "x".into()));
        assert_eq!(resolve(&cfg, &st("a", "two")), ("a".into(), "two".into()));
        assert_eq!(resolve(&cfg, &st("a", "gone")), ("a".into(), "one".into()));
        cfg.start_mode = "missing".into();
        assert_eq!(resolve(&cfg, &st("gone", "")), ("a".into(), "one".into()));
        cfg.modes.clear();
        assert_eq!(
            resolve(&cfg, &st("gone", "x")),
            (String::new(), String::new())
        );
    }

    #[test]
    fn start_page_opens_in_the_start_mode() {
        let mut cfg = cfg_with(vec![mode("a", &["one", "two"])]);
        cfg.start_page = "two".into();
        let st = ModeState {
            mode: "a".into(),
            ..ModeState::default()
        };
        assert_eq!(resolve(&cfg, &st).1, "two");
    }

    #[tokio::test(start_paused = true)]
    async fn picker_picks_by_segment_and_pages_with_turns() {
        let modes: Vec<Mode> = (0..6).map(|i| mode(&format!("m{i}"), &["p"])).collect();
        let (a, _d) = agent_with("picker", cfg_with(modes));
        a.open_picker();
        assert_eq!(a.picker_page(), Some(0));
        a.on_deck_input(Input::Touch {
            x: 450,
            y: 50,
            long: false,
        })
        .await;
        assert_eq!(a.current_mode(), "m2");
        assert_eq!(a.picker_page(), None);

        a.open_picker();
        a.on_deck_input(Input::DialTurn {
            index: 0,
            detents: 1,
        })
        .await;
        assert_eq!(a.picker_page(), Some(1));
        a.on_deck_input(Input::DialPress {
            index: 1,
            down: true,
        })
        .await;
        assert_eq!(a.current_mode(), "m5");
    }

    #[tokio::test(start_paused = true)]
    async fn picker_closes_after_5s() {
        let (a, _d) = agent_with(
            "picker-idle",
            cfg_with(vec![mode("a", &["p"]), mode("b", &["p"])]),
        );
        a.open_picker();
        tokio::time::advance(Duration::from_secs(4)).await;
        settle().await;
        assert!(a.picker_page().is_some());
        // A turn is input: it holds the picker open for another 5 s.
        a.on_deck_input(Input::DialTurn {
            index: 0,
            detents: 1,
        })
        .await;
        tokio::time::advance(Duration::from_secs(4)).await;
        settle().await;
        assert!(a.picker_page().is_some());
        tokio::time::advance(Duration::from_secs(2)).await;
        settle().await;
        assert_eq!(a.picker_page(), None);
        assert_eq!(a.current_mode(), "a", "closing picks nothing");
    }

    #[tokio::test(start_paused = true)]
    async fn keys_keep_their_actions_while_the_picker_is_open() {
        let mut m = mode("a", &["p"]);
        m.pages[0].keys = vec![Some(KeyBinding {
            label: "K".into(),
            color: "#000000".into(),
            icon: None,
            face: Default::default(),
            states: None,
            follow: None,
            hold_ms: 0,
            action: note("key"),
        })];
        let (a, _d) = agent_with("picker-keys", cfg_with(vec![m]));
        a.open_picker();
        a.on_deck_input(Input::Key {
            index: 0,
            down: true,
        })
        .await;
        assert_eq!(ran(&a), ["key"]);
        assert!(a.picker_page().is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn alt_turn_applies_after_press_until_idle() {
        let mut m = mode("a", &["p"]);
        m.strip = vec![Some(StripPanel {
            dial: Some(dial("L", "turn", Some("alt"))),
            ..StripPanel::default()
        })];
        let (a, _d) = agent_with("alt", cfg_with(vec![m]));
        let press = Input::DialPress {
            index: 0,
            down: true,
        };
        let turn = Input::DialTurn {
            index: 0,
            detents: 1,
        };
        a.on_deck_input(press.clone()).await;
        a.on_deck_input(turn.clone()).await;
        tokio::time::advance(Duration::from_secs(6)).await;
        a.on_deck_input(turn.clone()).await;
        assert_eq!(ran(&a), ["alt", "turn"]);
        // The next press ends the alternate role.
        a.on_deck_input(press.clone()).await;
        a.on_deck_input(press).await;
        a.on_deck_input(turn).await;
        assert_eq!(ran(&a), ["alt", "turn", "turn"]);
    }

    /// Prompter speed and deck brightness detents change one number: no image cache flush,
    /// no knob reset, no cancelled holds, and config.json is written once turning stops.
    #[tokio::test(start_paused = true)]
    async fn scalar_detents_keep_knob_roles_and_images() {
        let mut m = mode("a", &["p"]);
        m.strip = vec![
            Some(StripPanel {
                dial: Some(dial("L", "turn", Some("alt"))),
                ..StripPanel::default()
            }),
            Some(StripPanel {
                dial: Some(DialBinding {
                    turn: Some(Action::PrompterSpeedStep { step: 5 }),
                    ..dial("S", "x", None)
                }),
                ..StripPanel::default()
            }),
            Some(StripPanel {
                dial: Some(DialBinding {
                    turn: Some(Action::DeckBrightnessStep { step: 5 }),
                    ..dial("B", "x", None)
                }),
                ..StripPanel::default()
            }),
        ];
        m.pages[0].keys = vec![Some(KeyBinding {
            hold_ms: 1000,
            ..key("H", note("held"))
        })];
        let (a, d) = agent_with("scalar", cfg_with(vec![m]));
        let _rx = with_deck(&a);
        std::fs::create_dir_all(a.images_dir()).unwrap();
        let png = d.join("images").join("k.png");
        image::RgbaImage::new(4, 4).save(&png).unwrap();
        assert!(a.images.get("k.png").is_some());
        std::fs::remove_file(&png).unwrap();
        let speed = a.config().prompter.speed;
        let on_disk = || Config::load(&d.join("config.json")).unwrap();

        a.on_deck_input(Input::DialPress {
            index: 0,
            down: true,
        })
        .await;
        a.on_deck_input(key_input(0, true)).await;
        for (index, detents) in [(1, 1), (1, 1), (2, 1)] {
            a.on_deck_input(Input::DialTurn { index, detents }).await;
        }
        assert_eq!(a.config().prompter.speed, speed + 10);
        assert!(a.images.get("k.png").is_some(), "the image cache was kept");
        assert!(lock(&a.ui).holding(0), "the hold was kept");
        assert_eq!(on_disk().prompter.speed, speed, "not written yet");
        a.on_deck_input(Input::DialTurn {
            index: 0,
            detents: 1,
        })
        .await;
        assert_eq!(
            ran(&a).last().map(String::as_str),
            Some("alt"),
            "alt_turn kept"
        );
        tokio::time::advance(Duration::from_millis(1100)).await;
        settle().await;
        assert_eq!(
            on_disk().prompter.speed,
            speed + 10,
            "written once turning stopped"
        );
        assert_eq!(on_disk().deck_brightness, a.config().deck_brightness);
    }

    #[tokio::test(start_paused = true)]
    async fn choice_commit_press_previews_then_runs() {
        let mut m = mode("a", &["p"]);
        m.strip = vec![Some(StripPanel {
            kind: PanelKind::Choice,
            commit: Commit::Press,
            options: vec![
                ChoiceOption {
                    label: "A".into(),
                    action: note("first"),
                },
                ChoiceOption {
                    label: "B".into(),
                    action: note("second"),
                },
            ],
            ..StripPanel::default()
        })];
        let (a, _d) = agent_with("choice", cfg_with(vec![m]));
        let rx = with_deck(&a);
        a.on_deck_input(Input::DialTurn {
            index: 0,
            detents: 1,
        })
        .await;
        assert!(ran(&a).is_empty(), "a turn only previews");
        assert_eq!(drain(&rx), (0, 1), "and redraws just that segment");
        a.on_deck_input(Input::DialPress {
            index: 0,
            down: true,
        })
        .await;
        assert_eq!(ran(&a), ["second"]);
    }

    #[tokio::test(start_paused = true)]
    async fn choice_commit_turn_runs_as_it_moves() {
        let mut m = mode("a", &["p"]);
        m.strip = vec![Some(StripPanel {
            kind: PanelKind::Choice,
            options: vec![
                ChoiceOption {
                    label: "A".into(),
                    action: note("first"),
                },
                ChoiceOption {
                    label: "B".into(),
                    action: note("second"),
                },
            ],
            ..StripPanel::default()
        })];
        let (a, _d) = agent_with("choice-turn", cfg_with(vec![m]));
        let turn = |detents| Input::DialTurn { index: 0, detents };
        a.on_deck_input(turn(1)).await;
        a.on_deck_input(turn(5)).await;
        assert_eq!(ran(&a), ["second"], "the end of the list is not run twice");
    }

    #[tokio::test(start_paused = true)]
    async fn touch_runs_the_segments_press() {
        let mut m = mode("a", &["p"]);
        m.strip = vec![
            None,
            Some(StripPanel {
                press: Some(note("tap")),
                ..StripPanel::default()
            }),
        ];
        let (a, _d) = agent_with("touch", cfg_with(vec![m]));
        a.on_deck_input(Input::Touch {
            x: 250,
            y: 10,
            long: false,
        })
        .await;
        assert_eq!(ran(&a), ["tap"]);
    }

    #[tokio::test(start_paused = true)]
    async fn mode_actions_switch_and_cycle() {
        let (a, _d) = agent_with(
            "cycle",
            cfg_with(vec![
                mode("a", &["p"]),
                mode("b", &["p"]),
                mode("c", &["p"]),
            ]),
        );
        a.run(Action::ModeGo { mode: "c".into() }).await.unwrap();
        assert_eq!(a.current_mode(), "c");
        a.run(Action::ModeNext).await.unwrap();
        assert_eq!(a.current_mode(), "a", "cycling wraps");
        assert!(
            a.run(Action::ModeGo {
                mode: "nope".into()
            })
            .await
            .is_err()
        );
        let mut cfg = a.config();
        cfg.mode_key = novagear_core::modes::ModeKey::Cycle;
        a.set_config(cfg).unwrap();
        a.run(Action::ModePicker).await.unwrap();
        assert_eq!(a.current_mode(), "b");
        assert_eq!(a.picker_page(), None, "cycle mode never opens the picker");
    }

    #[tokio::test(start_paused = true)]
    async fn deleting_the_current_mode_falls_back() {
        let (a, _d) = agent_with(
            "deleted",
            cfg_with(vec![mode("a", &["p"]), mode("b", &["q"])]),
        );
        a.show_mode("b", true).unwrap();
        let mut cfg = a.config();
        cfg.modes.retain(|m| m.id != "b");
        a.set_config(cfg).unwrap();
        assert_eq!(a.current_mode(), "a");
        assert_eq!(a.current_page(), "p");
    }

    #[test]
    fn page_actions_stay_inside_the_mode() {
        let (a, _d) = agent_with(
            "pages",
            cfg_with(vec![mode("a", &["p1", "p2"]), mode("b", &["other"])]),
        );
        assert!(a.show_page("other".into(), true).is_err());
        a.show_page("p2".into(), true).unwrap();
        assert_eq!(a.current_page(), "p2");
    }

    #[tokio::test(start_paused = true)]
    async fn redraw_only_changed_panels_at_most_once_per_second() {
        let mut m = mode("a", &["p"]);
        m.strip = vec![
            Some(StripPanel {
                kind: PanelKind::Bar,
                label: "CPU".into(),
                feed: Some("test.cpu".into()),
                ..StripPanel::default()
            }),
            Some(StripPanel {
                label: "Fixed".into(),
                ..StripPanel::default()
            }),
        ];
        let (a, _d) = agent_with("redraw", cfg_with(vec![m]));
        let rx = with_deck(&a);
        a.redraw_deck();
        let _ = drain(&rx);
        assert!(a.feeds.is_wanted("test.cpu"));
        tokio::spawn(a.clone().redraw_loop());
        settle().await;

        for n in 0..10 {
            a.feeds
                .set("test.cpu", FeedValue::Number(n as f64), None, None, FOREVER);
            tokio::time::advance(Duration::from_millis(100)).await;
            settle().await;
        }
        let (keys, strips) = drain(&rx);
        assert_eq!(keys, 0, "no key reads that feed");
        assert!((1..=2).contains(&strips), "drew {strips} segments");
        // The last value is not lost: it is drawn once the second is up.
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        assert!(drain(&rx).1 <= 1);
    }

    /// Two panels: a gauge on `test.mem` and a value on `test.other`.
    fn gauge_and_value() -> Mode {
        let mut m = mode("a", &["p"]);
        m.strip = vec![
            Some(StripPanel {
                kind: PanelKind::Gauge,
                label: "MEM".into(),
                feed: Some("test.mem".into()),
                ..StripPanel::default()
            }),
            Some(StripPanel {
                kind: PanelKind::Value,
                label: "OTHER".into(),
                value: Some(novagear_core::modes::TextSource::Feed {
                    feed: "test.other".into(),
                }),
                ..StripPanel::default()
            }),
        ];
        m
    }

    #[tokio::test(start_paused = true)]
    async fn a_value_that_rounds_to_the_same_text_draws_nothing() {
        let (a, _d) = agent_with("same-text", cfg_with(vec![gauge_and_value()]));
        let rx = with_deck(&a);
        a.feeds
            .set("test.mem", FeedValue::Number(63.04), None, None, FOREVER);
        a.redraw_deck();
        let _ = drain(&rx);
        tokio::spawn(a.clone().redraw_loop());
        settle().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        // 63.04 and 63.01 both read "63.0" and fill the ring to 63%.
        a.feeds
            .set("test.mem", FeedValue::Number(63.01), None, None, FOREVER);
        tokio::time::advance(Duration::from_secs(2)).await;
        settle().await;
        assert_eq!(drain(&rx), (0, 0), "nothing on screen changed");
        // A real change still draws, and only that panel.
        a.feeds
            .set("test.mem", FeedValue::Number(64.2), None, None, FOREVER);
        settle().await;
        assert_eq!(drain(&rx), (0, 1));
    }

    #[tokio::test(start_paused = true)]
    async fn a_pushed_history_alone_does_not_redraw_a_panel_that_draws_no_history() {
        let (a, _d) = agent_with("history", cfg_with(vec![gauge_and_value()]));
        let rx = with_deck(&a);
        a.feeds
            .set("test.mem", FeedValue::Number(40.0), None, None, FOREVER);
        a.redraw_deck();
        let _ = drain(&rx);
        tokio::spawn(a.clone().redraw_loop());
        settle().await;
        // The gauge's feed is set to the same number again and again: its history grows,
        // its picture does not.
        for _ in 0..5 {
            a.feeds
                .set("test.mem", FeedValue::Number(40.0), None, None, FOREVER);
        }
        tokio::time::advance(Duration::from_secs(2)).await;
        a.feeds.set(
            "test.other",
            FeedValue::Text("hello".into()),
            None,
            None,
            FOREVER,
        );
        settle().await;
        assert_eq!(drain(&rx), (0, 1), "only the panel whose feed changed");
    }

    #[tokio::test(start_paused = true)]
    async fn key_redraws_when_its_feed_changes() {
        let mut m = mode("a", &["p"]);
        m.pages[0].keys = vec![
            Some(KeyBinding {
                label: "K".into(),
                color: "#000000".into(),
                icon: None,
                face: novagear_core::modes::Face {
                    glow: Some("test.glow".into()),
                    ..Default::default()
                },
                states: None,
                follow: None,
                hold_ms: 0,
                action: note("x"),
            }),
            None,
        ];
        let (a, _d) = agent_with("key-redraw", cfg_with(vec![m]));
        let rx = with_deck(&a);
        a.redraw_deck();
        let (keys, _) = drain(&rx);
        assert_eq!(keys, KEYS);
        tokio::spawn(a.clone().redraw_loop());
        settle().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        a.feeds
            .set("test.glow", FeedValue::Number(1.0), None, None, FOREVER);
        settle().await;
        assert_eq!(drain(&rx), (1, 0));
    }

    #[test]
    fn mode_survives_restart() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        rt.block_on(async {
            let cfg = cfg_with(vec![mode("a", &["p"]), mode("b", &["x", "y"])]);
            let (a, d) = agent_with("restart", cfg);
            a.show_mode("b", true).unwrap();
            a.show_page("y".into(), true).unwrap();
            tokio::time::advance(Duration::from_secs(1)).await;
            settle().await;
            assert!(load_state(&d).is_none(), "debounced: nothing written yet");
            tokio::time::advance(Duration::from_secs(3)).await;
            settle().await;
            assert_eq!(load_state(&d), Some(("b".into(), "y".into())));

            let b = Agent::new(d.join("config.json"), "t".repeat(48)).unwrap();
            assert_eq!(b.current_mode(), "b");
            assert_eq!(b.current_page(), "y");
        });
    }

    /// A change made in the last second reaches disk on shutdown, and the timers then
    /// have nothing left to write.
    #[test]
    fn flush_saves_writes_what_the_timers_still_hold() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        rt.block_on(async {
            let cfg = cfg_with(vec![mode("a", &["p"]), mode("b", &["x", "y"])]);
            let (a, d) = agent_with("flush", cfg);
            a.show_mode("b", true).unwrap();
            a.set_scalar(|c| c.deck_brightness = 37);
            assert!(load_state(&d).is_none());
            a.flush_saves();
            assert_eq!(load_state(&d).map(|s| s.0), Some("b".into()));
            let saved = Config::load(&d.join("config.json")).unwrap();
            assert_eq!(saved.deck_brightness, 37);
            // Nothing pending now: a second flush writes nothing, even if the file is gone.
            std::fs::remove_file(d.join("state.json")).unwrap();
            a.flush_saves();
            assert!(load_state(&d).is_none());
            // And the timers fire into nothing.
            tokio::time::advance(Duration::from_secs(5)).await;
            settle().await;
            assert!(load_state(&d).is_none());
        });
    }

    #[test]
    fn state_naming_a_deleted_mode_starts_on_the_first() {
        let (_a, d) = agent_with("stale-state", cfg_with(vec![mode("a", &["p"])]));
        save_state(&d, "gone", "gone");
        let b = Agent::new(d.join("config.json"), "t".repeat(48)).unwrap();
        assert_eq!(b.current_mode(), "a");
    }

    #[test]
    fn missing_image_resolves_after_it_appears_and_invalidation_runs() {
        let d = dir("images");
        let a = Agent::new(d.join("config.json"), "t".repeat(48)).unwrap();
        assert!(a.images.get("later.png").is_none());
        std::fs::create_dir_all(d.join("images")).unwrap();
        image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 0, 255]))
            .save(d.join("images/later.png"))
            .unwrap();
        assert!(
            a.images.get("later.png").is_none(),
            "a failed lookup stays cached until told otherwise"
        );
        a.images_changed();
        assert!(a.images.get("later.png").is_some());
    }

    #[test]
    fn config_changes_clear_the_image_cache() {
        let d = dir("images-cfg");
        let a = Agent::new(d.join("config.json"), "t".repeat(48)).unwrap();
        assert!(a.images.get("later.png").is_none());
        std::fs::create_dir_all(d.join("images")).unwrap();
        image::RgbaImage::from_pixel(4, 4, image::Rgba([0, 255, 0, 255]))
            .save(d.join("images/later.png"))
            .unwrap();
        a.set_config(a.config()).unwrap();
        assert!(a.images.get("later.png").is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn leaving_a_mode_drops_its_gif_from_the_cache() {
        let mut a_mode = mode("a", &["p"]);
        a_mode.pages[0].keys = vec![Some(KeyBinding {
            icon: Some("anim.gif".into()),
            ..key("G", note("g"))
        })];
        let (a, d) = agent_with("evict", cfg_with(vec![a_mode, mode("b", &["p"])]));
        std::fs::create_dir_all(d.join("images")).unwrap();
        let gif = {
            let mut out = Vec::new();
            let mut enc = image::codecs::gif::GifEncoder::new(&mut out);
            for c in [10, 200] {
                enc.encode_frame(image::Frame::new(image::RgbaImage::from_pixel(
                    8,
                    8,
                    image::Rgba([c, 2, 3, 255]),
                )))
                .unwrap();
            }
            drop(enc);
            out
        };
        std::fs::write(d.join("images/anim.gif"), gif).unwrap();
        let _rx = with_deck(&a);
        a.redraw_deck();
        assert!(
            a.images.cached("anim.gif"),
            "on screen, so decoded once and kept"
        );
        a.show_mode("b", true).unwrap();
        assert!(!a.images.cached("anim.gif"), "nothing on screen uses it");
        a.show_mode("a", true).unwrap();
        assert!(a.images.cached("anim.gif"), "back on screen");
    }

    #[test]
    fn import_goes_into_a_mode_called_imported() {
        let (a, d) = agent_with("import", cfg_with(vec![mode("a", &["mine"])]));
        let profiles = d.join("ProfilesV3");
        let prof = profiles.join("AAAAAAAA-0000-0000-0000-000000000000.sdProfile");
        let page_id = "11111111-0000-0000-0000-000000000000";
        let put = |p: PathBuf, v: serde_json::Value| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, v.to_string()).unwrap();
        };
        put(
            prof.join("manifest.json"),
            json!({ "Name": "Edit", "Device": { "Model": "20GBD9901" }, "Pages": { "Pages": [page_id] } }),
        );
        put(
            prof.join(format!("Profiles/{page_id}/manifest.json")),
            json!({ "Controllers": [{ "Type": "Keypad", "Actions": {
                "0,0": { "UUID": "com.elgato.streamdeck.system.website", "Name": "Site", "Settings": { "path": "example.com" }, "State": 0, "States": [{ "Title": "Site" }] },
            }}]}),
        );
        a.import_streamdeck(Some(profiles)).unwrap();
        let cfg = a.config();
        let imported = cfg.mode("imported").expect("an Imported mode");
        assert_eq!(imported.name, "Imported");
        assert_eq!(imported.pages[0].name, "Edit");
        assert!(cfg.mode("a").is_some_and(|m| m.page("mine").is_some()));
        assert_eq!(a.current_mode(), "imported");
        assert_eq!(a.current_page(), "Edit");
    }

    #[tokio::test(start_paused = true)]
    async fn value_panel_dial_press_runs_the_panels_press() {
        let mut m = mode("a", &["p"]);
        m.strip = vec![Some(StripPanel {
            press: Some(note("tap")),
            ..StripPanel::default()
        })];
        let (a, _d) = agent_with("value-press", cfg_with(vec![m]));
        a.on_deck_input(Input::DialPress {
            index: 0,
            down: true,
        })
        .await;
        assert_eq!(ran(&a), ["tap"]);
    }

    #[tokio::test(start_paused = true)]
    async fn feed_going_stale_redraws_its_panel_once() {
        let mut m = mode("a", &["p"]);
        m.strip = vec![Some(StripPanel {
            kind: PanelKind::Bar,
            feed: Some("test.ttl".into()),
            ..StripPanel::default()
        })];
        let (a, _d) = agent_with("stale", cfg_with(vec![m]));
        a.feeds.set(
            "test.ttl",
            FeedValue::Number(5.0),
            None,
            None,
            Duration::from_secs(2),
        );
        let rx = with_deck(&a);
        a.redraw_deck();
        a.feeds.set_wanted(["test.ttl".to_string()].into());
        let _ = drain(&rx);
        tokio::spawn(a.clone().redraw_loop());
        settle().await;
        tokio::time::advance(Duration::from_millis(1900)).await;
        settle().await;
        assert_eq!(drain(&rx), (0, 0), "still fresh");
        tokio::time::advance(Duration::from_millis(200)).await;
        settle().await;
        assert_eq!(drain(&rx), (0, 1), "redrawn once when it goes stale");
        assert!(a.feeds.get("test.ttl").unwrap().stale);
        tokio::time::advance(Duration::from_secs(10)).await;
        settle().await;
        assert_eq!(drain(&rx), (0, 0), "and not again");
    }

    #[test]
    fn status_lists_modes() {
        let (a, _d) = agent_with(
            "status",
            cfg_with(vec![mode("a", &["p"]), mode("b", &["p"])]),
        );
        let v = a.mode_status();
        assert_eq!(v["mode"]["id"], "a");
        assert_eq!(v["mode"]["accent"], "#2563eb");
        assert_eq!(v["modes"].as_array().unwrap().len(), 2);
    }

    fn key(label: &str, action: Action) -> KeyBinding {
        KeyBinding {
            label: label.into(),
            color: "#000000".into(),
            icon: None,
            face: Default::default(),
            states: None,
            follow: None,
            hold_ms: 0,
            action,
        }
    }

    fn keys_agent(tag: &str, keys: Vec<KeyBinding>) -> Arc<Agent> {
        let mut m = mode("a", &["p"]);
        m.pages[0].keys = keys.into_iter().map(Some).collect();
        agent_with(tag, cfg_with(vec![m])).0
    }

    fn key_input(index: usize, down: bool) -> Input {
        Input::Key { index, down }
    }

    /// Steps paused time in 100 ms ticks, letting the hold task run between them.
    async fn pass(ms: u64) {
        for _ in 0..ms / 100 {
            tokio::time::advance(Duration::from_millis(100)).await;
            settle().await;
        }
    }

    fn step(action: Action, delay_ms: u32) -> novagear_core::Step {
        novagear_core::Step { action, delay_ms }
    }

    #[tokio::test(start_paused = true)]
    async fn sequence_runs_all_steps_even_after_a_failure() {
        let (a, _d) = agent_with("seq", cfg_with(vec![mode("a", &["p"])]));
        let r = a
            .run(Action::Sequence {
                steps: vec![
                    step(Action::PrompterRestart, 0),
                    step(note("bad"), 0),
                    step(Action::PrompterPlayPause, 0),
                ],
            })
            .await;
        assert!(r.is_ok(), "the steps that ran count as done");
        assert_eq!(
            ran(&a),
            [
                "sequence, 3 steps",
                "prompter_restart",
                "bad",
                "prompter_play_pause"
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sequence_delay_is_capped() {
        let (a, _d) = agent_with("seq-cap", cfg_with(vec![mode("a", &["p"])]));
        let start = Instant::now();
        a.run(Action::Sequence {
            steps: vec![
                step(Action::PrompterRestart, 50_000),
                step(Action::PrompterPlayPause, 50_000),
            ],
        })
        .await
        .unwrap();
        assert_eq!(ran(&a), ["sequence, 2 steps", "prompter_restart"]);
        assert!(start.elapsed() < Duration::from_secs(60));
    }

    #[tokio::test(start_paused = true)]
    async fn nested_sequence_steps_are_skipped() {
        let (a, _d) = agent_with("seq-nest", cfg_with(vec![mode("a", &["p"])]));
        let inner = Action::Sequence {
            steps: vec![step(Action::PrompterRestart, 0)],
        };
        let _ = a
            .run(Action::Sequence {
                steps: vec![step(inner, 0), step(Action::PrompterPlayPause, 0)],
            })
            .await;
        assert_eq!(ran(&a), ["sequence, 2 steps", "prompter_play_pause"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_long_sequence_does_not_block_deck_input() {
        let seq = Action::Sequence {
            steps: vec![step(Action::PrompterRestart, 30_000)],
        };
        let a = keys_agent(
            "seq-input",
            vec![key("S", seq), key("T", Action::PrompterPlayPause)],
        );
        a.on_deck_input(key_input(0, true)).await;
        a.on_deck_input(key_input(1, true)).await;
        assert!(ran(&a).contains(&"prompter_play_pause".to_string()));
    }

    /// A dial press, a dial turn or a strip touch that runs a long sequence leaves the deck
    /// taking input; a second press while it runs is ignored.
    #[tokio::test(start_paused = true)]
    async fn a_dial_sequence_does_not_block_deck_input() {
        let seq = Action::Sequence {
            steps: vec![step(Action::PrompterRestart, 30_000)],
        };
        let mut m = mode("a", &["p"]);
        m.pages[0].keys = vec![Some(key("T", Action::PrompterPlayPause))];
        m.pages[0].dials = vec![
            Some(DialBinding {
                press: Some(seq.clone()),
                turn: Some(seq.clone()),
                ..dial("D", "x", None)
            }),
            Some(DialBinding {
                press: Some(seq),
                ..dial("E", "y", None)
            }),
        ];
        let (a, _d) = agent_with("dial-seq", cfg_with(vec![m]));
        let t = Instant::now();
        a.on_deck_input(Input::DialPress {
            index: 0,
            down: true,
        })
        .await;
        a.on_deck_input(Input::DialTurn {
            index: 0,
            detents: 1,
        })
        .await;
        a.on_deck_input(Input::Touch {
            x: SEG_W + 10,
            y: 50,
            long: false,
        })
        .await;
        a.on_deck_input(key_input(0, true)).await;
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
        settle().await;
        let seqs = ran(&a).iter().filter(|r| r.starts_with("sequence")).count();
        assert_eq!(
            seqs, 2,
            "dial 0 once (its turn was ignored), segment 1 once"
        );
        assert!(ran(&a).contains(&"prompter_play_pause".to_string()));
    }

    fn two_state(follow: Option<&str>) -> KeyBinding {
        let st = |label: &str, action| novagear_core::modes::KeyState {
            label: label.into(),
            color: None,
            icon: None,
            face: Default::default(),
            action,
        };
        KeyBinding {
            states: Some([
                st("OFF", Action::PrompterRestart),
                st("ON", Action::PrompterPlayPause),
            ]),
            follow: follow.map(Into::into),
            ..key("T", Action::PrompterRestart)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn two_state_key_flips_and_follow_wins() {
        let a = keys_agent("two", vec![two_state(None), two_state(Some("test.on"))]);
        for _ in 0..3 {
            a.on_deck_input(key_input(0, true)).await;
        }
        assert_eq!(
            ran(&a),
            [
                "prompter_restart",
                "prompter_play_pause",
                "prompter_restart"
            ]
        );
        // A followed key reads its feed and never flips by itself.
        lock(&a.ran).clear();
        a.feeds
            .set("test.on", FeedValue::Number(1.0), None, None, FOREVER);
        for _ in 0..2 {
            a.on_deck_input(key_input(1, true)).await;
        }
        a.feeds
            .set("test.on", FeedValue::Number(0.0), None, None, FOREVER);
        a.on_deck_input(key_input(1, true)).await;
        assert_eq!(
            ran(&a),
            [
                "prompter_play_pause",
                "prompter_play_pause",
                "prompter_restart"
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_press_does_not_flip_the_state() {
        let mut k = two_state(None);
        k.states.as_mut().unwrap()[0].action = note("bad");
        let a = keys_agent("two-fail", vec![k]);
        a.on_deck_input(key_input(0, true)).await;
        a.on_deck_input(key_input(0, true)).await;
        assert_eq!(ran(&a), ["bad", "bad"]);
    }

    #[tokio::test(start_paused = true)]
    async fn two_state_memory_follows_the_config() {
        let a = keys_agent("two-gc", vec![two_state(None)]);
        a.on_deck_input(key_input(0, true)).await;
        assert_eq!(a.two_state_count(), 1);
        let mut cfg = a.config();
        cfg.modes[0].pages[0].keys = vec![Some(key("plain", Action::PrompterRestart))];
        a.set_config(cfg).unwrap();
        assert_eq!(a.two_state_count(), 0);
    }

    fn hold_key() -> KeyBinding {
        KeyBinding {
            hold_ms: 1000,
            ..key("H", Action::PrompterRestart)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn hold_fires_only_after_hold_ms() {
        let a = keys_agent("hold", vec![hold_key()]);
        let rx = with_deck(&a);
        a.on_deck_input(key_input(0, true)).await;
        pass(900).await;
        a.on_deck_input(key_input(0, false)).await;
        pass(500).await;
        assert!(ran(&a).is_empty(), "released early");
        let (keys, _) = drain(&rx);
        assert!(keys >= 5, "the key redraws as it fills ({keys})");

        a.on_deck_input(key_input(0, true)).await;
        pass(1100).await;
        a.on_deck_input(key_input(0, false)).await;
        pass(500).await;
        assert_eq!(ran(&a), ["prompter_restart"], "once, not again on release");
    }

    /// Key holds follow the Approve rule: a tap read at 0 and 400 ms but processed after a
    /// 1.5 s stall never fires, in either order; a real hold still does.
    #[tokio::test(start_paused = true)]
    async fn a_late_tap_never_fires_a_hold_key() {
        let ms = Duration::from_millis;
        let a = keys_agent("hold-late", vec![hold_key()]);
        let _rx = with_deck(&a);
        // The timer wakes late first.
        let t0 = Instant::now();
        a.on_deck_input_at(key_input(0, true), t0).await;
        a.deck_read(&key_input(0, false), t0 + ms(400));
        tokio::time::advance(ms(1500)).await;
        settle().await;
        a.on_deck_input_at(key_input(0, false), t0 + ms(400)).await;
        pass(1000).await;
        assert!(ran(&a).is_empty(), "timer first");

        // Both processed late, the release first.
        let t1 = Instant::now();
        a.deck_read(&key_input(0, false), t1 + ms(400));
        tokio::time::advance(ms(1500)).await;
        a.on_deck_input_at(key_input(0, true), t1).await;
        a.on_deck_input_at(key_input(0, false), t1 + ms(400)).await;
        pass(1000).await;
        assert!(ran(&a).is_empty(), "release first");

        // A real hold, released after its second, processed late: it fires.
        let t2 = Instant::now();
        a.on_deck_input_at(key_input(0, true), t2).await;
        tokio::time::advance(ms(1500)).await;
        settle().await;
        a.deck_read(&key_input(0, false), t2 + ms(1100));
        a.on_deck_input_at(key_input(0, false), t2 + ms(1100)).await;
        settle().await;
        assert_eq!(ran(&a), ["prompter_restart"]);
    }

    #[tokio::test(start_paused = true)]
    async fn hold_cancels_when_deck_closes() {
        let a = keys_agent("hold-close", vec![hold_key()]);
        let _rx = with_deck(&a);
        a.on_deck_input(key_input(0, true)).await;
        pass(500).await;
        lock(&a.deck).commands = None;
        pass(1500).await;
        assert!(ran(&a).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn hold_cancels_when_the_mode_changes() {
        let mut m = mode("a", &["p"]);
        m.pages[0].keys = vec![Some(hold_key())];
        let (a, _d) = agent_with("hold-mode", cfg_with(vec![m, mode("b", &["p"])]));
        let _rx = with_deck(&a);
        a.on_deck_input(key_input(0, true)).await;
        pass(500).await;
        a.show_mode("b", true).unwrap();
        pass(1500).await;
        assert!(ran(&a).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn hold_progress_and_start_time() {
        let t0 = Instant::now();
        let h = Hold::start(1, Duration::from_millis(1000), t0);
        assert_eq!(h.progress(t0), 0.0);
        assert!((h.progress(t0 + Duration::from_millis(250)) - 0.25).abs() < 1e-6);
        assert_eq!(h.progress(t0 + Duration::from_secs(5)), 1.0);
        assert!(h.started_before(t0 + Duration::from_millis(1)));
        assert!(!h.started_before(t0));
    }

    fn failing_sequence() -> Action {
        Action::Sequence {
            steps: vec![step(note("bad"), 0), step(Action::PrompterRestart, 0)],
        }
    }

    #[tokio::test(start_paused = true)]
    async fn two_state_key_flips_after_a_sequence_with_a_failing_step() {
        let mut k = two_state(None);
        k.states.as_mut().unwrap()[0].action = failing_sequence();
        let a = keys_agent("two-seq", vec![k]);
        a.on_deck_input(key_input(0, true)).await;
        settle().await;
        a.on_deck_input(key_input(0, true)).await;
        settle().await;
        let r = ran(&a);
        assert_eq!(r.last().unwrap(), "prompter_play_pause", "{r:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_running_sequence_ignores_another_press() {
        let seq = Action::Sequence {
            steps: vec![step(Action::PrompterRestart, 30_000)],
        };
        let a = keys_agent("seq-guard", vec![key("S", seq)]);
        for _ in 0..3 {
            a.on_deck_input(key_input(0, true)).await;
            settle().await;
        }
        assert_eq!(
            ran(&a),
            ["sequence, 1 steps"],
            "the re-presses were ignored"
        );
        pass(31_000).await;
        assert_eq!(ran(&a).len(), 2, "it ran its step");
        a.on_deck_input(key_input(0, true)).await;
        settle().await;
        assert_eq!(ran(&a).len(), 3, "free again once it finished");
    }

    fn counter() -> (Arc<std::sync::atomic::AtomicUsize>, HoldDone) {
        let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let n2 = n.clone();
        (
            n,
            Box::new(move || {
                Box::pin(async move {
                    n2.fetch_add(1, Ordering::SeqCst);
                })
            }),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn a_hold_runs_its_own_completion() {
        let (a, _d) = agent_with("hold-custom", cfg_with(vec![mode("a", &["p"])]));
        let _rx = with_deck(&a);
        let (n, done) = counter();
        a.start_hold(
            3,
            Duration::from_millis(500),
            Arc::new(|| true),
            done,
            Instant::now(),
        );
        pass(400).await;
        assert_eq!(n.load(Ordering::SeqCst), 0);
        pass(200).await;
        assert_eq!(n.load(Ordering::SeqCst), 1);
        pass(1000).await;
        assert_eq!(n.load(Ordering::SeqCst), 1, "once");
    }

    #[tokio::test(start_paused = true)]
    async fn a_hold_whose_check_fails_is_cancelled() {
        let (a, _d) = agent_with("hold-valid", cfg_with(vec![mode("a", &["p"])]));
        let _rx = with_deck(&a);
        let still = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let s2 = still.clone();
        let (n, done) = counter();
        a.start_hold(
            3,
            Duration::from_millis(500),
            Arc::new(move || s2.load(Ordering::SeqCst)),
            done,
            Instant::now(),
        );
        pass(200).await;
        still.store(false, Ordering::SeqCst);
        pass(1000).await;
        assert_eq!(n.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn releasing_at_the_deadline_fires_once() {
        let a = keys_agent("hold-edge", vec![hold_key()]);
        let _rx = with_deck(&a);
        a.on_deck_input(key_input(0, true)).await;
        tokio::time::advance(Duration::from_millis(1000)).await;
        // The key comes up at the deadline, maybe before the waiting task has woken.
        a.on_deck_input(key_input(0, false)).await;
        pass(500).await;
        assert_eq!(ran(&a), ["prompter_restart"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_release_that_is_not_the_holds_own_does_not_fire_it() {
        let a = keys_agent("hold-paired", vec![hold_key()]);
        let _rx = with_deck(&a);
        a.on_deck_input(key_input(0, true)).await;
        let t0 = Instant::now();
        // The deck read this: up at 200 ms (the hold's own release), down again at 900 ms, up at
        // 1100 ms. The last release is past the deadline but belongs to the second press.
        a.deck_read(&key_input(0, false), t0 + Duration::from_millis(200));
        a.deck_read(&key_input(0, true), t0 + Duration::from_millis(900));
        a.deck_read(&key_input(0, false), t0 + Duration::from_millis(1100));
        a.release_hold(0, t0 + Duration::from_millis(1100)).await;
        pass(2000).await;
        assert!(
            ran(&a).is_empty(),
            "fired from another press's release: {:?}",
            ran(&a)
        );
    }

    // ---- mode rules, the OS observer and deck sleep ----

    use crate::macos_events::OsEvent;
    use crate::obs::ObsEvent;
    use novagear_core::modes::{ModeRule, RuleWhen};

    fn rule(when: RuleWhen, mode: &str) -> ModeRule {
        ModeRule {
            when,
            mode: mode.into(),
        }
    }

    fn app(id: &str) -> RuleWhen {
        RuleWhen::AppFrontmost {
            bundle_id: id.into(),
        }
    }

    fn facts(recording: bool, streaming: bool, front: Option<&str>) -> Facts {
        Facts {
            obs_recording: recording,
            obs_streaming: streaming,
            frontmost: front.map(str::to_string),
        }
    }

    fn rules_cfg(rules: Vec<ModeRule>) -> Config {
        let mut cfg = cfg_with(vec![
            mode("dev", &["p"]),
            mode("create", &["p"]),
            mode("live", &["p"]),
        ]);
        cfg.mode_rules = rules;
        cfg
    }

    fn on(mode: &str) -> ModeState {
        ModeState {
            mode: mode.into(),
            page: "p".into(),
            ..ModeState::default()
        }
    }

    #[test]
    fn rule_switches_and_returns() {
        let cfg = rules_cfg(vec![rule(RuleWhen::ObsRecording, "create")]);
        let mut st = on("dev");
        assert_eq!(apply_rules(&cfg, &mut st, &facts(false, false, None)), None);
        assert_eq!(
            apply_rules(&cfg, &mut st, &facts(true, false, None)),
            Some("create".into())
        );
        // Nothing to do while it keeps holding.
        assert_eq!(apply_rules(&cfg, &mut st, &facts(true, false, None)), None);
        assert_eq!(
            apply_rules(&cfg, &mut st, &facts(false, false, None)),
            Some("dev".into())
        );
        assert_eq!(st.rule_active, None);
    }

    #[test]
    fn manual_choice_wins_until_rule_ends() {
        let cfg = rules_cfg(vec![rule(RuleWhen::ObsRecording, "create")]);
        let mut st = on("dev");
        apply_rules(&cfg, &mut st, &facts(true, false, None));
        // Picked Live by hand while recording.
        st.mode = "live".into();
        st.manual = Some("live".into());
        assert_eq!(apply_rules(&cfg, &mut st, &facts(true, false, None)), None);
        // Recording stops: stay on Live, and the choice is spent.
        assert_eq!(apply_rules(&cfg, &mut st, &facts(false, false, None)), None);
        assert_eq!(st.manual, None);
        assert_eq!(st.rule_active, None);
    }

    #[test]
    fn first_listed_rule_wins() {
        let cfg = rules_cfg(vec![
            rule(RuleWhen::ObsStreaming, "live"),
            rule(RuleWhen::ObsRecording, "create"),
        ]);
        let mut st = on("dev");
        assert_eq!(
            apply_rules(&cfg, &mut st, &facts(true, true, None)),
            Some("live".into())
        );
        // The streaming rule ends but recording goes on: the next rule takes over, and the
        // mode to come back to is still the one from before any rule.
        assert_eq!(
            apply_rules(&cfg, &mut st, &facts(true, false, None)),
            Some("create".into())
        );
        assert_eq!(
            apply_rules(&cfg, &mut st, &facts(false, false, None)),
            Some("dev".into())
        );
    }

    #[test]
    fn rule_with_missing_mode_is_skipped() {
        let cfg = rules_cfg(vec![
            rule(RuleWhen::ObsRecording, "gone"),
            rule(app("com.obsproject.obs-studio"), "live"),
        ]);
        let mut st = on("dev");
        assert_eq!(
            apply_rules(
                &cfg,
                &mut st,
                &facts(true, false, Some("com.obsproject.obs-studio"))
            ),
            Some("live".into())
        );
        let mut st = on("dev");
        assert_eq!(apply_rules(&cfg, &mut st, &facts(true, false, None)), None);
        assert_eq!(st.rule_active, None);
    }

    #[test]
    fn app_rule_matches_the_frontmost_bundle_id() {
        let cfg = rules_cfg(vec![rule(app("com.apple.Safari"), "live")]);
        let mut st = on("dev");
        assert_eq!(
            apply_rules(
                &cfg,
                &mut st,
                &facts(false, false, Some("com.apple.finder"))
            ),
            None
        );
        assert_eq!(
            apply_rules(
                &cfg,
                &mut st,
                &facts(false, false, Some("com.apple.Safari"))
            ),
            Some("live".into())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn events_drive_the_deck_through_the_rules() {
        let mut cfg = rules_cfg(vec![
            rule(RuleWhen::ObsRecording, "create"),
            rule(app("com.apple.Safari"), "live"),
        ]);
        cfg.start_mode = "dev".into();
        let (a, _d) = agent_with("rules-agent", cfg);
        a.obs_event(ObsEvent::Recording(true));
        assert_eq!(a.current_mode(), "create");
        // A pick by hand while the rule holds stays after it ends.
        a.show_mode("live", true).unwrap();
        a.obs_event(ObsEvent::Recording(false));
        assert_eq!(a.current_mode(), "live");
        a.os_event(OsEvent::Frontmost("com.apple.Safari".into()));
        assert_eq!(a.current_mode(), "live");
        a.os_event(OsEvent::Frontmost("com.apple.finder".into()));
        assert_eq!(a.current_mode(), "live", "nothing to return to but live");
        // A fresh rule remembers where it started.
        a.show_mode("dev", true).unwrap();
        a.os_event(OsEvent::Frontmost("com.apple.Safari".into()));
        assert_eq!(a.current_mode(), "live");
        a.os_event(OsEvent::Frontmost("com.apple.finder".into()));
        assert_eq!(a.current_mode(), "dev");
    }

    #[test]
    fn observer_not_started_without_need() {
        let mut cfg = rules_cfg(vec![rule(RuleWhen::ObsRecording, "create")]);
        assert!(!os_events_wanted(&cfg), "an obs rule needs no OS events");
        let (a, _d) = agent_with("observer-need", cfg.clone());
        a.reconcile_os_events();
        assert_eq!(a.os_spawns.load(Ordering::SeqCst), 0);

        cfg.mode_rules.push(rule(app("com.apple.Safari"), "live"));
        assert!(os_events_wanted(&cfg));
        a.set_config(cfg.clone()).unwrap();
        assert_eq!(a.os_spawns.load(Ordering::SeqCst), 1);
        a.set_config(cfg.clone()).unwrap();
        assert_eq!(a.os_spawns.load(Ordering::SeqCst), 1, "already running");

        cfg.mode_rules.clear();
        cfg.deck_sleep_min = 5;
        assert!(os_events_wanted(&cfg), "sleep needs the display events");
        a.set_config(cfg.clone()).unwrap();
        assert_eq!(a.os_spawns.load(Ordering::SeqCst), 1);

        cfg.deck_sleep_min = 0;
        a.set_config(cfg).unwrap();
        assert!(lock(&a.os_observer).is_none(), "stopped when not needed");
    }

    fn sleepy(tag: &str, min: u32, image: Option<&str>) -> (Arc<Agent>, mpsc::Receiver<Command>) {
        let mut m = mode("a", &["p"]);
        m.pages[0].keys = vec![Some(KeyBinding {
            face: novagear_core::modes::Face {
                value: Some(novagear_core::modes::TextSource::Feed {
                    feed: "x.value".into(),
                }),
                ..Default::default()
            },
            ..key("K", Action::PrompterRestart)
        })];
        let mut cfg = cfg_with(vec![m]);
        cfg.deck_sleep_min = min;
        cfg.deck_sleep_image = image.map(str::to_string);
        let (a, _d) = agent_with(tag, cfg);
        let rx = with_deck(&a);
        a.redraw_deck();
        (a, rx)
    }

    #[tokio::test(start_paused = true)]
    async fn sleep_after_idle_and_wake_input_is_swallowed() {
        let (a, rx) = sleepy("sleep-idle", 1, None);
        a.check_sleep();
        assert!(!a.asleep());
        tokio::time::advance(Duration::from_secs(61)).await;
        a.check_sleep();
        assert!(a.asleep());
        let mut cmds = Vec::new();
        while let Ok(c) = rx.try_recv() {
            cmds.push(c);
        }
        assert!(
            matches!(cmds.last(), Some(Command::Brightness(0))),
            "no image: black at brightness 0"
        );
        // The waking press runs nothing, and neither does its release.
        a.on_deck_input(key_input(0, true)).await;
        assert!(!a.asleep());
        a.on_deck_input(key_input(0, false)).await;
        pass(200).await;
        assert!(ran(&a).is_empty());
        let (keys, _) = drain(&rx);
        assert!(keys >= 1, "the deck is drawn again on waking");
        // Awake, a press runs.
        a.on_deck_input(key_input(0, true)).await;
        assert_eq!(ran(&a), ["prompter_restart"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_waking_press_does_not_start_a_hold() {
        let mut m = mode("a", &["p"]);
        m.pages[0].keys = vec![Some(KeyBinding {
            hold_ms: 500,
            ..key("H", Action::PrompterRestart)
        })];
        let mut cfg = cfg_with(vec![m]);
        cfg.deck_sleep_min = 1;
        let (a, _d) = agent_with("sleep-hold", cfg);
        let _rx = with_deck(&a);
        tokio::time::advance(Duration::from_secs(61)).await;
        a.check_sleep();
        a.on_deck_input(key_input(0, true)).await;
        assert!(!lock(&a.ui).holding(0));
        pass(1000).await;
        a.on_deck_input(key_input(0, false)).await;
        pass(100).await;
        assert!(ran(&a).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn asleep_wants_no_feeds() {
        let (a, _rx) = sleepy("sleep-feeds", 1, None);
        assert!(a.feeds.is_wanted("x."), "awake, the key's feed is wanted");
        tokio::time::advance(Duration::from_secs(61)).await;
        a.check_sleep();
        assert!(a.asleep());
        assert!(!a.feeds.is_wanted(""), "asleep, nothing is");
        a.os_event(OsEvent::DisplaysAwake);
        assert!(!a.asleep());
        assert!(a.feeds.is_wanted("x."));
    }

    #[tokio::test(start_paused = true)]
    async fn displays_sleeping_sleeps_the_deck_at_once() {
        let (a, _rx) = sleepy("sleep-displays", 30, None);
        a.os_event(OsEvent::DisplaysAsleep);
        assert!(a.asleep());
        a.os_event(OsEvent::DisplaysAwake);
        assert!(!a.asleep());
        // With sleep off the displays make no difference.
        let (a, _rx) = sleepy("sleep-displays-off", 0, None);
        a.os_event(OsEvent::DisplaysAsleep);
        assert!(!a.asleep());
    }

    #[tokio::test(start_paused = true)]
    async fn the_sleep_image_covers_every_key_and_the_strip_is_black() {
        let (a, rx) = sleepy("sleep-image", 1, Some("marks/novahelm.svg"));
        let _ = drain(&rx);
        a.os_event(OsEvent::DisplaysAsleep);
        let mut brightness = Vec::new();
        let (mut keys, mut strips) = (0, 0);
        while let Ok(c) = rx.try_recv() {
            match c {
                Command::KeyImage { jpeg, .. } => {
                    keys += 1;
                    assert_ne!(jpeg, deck::sleep_key_jpeg(None).unwrap(), "not black");
                }
                Command::StripImage { .. } => strips += 1,
                Command::Brightness(b) => brightness.push(b),
                _ => {}
            }
        }
        assert_eq!((keys, strips), (KEYS, 1));
        assert_eq!(brightness, [a.config().deck_brightness]);
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_redraws_while_asleep() {
        let (a, rx) = sleepy("sleep-quiet", 1, None);
        tokio::time::advance(Duration::from_secs(61)).await;
        a.check_sleep();
        let _ = drain(&rx);
        a.feeds
            .set("x.value", FeedValue::Number(3.0), None, None, FOREVER);
        a.redraw_strip();
        a.redraw_key(0);
        a.redraw_pass();
        assert_eq!(drain(&rx), (0, 0));
    }

    #[tokio::test(start_paused = true)]
    async fn idle_sleep_fires_again_after_a_wake_by_input() {
        let (a, _rx) = sleepy("sleep-again", 1, None);
        tokio::spawn(a.clone().events_loop());
        pass(100).await;
        pass(61_000).await;
        assert!(a.asleep(), "first sleep");
        a.on_deck_input(key_input(0, true)).await;
        a.on_deck_input(key_input(0, false)).await;
        assert!(!a.asleep());
        pass(61_000).await;
        assert!(a.asleep(), "asleep again after the idle time");
    }

    #[tokio::test(start_paused = true)]
    async fn a_restart_mid_rule_comes_back_to_the_users_mode() {
        let cfg = rules_cfg(vec![rule(RuleWhen::ObsRecording, "create")]);
        let (a, d) = agent_with("rule-restart", cfg);
        a.obs_event(ObsEvent::Recording(true));
        assert_eq!(a.current_mode(), "create");
        pass(2_500).await;
        assert_eq!(load_state(&d).map(|s| s.0), Some("dev".to_string()));
        // Restart: nothing holds any more.
        let b = Agent::new(d.join("config.json"), "t".repeat(48)).unwrap();
        assert_eq!(b.current_mode(), "dev");
        // A pick by hand while the rule holds is what survives instead.
        a.show_mode("live", true).unwrap();
        pass(2_500).await;
        assert_eq!(load_state(&d).map(|s| s.0), Some("live".to_string()));
    }

    #[tokio::test(start_paused = true)]
    async fn paging_inside_a_rules_mode_still_saves_the_users_mode_and_page() {
        let mut cfg = rules_cfg(vec![rule(RuleWhen::ObsRecording, "create")]);
        cfg.modes[0] = mode("dev", &["home", "two"]);
        cfg.modes[1] = mode("create", &["home", "other"]);
        let (a, d) = agent_with("rule-page", cfg);
        a.show_page("two".into(), true).unwrap();
        a.obs_event(ObsEvent::Recording(true));
        assert_eq!(a.current_mode(), "create");
        a.show_page("other".into(), true).unwrap();
        pass(2_500).await;
        assert_eq!(
            load_state(&d),
            Some(("dev".to_string(), "two".to_string())),
            "the user's mode and page"
        );
        let b = Agent::new(d.join("config.json"), "t".repeat(48)).unwrap();
        assert_eq!(
            (b.current_mode(), b.current_page()),
            ("dev".into(), "two".into())
        );
    }
}
