//! A plain description of the tray menu, the pairing prompts and the icon, built from the
//! agent's status. Nothing here touches the OS.

use novalink_client::{Check, ClientError, Discovered, Peer, PendingPair, Status};
use novalink_proto::Edge;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionId {
    Pair(String),
    Accept(String),
    Reject(String),
    Layout { peer: String, edge: Edge },
    Unpair(String),
    RequestPermissions,
    OpenLogs,
    Diagnostics,
    TurnOff,
    TurnOn,
    StartAtLogin,
    Quit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    Label(String),
    Action {
        id: ActionId,
        text: String,
        enabled: bool,
    },
    Check {
        id: ActionId,
        text: String,
        checked: bool,
    },
    Submenu {
        text: String,
        items: Vec<Item>,
    },
    Separator,
}

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)] // one value per poll; boxing `Up` buys nothing
pub enum UiState {
    /// The agent isn't answering. `restarting` is true while the tray is bringing it back.
    AgentDown { restarting: bool },
    /// Something that fails the agent's proof is listening on the agent's port.
    Impostor,
    /// The agent couldn't be started at all (missing file, blocked, access denied).
    StartFailed { reason: String },
    /// The user turned NovaLink off from the menu.
    Off,
    /// The user turned NovaLink off, but the agent still answers.
    OffButRunning,
    /// The agent answers, but its status keeps failing for some other reason.
    Unreadable { reason: String },
    Up {
        status: Status,
        pending: Vec<PendingPair>,
        discovered: Vec<Discovered>,
        checks: Vec<Check>,
        start_at_login: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IconState {
    InControl,
    Remote,
    Disconnected,
    Problem,
}

fn action(id: ActionId, text: &str, enabled: bool) -> Item {
    Item::Action {
        id,
        text: text.into(),
        enabled,
    }
}

fn quit() -> Item {
    action(ActionId::Quit, "Quit", true)
}

pub fn menu(state: &UiState) -> Vec<Item> {
    match state {
        UiState::AgentDown { restarting } => vec![
            Item::Label(
                if *restarting {
                    "Restarting NovaLink…"
                } else {
                    "NovaLink isn't running — choose Turn On NovaLink"
                }
                .into(),
            ),
            action(ActionId::TurnOn, "Turn On NovaLink", !restarting),
            Item::Separator,
            action(ActionId::OpenLogs, "Open logs", true),
            quit(),
        ],
        UiState::Impostor => vec![
            Item::Label("Something else is using NovaLink's port (7979)".into()),
            Item::Label("Close that program, then quit and reopen NovaLink.".into()),
            Item::Separator,
            action(ActionId::OpenLogs, "Open logs", true),
            quit(),
        ],
        UiState::StartFailed { reason } => {
            let mut items: Vec<Item> = start_failed_lines(reason)
                .into_iter()
                .map(Item::Label)
                .collect();
            items.extend([
                action(ActionId::TurnOn, "Try again", true),
                Item::Separator,
                action(ActionId::OpenLogs, "Open logs", true),
                quit(),
            ]);
            items
        }
        UiState::Off => vec![
            Item::Label("NovaLink is turned off".into()),
            action(ActionId::TurnOn, "Turn On NovaLink", true),
            Item::Separator,
            quit(),
        ],
        UiState::OffButRunning => vec![
            Item::Label("NovaLink is turned off, but it is still running".into()),
            action(ActionId::TurnOff, "Turn Off NovaLink", true),
            Item::Separator,
            action(ActionId::OpenLogs, "Open logs", true),
            quit(),
        ],
        UiState::Unreadable { reason } => vec![
            Item::Label(format!("Can't read NovaLink's status: {reason}")),
            action(ActionId::TurnOff, "Turn Off NovaLink", true),
            Item::Separator,
            action(ActionId::OpenLogs, "Open logs", true),
            quit(),
        ],
        UiState::Up {
            status,
            pending,
            discovered,
            checks,
            start_at_login,
        } => up_menu(status, pending, discovered, checks, *start_at_login),
    }
}

fn up_menu(
    status: &Status,
    pending: &[PendingPair],
    discovered: &[Discovered],
    checks: &[Check],
    start_at_login: bool,
) -> Vec<Item> {
    let mut items = Vec::new();
    for p in pending {
        items.push(Item::Label(format!(
            "Pair request from {} ({}) — code {}",
            p.name,
            p.peer.chars().take(8).collect::<String>(),
            p.code
        )));
        items.push(action(ActionId::Accept(p.peer.clone()), "Accept", true));
        items.push(action(ActionId::Reject(p.peer.clone()), "Reject", true));
        items.push(Item::Separator);
    }

    items.push(Item::Label(header(status)));
    if status.capture == "waiting_for_permission" {
        items.push(Item::Label(
            "NovaLink needs permission to read the keyboard and mouse.".into(),
        ));
        items.push(action(
            ActionId::RequestPermissions,
            "Grant permission…",
            true,
        ));
    } else if status.capture == "failed" {
        items.push(Item::Label(
            "NovaLink can't read the keyboard and mouse. Open the logs for details.".into(),
        ));
    }
    for c in checks.iter().filter(|c| !c.ok) {
        items.push(Item::Label(format!("Problem: {} — {}", c.name, c.advice)));
    }
    items.push(Item::Separator);

    let names = display_names(&status.peers);
    for (peer, name) in status.peers.iter().zip(&names) {
        items.push(Item::Submenu {
            text: device_row(peer, name),
            items: device_items(status, peer),
        });
    }
    if !status.peers.is_empty() {
        items.push(Item::Separator);
    }

    items.push(Item::Submenu {
        text: "Pair a new device".into(),
        items: pair_items(discovered),
    });
    items.push(Item::Separator);
    items.push(action(ActionId::Diagnostics, "Run diagnostics", true));
    items.push(action(ActionId::OpenLogs, "Open logs", true));
    items.push(Item::Check {
        id: ActionId::StartAtLogin,
        text: "Start at login".into(),
        checked: start_at_login,
    });
    items.push(Item::Separator);
    items.push(action(ActionId::TurnOff, "Turn Off NovaLink", true));
    items.push(quit());
    items
}

fn header(status: &Status) -> String {
    if status.peers.is_empty() {
        return "Not paired yet".into();
    }
    if !status.peers.iter().any(|p| p.connected) {
        return "Not connected to any device".into();
    }
    if status.controller == status.me {
        return "This PC is in control".into();
    }
    let names = display_names(&status.peers);
    let name = status
        .peers
        .iter()
        .zip(&names)
        .find(|(p, _)| p.id == status.controller)
        .map_or("Another device", |(_, n)| n.as_str());
    format!("{name} is in control")
}

/// Each device's name, with the first 8 hex chars of its id added where two share a name.
fn display_names(peers: &[Peer]) -> Vec<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for p in peers {
        *counts.entry(p.name.as_str()).or_default() += 1;
    }
    peers
        .iter()
        .map(|p| {
            if counts[p.name.as_str()] > 1 {
                format!("{} ({})", p.name, p.id.chars().take(8).collect::<String>())
            } else {
                p.name.clone()
            }
        })
        .collect()
}

fn device_row(peer: &Peer, name: &str) -> String {
    match (peer.connected, peer.rtt_us) {
        (true, Some(us)) => format!("{name} — connected, {:.1} ms", us as f64 / 1000.0),
        (true, None) => format!("{name} — connected"),
        (false, _) => format!("{name} — not connected"),
    }
}

fn device_items(status: &Status, peer: &Peer) -> Vec<Item> {
    let side = |edge: Edge| {
        status.layout.links.iter().any(|l| {
            (l.from == status.me && l.to == peer.id && l.from_edge == edge)
                || (l.from == peer.id && l.to == status.me && l.from_edge == edge.opposite())
        })
    };
    let mut items: Vec<Item> = [
        (Edge::Left, "Left of this PC"),
        (Edge::Right, "Right of this PC"),
        (Edge::Top, "Above this PC"),
        (Edge::Bottom, "Below this PC"),
    ]
    .into_iter()
    .map(|(edge, text)| Item::Check {
        id: ActionId::Layout {
            peer: peer.id.clone(),
            edge,
        },
        text: text.into(),
        checked: side(edge),
    })
    .collect();
    items.push(Item::Separator);
    items.push(action(ActionId::Unpair(peer.id.clone()), "Unpair…", true));
    items
}

fn pair_items(discovered: &[Discovered]) -> Vec<Item> {
    if discovered.is_empty() {
        return vec![Item::Label("Looking for devices on your network…".into())];
    }
    let as_peers: Vec<Peer> = discovered
        .iter()
        .map(|d| Peer {
            id: d.id.clone(),
            name: d.name.clone(),
            connected: false,
            rtt_us: None,
            path: String::new(),
            dry_inject: None,
        })
        .collect();
    discovered
        .iter()
        .zip(display_names(&as_peers))
        .map(|(d, name)| action(ActionId::Pair(d.id.clone()), &name, true))
        .collect()
}

pub fn icon_state(state: &UiState) -> IconState {
    match state {
        UiState::AgentDown { .. }
        | UiState::Impostor
        | UiState::StartFailed { .. }
        | UiState::OffButRunning
        | UiState::Unreadable { .. } => IconState::Problem,
        UiState::Off => IconState::Disconnected,
        UiState::Up { status, .. } => {
            if status.capture != "running" {
                IconState::Problem
            } else if !status.peers.iter().any(|p| p.connected) {
                IconState::Disconnected
            } else if status.controller == status.me {
                IconState::InControl
            } else {
                IconState::Remote
            }
        }
    }
}

/// Status replies that failed for a reason that isn't "not running" or "impostor".
/// One odd reply keeps the current menu; a few in a row are shown.
#[derive(Debug, Default)]
pub struct StatusErrors {
    in_a_row: u32,
}

const UNREADABLE_AFTER: u32 = 3;

impl StatusErrors {
    /// Another odd reply; the state to show once there have been enough in a row.
    pub fn other(&mut self, reason: String) -> Option<UiState> {
        self.in_a_row += 1;
        (self.in_a_row >= UNREADABLE_AFTER).then_some(UiState::Unreadable { reason })
    }

    /// Any other outcome.
    pub fn reset(&mut self) {
        self.in_a_row = 0;
    }
}

/// A failed agent call, in the tray's words: "not running" points at the menu, not the CLI.
pub fn error_text(e: &ClientError) -> String {
    match e {
        ClientError::NotRunning | ClientError::TokenMissing => {
            "NovaLink isn't running — choose Turn On NovaLink.".into()
        }
        e => e.to_string(),
    }
}

/// Why the agent couldn't start and what to do, one menu line each. Windows refusing to run
/// an unsigned program gets its own advice: reinstalling puts the same unsigned file back.
pub fn start_failed_lines(reason: &str) -> Vec<String> {
    let lines: &[&str] = match os_error(reason) {
        // Smart App Control or an App Control (WDAC) policy refused the file.
        Some(4551 | 4556..=4559 | 4580..=4582) => &[
            "Windows blocked NovaLink from starting.",
            "Smart App Control (or a company App Control policy) only runs signed programs, and NovaLink isn't signed yet.",
            "Turn Smart App Control off in Windows Security > App & browser control (it can't be turned back on without resetting Windows), or ask your IT admin to allow NovaLink.",
        ],
        // ERROR_ACCESS_DISABLED_BY_POLICY: a Software Restriction or AppLocker rule.
        Some(1260) => &[
            "Windows blocked NovaLink from starting.",
            "A policy on this PC doesn't allow it. Ask your IT admin to allow NovaLink.",
        ],
        _ => {
            return vec![
                format!("Couldn't start NovaLink: {reason}."),
                "Reinstall with NovaLink-Setup.exe.".into(),
            ];
        }
    };
    lines.iter().map(|l| l.to_string()).collect()
}

/// The same advice as one sentence run, for a notification.
pub fn start_failed_text(reason: &str) -> String {
    start_failed_lines(reason).join(" ")
}

/// The Windows error code in an `io::Error` message, which ends "(os error N)".
fn os_error(reason: &str) -> Option<u32> {
    let rest = reason.rsplit_once("(os error ")?.1;
    rest.strip_suffix(')')?.parse().ok()
}

/// What to tell the user when Turn Off's shutdown request failed; `None` when the agent
/// stopped anyway. The reply can be lost while the agent exits, so whether it is still
/// running is what counts.
pub fn turn_off_failure(err: &ClientError, still_running: bool) -> Option<String> {
    match err {
        ClientError::NotRunning => None,
        _ if !still_running => None,
        e => Some(format!(
            "NovaLink didn't stop. Try Turn Off again, or restart Windows if the keyboard or mouse misbehaves. ({e})"
        )),
    }
}

/// A failed OS call, in plain English.
pub fn os_failure_text(what: &str, reason: &str) -> String {
    format!(
        "NovaLink couldn't {what}: {reason}. Close and reopen NovaLink; if it keeps happening, restart Windows."
    )
}

/// The pairing question: title and text. The short id tells apart devices that share a name,
/// and the code sits on its own line so it is easy to compare.
pub fn pair_prompt(p: &PendingPair) -> (String, String) {
    let who = format!(
        "{} ({})",
        p.name,
        p.peer.chars().take(8).collect::<String>()
    );
    (
        format!("NovaLink — pair with {who}?"),
        format!(
            "Does {who} show this code?\n\n    {}\n\nChoose Yes only if the codes match on both screens.",
            p.code
        ),
    )
}

/// Remembers which pairing requests the user has already been shown.
#[derive(Debug, Default)]
pub struct PromptTracker {
    shown: HashMap<String, String>,
}

impl PromptTracker {
    /// Returns the requests to prompt for now (new, or the same device with a new code) and
    /// the ids whose prompt should be withdrawn because the request is gone.
    pub fn update(&mut self, pending: &[PendingPair]) -> (Vec<PendingPair>, Vec<String>) {
        let new = pending
            .iter()
            .filter(|p| self.shown.get(&p.peer) != Some(&p.code))
            .cloned()
            .collect();
        let live: HashSet<&str> = pending.iter().map(|p| p.peer.as_str()).collect();
        let mut gone: Vec<String> = self
            .shown
            .keys()
            .filter(|id| !live.contains(id.as_str()))
            .cloned()
            .collect();
        gone.sort();
        self.shown = pending
            .iter()
            .map(|p| (p.peer.clone(), p.code.clone()))
            .collect();
        (new, gone)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use novalink_client::{Layout, Link, Peer};

    const ME: &str = "aa";
    const PC: &str = "bb";

    fn peer(id: &str, name: &str, connected: bool) -> Peer {
        Peer {
            id: id.into(),
            name: name.into(),
            connected,
            rtt_us: connected.then_some(2100),
            path: "Ethernet".into(),
            dry_inject: None,
        }
    }

    fn status(controller: &str, peers: Vec<Peer>, links: Vec<Link>) -> Status {
        Status {
            me: ME.into(),
            name: "Mac".into(),
            controller: controller.into(),
            focus: controller.into(),
            peers,
            layout: Layout { version: 1, links },
            capture: "running".into(),
        }
    }

    fn up(status: Status, pending: Vec<PendingPair>) -> UiState {
        UiState::Up {
            status,
            pending,
            discovered: vec![],
            checks: vec![],
            start_at_login: false,
        }
    }

    fn pair(peer: &str, code: &str) -> PendingPair {
        PendingPair {
            peer: peer.into(),
            name: "Desk".into(),
            code: code.into(),
        }
    }

    fn header(items: &[Item]) -> String {
        items
            .iter()
            .find_map(|i| match i {
                Item::Label(t) => Some(t.clone()),
                _ => None,
            })
            .unwrap()
    }

    fn find_actions(items: &[Item], out: &mut Vec<ActionId>) {
        for i in items {
            match i {
                Item::Action { id, .. } | Item::Check { id, .. } => out.push(id.clone()),
                Item::Submenu { items, .. } => find_actions(items, out),
                _ => {}
            }
        }
    }

    #[test]
    fn pair_prompt_shows_short_id_and_code_line() {
        let p = pair("0123456789abcdef", "123456");
        let (title, text) = pair_prompt(&p);
        assert_eq!(title, "NovaLink — pair with Desk (01234567)?");
        assert_eq!(
            text,
            "Does Desk (01234567) show this code?\n\n    123456\n\nChoose Yes only if the codes match on both screens."
        );
    }

    #[test]
    fn a_policy_block_is_not_told_to_reinstall() {
        for code in [4551, 4556, 4580] {
            let lines = start_failed_lines(&format!(
                "An Application Control policy has blocked this file. (os error {code})"
            ));
            let all = lines.join(" ");
            assert!(lines.len() > 1, "{lines:?}");
            assert!(
                all.contains("Smart App Control") && !all.contains("Reinstall"),
                "{all}"
            );
        }
        let all = start_failed_text("This program is blocked by group policy. (os error 1260)");
        assert!(
            all.contains("IT admin")
                && !all.contains("Smart App Control")
                && !all.contains("Reinstall"),
            "{all}"
        );
        // A path that happens to contain the words, without an OS code, keeps the general advice.
        assert!(start_failed_text("C:\\os error 4551 is missing").contains("Reinstall"));
    }

    #[test]
    fn failures_are_worded_for_people() {
        assert!(turn_off_failure(&ClientError::NotRunning, true).is_none());
        // The agent logged the request and exited, but its reply never arrived.
        let lost = ClientError::Other("The NovaLink agent didn't answer in time.".into());
        assert!(turn_off_failure(&lost, false).is_none());
        let m = turn_off_failure(&lost, true).unwrap();
        assert!(
            m.contains("didn't stop") && m.contains("restart Windows"),
            "{m}"
        );
        assert_eq!(
            start_failed_text("Access is denied. (os error 5)"),
            "Couldn't start NovaLink: Access is denied. (os error 5). Reinstall with NovaLink-Setup.exe."
        );
        assert_eq!(
            os_failure_text("save the Off setting", "disk full"),
            "NovaLink couldn't save the Off setting: disk full. Close and reopen NovaLink; if it keeps happening, restart Windows."
        );
    }

    #[test]
    fn not_running_errors_point_at_the_menu_not_the_cli() {
        for e in [ClientError::NotRunning, ClientError::TokenMissing] {
            let t = error_text(&e);
            assert_eq!(t, "NovaLink isn't running — choose Turn On NovaLink.");
            assert!(!t.contains("novalink-agent"));
        }
        assert_eq!(error_text(&ClientError::Other("x".into())), "x");
    }

    #[test]
    fn unreadable_status_shows_after_three_odd_replies_in_a_row() {
        let mut r = StatusErrors::default();
        assert!(r.other("bad reply".into()).is_none());
        assert!(r.other("bad reply".into()).is_none());
        let s = r.other("bad reply".into()).expect("third in a row");
        assert_eq!(header(&menu(&s)), "Can't read NovaLink's status: bad reply");
        assert_eq!(icon_state(&s), IconState::Problem);
        // Any other outcome starts the count again.
        r.reset();
        assert!(r.other("bad reply".into()).is_none());
    }

    #[test]
    fn broken_states_show_a_problem_icon() {
        for s in [
            UiState::StartFailed { reason: "x".into() },
            UiState::OffButRunning,
            UiState::Unreadable { reason: "x".into() },
        ] {
            assert_eq!(icon_state(&s), IconState::Problem);
            assert!(!menu(&s).is_empty());
        }
    }

    #[test]
    fn header_says_who_controls() {
        let peers = vec![peer(PC, "Desk", true)];
        let m = menu(&up(status(ME, peers.clone(), vec![]), vec![]));
        assert_eq!(header(&m), "This PC is in control");
        let m = menu(&up(status(PC, peers, vec![]), vec![]));
        assert_eq!(header(&m), "Desk is in control");
    }

    #[test]
    fn header_covers_unpaired_and_disconnected() {
        let m = menu(&up(status(ME, vec![], vec![]), vec![]));
        assert_eq!(header(&m), "Not paired yet");
        let m = menu(&up(
            status(ME, vec![peer(PC, "Desk", false)], vec![]),
            vec![],
        ));
        assert_eq!(header(&m), "Not connected to any device");
    }

    fn device_checks(m: &[Item]) -> Vec<(String, bool)> {
        let items = m
            .iter()
            .find_map(|i| match i {
                Item::Submenu { text, items } if text.starts_with("Desk") => Some(items),
                _ => None,
            })
            .unwrap();
        items
            .iter()
            .filter_map(|i| match i {
                Item::Check { text, checked, .. } => Some((text.clone(), *checked)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn layout_checks_reflect_links() {
        let link = Link {
            from: ME.into(),
            from_edge: Edge::Right,
            to: PC.into(),
        };
        let m = menu(&up(
            status(ME, vec![peer(PC, "Desk", true)], vec![link]),
            vec![],
        ));
        assert_eq!(
            device_checks(&m),
            [
                ("Left of this PC".to_string(), false),
                ("Right of this PC".to_string(), true),
                ("Above this PC".to_string(), false),
                ("Below this PC".to_string(), false),
            ]
        );
    }

    #[test]
    fn layout_checks_resolve_the_reverse_link() {
        // The peer's Left edge touches this PC, so the peer is on this PC's Right.
        let link = Link {
            from: PC.into(),
            from_edge: Edge::Left,
            to: ME.into(),
        };
        let m = menu(&up(
            status(ME, vec![peer(PC, "Desk", true)], vec![link]),
            vec![],
        ));
        assert_eq!(device_checks(&m)[1], ("Right of this PC".to_string(), true));
        assert_eq!(device_checks(&m).iter().filter(|c| c.1).count(), 1);
    }

    #[test]
    fn device_rows_show_state_and_latency() {
        let peers = vec![peer(PC, "Desk", true), peer("cc", "Laptop", false)];
        let m = menu(&up(status(ME, peers, vec![]), vec![]));
        let texts: Vec<_> = m
            .iter()
            .filter_map(|i| match i {
                Item::Submenu { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(texts.contains(&"Desk — connected, 2.1 ms"));
        assert!(texts.contains(&"Laptop — not connected"));
    }

    #[test]
    fn duplicate_names_get_an_id_prefix() {
        let a = "1234567890abcdef".repeat(4);
        let b = "fedcba0987654321".repeat(4);
        let peers = vec![peer(&a, "Desk", true), peer(&b, "Desk", true)];
        let m = menu(&up(status(ME, peers, vec![]), vec![]));
        let texts: Vec<_> = m
            .iter()
            .filter_map(|i| match i {
                Item::Submenu { text, .. } if text.starts_with("Desk") => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(texts[0].starts_with("Desk (12345678)"), "{texts:?}");
        assert!(texts[1].starts_with("Desk (fedcba09)"), "{texts:?}");
    }

    #[test]
    fn pair_menu_lists_discovered_or_waits() {
        let mut s = up(status(ME, vec![], vec![]), vec![]);
        let find = |m: &[Item]| {
            m.iter()
                .find_map(|i| match i {
                    Item::Submenu { text, items } if text == "Pair a new device" => {
                        Some(items.clone())
                    }
                    _ => None,
                })
                .unwrap()
        };
        match &find(&menu(&s))[..] {
            [Item::Label(text)] => assert_eq!(text, "Looking for devices on your network…"),
            other => panic!("{other:?}"),
        }
        if let UiState::Up { discovered, .. } = &mut s {
            discovered.push(Discovered {
                id: "dd".into(),
                name: "Studio".into(),
            });
        }
        match &find(&menu(&s))[..] {
            [
                Item::Action {
                    id: ActionId::Pair(id),
                    text,
                    enabled: true,
                },
            ] => {
                assert_eq!((id.as_str(), text.as_str()), ("dd", "Studio"))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn pending_pair_shows_code_with_accept_and_reject() {
        let m = menu(&up(status(ME, vec![], vec![]), vec![pair("dd", "123456")]));
        match &m[..3] {
            [
                Item::Label(t),
                Item::Action {
                    id: ActionId::Accept(a),
                    ..
                },
                Item::Action {
                    id: ActionId::Reject(r),
                    ..
                },
            ] => {
                assert_eq!(t, "Pair request from Desk (dd) — code 123456");
                assert_eq!((a.as_str(), r.as_str()), ("dd", "dd"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn agent_down_offers_turn_on_only() {
        let m = menu(&UiState::AgentDown { restarting: false });
        assert_eq!(
            header(&m),
            "NovaLink isn't running — choose Turn On NovaLink"
        );
        let mut ids = vec![];
        find_actions(&m, &mut ids);
        assert!(ids.contains(&ActionId::TurnOn));
        assert!(
            !ids.iter()
                .any(|i| matches!(i, ActionId::Pair(_) | ActionId::Unpair(_)))
        );
        assert!(m.iter().any(|i| matches!(
            i,
            Item::Action {
                id: ActionId::TurnOn,
                enabled: true,
                ..
            }
        )));
        let m = menu(&UiState::AgentDown { restarting: true });
        assert!(m.iter().any(|i| matches!(
            i,
            Item::Action {
                id: ActionId::TurnOn,
                enabled: false,
                ..
            }
        )));
    }

    #[test]
    fn impostor_never_offers_device_actions() {
        let m = menu(&UiState::Impostor);
        assert_eq!(header(&m), "Something else is using NovaLink's port (7979)");
        let mut ids = vec![];
        find_actions(&m, &mut ids);
        assert!(
            ids.iter()
                .all(|i| matches!(i, ActionId::OpenLogs | ActionId::Quit)),
            "{ids:?}"
        );
    }

    #[test]
    fn up_menu_has_turn_off_login_and_quit() {
        let m = menu(&up(status(ME, vec![], vec![]), vec![]));
        let mut ids = vec![];
        find_actions(&m, &mut ids);
        for want in [
            ActionId::TurnOff,
            ActionId::StartAtLogin,
            ActionId::Quit,
            ActionId::Diagnostics,
        ] {
            assert!(ids.contains(&want), "{want:?}");
        }
        assert!(!ids.contains(&ActionId::RequestPermissions));
    }

    #[test]
    fn missing_permission_offers_request_and_flags_problem() {
        let mut s = status(ME, vec![peer(PC, "Desk", true)], vec![]);
        s.capture = "waiting_for_permission".into();
        let st = up(s, vec![]);
        let mut ids = vec![];
        find_actions(&menu(&st), &mut ids);
        assert!(ids.contains(&ActionId::RequestPermissions));
        assert_eq!(icon_state(&st), IconState::Problem);
    }

    #[test]
    fn pairing_row_shows_short_id() {
        let id = "1a2b3c4d5e6f7a8b".repeat(4);
        let m = menu(&up(status(ME, vec![], vec![]), vec![pair(&id, "123456")]));
        assert_eq!(
            m[0],
            Item::Label("Pair request from Desk (1a2b3c4d) — code 123456".into())
        );
    }

    #[test]
    fn failing_check_keeps_icon_on_controller_but_failed_capture_is_a_problem() {
        let check = Check {
            name: "peer_connected".into(),
            ok: false,
            detail: String::new(),
            advice: "Pair a device".into(),
        };
        let peers = vec![peer(PC, "Desk", true)];
        let mut st = up(status(ME, peers.clone(), vec![]), vec![]);
        if let UiState::Up { checks, .. } = &mut st {
            checks.push(check);
        }
        assert_eq!(icon_state(&st), IconState::InControl);
        assert!(
            menu(&st)
                .iter()
                .any(|i| matches!(i, Item::Label(t) if t.starts_with("Problem: peer_connected")))
        );
        let mut s = status(ME, peers, vec![]);
        s.capture = "failed".into();
        assert_eq!(icon_state(&up(s, vec![])), IconState::Problem);
    }

    #[test]
    fn icon_follows_state() {
        let peers = vec![peer(PC, "Desk", true)];
        assert_eq!(
            icon_state(&up(status(ME, peers.clone(), vec![]), vec![])),
            IconState::InControl
        );
        assert_eq!(
            icon_state(&up(status(PC, peers, vec![]), vec![])),
            IconState::Remote
        );
        assert_eq!(
            icon_state(&up(status(ME, vec![], vec![]), vec![])),
            IconState::Disconnected
        );
        assert_eq!(icon_state(&UiState::Off), IconState::Disconnected);
        assert_eq!(icon_state(&UiState::Impostor), IconState::Problem);
    }

    #[test]
    fn prompt_tracker_prompts_once_and_withdraws() {
        let mut t = PromptTracker::default();
        let a = pair("a", "111111");
        let (new, gone) = t.update(std::slice::from_ref(&a));
        assert_eq!((new.len(), gone.len()), (1, 0));
        assert_eq!(new[0].peer, "a");
        let (new, gone) = t.update(&[a]);
        assert!(new.is_empty() && gone.is_empty());
        let (new, gone) = t.update(&[]);
        assert!(new.is_empty());
        assert_eq!(gone, ["a"]);
    }

    #[test]
    fn prompt_tracker_reprompts_new_code_from_same_device() {
        let mut t = PromptTracker::default();
        t.update(&[pair("a", "111111")]);
        let (new, gone) = t.update(&[pair("a", "222222")]);
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].code, "222222");
        assert!(gone.is_empty());
    }
}
