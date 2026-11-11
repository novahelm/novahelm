//! Types shared by the agent, its drivers and the CLI: devices, actions, bindings and config.

pub mod client;
pub mod config;
pub mod modes;
pub mod proof;

use modes::{Face, KeyState};

use serde::{Deserialize, Serialize};

/// Local API port. The agent binds 127.0.0.1 only.
pub const API_PORT: u16 = 47820;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Light,
    StreamDeck,
    Prompter,
    Capture,
    Mouse,
    Keyboard,
    Audio,
    Controller,
    /// Anything a newer record file names that this build does not know.
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "reason")]
pub enum Status {
    /// Found and controllable.
    Ready,
    /// Found, but only listed: no driver controls it yet.
    Listed,
    /// Found, but another app holds it or a call failed.
    Busy(String),
}

/// One device as the API reports it. `id` is stable across reconnects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub id: String,
    pub kind: DeviceKind,
    pub name: String,
    pub vendor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usb: Option<UsbId>,
    pub status: Status,
    /// Driver-specific state, such as a light's brightness or a mouse's DPI.
    pub state: serde_json::Value,
    /// Firmware status; every device the agent lists has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<FirmwareInfo>,
}

/// What is known about a device's firmware. Only Key Lights ever have `latest`: the
/// others are updated with the vendor's own tool, and NovaGear just points at it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FirmwareInfo {
    /// "1.0.3 (200)" for lights, the deck's and Razer's own strings, or the USB
    /// bcdDevice for the rest.
    pub installed: Option<String>,
    /// True when `installed` is the USB bcdDevice rather than a firmware string.
    pub installed_is_device_version: bool,
    /// "native", "fwupd", "vendor_tool" or "none".
    pub policy: String,
    pub tool: Option<String>,
    pub url: Option<String>,
    pub latest: Option<String>,
    pub update_available: bool,
    /// Where `latest` came from, for the panel to say.
    pub source: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UsbId {
    pub vid: u16,
    pub pid: u16,
}

/// Something a binding or the API can ask the agent to do. Light actions without an
/// `id` apply to every light.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "do")]
pub enum Action {
    LightToggle {
        #[serde(default)]
        id: Option<String>,
    },
    LightSet {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        on: Option<bool>,
        #[serde(default)]
        brightness: Option<u8>,
        /// Kelvin, 2900–7000.
        #[serde(default)]
        kelvin: Option<u16>,
    },
    /// Brightness change in percent points; dials send it once per detent.
    LightBrightnessStep {
        #[serde(default)]
        id: Option<String>,
        step: i16,
    },
    LightKelvinStep {
        #[serde(default)]
        id: Option<String>,
        step: i16,
    },
    PrompterPlayPause,
    PrompterSpeedStep {
        step: i16,
    },
    PrompterRestart,
    MouseDpi {
        dpi: u16,
    },
    MouseDpiStep {
        step: i16,
    },
    /// `#rrggbb`, static colour on every LED.
    MouseColor {
        color: String,
    },
    DeckBrightnessStep {
        step: i16,
    },
    /// Shows a page; the current one goes on the back stack.
    PageGo {
        page: String,
    },
    PageBack,
    /// An app bundle path (macOS), executable or `.desktop` name.
    OpenApp {
        path: String,
    },
    OpenUrl {
        url: String,
    },
    TypeText {
        text: String,
        #[serde(default)]
        enter: bool,
    },
    /// `"cmd+shift+4"`; a key can also be a raw platform code, `"ctrl+code:90"`.
    Hotkey {
        keys: String,
    },
    /// Output volume change in percent points.
    VolumeStep {
        step: i16,
    },
    MicVolumeStep {
        step: i16,
    },
    MuteToggle,
    MicMuteToggle,
    ObsScene {
        scene: String,
    },
    ObsRecordToggle,
    ObsStreamToggle,
    /// An imported action NovaGear can't run yet; pressing it says so.
    Placeholder {
        note: String,
    },
    /// Switches to a mode, showing its home page.
    ModeGo {
        mode: String,
    },
    ModeNext,
    ModePicker,
    /// Runs the steps in order, each after its own delay.
    Sequence {
        steps: Vec<Step>,
    },
    ObsChapterMarker,
    ObsReplaySave,
    /// Input volume change in dB.
    ObsInputVolumeStep {
        input: String,
        step: i16,
    },
    ObsInputMuteToggle {
        input: String,
    },
    /// An event for a named client app; only the client receives it.
    ClientEvent {
        client: String,
        name: String,
        #[serde(default)]
        value: Option<serde_json::Value>,
    },
    /// Moves through the waiting approvals; knob 1 uses it during a pop-up.
    ApprovalStep {
        step: i16,
    },
}

/// One step of a `Sequence`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub action: Action,
    /// Wait before this step, in milliseconds.
    #[serde(default)]
    pub delay_ms: u32,
}

/// A Stream Deck key: what it shows and what pressing it does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyBinding {
    pub label: String,
    /// `#rrggbb` background.
    #[serde(default = "default_key_color")]
    pub color: String,
    /// A PNG in the config dir's `icons/`, drawn above the label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Face::is_empty")]
    pub face: Face,
    /// Two looks for one key; `follow` picks which shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub states: Option<[KeyState; 2]>,
    /// A feed key; while its value is truthy, state 1 shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow: Option<String>,
    /// Hold time before the action runs; 0 runs it on press.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub hold_ms: u32,
    pub action: Action,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

fn default_key_color() -> String {
    "#1e2230".into()
}

/// A Stream Deck + dial. `turn` runs once per turn with its step multiplied by the detent
/// count (negative when turned left). Actions without a step, such as hotkeys, use
/// `turn_left` / `turn_right` instead, run once per detent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DialBinding {
    pub label: String,
    #[serde(default)]
    pub turn: Option<Action>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_left: Option<Action>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_right: Option<Action>,
    #[serde(default)]
    pub press: Option<Action>,
    /// What a turn does while the dial is held down or in its alternate role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alt_turn: Option<Action>,
}

impl DialBinding {
    /// What one turn of `detents` runs: the scaled `turn` action once, or the left/right
    /// action once per detent.
    pub fn turn_actions(&self, detents: i16) -> Vec<Action> {
        if let Some(t) = &self.turn {
            return vec![t.scaled(detents)];
        }
        let side = if detents < 0 {
            &self.turn_left
        } else {
            &self.turn_right
        };
        side.iter()
            .cloned()
            .cycle()
            .take(detents.unsigned_abs().min(10) as usize)
            .collect()
    }
}

/// One screenful of the deck: 8 keys and 4 dials.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Page {
    pub name: String,
    #[serde(default)]
    pub keys: Vec<Option<KeyBinding>>,
    #[serde(default)]
    pub dials: Vec<Option<DialBinding>>,
}

impl Action {
    /// A short description that is safe to log: the action's name, and for typed text only
    /// its length. Never a field's contents (typed text, hotkeys, URLs and their query
    /// strings, paths, scene names), because those can hold secrets.
    pub fn log_label(&self) -> String {
        let name = match self {
            Action::LightToggle { .. } => "light_toggle",
            Action::LightSet { .. } => "light_set",
            Action::LightBrightnessStep { .. } => "light_brightness_step",
            Action::LightKelvinStep { .. } => "light_kelvin_step",
            Action::PrompterPlayPause => "prompter_play_pause",
            Action::PrompterSpeedStep { .. } => "prompter_speed_step",
            Action::PrompterRestart => "prompter_restart",
            Action::MouseDpi { .. } => "mouse_dpi",
            Action::MouseDpiStep { .. } => "mouse_dpi_step",
            Action::MouseColor { .. } => "mouse_color",
            Action::DeckBrightnessStep { .. } => "deck_brightness_step",
            Action::PageGo { .. } => "page_go",
            Action::PageBack => "page_back",
            Action::OpenApp { .. } => "open_app",
            Action::OpenUrl { .. } => "open_url",
            Action::TypeText { text, .. } => {
                return format!("type_text, {} chars", text.chars().count());
            }
            Action::Hotkey { .. } => "hotkey",
            Action::VolumeStep { .. } => "volume_step",
            Action::MicVolumeStep { .. } => "mic_volume_step",
            Action::MuteToggle => "mute_toggle",
            Action::MicMuteToggle => "mic_mute_toggle",
            Action::ObsScene { .. } => "obs_scene",
            Action::ObsRecordToggle => "obs_record_toggle",
            Action::ObsStreamToggle => "obs_stream_toggle",
            Action::Placeholder { .. } => "placeholder",
            Action::ModeGo { .. } => "mode_go",
            Action::ModeNext => "mode_next",
            Action::ModePicker => "mode_picker",
            Action::Sequence { steps } => {
                return format!("sequence, {} steps", steps.len());
            }
            Action::ObsChapterMarker => "obs_chapter_marker",
            Action::ObsReplaySave => "obs_replay_save",
            Action::ObsInputVolumeStep { .. } => "obs_input_volume_step",
            Action::ObsInputMuteToggle { .. } => "obs_input_mute_toggle",
            // Not the client, event name or value: a client's events can hold anything.
            Action::ClientEvent { .. } => "client_event",
            Action::ApprovalStep { .. } => "approval_step",
        };
        name.to_string()
    }

    /// The action with its step scaled by `n` detents, for dial turns. Actions without a
    /// step are returned as they are.
    pub fn scaled(&self, n: i16) -> Action {
        let mut a = self.clone();
        match &mut a {
            Action::LightBrightnessStep { step, .. }
            | Action::LightKelvinStep { step, .. }
            | Action::PrompterSpeedStep { step }
            | Action::MouseDpiStep { step }
            | Action::DeckBrightnessStep { step }
            | Action::VolumeStep { step }
            | Action::MicVolumeStep { step }
            | Action::ObsInputVolumeStep { step, .. }
            | Action::ApprovalStep { step } => *step = step.saturating_mul(n),
            _ => {}
        }
        a
    }
}

/// Client-side helpers so the CLI and agent agree on where the token lives.
pub fn api_base() -> String {
    format!("http://127.0.0.1:{API_PORT}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn firmware_info_parses_from_a_trimmed_payload() {
        let f: FirmwareInfo = serde_json::from_str("{}").unwrap();
        assert_eq!(f, FirmwareInfo::default());
        let f: FirmwareInfo = serde_json::from_str(r#"{"installed":"1.0","future":1}"#).unwrap();
        assert_eq!(f.installed.as_deref(), Some("1.0"));
        assert!(!f.update_available && f.policy.is_empty());
    }

    use super::*;

    #[test]
    fn unknown_device_kind_reads_as_other() {
        assert_eq!(
            serde_json::from_str::<DeviceKind>("\"hovercraft\"").unwrap(),
            DeviceKind::Other
        );
        assert_eq!(
            serde_json::from_str::<DeviceKind>("\"controller\"").unwrap(),
            DeviceKind::Controller
        );
    }

    #[test]
    fn type_text_label_hides_content() {
        assert_eq!(
            Action::TypeText {
                text: "hunter2 secret".into(),
                enter: false
            }
            .log_label(),
            "type_text, 14 chars"
        );
    }

    #[test]
    fn labels_never_carry_field_contents() {
        let all = [
            Action::OpenUrl {
                url: "https://x.example/?k=SECRET".into(),
            },
            Action::Hotkey {
                keys: "SECRET".into(),
            },
            Action::OpenApp {
                path: "SECRET".into(),
            },
            Action::ObsScene {
                scene: "SECRET".into(),
            },
            Action::Placeholder {
                note: "SECRET".into(),
            },
            Action::MouseColor {
                color: "SECRET".into(),
            },
            Action::PageGo {
                page: "SECRET".into(),
            },
            Action::LightSet {
                id: Some("SECRET".into()),
                on: None,
                brightness: None,
                kelvin: None,
            },
            Action::LightToggle {
                id: Some("SECRET".into()),
            },
        ];
        for a in all {
            assert!(!a.log_label().contains("SECRET"), "{a:?}");
        }
        assert_eq!(
            Action::OpenUrl {
                url: "https://a/?t=1".into()
            }
            .log_label(),
            "open_url"
        );
        assert_eq!(Action::PrompterPlayPause.log_label(), "prompter_play_pause");
        assert_eq!(
            Action::LightSet {
                id: None,
                on: None,
                brightness: None,
                kelvin: None
            }
            .log_label(),
            "light_set"
        );
    }

    #[test]
    fn dial_turn_scales_step_by_detents() {
        let a = Action::LightBrightnessStep { id: None, step: 2 };
        assert_eq!(
            a.scaled(-3),
            Action::LightBrightnessStep { id: None, step: -6 }
        );
        assert_eq!(
            Action::PrompterPlayPause.scaled(4),
            Action::PrompterPlayPause
        );
    }

    #[test]
    fn hotkey_dials_run_one_action_per_detent() {
        let d = DialBinding {
            label: "UNDO".into(),
            turn: None,
            turn_left: Some(Action::Hotkey {
                keys: "cmd+z".into(),
            }),
            turn_right: Some(Action::Hotkey {
                keys: "cmd+shift+z".into(),
            }),
            press: None,
            alt_turn: None,
        };
        assert_eq!(
            d.turn_actions(-2),
            vec![
                Action::Hotkey {
                    keys: "cmd+z".into()
                };
                2
            ]
        );
        assert_eq!(
            d.turn_actions(1),
            vec![Action::Hotkey {
                keys: "cmd+shift+z".into()
            }]
        );
        assert_eq!(d.turn_actions(50).len(), 10, "a fast spin is capped");
        let v = DialBinding {
            turn: Some(Action::VolumeStep { step: 3 }),
            ..d
        };
        assert_eq!(v.turn_actions(-2), vec![Action::VolumeStep { step: -6 }]);
    }

    #[test]
    fn actions_round_trip_as_tagged_json() {
        let a: Action =
            serde_json::from_str(r#"{"do":"light_set","on":true,"brightness":40}"#).unwrap();
        assert_eq!(
            a,
            Action::LightSet {
                id: None,
                on: Some(true),
                brightness: Some(40),
                kelvin: None
            }
        );
        let back = serde_json::to_value(&a).unwrap();
        assert_eq!(back["do"], "light_set");
    }
}
