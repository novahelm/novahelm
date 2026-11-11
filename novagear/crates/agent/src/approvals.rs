//! Approvals: a local program asks for a yes or no, the deck shows it, and the answer goes
//! back. Programs that ask run as the same user as the agent and can read anything the agent
//! token can, so **only a physical 1-second hold on the deck says yes**. There is no approve
//! route, panel button or action: `approve_from_deck` is `pub(crate)` and its one caller is
//! the completion of the Approve key's hold (`start_approve`). The panel and the Mac popover
//! can deny; denying is always safe.
//!
//! Requests live in memory only. Only `id`, client name, risk and outcome are logged, never
//! the title, lines, source or URL.

use crate::clients::Caller;
use crate::deck::{self, ApprovalView};
use crate::modes::{HoldDone, HoldValid};
use crate::{Agent, lock};
use anyhow::{Result, bail};
use novagear_core::modes::{ApprovalSlots, SlotRole};
use novagear_core::{Action, KeyBinding};
use novagear_drivers::streamdeck::{Command, Input, STRIP_W};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

pub const TITLE_MAX: usize = 120;
pub const LINES_MAX: usize = 3;
pub const LINE_MAX: usize = 160;
const SOURCE_MAX: usize = 64;
const URL_MAX: usize = 2048;
pub const EXPIRES_DEFAULT_S: u64 = 600;
pub const EXPIRES_MAX_S: u64 = 3600;
/// How long a finished request can still be asked about, so a slow waiter gets its answer.
const FINISHED_KEEP: Duration = Duration::from_secs(600);

/// Approve needs the key held this long, with a bar filling across it.
pub(crate) const APPROVE_HOLD: Duration = Duration::from_millis(1000);
/// A press that began before the pop-up appeared, or within this long of it appearing, never
/// counts as Approve.
pub(crate) const PRESS_GUARD: Duration = Duration::from_millis(300);
/// With no deck input for this long the deck goes back to normal; the request stays waiting.
pub(crate) const POPUP_IDLE: Duration = Duration::from_secs(30);

/// Who a request is recorded against when the agent token or a panel session sends it. Not
/// valid client names (those are `[a-z][a-z0-9-]*`), so no client can be mistaken for them.
const OWNER_AGENT: &str = "(agent)";
const OWNER_PANEL: &str = "(panel)";

/// The name a request is filed under for this caller.
pub fn owner_name(caller: &Caller) -> String {
    match caller {
        Caller::Client(c) => c.clone(),
        Caller::Agent => OWNER_AGENT.into(),
        Caller::Panel => OWNER_PANEL.into(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    #[default]
    Low,
    High,
}

impl Risk {
    fn as_str(self) -> &'static str {
        match self {
            Risk::Low => "low",
            Risk::High => "high",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Approved,
    Denied,
    Expired,
    Cancelled,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Approved => "approved",
            Outcome::Denied => "denied",
            Outcome::Expired => "expired",
            Outcome::Cancelled => "cancelled",
        }
    }
}

/// What a client or the panel sends to `POST /approvals`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewApproval {
    pub title: String,
    #[serde(default)]
    pub lines: Vec<String>,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub risk: Risk,
    pub details_url: Option<String>,
    pub expires_in_s: Option<u64>,
}

fn plain(s: &str) -> bool {
    !s.chars().any(char::is_control)
}

impl NewApproval {
    /// Why the request is refused, in words that never repeat what was sent.
    pub fn validate(&self) -> Result<(), &'static str> {
        let n = self.title.chars().count();
        if n == 0 || n > TITLE_MAX || self.title.trim().is_empty() {
            return Err("title is 1-120 characters");
        }
        if self.lines.len() > LINES_MAX || self.lines.iter().any(|l| l.chars().count() > LINE_MAX) {
            return Err("at most 3 lines of 160 characters");
        }
        if self.source.chars().count() > SOURCE_MAX {
            return Err("source is at most 64 characters");
        }
        if self
            .details_url
            .as_ref()
            .is_some_and(|u| u.len() > URL_MAX || !plain(u))
        {
            return Err("details_url is at most 2048 characters");
        }
        // Control characters (a newline in a title) would draw as junk on a pop-up that
        // someone is about to decide on.
        if !plain(&self.title) || !plain(&self.source) || !self.lines.iter().all(|l| plain(l)) {
            return Err("text can't hold control characters");
        }
        if self
            .expires_in_s
            .is_some_and(|s| !(1..=EXPIRES_MAX_S).contains(&s))
        {
            return Err("expires_in_s is 1-3600");
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct Approval {
    pub id: String,
    pub client: String,
    pub title: String,
    pub lines: Vec<String>,
    pub source: String,
    pub risk: Risk,
    pub details_url: Option<String>,
    pub created: Instant,
    pub expires: Instant,
    pub outcome: Option<Outcome>,
}

impl Approval {
    pub fn status(&self) -> &'static str {
        self.outcome.map_or("waiting", Outcome::as_str)
    }

    /// What the pop-up and the panel call its source: the authenticated client first, so a
    /// program can't pass itself off as another by choosing its own `source`.
    pub fn source_line(&self) -> String {
        if self.source.is_empty() {
            self.client.clone()
        } else {
            format!("{}: {}", self.client, self.source)
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum CreateError {
    Invalid(&'static str),
    /// The caller already has the most it may have waiting.
    TooMany,
    /// The client was removed after its request was authenticated.
    Gone,
    /// The client's one-per-5-seconds rate.
    Rate,
    Internal,
}

#[derive(Debug, PartialEq)]
pub enum LookupError {
    NotFound,
    /// It exists, but belongs to someone else.
    Forbidden,
}

struct Slot {
    a: Approval,
    tx: watch::Sender<Option<Outcome>>,
    finished: Option<Instant>,
}

/// The requests, oldest first.
#[derive(Default)]
pub struct Approvals {
    slots: Vec<Slot>,
}

impl Approvals {
    pub fn new() -> Approvals {
        Approvals::default()
    }

    /// Adds a waiting request and returns its id.
    pub fn create(&mut self, client: &str, req: NewApproval) -> Result<String, CreateError> {
        req.validate().map_err(CreateError::Invalid)?;
        let waiting = self
            .slots
            .iter()
            .filter(|s| s.a.client == client && s.a.outcome.is_none())
            .count();
        if waiting >= crate::clients::MAX_WAITING_APPROVALS {
            return Err(CreateError::TooMany);
        }
        let id = novagear_core::proof::new_nonce().map_err(|_| CreateError::Internal)?;
        let now = Instant::now();
        let secs = req.expires_in_s.unwrap_or(EXPIRES_DEFAULT_S);
        let a = Approval {
            id: id.clone(),
            client: client.to_string(),
            title: req.title,
            lines: req.lines,
            source: req.source,
            risk: req.risk,
            details_url: req.details_url,
            created: now,
            expires: now + Duration::from_secs(secs),
            outcome: None,
        };
        log_outcome(&a, "requested");
        self.slots.push(Slot {
            a,
            tx: watch::channel(None).0,
            finished: None,
        });
        Ok(id)
    }

    pub fn get(&self, id: &str) -> Option<&Approval> {
        self.slots.iter().map(|s| &s.a).find(|a| a.id == id)
    }

    /// How many requests `client` has waiting.
    pub fn waiting_for(&self, client: &str) -> usize {
        self.slots
            .iter()
            .filter(|s| s.a.client == client && s.a.outcome.is_none())
            .count()
    }

    /// The waiting requests, oldest first.
    pub fn pending(&self) -> Vec<&Approval> {
        self.slots
            .iter()
            .map(|s| &s.a)
            .filter(|a| a.outcome.is_none())
            .collect()
    }

    /// A receiver that holds `Some(outcome)` once the request is decided.
    pub fn wait(&self, id: &str) -> Option<watch::Receiver<Option<Outcome>>> {
        self.slots
            .iter()
            .find(|s| s.a.id == id)
            .map(|s| s.tx.subscribe())
    }

    fn finish(&mut self, i: usize, outcome: Outcome) -> bool {
        let s = &mut self.slots[i];
        if s.a.outcome.is_some() {
            return false;
        }
        s.a.outcome = Some(outcome);
        s.finished = Some(Instant::now());
        // `send_replace`: it must land even when nobody is waiting yet.
        s.tx.send_replace(Some(outcome));
        log_outcome(&s.a, outcome.as_str());
        true
    }

    fn index_of(&self, id: &str) -> Option<usize> {
        self.slots.iter().position(|s| s.a.id == id)
    }

    /// Denies a waiting request (the panel, the popover or the deck's Deny).
    pub fn deny(&mut self, id: &str) -> Result<()> {
        match self.index_of(id) {
            Some(i) if self.finish(i, Outcome::Denied) => Ok(()),
            Some(_) => bail!("it is no longer waiting"),
            None => bail!("no such approval"),
        }
    }

    /// The asker takes its own request back. Cancelling one that is already decided leaves
    /// it as it was.
    pub fn cancel(&mut self, client: &str, id: &str) -> Result<Outcome, LookupError> {
        let i = self.index_of(id).ok_or(LookupError::NotFound)?;
        if self.slots[i].a.client != client {
            return Err(LookupError::Forbidden);
        }
        self.finish(i, Outcome::Cancelled);
        Ok(self.slots[i].a.outcome.unwrap_or(Outcome::Cancelled))
    }

    /// Cancels everything one client has waiting (it was removed). Returns the ids.
    pub fn cancel_all_of(&mut self, client: &str) -> Vec<String> {
        let mut ids = Vec::new();
        for i in 0..self.slots.len() {
            if self.slots[i].a.client == client && self.finish(i, Outcome::Cancelled) {
                ids.push(self.slots[i].a.id.clone());
            }
        }
        ids
    }

    /// Expires what is due and forgets what finished long ago. Returns the expired ids.
    pub fn expire_due(&mut self, now: Instant) -> Vec<String> {
        let mut ids = Vec::new();
        for i in 0..self.slots.len() {
            if self.slots[i].a.expires <= now && self.finish(i, Outcome::Expired) {
                ids.push(self.slots[i].a.id.clone());
            }
        }
        self.slots.retain(|s| {
            s.finished
                .is_none_or(|f| now.saturating_duration_since(f) < FINISHED_KEEP)
        });
        ids
    }

    /// Says yes. **Only the deck's Approve hold calls this**; no route, action or panel
    /// control reaches it (`no_route_can_approve` and the source scan below prove it).
    pub(crate) fn approve_from_deck(&mut self, id: &str) -> Result<()> {
        let Some(i) = self.index_of(id) else {
            bail!("no such approval");
        };
        if self.slots[i].a.outcome.is_some() {
            bail!("it is no longer waiting");
        }
        if self.slots[i].a.expires <= Instant::now() {
            self.finish(i, Outcome::Expired);
            bail!("it expired");
        }
        self.finish(i, Outcome::Approved);
        Ok(())
    }
}

/// An agent restart (or the store dropping) ends what is waiting as `cancelled`, so anyone
/// waiting is told rather than left hanging.
impl Drop for Approvals {
    fn drop(&mut self) {
        for i in 0..self.slots.len() {
            self.finish(i, Outcome::Cancelled);
        }
    }
}

/// The only line approvals ever log: ids, the client's name, risk and outcome.
fn log_outcome(a: &Approval, outcome: &str) {
    tracing::info!(
        "approval {} from {} risk={}: {}",
        a.id,
        a.client,
        a.risk.as_str(),
        outcome
    );
}

// ---- the pop-up ----------------------------------------------------------------------

/// What the deck shows for one key while the pop-up is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Pop {
    /// No pop-up: the key is as configured.
    #[default]
    Off,
    /// Outside the slots: drawn dimmed.
    Dim,
    /// The Approvals key, with how many are waiting.
    Approvals(usize),
    Details,
    Approve,
    Deny,
}

/// The face of a borrowed key. Approve's bar is the hold's progress, drawn by the key.
pub(crate) fn borrowed_key(pop: Pop) -> Option<KeyBinding> {
    use novagear_core::modes::{Face, TextSource};
    let (label, color, badge) = match pop {
        Pop::Off | Pop::Dim => return None,
        Pop::Approvals(n) => ("Approvals", "#92400e", Some(n.to_string())),
        Pop::Details => ("Details", "#334155", None),
        Pop::Approve => ("Approve", "#15803d", None),
        Pop::Deny => ("Deny", "#b91c1c", None),
    };
    Some(KeyBinding {
        label: label.into(),
        color: color.into(),
        icon: None,
        face: Face {
            badge: badge.map(TextSource::Literal),
            ..Face::default()
        },
        states: None,
        follow: None,
        hold_ms: 0,
        // Never run: input during the pop-up is handled by `popup_input`.
        action: Action::Placeholder {
            note: String::new(),
        },
    })
}

/// The request the pop-up shows, and from when.
#[derive(Debug, Clone)]
pub(crate) struct Showing {
    pub id: String,
    /// When this request (in this mode) appeared; a press must come 300 ms after.
    pub since: Instant,
    /// Its place among the waiting ones, for "APPROVAL 2 OF 3".
    pub index: usize,
    mode: String,
}

#[derive(Default)]
pub(crate) struct Popup {
    pub showing: Option<Showing>,
    /// The deck was idle for 30 s: no pop-up until the next approval change or the
    /// Approvals key.
    pub dismissed: bool,
    /// The request a knob turn stepped to; the oldest shows otherwise.
    pub sel: Option<String>,
    /// How many are waiting.
    pub count: usize,
    pub idle_at: Option<Instant>,
}

impl Popup {
    pub(crate) fn pop_for(&self, slots: Option<&ApprovalSlots>, i: usize) -> Pop {
        let (Some(_), Some(s)) = (&self.showing, slots) else {
            return Pop::Off;
        };
        // The same `role` that `popup_input` acts on.
        match s.role(i) {
            Some(SlotRole::Approvals) => Pop::Approvals(self.count),
            Some(SlotRole::Details) => Pop::Details,
            Some(SlotRole::Approve) => Pop::Approve,
            Some(SlotRole::Deny) => Pop::Deny,
            None => Pop::Dim,
        }
    }

    pub(crate) fn idle_expired(&self, now: Instant) -> bool {
        self.showing.is_some() && self.idle_at.is_some_and(|t| now >= t)
    }
}

impl Agent {
    // ---- the store, with its side effects ----

    /// Tells the panel, the `approvals.pending` feed and the deck that something changed.
    fn approvals_changed(&self, id: &str, status: &str) {
        self.emit("approval", json!({ "id": id, "status": status }));
        let n = lock(&self.approvals).pending().len();
        self.feeds.set(
            "approvals.pending",
            crate::feeds::FeedValue::Number(n as f64),
            None,
            (n > 0).then_some(crate::feeds::FeedState::Warn),
            crate::feeds::FOREVER,
        );
        // Any change brings a dismissed pop-up back.
        lock(&self.ui).popup.dismissed = false;
        if self.sync_popup() {
            self.redraw_deck();
        }
    }

    /// Makes a request. A client is re-checked under the clients lock, so one removed after
    /// its request was authenticated can't leave an approval behind.
    pub fn create_approval(
        self: &Arc<Self>,
        caller: &Caller,
        req: NewApproval,
    ) -> Result<String, CreateError> {
        req.validate().map_err(CreateError::Invalid)?;
        let owner = owner_name(caller);
        let clients = lock(&self.clients);
        if let Caller::Client(c) = caller {
            if !clients.exists(c) {
                return Err(CreateError::Gone);
            }
            // The cap first: a refusal for too many waiting must not spend the allowance.
            if lock(&self.approvals).waiting_for(&owner) >= crate::clients::MAX_WAITING_APPROVALS {
                return Err(CreateError::TooMany);
            }
            if !lock(&self.limits).allow_approval(c) {
                return Err(CreateError::Rate);
            }
        }
        let made = {
            let mut ap = lock(&self.approvals);
            ap.create(&owner, req)
                .map(|id| (ap.get(&id).map(|a| a.expires), id))
        };
        drop(clients);
        let (expires, id) = made?;
        self.approvals_changed(&id, "waiting");
        if let Some(at) = expires {
            let weak = Arc::downgrade(self);
            tokio::spawn(async move {
                tokio::time::sleep_until(at).await;
                if let Some(a) = weak.upgrade() {
                    a.sweep_approvals();
                }
            });
        }
        Ok(id)
    }

    /// Expires what is due.
    pub(crate) fn sweep_approvals(&self) {
        let expired = lock(&self.approvals).expire_due(Instant::now());
        for id in expired {
            self.approvals_changed(&id, "expired");
        }
    }

    pub fn deny_approval(&self, id: &str) -> Result<()> {
        lock(&self.approvals).deny(id)?;
        self.approvals_changed(id, "denied");
        Ok(())
    }

    /// The asker cancels its own request.
    pub fn cancel_approval(&self, owner: &str, id: &str) -> Result<Outcome, LookupError> {
        let before = lock(&self.approvals).get(id).and_then(|a| a.outcome);
        let outcome = lock(&self.approvals).cancel(owner, id)?;
        if before.is_none() {
            self.approvals_changed(id, outcome.as_str());
        }
        Ok(outcome)
    }

    /// Cancels what a removed client had waiting. Called with the clients lock held, so
    /// the ids come back for `approvals_gone` to announce once it is released.
    pub(crate) fn cancel_client_approvals(&self, client: &str) -> Vec<String> {
        lock(&self.approvals).cancel_all_of(client)
    }

    pub(crate) fn approvals_gone(&self, ids: Vec<String>) {
        for id in ids {
            self.approvals_changed(&id, "cancelled");
        }
    }

    /// The status of `owner`'s request, after waiting up to `wait` for it to be decided.
    pub async fn approval_status(
        &self,
        owner: &str,
        id: &str,
        wait: Duration,
    ) -> Result<&'static str, LookupError> {
        let mut rx = {
            let ap = lock(&self.approvals);
            let a = ap.get(id).ok_or(LookupError::NotFound)?;
            if a.client != owner {
                return Err(LookupError::Forbidden);
            }
            ap.wait(id).ok_or(LookupError::NotFound)?
        };
        if !wait.is_zero() && rx.borrow().is_none() {
            // A closed channel (the store dropped) ends the wait early too.
            let _ = tokio::time::timeout(wait, rx.wait_for(Option::is_some)).await;
        }
        let status = rx.borrow().map_or("waiting", Outcome::as_str);
        Ok(status)
    }

    /// The waiting requests with their text, for the panel (staff only).
    pub fn pending_approvals(&self) -> Vec<serde_json::Value> {
        let now = Instant::now();
        lock(&self.approvals)
            .pending()
            .into_iter()
            .map(|a| {
                json!({
                    "id": a.id,
                    "client": a.client,
                    "title": a.title,
                    "lines": a.lines,
                    "source": a.source,
                    "risk": a.risk.as_str(),
                    "details_url": a.details_url,
                    "status": a.status(),
                    "age_s": now.saturating_duration_since(a.created).as_secs(),
                    "expires_in_s": a.expires.saturating_duration_since(now).as_secs(),
                })
            })
            .collect()
    }

    /// `/status`'s `approvals`: how many wait, and (staff only) the oldest.
    pub fn approvals_status(&self) -> serde_json::Value {
        let ap = lock(&self.approvals);
        let pending = ap.pending();
        let top = pending
            .first()
            .map(|a| json!({ "id": a.id, "title": a.title, "source": a.source_line() }));
        json!({ "pending": pending.len(), "top": top })
    }

    /// Says yes to a request. Only the deck's Approve hold reaches this.
    pub(crate) fn approve_from_deck(&self, id: &str) -> Result<()> {
        lock(&self.approvals).approve_from_deck(id)?;
        self.approvals_changed(id, "approved");
        Ok(())
    }

    // ---- the pop-up's state ----

    /// Decides whether the pop-up shows, which request, and since when; cancels holds when
    /// what is on screen changes under them. Returns whether the deck needs redrawing.
    // Lock order: the approvals lock is a leaf, taken after `ui` and never held while taking
    // anything else; `clients` comes before it.
    pub(crate) fn sync_popup(&self) -> bool {
        let cfg = self.config_arc();
        let (mode, _) = self.view(&cfg);
        // A sleeping deck never shows it (the badge and feed still count); on wake it may.
        let allowed = cfg.deck_control
            && !self.asleep()
            && lock(&self.deck).commands.is_some()
            && mode.is_some_and(|m| m.popups && m.approval_slots.is_some());
        let mode_id = mode.map(|m| m.id.clone()).unwrap_or_default();
        let ids: Vec<String> = lock(&self.approvals)
            .pending()
            .iter()
            .map(|a| a.id.clone())
            .collect();
        let now = Instant::now();
        let mut ui = lock(&self.ui);
        let before = (
            ui.popup.showing.as_ref().map(|s| (s.id.clone(), s.index)),
            ui.popup.count,
        );
        ui.popup.count = ids.len();
        if !(allowed && !ids.is_empty() && ui.picker.is_none() && !ui.popup.dismissed) {
            ui.popup.idle_at = None;
            ui.popup.sel = None;
            let had = ui.popup.showing.take().is_some();
            if had {
                ui.cancel_holds();
            }
            return had;
        }
        if ui.popup.sel.as_ref().is_some_and(|s| !ids.contains(s)) {
            ui.popup.sel = None;
        }
        let at = ui
            .popup
            .sel
            .as_ref()
            .and_then(|s| ids.iter().position(|i| i == s))
            .unwrap_or(0);
        let same = ui
            .popup
            .showing
            .as_ref()
            .is_some_and(|s| s.id == ids[at] && s.mode == mode_id);
        if same {
            if let Some(s) = ui.popup.showing.as_mut() {
                s.index = at;
            }
        } else {
            // A different request or mode: a press already down was meant for what was
            // there before, so the guard starts again and no hold carries over.
            ui.popup.showing = Some(Showing {
                id: ids[at].clone(),
                since: now,
                index: at,
                mode: mode_id,
            });
            ui.popup.idle_at = Some(now + POPUP_IDLE);
            ui.cancel_holds();
        }
        let after = (
            ui.popup.showing.as_ref().map(|s| (s.id.clone(), s.index)),
            ui.popup.count,
        );
        before != after
    }

    /// The strip while the pop-up shows: one amber panel with the request's text.
    pub(crate) fn popup_strip(&self, popup: &Popup) -> Option<Command> {
        let s = popup.showing.as_ref()?;
        #[cfg(test)]
        if self
            .popup_draw_fails
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            tracing::warn!("drawing the approval pop-up failed");
            return None;
        }
        let view = {
            let ap = lock(&self.approvals);
            let a = ap.get(&s.id).filter(|a| a.outcome.is_none())?;
            ApprovalView {
                title: a.title.clone(),
                lines: a.lines.clone(),
                source: a.source_line(),
                index: s.index,
                total: popup.count,
            }
        };
        match deck::popup_strip_jpeg(&view) {
            Ok(jpeg) => Some(Command::StripImage {
                x: 0,
                width: STRIP_W as u16,
                jpeg,
            }),
            Err(_) => {
                // Not the error: it could quote the text.
                tracing::warn!("drawing the approval pop-up failed");
                None
            }
        }
    }

    /// Whether the pop-up still shows `id` and it is still waiting.
    fn popup_shows(&self, id: &str) -> bool {
        let ui = lock(&self.ui);
        ui.popup.showing.as_ref().is_some_and(|s| s.id == id) && self.is_waiting(id)
    }

    fn is_waiting(&self, id: &str) -> bool {
        lock(&self.approvals)
            .get(id)
            .is_some_and(|a| a.outcome.is_none())
    }

    /// Knob 1 during the pop-up: steps through the waiting requests, wrapping.
    pub(crate) fn approval_step(&self, step: i16) -> Result<()> {
        {
            let mut ui = lock(&self.ui);
            let Some(cur) = ui.popup.showing.as_ref().map(|s| s.id.clone()) else {
                bail!("no approval is showing");
            };
            let ids: Vec<String> = lock(&self.approvals)
                .pending()
                .iter()
                .map(|a| a.id.clone())
                .collect();
            let n = ids.len() as i64;
            if n == 0 {
                bail!("nothing is waiting");
            }
            let at = ids.iter().position(|i| *i == cur).unwrap_or(0) as i64;
            let to = (at + i64::from(step)).rem_euclid(n) as usize;
            ui.popup.sel = Some(ids[to].clone());
        }
        if self.sync_popup() {
            self.redraw_deck();
        }
        Ok(())
    }

    /// Opens the request's `https` details link, or the panel at its approval.
    fn show_details(&self, id: &str) {
        let url = lock(&self.approvals)
            .get(id)
            .and_then(|a| a.details_url.clone())
            .filter(|u| {
                u.get(..8)
                    .is_some_and(|p| p.eq_ignore_ascii_case("https://"))
            })
            .filter(|u| crate::system::safe_url(u));
        let url = match url {
            Some(u) => u,
            None => {
                let Ok(code) = lock(&self.panel).issue_code(std::time::Instant::now()) else {
                    tracing::warn!("opening the panel for an approval failed");
                    return;
                };
                format!("{}/#code={code}&approvals/{id}", novagear_core::api_base())
            }
        };
        self.open_link(url);
    }

    #[cfg(not(test))]
    fn open_link(&self, url: String) {
        // Never the address: it can carry a secret in its query.
        std::thread::spawn(move || {
            if crate::system::open_url(&url).is_err() {
                tracing::warn!("opening an approval's details failed");
            }
        });
    }

    /// Tests record the link instead of opening a browser.
    #[cfg(test)]
    fn open_link(&self, url: String) {
        lock(&self.opened).push(url);
    }

    // ---- deck input ----

    /// Input while the pop-up shows (or the Approvals key that brings it back). Returns
    /// whether it was used. While it shows everything is used: keys outside the four slots,
    /// the strip and the dials do nothing (dial 0 steps).
    pub(crate) async fn popup_input(
        self: &Arc<Self>,
        cfg: &novagear_core::config::Config,
        input: &Input,
        at: Instant,
    ) -> bool {
        let (mode, _) = self.view(cfg);
        let Some(slots) = mode
            .filter(|m| m.popups)
            .and_then(|m| m.approval_slots.clone())
        else {
            return false;
        };
        let now = Instant::now();
        let showing = {
            let mut ui = lock(&self.ui);
            if ui.popup.showing.is_some() {
                ui.popup.idle_at = Some(now + POPUP_IDLE);
            }
            ui.popup.showing.clone()
        };
        let Some(showing) = showing else {
            // Dismissed by idling: the Approvals key brings it back.
            if let Input::Key { index, down: true } = input
                && slots.role(*index) == Some(SlotRole::Approvals)
                && !lock(&self.approvals).pending().is_empty()
            {
                lock(&self.ui).popup.dismissed = false;
                if self.sync_popup() {
                    self.redraw_deck();
                    return true;
                }
            }
            return false;
        };
        match input {
            // The same `role` that `pop_for` draws from: a key does what it says.
            Input::Key { index, down } => {
                match slots.role(*index) {
                    Some(SlotRole::Approve) => {
                        if *down {
                            self.start_approve(*index, &showing, at);
                        } else {
                            self.release_hold(*index, at).await;
                        }
                    }
                    Some(SlotRole::Deny) => {
                        // Same guard as Approve: a press already down when the pop-up (or its
                        // request) appeared was not meant for it.
                        if *down
                            && at >= showing.since + PRESS_GUARD
                            && let Err(e) = self.deny_approval(&showing.id)
                        {
                            tracing::info!("deny from the deck: {}", crate::api::error_kind(&e));
                        }
                    }
                    Some(SlotRole::Details) if *down => self.show_details(&showing.id),
                    // Every other key is swallowed: dimmed reads as inactive, and an accidental
                    // fire (a bump on Record) is the anti-goal. Out: Deny, the 30 s idle return,
                    // or the request resolving.
                    _ => {}
                }
                true
            }
            Input::DialTurn { index: 0, detents } => {
                let step = Action::ApprovalStep { step: 1 }.scaled(i16::from(*detents));
                if let Err(e) = self.run(step).await {
                    tracing::info!("approval step: {}", crate::api::error_kind(&e));
                }
                true
            }
            // The strip shows only the pop-up, so what is under it is not touched blind.
            _ => true,
        }
    }

    /// Approve went down at `down_at` (the deck's stamp): starts the 1 s hold from then,
    /// unless the press is too early to trust.
    fn start_approve(self: &Arc<Self>, index: usize, showing: &Showing, down_at: Instant) {
        let guard_until = showing.since + PRESS_GUARD;
        if down_at < guard_until || lock(&self.ui).holding(index) {
            return;
        }
        let id = showing.id.clone();
        let valid: HoldValid = {
            let (me, id) = (self.clone(), id.clone());
            Arc::new(move || me.approve_wanted(&id, index, guard_until))
        };
        let me = self.clone();
        let done: HoldDone = Box::new(move || {
            Box::pin(async move {
                // The last look before saying yes: still this request, still waiting.
                if me.popup_shows(&id)
                    && let Err(e) = me.approve_from_deck(&id)
                {
                    tracing::info!("approve from the deck: {}", crate::api::error_kind(&e));
                }
            })
        });
        self.start_hold(index, APPROVE_HOLD, valid, done, down_at);
    }

    /// The Approve hold's `valid` check: the pop-up still shows this request, it is still
    /// waiting, and the hold itself began after the guard.
    fn approve_wanted(&self, id: &str, index: usize, guard_until: Instant) -> bool {
        // The deck is still there. Taken and released before `ui`, so no lock nests.
        if lock(&self.deck).commands.is_none() {
            return false;
        }
        let ui = lock(&self.ui);
        ui.popup.showing.as_ref().is_some_and(|s| s.id == id)
            && ui.holding(index)
            && !ui.hold_began_before(index, guard_until)
            && self.is_waiting(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use novagear_core::Page;
    use novagear_core::config::Config;
    use novagear_core::modes::Mode;
    use std::path::PathBuf;
    use std::sync::mpsc;

    fn req(title: &str) -> NewApproval {
        NewApproval {
            title: title.into(),
            lines: vec![],
            source: String::new(),
            risk: Risk::Low,
            details_url: None,
            expires_in_s: None,
        }
    }

    #[test]
    fn create_validates() {
        let mut a = Approvals::new();
        assert!(a.create("c", req("Deploy?")).is_ok());
        for bad in [
            NewApproval {
                title: String::new(),
                ..req("x")
            },
            NewApproval {
                title: "   ".into(),
                ..req("x")
            },
            NewApproval {
                title: "x".repeat(121),
                ..req("x")
            },
            NewApproval {
                title: "a\nb".into(),
                ..req("x")
            },
            NewApproval {
                lines: vec!["a".into(); 4],
                ..req("x")
            },
            NewApproval {
                lines: vec!["x".repeat(161)],
                ..req("x")
            },
            NewApproval {
                source: "x".repeat(65),
                ..req("x")
            },
            NewApproval {
                expires_in_s: Some(0),
                ..req("x")
            },
            NewApproval {
                expires_in_s: Some(3601),
                ..req("x")
            },
        ] {
            assert!(matches!(a.create("c", bad), Err(CreateError::Invalid(_))));
        }
        let max = NewApproval {
            title: "x".repeat(120),
            lines: vec!["y".repeat(160); 3],
            expires_in_s: Some(3600),
            ..req("x")
        };
        assert!(a.create("c", max).is_ok());
        // Multi-byte text counts characters, not bytes.
        assert!(a.create("c", req(&"é".repeat(120))).is_ok());
        let parsed: Result<NewApproval, _> =
            serde_json::from_str(r#"{"title":"t","status":"approved"}"#);
        assert!(parsed.is_err(), "unknown fields are refused");
    }

    #[tokio::test(start_paused = true)]
    async fn pending_is_oldest_first_and_waiting_is_capped() {
        let mut a = Approvals::new();
        let first = a.create("c", req("one")).unwrap();
        tokio::time::advance(Duration::from_secs(1)).await;
        let second = a.create("c", req("two")).unwrap();
        let ids: Vec<_> = a.pending().iter().map(|p| p.id.clone()).collect();
        assert_eq!(ids, [first.clone(), second]);
        for _ in 0..18 {
            a.create("c", req("n")).unwrap();
        }
        assert_eq!(a.create("c", req("21st")), Err(CreateError::TooMany));
        // Another client has its own 20; a decided request frees a place.
        assert!(a.create("d", req("other")).is_ok());
        a.deny(&first).unwrap();
        assert!(a.create("c", req("again")).is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn expiry_cancel_and_deny() {
        let mut a = Approvals::new();
        let short = a
            .create(
                "c",
                NewApproval {
                    expires_in_s: Some(5),
                    ..req("s")
                },
            )
            .unwrap();
        let mine = a.create("c", req("m")).unwrap();
        let theirs = a.create("d", req("t")).unwrap();
        tokio::time::advance(Duration::from_secs(4)).await;
        assert!(a.expire_due(Instant::now()).is_empty());
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(a.expire_due(Instant::now()), std::slice::from_ref(&short));
        assert_eq!(a.get(&short).unwrap().status(), "expired");
        assert_eq!(a.cancel("c", &theirs), Err(LookupError::Forbidden));
        assert_eq!(a.get(&theirs).unwrap().status(), "waiting");
        assert_eq!(a.cancel("c", "nope"), Err(LookupError::NotFound));
        assert_eq!(a.cancel("c", &mine), Ok(Outcome::Cancelled));
        // A decided request stays as it was.
        assert_eq!(a.cancel("c", &short), Ok(Outcome::Expired));
        assert!(a.deny(&mine).is_err());
        a.deny(&theirs).unwrap();
        // Finished ones are forgotten after ten minutes.
        tokio::time::advance(Duration::from_secs(601)).await;
        a.expire_due(Instant::now());
        assert!(a.get(&mine).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn approve_from_deck_refuses_the_decided_and_the_expired() {
        let mut a = Approvals::new();
        let id = a
            .create(
                "c",
                NewApproval {
                    expires_in_s: Some(5),
                    ..req("x")
                },
            )
            .unwrap();
        let gone = a.create("c", req("y")).unwrap();
        a.deny(&gone).unwrap();
        assert!(a.approve_from_deck(&gone).is_err());
        assert!(a.approve_from_deck("nope").is_err());
        tokio::time::advance(Duration::from_secs(6)).await;
        assert!(a.approve_from_deck(&id).is_err());
        assert_eq!(a.get(&id).unwrap().status(), "expired");
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_store_cancels_what_waits() {
        let mut a = Approvals::new();
        let done = a.create("c", req("d")).unwrap();
        a.deny(&done).unwrap();
        let id = a.create("c", req("x")).unwrap();
        let rx = a.wait(&id).unwrap();
        let rx_done = a.wait(&done).unwrap();
        assert!(rx.borrow().is_none());
        drop(a);
        assert_eq!(*rx.borrow(), Some(Outcome::Cancelled));
        assert_eq!(
            *rx_done.borrow(),
            Some(Outcome::Denied),
            "decided ones keep their answer"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn wait_hears_the_outcome() {
        let mut a = Approvals::new();
        let id = a.create("c", req("x")).unwrap();
        let mut rx = a.wait(&id).unwrap();
        a.deny(&id).unwrap();
        rx.wait_for(Option::is_some).await.unwrap();
        assert_eq!(*rx.borrow(), Some(Outcome::Denied));
    }

    #[test]
    fn the_source_line_names_the_authenticated_client_first() {
        let mut a = Approvals::new();
        let id = a
            .create(
                "rogue",
                NewApproval {
                    source: "novaguard".into(),
                    ..req("x")
                },
            )
            .unwrap();
        assert_eq!(a.get(&id).unwrap().source_line(), "rogue: novaguard");
        let id = a.create("c", req("x")).unwrap();
        assert_eq!(a.get(&id).unwrap().source_line(), "c");
    }

    /// No route, action or panel code may reach `approve_from_deck`: only the deck hold in
    /// this file and the store itself mention it.
    #[test]
    fn nothing_but_the_deck_hold_names_approve_from_deck() {
        for (name, src) in [
            ("api.rs", include_str!("api.rs")),
            ("lib.rs", include_str!("lib.rs")),
            ("clients.rs", include_str!("clients.rs")),
            ("modes.rs", include_str!("modes.rs")),
            ("feeds.rs", include_str!("feeds.rs")),
            ("obs.rs", include_str!("obs.rs")),
            ("system.rs", include_str!("system.rs")),
        ] {
            assert!(
                !src.contains("approve_from_deck"),
                "{name} must not call approve_from_deck"
            );
        }
        let me = include_str!("approvals.rs");
        let body = me.split("#[cfg(test)]\nmod tests").next().unwrap();
        // The definition on the store, the definition on the agent, the one call in the
        // hold's completion, and comments.
        let calls: Vec<&str> = body
            .lines()
            .filter(|l| l.contains("approve_from_deck") && !l.trim_start().starts_with("//"))
            .collect();
        assert_eq!(calls.len(), 4, "{calls:?}");
    }

    // ---- the pop-up on a fake deck ----

    fn dir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("novagear-approvals-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn mode(id: &str, popups: bool) -> Mode {
        Mode {
            id: id.into(),
            name: id.to_uppercase(),
            pages: vec![Page {
                name: "p".into(),
                keys: Vec::new(),
                dials: Vec::new(),
            }],
            popups,
            approval_slots: Some(ApprovalSlots {
                approvals: 0,
                details: 1,
                approve: 2,
                deny: 3,
            }),
            ..Mode::default()
        }
    }

    fn agent(tag: &str, modes: Vec<Mode>) -> (Arc<Agent>, mpsc::Receiver<Command>) {
        let d = dir(tag);
        let cfg = Config {
            start_mode: modes[0].id.clone(),
            modes,
            deck_control: true,
            ..Config::default()
        };
        cfg.save(&d.join("config.json")).unwrap();
        let a = Agent::new(d.join("config.json"), "t".repeat(48)).unwrap();
        let (tx, rx) = mpsc::channel();
        lock(&a.deck).commands = Some(tx);
        a.redraw_deck();
        tokio::spawn(a.clone().redraw_loop());
        (a, rx)
    }

    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    async fn pass(ms: u64) {
        for _ in 0..ms / 100 {
            tokio::time::advance(Duration::from_millis(100)).await;
            settle().await;
        }
    }

    fn ask(a: &Arc<Agent>, title: &str) -> String {
        a.create_approval(&Caller::Agent, req(title)).unwrap()
    }

    fn key(index: usize, down: bool) -> Input {
        Input::Key { index, down }
    }

    fn status(a: &Agent, id: &str) -> &'static str {
        lock(&a.approvals).get(id).unwrap().status()
    }

    fn strips(rx: &mpsc::Receiver<Command>) -> usize {
        let mut n = 0;
        while let Ok(c) = rx.try_recv() {
            if matches!(c, Command::StripImage { .. }) {
                n += 1;
            }
        }
        n
    }

    fn showing(a: &Agent) -> Option<String> {
        lock(&a.ui).popup.showing.as_ref().map(|s| s.id.clone())
    }

    #[tokio::test(start_paused = true)]
    async fn hold_approves_only_after_one_second() {
        let (a, _rx) = agent("hold", vec![mode("dev", true)]);
        let id = ask(&a, "Deploy?");
        assert_eq!(showing(&a), Some(id.clone()));
        pass(400).await;
        a.on_deck_input(key(2, true)).await;
        pass(900).await;
        a.on_deck_input(key(2, false)).await;
        pass(500).await;
        assert_eq!(status(&a, &id), "waiting", "released at 900 ms");

        // Released at 999 ms: still not.
        a.on_deck_input(key(2, true)).await;
        pass(900).await;
        tokio::time::advance(Duration::from_millis(99)).await;
        a.on_deck_input(key(2, false)).await;
        pass(300).await;
        assert_eq!(status(&a, &id), "waiting", "released at 999 ms");

        // Released exactly at 1000 ms, before the waiting task has woken.
        a.on_deck_input(key(2, true)).await;
        tokio::time::advance(Duration::from_millis(1000)).await;
        a.on_deck_input(key(2, false)).await;
        pass(300).await;
        assert_eq!(status(&a, &id), "approved");
        assert_eq!(
            a.feeds.get("approvals.pending").unwrap().value,
            crate::feeds::FeedValue::Number(0.0)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_hold_kept_down_approves_at_the_deadline() {
        let (a, _rx) = agent("held", vec![mode("dev", true)]);
        let id = ask(&a, "Deploy?");
        pass(400).await;
        a.on_deck_input(key(2, true)).await;
        pass(1200).await;
        assert_eq!(status(&a, &id), "approved");
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// The runtime stalls for 1.5 s: a tap (down at 0, up at 400 ms, as the deck read them)
    /// is processed late. The hold task wakes late first, then the release is processed.
    #[tokio::test(start_paused = true)]
    async fn a_late_tap_never_approves_when_the_timer_wakes_first() {
        let (a, _rx) = agent("late-tap-timer", vec![mode("dev", true)]);
        let id = ask(&a, "Deploy?");
        pass(400).await;
        let t0 = Instant::now();
        a.on_deck_input_at(key(2, true), t0).await;
        // The deck's thread read the release at 400 ms, while the agent was stuck.
        a.deck_read(&key(2, false), t0 + ms(400));
        tokio::time::advance(ms(1500)).await;
        settle().await;
        a.on_deck_input_at(key(2, false), t0 + ms(400)).await;
        pass(1000).await;
        assert_eq!(status(&a, &id), "waiting");
    }

    /// Same stall, but the deck's thread was behind too: nothing has noted the release when
    /// the timer wakes late. The late wake alone must not complete the hold.
    #[tokio::test(start_paused = true)]
    async fn a_late_timer_wake_does_not_complete_a_hold() {
        let (a, _rx) = agent("late-wake", vec![mode("dev", true)]);
        let id = ask(&a, "Deploy?");
        pass(400).await;
        let t0 = Instant::now();
        a.on_deck_input_at(key(2, true), t0).await;
        tokio::time::advance(ms(1500)).await;
        settle().await;
        assert_eq!(status(&a, &id), "waiting", "the late wake left it");
        a.deck_read(&key(2, false), t0 + ms(400));
        a.on_deck_input_at(key(2, false), t0 + ms(400)).await;
        pass(1000).await;
        assert_eq!(status(&a, &id), "waiting");
    }

    /// Both inputs are processed at 1.5 s, the release first.
    #[tokio::test(start_paused = true)]
    async fn a_late_tap_never_approves_when_the_release_comes_first() {
        let (a, _rx) = agent("late-tap-release", vec![mode("dev", true)]);
        let id = ask(&a, "Deploy?");
        pass(400).await;
        let t0 = Instant::now();
        a.deck_read(&key(2, true), t0);
        a.deck_read(&key(2, false), t0 + ms(400));
        tokio::time::advance(ms(1500)).await;
        a.on_deck_input_at(key(2, true), t0).await;
        a.on_deck_input_at(key(2, false), t0 + ms(400)).await;
        pass(1000).await;
        assert_eq!(status(&a, &id), "waiting");

        // The key-down processed late, its hold task ticking before the release arrives.
        let t1 = Instant::now();
        a.deck_read(&key(2, false), t1 + ms(400));
        tokio::time::advance(ms(1500)).await;
        a.on_deck_input_at(key(2, true), t1).await;
        pass(300).await;
        a.on_deck_input_at(key(2, false), t1 + ms(400)).await;
        pass(1000).await;
        assert_eq!(status(&a, &id), "waiting");
    }

    /// A real hold still approves through a stall: by its stamped release, or, held on, at
    /// the next tick that comes on time.
    #[tokio::test(start_paused = true)]
    async fn a_real_hold_approves_through_a_stall() {
        let (a, _rx) = agent("stall-hold", vec![mode("dev", true)]);
        let id = ask(&a, "Deploy?");
        pass(400).await;
        let t0 = Instant::now();
        a.on_deck_input_at(key(2, true), t0).await;
        tokio::time::advance(ms(1500)).await;
        settle().await;
        a.deck_read(&key(2, false), t0 + ms(1200));
        a.on_deck_input_at(key(2, false), t0 + ms(1200)).await;
        settle().await;
        assert_eq!(
            status(&a, &id),
            "approved",
            "released after the full second"
        );

        let id = ask(&a, "Again?");
        pass(400).await;
        a.on_deck_input(key(2, true)).await;
        tokio::time::advance(ms(1500)).await;
        settle().await;
        assert_eq!(status(&a, &id), "waiting", "not on the late wake");
        pass(200).await;
        assert_eq!(status(&a, &id), "approved", "still down at an on-time tick");
    }

    /// down@0, up@400, down@900, up@1100 are all noted by the deck's thread while the agent
    /// is stalled. The 1100 ms release belongs to the second press, not to the hold.
    #[tokio::test(start_paused = true)]
    async fn a_release_is_paired_with_its_own_press() {
        for timer_first in [true, false] {
            let (a, _rx) = agent("paired", vec![mode("dev", true)]);
            let id = ask(&a, "Deploy?");
            pass(400).await;
            let t0 = Instant::now();
            a.on_deck_input_at(key(2, true), t0).await;
            let all = [
                (key(2, false), 400),
                (key(2, true), 900),
                (key(2, false), 1100),
            ];
            for (k, at) in &all {
                a.deck_read(k, t0 + ms(*at));
            }
            // The hold's task is asleep until its next tick; the stall then wakes it 1.9 s late.
            settle().await;
            tokio::time::advance(ms(2000)).await;
            if timer_first {
                settle().await;
                assert_eq!(status(&a, &id), "waiting", "timer first");
            }
            for (k, at) in all {
                a.on_deck_input_at(k, t0 + ms(at)).await;
            }
            pass(1500).await;
            assert_eq!(status(&a, &id), "waiting", "timer_first = {timer_first}");
        }
    }

    /// A held key whose timer woke late still completes, once, at the next timely tick.
    #[tokio::test(start_paused = true)]
    async fn a_late_woken_hold_completes_exactly_once() {
        let (a, rx) = agent("once", vec![mode("dev", true)]);
        let id = ask(&a, "Deploy?");
        pass(400).await;
        a.on_deck_input(key(2, true)).await;
        settle().await;
        tokio::time::advance(ms(1500)).await;
        settle().await;
        assert_eq!(status(&a, &id), "waiting");
        pass(300).await;
        assert_eq!(status(&a, &id), "approved");
        pass(1000).await;
        assert_eq!(status(&a, &id), "approved");
        assert_eq!(
            a.feeds.get("approvals.pending").unwrap().value,
            crate::feeds::FeedValue::Number(0.0)
        );
        drop(rx);
    }

    /// The 300 ms guard compares the deck's stamps too: a press read inside it is refused
    /// even when it is processed after the guard has passed.
    #[tokio::test(start_paused = true)]
    async fn the_guard_uses_the_press_time() {
        let (a, _rx) = agent("stamped-guard", vec![mode("dev", true)]);
        let id = ask(&a, "Deploy?");
        let t0 = Instant::now();
        pass(500).await;
        a.on_deck_input_at(key(2, true), t0 + ms(100)).await;
        pass(1500).await;
        assert_eq!(status(&a, &id), "waiting");
        a.on_deck_input_at(key(3, true), t0 + ms(100)).await;
        assert_eq!(status(&a, &id), "waiting", "Deny is guarded the same way");
    }

    #[tokio::test(start_paused = true)]
    async fn press_from_before_popup_never_approves() {
        let (a, _rx) = agent("stale", vec![mode("dev", true)]);
        // The key goes down with no pop-up, then the request arrives 100 ms later.
        a.on_deck_input(key(2, true)).await;
        pass(100).await;
        let id = ask(&a, "Deploy?");
        pass(1400).await;
        a.on_deck_input(key(2, false)).await;
        pass(500).await;
        assert_eq!(status(&a, &id), "waiting");
    }

    #[tokio::test(start_paused = true)]
    async fn a_press_inside_the_300ms_guard_is_ignored() {
        let (a, _rx) = agent("guard", vec![mode("dev", true)]);
        let id = ask(&a, "Deploy?");
        pass(200).await;
        a.on_deck_input(key(2, true)).await;
        pass(2000).await;
        a.on_deck_input(key(2, false)).await;
        assert_eq!(status(&a, &id), "waiting");
        // After the guard the same key works.
        a.on_deck_input(key(2, true)).await;
        pass(1200).await;
        assert_eq!(status(&a, &id), "approved");
    }

    #[tokio::test(start_paused = true)]
    async fn a_hold_the_popup_cancelled_does_not_fire_the_key_underneath() {
        // The key at the Approve slot has its own hold-to-fire action.
        let mut m = mode("dev", true);
        m.pages[0].keys = vec![
            None,
            None,
            Some(KeyBinding {
                label: "Under".into(),
                color: "#000000".into(),
                icon: None,
                face: Default::default(),
                states: None,
                follow: None,
                hold_ms: 1000,
                action: Action::Placeholder {
                    note: "under".into(),
                },
            }),
        ];
        let (a, _rx) = agent("under", vec![m]);
        a.on_deck_input(key(2, true)).await;
        pass(100).await;
        let id = ask(&a, "Deploy?");
        pass(1500).await;
        assert_eq!(status(&a, &id), "waiting");
        assert!(
            lock(&a.ran).is_empty(),
            "the pop-up took the key; its old hold must not fire"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_hold_rechecks_the_request_at_completion() {
        // Cancelled, denied, expired or replaced between the deadline and the key coming up.
        for case in ["cancelled", "denied", "expired", "moved"] {
            let (a, _rx) = agent(&format!("recheck-{case}"), vec![mode("dev", true)]);
            let id = a
                .create_approval(
                    &Caller::Agent,
                    NewApproval {
                        expires_in_s: Some(2),
                        ..req("one")
                    },
                )
                .unwrap();
            pass(400).await;
            a.on_deck_input(key(2, true)).await;
            // The deadline passes; the key is still down and the task has not woken yet.
            tokio::time::advance(Duration::from_millis(1000)).await;
            match case {
                "cancelled" => {
                    lock(&a.approvals).cancel("(agent)", &id).unwrap();
                }
                "denied" => lock(&a.approvals).deny(&id).unwrap(),
                "expired" => {
                    tokio::time::advance(Duration::from_secs(2)).await;
                    lock(&a.approvals).expire_due(Instant::now());
                }
                _ => {
                    // The pop-up moves to another request without the store changing.
                    let other = ask(&a, "two");
                    lock(&a.ui).popup.showing.as_mut().unwrap().id = other;
                }
            }
            a.on_deck_input(key(2, false)).await;
            pass(500).await;
            assert_ne!(status(&a, &id), "approved", "{case}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn deck_unplug_mid_hold_does_not_approve_and_popup_returns() {
        let (a, _rx) = agent("unplug", vec![mode("dev", true)]);
        let id = ask(&a, "Deploy?");
        pass(400).await;
        a.on_deck_input(key(2, true)).await;
        pass(500).await;
        // The deck is unplugged.
        lock(&a.deck).commands = None;
        a.on_deck_closed();
        pass(1500).await;
        assert_eq!(status(&a, &id), "waiting");
        assert_eq!(showing(&a), None);
        // Plugged back in: the pop-up shows again, with a fresh guard.
        let (tx, _rx2) = mpsc::channel();
        lock(&a.deck).commands = Some(tx);
        a.redraw_deck();
        assert_eq!(showing(&a), Some(id.clone()));
        a.on_deck_input(key(2, true)).await;
        pass(1500).await;
        assert_eq!(
            status(&a, &id),
            "waiting",
            "pressed within 300 ms of reopening"
        );
        a.on_deck_input(key(2, false)).await;
        a.on_deck_input(key(2, true)).await;
        pass(1200).await;
        assert_eq!(status(&a, &id), "approved");
    }

    #[tokio::test(start_paused = true)]
    async fn no_popup_while_the_deck_sleeps_and_it_may_show_on_wake() {
        let (a, rx) = agent("asleep", vec![mode("dev", true)]);
        let id = ask(&a, "Deploy?");
        pass(400).await;
        assert_eq!(showing(&a), Some(id.clone()));
        // The displays sleep: the pop-up goes, the request stays, the badge still counts.
        let mut cfg = a.config();
        cfg.deck_sleep_min = 5;
        a.set_config(cfg).unwrap();
        a.os_event(crate::macos_events::OsEvent::DisplaysAsleep);
        assert!(a.asleep());
        assert_eq!(showing(&a), None);
        let _ = strips(&rx);
        let id2 = ask(&a, "Another?");
        pass(500).await;
        assert_eq!(showing(&a), None, "a new request does not wake the pop-up");
        assert_eq!(strips(&rx), 0, "and draws nothing");
        assert_eq!(
            a.feeds.get("approvals.pending").unwrap().value,
            crate::feeds::FeedValue::Number(2.0)
        );
        // Waking (the press is swallowed, so it cannot approve) shows it again.
        a.on_deck_input(key(2, true)).await;
        assert!(!a.asleep());
        assert_eq!(showing(&a), Some(id.clone()));
        a.on_deck_input(key(2, false)).await;
        pass(1500).await;
        assert_eq!(status(&a, &id), "waiting");
        assert_eq!(status(&a, &id2), "waiting");
    }

    #[tokio::test(start_paused = true)]
    async fn a_lost_wake_release_does_not_turn_a_tap_into_an_approval() {
        let (a, _rx) = agent("lost-up", vec![mode("dev", true)]);
        let mut cfg = a.config();
        cfg.deck_sleep_min = 5;
        a.set_config(cfg).unwrap();
        let id = ask(&a, "Deploy?");
        a.os_event(crate::macos_events::OsEvent::DisplaysAsleep);
        // The waking press is on the Approve slot; its release is lost.
        a.on_deck_input(key(2, true)).await;
        pass(500).await;
        assert_eq!(showing(&a), Some(id.clone()));
        // A short tap: its release must reach the hold and cancel it.
        a.on_deck_input(key(2, true)).await;
        pass(200).await;
        a.on_deck_input(key(2, false)).await;
        pass(1500).await;
        assert_eq!(status(&a, &id), "waiting");
    }

    #[tokio::test(start_paused = true)]
    async fn no_popup_in_create_mode() {
        let (a, rx) = agent("create", vec![mode("create", false)]);
        let _ = strips(&rx);
        ask(&a, "Deploy?");
        pass(500).await;
        assert_eq!(showing(&a), None);
        assert_eq!(strips(&rx), 0, "the strip is left alone");
        assert_eq!(
            a.feeds.get("approvals.pending").unwrap().value,
            crate::feeds::FeedValue::Number(1.0),
            "the badge feed says 1"
        );
        // The slot keys are ordinary keys here: nothing approves.
        a.on_deck_input(key(2, true)).await;
        pass(1500).await;
        assert_eq!(lock(&a.approvals).pending().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn returns_after_30s_idle_and_request_stays() {
        let (a, rx) = agent("idle", vec![mode("dev", true)]);
        let id = ask(&a, "Deploy?");
        assert_eq!(showing(&a), Some(id.clone()));
        pass(20_000).await;
        // Input keeps it up.
        a.on_deck_input(Input::DialTurn {
            index: 1,
            detents: 1,
        })
        .await;
        pass(20_000).await;
        assert_eq!(showing(&a), Some(id.clone()));
        pass(11_000).await;
        assert_eq!(showing(&a), None, "30 s with no input");
        assert_eq!(status(&a, &id), "waiting", "the request stays");
        let _ = strips(&rx);
        // The Approvals key brings it back.
        a.on_deck_input(key(0, true)).await;
        assert_eq!(showing(&a), Some(id.clone()));
        assert!(strips(&rx) > 0, "the strip was redrawn");
        // So does the next approval change.
        pass(31_000).await;
        assert_eq!(showing(&a), None);
        let id2 = ask(&a, "another");
        assert_eq!(showing(&a), Some(id), "still the oldest");
        let _ = id2;
    }

    #[tokio::test(start_paused = true)]
    async fn deny_is_one_tap_and_the_knob_steps() {
        let (a, _rx) = agent("deny", vec![mode("dev", true)]);
        let one = ask(&a, "one");
        let two = ask(&a, "two");
        assert_eq!(showing(&a), Some(one.clone()));
        a.on_deck_input(Input::DialTurn {
            index: 0,
            detents: 1,
        })
        .await;
        assert_eq!(showing(&a), Some(two.clone()));
        a.on_deck_input(Input::DialTurn {
            index: 0,
            detents: 1,
        })
        .await;
        assert_eq!(showing(&a), Some(one.clone()), "wraps");
        a.on_deck_input(Input::DialTurn {
            index: 0,
            detents: -1,
        })
        .await;
        assert_eq!(showing(&a), Some(two.clone()));
        // The stepped-to request has its own guard: no instant approve after a turn.
        a.on_deck_input(key(2, true)).await;
        pass(1500).await;
        assert_eq!(status(&a, &two), "waiting");
        a.on_deck_input(key(2, false)).await;
        a.on_deck_input(key(3, true)).await;
        assert_eq!(status(&a, &two), "denied");
        assert_eq!(showing(&a), Some(one.clone()));
        pass(400).await;
        a.on_deck_input(key(3, true)).await;
        assert_eq!(status(&a, &one), "denied");
        assert_eq!(showing(&a), None);
    }

    #[tokio::test(start_paused = true)]
    async fn details_opens_https_or_the_panel() {
        let (a, _rx) = agent("details", vec![mode("dev", true)]);
        let with = a
            .create_approval(
                &Caller::Agent,
                NewApproval {
                    details_url: Some("https://example.com/run/1".into()),
                    ..req("a")
                },
            )
            .unwrap();
        pass(400).await;
        a.on_deck_input(key(1, true)).await;
        assert_eq!(lock(&a.opened).as_slice(), ["https://example.com/run/1"]);
        a.deny_approval(&with).unwrap();
        // http, or nothing: the panel opens at the request instead.
        let plain = a
            .create_approval(
                &Caller::Agent,
                NewApproval {
                    details_url: Some("http://example.com".into()),
                    ..req("b")
                },
            )
            .unwrap();
        pass(400).await;
        a.on_deck_input(key(1, true)).await;
        let opened = lock(&a.opened).clone();
        assert_eq!(opened.len(), 2);
        assert!(opened[1].starts_with("http://127.0.0.1:"), "{}", opened[1]);
        assert!(
            opened[1].ends_with(&format!("&approvals/{plain}")),
            "{}",
            opened[1]
        );
        let _ = with;
    }

    /// Each key does what it is drawn as, for slots away from the defaults.
    #[tokio::test(start_paused = true)]
    async fn labels_and_behaviour_agree() {
        let mut m = mode("dev", true);
        let slots = ApprovalSlots {
            approvals: 5,
            details: 7,
            approve: 4,
            deny: 6,
        };
        m.approval_slots = Some(slots.clone());
        let (a, _rx) = agent("agree", vec![m]);
        let id = ask(&a, "Deploy?");
        pass(400).await;
        let pops: Vec<Pop> = (0..8)
            .map(|i| lock(&a.ui).popup.pop_for(Some(&slots), i))
            .collect();
        let at = |p: Pop| pops.iter().position(|x| *x == p).unwrap();
        assert!(matches!(pops[5], Pop::Approvals(1)));
        // Every key but the one drawn as Approve and the one drawn as Deny, held a second:
        // nothing is decided; the one drawn as Details opens a link.
        for i in (0..8).filter(|&i| pops[i] != Pop::Approve && pops[i] != Pop::Deny) {
            a.on_deck_input(key(i, true)).await;
            pass(1200).await;
            a.on_deck_input(key(i, false)).await;
            assert_eq!(status(&a, &id), "waiting", "key {i} ({:?})", pops[i]);
        }
        assert_eq!(
            lock(&a.opened).len(),
            1,
            "only the Details key opened a link"
        );
        a.on_deck_input(key(at(Pop::Approve), true)).await;
        pass(1200).await;
        assert_eq!(status(&a, &id), "approved");
        let id = ask(&a, "Again?");
        pass(400).await;
        a.on_deck_input(key(at(Pop::Deny), true)).await;
        assert_eq!(status(&a, &id), "denied");
    }

    #[tokio::test(start_paused = true)]
    async fn removing_a_client_cancels_what_it_asked() {
        let (a, _rx) = agent("remove", vec![mode("dev", true)]);
        let token = lock(&a.clients).add("deployer").unwrap();
        let _ = token;
        let id = a
            .create_approval(&Caller::Client("deployer".into()), req("x"))
            .unwrap();
        assert_eq!(showing(&a), Some(id.clone()));
        a.remove_client("deployer").unwrap();
        assert_eq!(status(&a, &id), "cancelled");
        assert_eq!(showing(&a), None);
        // A request authenticated before the removal can't make a new one.
        assert_eq!(
            a.create_approval(&Caller::Client("deployer".into()), req("y")),
            Err(CreateError::Gone)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_picker_hides_the_popup_and_it_returns() {
        let (a, _rx) = agent("picker", vec![mode("dev", true), mode("other", true)]);
        let id = ask(&a, "x");
        assert_eq!(showing(&a), Some(id.clone()));
        a.open_picker();
        assert_eq!(showing(&a), None);
        pass(6000).await;
        assert_eq!(showing(&a), Some(id), "back when the picker closes");
    }

    #[tokio::test(start_paused = true)]
    async fn deny_has_the_same_300ms_guard() {
        let (a, _rx) = agent("deny-guard", vec![mode("dev", true)]);
        let id = ask(&a, "x");
        pass(200).await;
        a.on_deck_input(key(3, true)).await;
        assert_eq!(status(&a, &id), "waiting", "inside the guard");
        pass(200).await;
        a.on_deck_input(key(3, true)).await;
        assert_eq!(status(&a, &id), "denied");
        // A press that began before the pop-up appeared never denies.
        let (b, _rx) = agent("deny-stale", vec![mode("dev", true)]);
        b.on_deck_input(key(3, true)).await;
        pass(100).await;
        let id = ask(&b, "x");
        pass(1000).await;
        b.on_deck_input(key(3, false)).await;
        assert_eq!(status(&b, &id), "waiting");
    }

    #[tokio::test(start_paused = true)]
    async fn dimmed_keys_do_nothing_while_the_popup_shows() {
        let mut m = mode("dev", true);
        let k = |n: &str| {
            Some(KeyBinding {
                label: n.into(),
                color: "#000000".into(),
                icon: None,
                face: Default::default(),
                states: None,
                follow: None,
                hold_ms: 0,
                action: Action::Placeholder { note: n.into() },
            })
        };
        m.pages[0].keys = vec![None, None, None, None, k("a"), k("b")];
        let (a, _rx) = agent("swallow", vec![m]);
        a.on_deck_input(key(4, true)).await;
        assert_eq!(lock(&a.ran).len(), 1, "an ordinary key runs with no pop-up");
        let id = ask(&a, "x");
        pass(500).await;
        for i in [4, 5] {
            a.on_deck_input(key(i, true)).await;
            a.on_deck_input(key(i, false)).await;
        }
        assert_eq!(lock(&a.ran).len(), 1, "swallowed while it shows");
        a.deny_approval(&id).unwrap();
        a.on_deck_input(key(5, true)).await;
        assert_eq!(lock(&a.ran).len(), 2, "normal again once it has gone");
    }

    #[tokio::test(start_paused = true)]
    async fn the_approve_check_needs_the_deck() {
        let (a, _rx) = agent("deckcheck", vec![mode("dev", true)]);
        let id = ask(&a, "x");
        pass(400).await;
        a.on_deck_input(key(2, true)).await;
        let since = lock(&a.ui).popup.showing.as_ref().unwrap().since;
        assert!(a.approve_wanted(&id, 2, since + PRESS_GUARD));
        lock(&a.deck).commands = None;
        assert!(!a.approve_wanted(&id, 2, since + PRESS_GUARD));
    }

    #[tokio::test(start_paused = true)]
    async fn too_many_waiting_does_not_spend_the_rate_allowance() {
        let (a, _rx) = agent("cap-first", vec![mode("dev", true)]);
        lock(&a.clients).add("deployer").unwrap();
        let who = Caller::Client("deployer".into());
        *lock(&a.limits) = crate::clients::ClientLimits::new(10.0, Duration::from_millis(1));
        let mut ids = Vec::new();
        for _ in 0..20 {
            tokio::time::advance(Duration::from_millis(5)).await;
            ids.push(a.create_approval(&who, req("n")).unwrap());
        }
        // From here the real rate: one per hour, and the one token is unspent.
        *lock(&a.limits) = crate::clients::ClientLimits::new(10.0, Duration::from_secs(3600));
        assert_eq!(
            a.create_approval(&who, req("21st")),
            Err(CreateError::TooMany)
        );
        assert_eq!(
            a.create_approval(&who, req("21st")),
            Err(CreateError::TooMany)
        );
        a.cancel_approval("deployer", &ids[0]).unwrap();
        assert!(
            a.create_approval(&who, req("fits")).is_ok(),
            "the allowance was kept"
        );
        a.cancel_approval("deployer", &ids[1]).unwrap();
        assert_eq!(
            a.create_approval(&who, req("again")),
            Err(CreateError::Rate)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_popup_that_cannot_draw_is_not_shown() {
        let (a, _rx) = agent("nodraw", vec![mode("dev", true)]);
        a.popup_draw_fails
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let id = ask(&a, "x");
        pass(500).await;
        assert_eq!(showing(&a), None, "no borrowed keys without the request");
        a.on_deck_input(key(2, true)).await;
        pass(1500).await;
        assert_eq!(status(&a, &id), "waiting");
        // Once it can draw, it shows (and with a fresh guard).
        a.popup_draw_fails
            .store(false, std::sync::atomic::Ordering::Relaxed);
        a.redraw_deck();
        assert_eq!(showing(&a), Some(id));
    }
}
