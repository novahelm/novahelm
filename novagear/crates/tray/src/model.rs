//! What the menu shows, worked out from one snapshot of the agent. Kept free of UI types so
//! it can be tested without a tray.

use novagear_core::config::Config;
use novagear_core::{DeviceInfo, DeviceKind, Status};
use serde_json::Value;

/// The mode half of `/api/status`. Every field is optional on the wire: an older agent
/// sends none of them, and a caller that isn't staff gets no modes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModeStatus {
    pub mode: Option<ModeRef>,
    pub modes: Vec<ModeRef>,
    pub pending: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModeRef {
    pub id: String,
    pub name: String,
}

impl ModeStatus {
    pub fn from_status(v: &Value) -> ModeStatus {
        let one = |m: &Value| {
            Some(ModeRef {
                id: m["id"].as_str()?.to_string(),
                name: m["name"].as_str()?.to_string(),
            })
        };
        ModeStatus {
            mode: one(&v["mode"]),
            modes: v["modes"]
                .as_array()
                .map(|a| a.iter().filter_map(one).collect())
                .unwrap_or_default(),
            pending: v["approvals"]["pending"].as_u64().unwrap_or(0),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub modes: ModeStatus,
    pub devices: Vec<DeviceInfo>,
    pub config: Config,
    pub prompter_playing: bool,
}

impl Snapshot {
    pub fn fetch() -> anyhow::Result<Snapshot> {
        use novagear_core::client::call;
        let devices = serde_json::from_value(call("GET", "/api/devices", None)?)?;
        let config = serde_json::from_value(call("GET", "/api/config", None)?)?;
        let prompter = call("GET", "/api/prompter", None)?;
        // An agent without /api/status still gives a menu.
        let modes = call("GET", "/api/status", None)
            .map(|v| ModeStatus::from_status(&v))
            .unwrap_or_default();
        Ok(Snapshot {
            modes,
            devices,
            config,
            prompter_playing: prompter["run"]["playing"].as_bool().unwrap_or(false),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub status: String,
    pub have_lights: bool,
    pub lights_on: bool,
    pub lights_label: String,
    pub have_deck: bool,
    pub deck_control: bool,
    pub deck_label: String,
    pub have_prompter: bool,
    pub prompter_playing: bool,
    pub prompter_speed: u16,
    pub mouse_dpi: Option<u16>,
    pub dpi_stages: Vec<u16>,
    pub mode_label: Option<String>,
    /// (id, name) of every mode, for the switcher.
    pub modes: Vec<(String, String)>,
    pub current_mode: Option<String>,
    pub approvals_label: Option<String>,
}

fn of_kind(s: &Snapshot, k: DeviceKind) -> impl Iterator<Item = &DeviceInfo> {
    s.devices.iter().filter(move |d| d.kind == k)
}

pub fn view(s: &Snapshot) -> View {
    let ready = s
        .devices
        .iter()
        .filter(|d| d.status == Status::Ready)
        .count();
    let lights: Vec<&DeviceInfo> = of_kind(s, DeviceKind::Light).collect();
    let lights_on = lights.iter().any(|l| l.state["on"] == Value::Bool(true));
    let first = lights.iter().find(|l| l.state["brightness"].is_number());
    let lights_label = match (lights.len(), first) {
        (0, _) => "No lights found".into(),
        (n, Some(l)) => format!(
            "{n} light{} · {} · {}% · {}K",
            if n == 1 { "" } else { "s" },
            if lights_on { "on" } else { "off" },
            l.state["brightness"],
            l.state["kelvin"]
        ),
        (n, None) => format!("{n} light{} · not answering", if n == 1 { "" } else { "s" }),
    };
    let deck = of_kind(s, DeviceKind::StreamDeck).next();
    let deck_label = match deck.map(|d| &d.status) {
        None => "No Stream Deck".into(),
        Some(Status::Busy(_)) => "Stream Deck held by another app".into(),
        Some(_) => "NovaGear drives the Stream Deck".into(),
    };
    let mouse_dpi = of_kind(s, DeviceKind::Mouse)
        .find_map(|m| m.state["dpi"].as_u64())
        .map(|d| d as u16);
    View {
        status: format!("NovaGear · {ready} of {} devices ready", s.devices.len()),
        have_lights: !lights.is_empty(),
        lights_on,
        lights_label,
        have_deck: deck.is_some_and(|d| !matches!(d.status, Status::Busy(_))),
        deck_control: s.config.deck_control,
        deck_label,
        have_prompter: of_kind(s, DeviceKind::Prompter).next().is_some(),
        prompter_playing: s.prompter_playing,
        prompter_speed: s.config.prompter.speed,
        mouse_dpi,
        dpi_stages: s.config.dpi_stages.clone(),
        mode_label: s.modes.mode.as_ref().map(|m| format!("Mode · {}", m.name)),
        modes: s
            .modes
            .modes
            .iter()
            .map(|m| (m.id.clone(), m.name.clone()))
            .collect(),
        current_mode: s.modes.mode.as_ref().map(|m| m.id.clone()),
        // Only a count: titles come from other programs, and approving is the deck's job.
        approvals_label: (s.modes.pending > 0).then(|| {
            format!(
                "{} approval{} waiting",
                s.modes.pending,
                if s.modes.pending == 1 { "" } else { "s" }
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dev(kind: DeviceKind, status: Status, state: Value) -> DeviceInfo {
        DeviceInfo {
            id: format!("{kind:?}"),
            kind,
            name: "x".into(),
            vendor: "v".into(),
            usb: None,
            status,
            state,
            firmware: None,
        }
    }

    #[test]
    fn summarises_a_sample_desk() {
        let s = Snapshot {
            devices: vec![
                dev(
                    DeviceKind::Light,
                    Status::Ready,
                    json!({"on": false, "brightness": 11, "kelvin": 5200}),
                ),
                dev(
                    DeviceKind::Light,
                    Status::Ready,
                    json!({"on": true, "brightness": 6, "kelvin": 5300}),
                ),
                dev(DeviceKind::StreamDeck, Status::Ready, json!({})),
                dev(DeviceKind::Mouse, Status::Ready, json!({"dpi": 6400})),
                dev(DeviceKind::Prompter, Status::Ready, json!({})),
                dev(DeviceKind::Keyboard, Status::Listed, json!({})),
            ],
            config: Config::default(),
            prompter_playing: true,
            ..Default::default()
        };
        let v = view(&s);
        assert_eq!(v.status, "NovaGear · 5 of 6 devices ready");
        assert!(v.lights_on, "any light on counts as on");
        assert_eq!(v.lights_label, "2 lights · on · 11% · 5200K");
        assert!(v.have_deck && !v.deck_control);
        assert_eq!(v.mouse_dpi, Some(6400));
        assert!(v.have_prompter && v.prompter_playing);
    }

    #[test]
    fn busy_deck_and_no_lights() {
        let s = Snapshot {
            devices: vec![dev(
                DeviceKind::StreamDeck,
                Status::Busy("held".into()),
                json!({}),
            )],
            ..Default::default()
        };
        let v = view(&s);
        assert_eq!(v.lights_label, "No lights found");
        assert!(!v.have_deck);
        assert_eq!(v.deck_label, "Stream Deck held by another app");
    }

    #[test]
    fn status_with_modes_decodes() {
        let full = json!({
            "version": "0.1.0",
            "mode": {"id": "dev", "name": "Developer", "accent": "#2563eb"},
            "modes": [{"id": "dev", "name": "Developer"}, {"id": "stream", "name": "Stream"}],
            "approvals": {"pending": 2, "top": {"id": "a1", "title": "t", "source": "s"}}
        });
        let m = ModeStatus::from_status(&full);
        assert_eq!(m.mode.as_ref().map(|m| m.name.as_str()), Some("Developer"));
        assert_eq!(m.modes.len(), 2);
        assert_eq!(m.pending, 2);
        // An older agent, or a null mode, leaves everything empty.
        for old in [
            json!({"version": "0.0.9"}),
            json!({"mode": null, "modes": []}),
        ] {
            let m = ModeStatus::from_status(&old);
            assert!(m.mode.is_none() && m.modes.is_empty() && m.pending == 0);
        }
    }

    #[test]
    fn view_names_the_mode() {
        let s = Snapshot {
            modes: ModeStatus::from_status(&json!({
                "mode": {"id": "dev", "name": "Developer"},
                "modes": [{"id": "dev", "name": "Developer"}],
                "approvals": {"pending": 1, "top": null}
            })),
            ..Default::default()
        };
        let v = view(&s);
        assert_eq!(v.mode_label.as_deref(), Some("Mode · Developer"));
        assert_eq!(v.modes, vec![("dev".to_string(), "Developer".to_string())]);
        assert_eq!(v.approvals_label.as_deref(), Some("1 approval waiting"));
        assert!(view(&Snapshot::default()).mode_label.is_none());
    }

    #[test]
    fn menu_tolerates_controllers() {
        let mut unknown = dev(DeviceKind::Other, Status::Listed, json!({"unknown": true}));
        unknown.id = "usb-1".into();
        let s = Snapshot {
            devices: vec![
                dev(DeviceKind::Controller, Status::Ready, json!({})),
                unknown,
                dev(
                    DeviceKind::Light,
                    Status::Ready,
                    json!({"on": true, "brightness": 20, "kelvin": 5000}),
                ),
            ],
            ..Default::default()
        };
        let v = view(&s);
        assert_eq!(v.status, "NovaGear · 2 of 3 devices ready");
        assert!(v.have_lights && !v.have_deck);
    }
}
