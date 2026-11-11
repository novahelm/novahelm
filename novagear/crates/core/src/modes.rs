//! Stream Deck modes: named sets of pages, optional strip panels, and the rules and
//! templates around them. Nothing here assumes 8 keys or 4 dials; a mode on a bigger
//! deck just has more keys, and the strip is optional.

use crate::{Action, DialBinding, Page};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Text that is either fixed or read from a feed. `"Rec"` is a literal;
/// `{"feed":"obs.record.time"}` follows a feed key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TextSource {
    Literal(String),
    Feed { feed: String },
}

/// A coloured edge on a key: always on (a colour), or only while a feed is non-zero.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Ring {
    Always(String),
    When { color: String, feed: String },
}

/// What a key draws beyond its label. Every field is optional so an empty face is `{}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Face {
    /// An image name from the config's images.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background_image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<TextSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub badge: Option<TextSource>,
    /// A feed key whose value is 0–100.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ring: Option<Ring>,
    /// A feed key; the glow shows while its value is non-zero.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub glow: Option<String>,
}

/// One of the two looks of a two-state key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyState {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default)]
    pub face: Face,
    pub action: Action,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelKind {
    #[default]
    Value,
    Gauge,
    Bar,
    Meter,
    Choice,
    Dots,
    Sparkline,
    Message,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChoiceOption {
    pub label: String,
    pub action: Action,
}

/// When a choice panel applies the option the dial points at.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Commit {
    /// As the dial turns.
    #[default]
    Turn,
    /// When the dial is pressed.
    Press,
}

/// One touch-strip panel, above one dial.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StripPanel {
    pub kind: PanelKind,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<TextSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub: Option<TextSource>,
    /// The gauge, bar, meter, dots or sparkline source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feed: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<ChoiceOption>,
    pub commit: Commit,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dial: Option<DialBinding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub press: Option<Action>,
}

/// Key indexes on a mode's home page that show and answer approvals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalSlots {
    pub approvals: usize,
    pub details: usize,
    pub approve: usize,
    pub deny: usize,
}

/// Keys every supported deck has (a Stream Deck + has 8); a home page with more allows more.
pub const MIN_DECK_KEYS: usize = 8;

/// What one key does while the approval pop-up shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotRole {
    Approvals,
    Details,
    Approve,
    Deny,
}

impl ApprovalSlots {
    /// Four different keys, each one the deck has: `keys` is how many the home page has,
    /// and at least `MIN_DECK_KEYS` count. Two roles on one key would draw one and do the
    /// other.
    pub fn valid_for(&self, keys: usize) -> bool {
        let all = [self.approvals, self.details, self.approve, self.deny];
        let n = keys.max(MIN_DECK_KEYS);
        all.iter().all(|&i| i < n) && all.iter().enumerate().all(|(k, i)| !all[..k].contains(i))
    }

    /// The role of key `i`: the one answer both the pop-up's drawing and its input use, so
    /// what a key says and what it does can never differ.
    pub fn role(&self, i: usize) -> Option<SlotRole> {
        [
            (self.approvals, SlotRole::Approvals),
            (self.details, SlotRole::Details),
            (self.approve, SlotRole::Approve),
            (self.deny, SlotRole::Deny),
        ]
        .into_iter()
        .find_map(|(k, r)| (k == i).then_some(r))
    }
}

fn default_accent() -> String {
    "#2563eb".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Mode {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    pub accent: String,
    pub pages: Vec<Page>,
    /// One optional panel per dial; the strip is optional as a whole.
    pub strip: Vec<Option<StripPanel>>,
    /// Whether approval pop-ups may take over the deck in this mode.
    pub popups: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_slots: Option<ApprovalSlots>,
}

impl Default for Mode {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            icon: None,
            accent: default_accent(),
            pages: Vec::new(),
            strip: Vec::new(),
            popups: false,
            approval_slots: None,
        }
    }
}

impl Face {
    pub fn is_empty(&self) -> bool {
        *self == Face::default()
    }
}

impl Mode {
    pub fn page(&self, name: &str) -> Option<&Page> {
        self.pages.iter().find(|p| p.name == name)
    }

    /// The mode's first page.
    pub fn home(&self) -> Option<&Page> {
        self.pages.first()
    }
}

/// Switches to `mode` while the condition holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModeRule {
    pub when: RuleWhen,
    pub mode: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "on", rename_all = "snake_case")]
pub enum RuleWhen {
    ObsRecording,
    ObsStreaming,
    AppFrontmost { bundle_id: String },
}

/// What the deck's mode key does when pressed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModeKey {
    #[default]
    Picker,
    Cycle,
}

/// A shareable set of modes and rules, with its images inline.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Template {
    pub name: String,
    pub description: String,
    pub modes: Vec<Mode>,
    pub mode_rules: Vec<ModeRule>,
    /// Image name → base64 bytes.
    pub images: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{KeyBinding, Step};
    use serde_json::json;

    fn key(label: &str) -> KeyBinding {
        KeyBinding {
            label: label.into(),
            color: "#000000".into(),
            icon: None,
            face: Face::default(),
            states: None,
            follow: None,
            hold_ms: 0,
            action: Action::ObsRecordToggle,
        }
    }

    #[test]
    fn mode_round_trips() {
        let panel = |kind| StripPanel {
            kind,
            label: "L".into(),
            value: Some(TextSource::Literal("Rec".into())),
            sub: Some(TextSource::Feed {
                feed: "obs.record.time".into(),
            }),
            feed: Some("obs.mic".into()),
            options: vec![ChoiceOption {
                label: "A".into(),
                action: Action::ModeNext,
            }],
            commit: Commit::Press,
            dial: None,
            press: Some(Action::ModePicker),
        };
        let kinds = [
            PanelKind::Value,
            PanelKind::Gauge,
            PanelKind::Bar,
            PanelKind::Meter,
            PanelKind::Choice,
            PanelKind::Dots,
            PanelKind::Sparkline,
            PanelKind::Message,
        ];
        let mut k = key("REC");
        k.face = Face {
            background_image: Some("bg".into()),
            value: Some(TextSource::Literal("x".into())),
            badge: Some(TextSource::Feed { feed: "n".into() }),
            progress: Some("p".into()),
            ring: Some(Ring::When {
                color: "#f00".into(),
                feed: "obs.record".into(),
            }),
            glow: Some("g".into()),
        };
        let state = |l: &str| KeyState {
            label: l.into(),
            color: Some("#111111".into()),
            icon: None,
            face: Face {
                ring: Some(Ring::Always("#fff".into())),
                ..Face::default()
            },
            action: Action::ObsRecordToggle,
        };
        k.states = Some([state("OFF"), state("ON")]);
        k.follow = Some("obs.record".into());
        k.hold_ms = 1000;
        let mode = Mode {
            id: "dev".into(),
            name: "Dev".into(),
            icon: Some("code".into()),
            pages: vec![Page {
                name: "Home".into(),
                keys: vec![Some(k), None],
                dials: vec![],
            }],
            strip: kinds.iter().map(|k| Some(panel(*k))).collect(),
            popups: true,
            approval_slots: Some(ApprovalSlots {
                approvals: 0,
                details: 1,
                approve: 2,
                deny: 3,
            }),
            ..Mode::default()
        };
        let back: Mode = serde_json::from_str(&serde_json::to_string(&mode).unwrap()).unwrap();
        assert_eq!(back, mode);
    }

    #[test]
    fn text_source_accepts_plain_string_and_feed_object() {
        assert_eq!(
            serde_json::from_str::<TextSource>("\"Rec\"").unwrap(),
            TextSource::Literal("Rec".into())
        );
        assert_eq!(
            serde_json::from_str::<TextSource>(r#"{"feed":"obs.record.time"}"#).unwrap(),
            TextSource::Feed {
                feed: "obs.record.time".into()
            }
        );
    }

    #[test]
    fn new_actions_log_without_contents() {
        let l = Action::ClientEvent {
            client: "novaguard".into(),
            name: "restart".into(),
            value: Some(json!("api-2")),
        }
        .log_label();
        assert!(l.contains("client_event"));
        assert!(!l.contains("api-2") && !l.contains("restart") && !l.contains("novaguard"));
        let seq = Action::Sequence {
            steps: vec![
                Step {
                    action: Action::ModeNext,
                    delay_ms: 0,
                };
                3
            ],
        };
        assert_eq!(seq.log_label(), "sequence, 3 steps");
        let v = Action::ObsInputVolumeStep {
            input: "Mic/Aux".into(),
            step: 1,
        };
        assert!(!v.log_label().contains("Mic"));
        for a in [
            Action::ModeGo {
                mode: "SECRET".into(),
            },
            Action::ObsInputMuteToggle {
                input: "SECRET".into(),
            },
        ] {
            assert!(!a.log_label().contains("SECRET"));
        }
    }

    #[test]
    fn new_step_actions_scale_with_detents() {
        assert_eq!(
            Action::ObsInputVolumeStep {
                input: "m".into(),
                step: 2
            }
            .scaled(-3),
            Action::ObsInputVolumeStep {
                input: "m".into(),
                step: -6
            }
        );
        assert_eq!(
            Action::ApprovalStep { step: 1 }.scaled(4),
            Action::ApprovalStep { step: 4 }
        );
    }

    #[test]
    fn rule_when_tags() {
        let w: RuleWhen = serde_json::from_str(
            r#"{"on":"app_frontmost","bundle_id":"com.obsproject.obs-studio"}"#,
        )
        .unwrap();
        assert_eq!(
            w,
            RuleWhen::AppFrontmost {
                bundle_id: "com.obsproject.obs-studio".into()
            }
        );
        assert_eq!(
            serde_json::from_str::<RuleWhen>(r#"{"on":"obs_recording"}"#).unwrap(),
            RuleWhen::ObsRecording
        );
    }

    #[test]
    fn mode_is_not_tied_to_the_plus() {
        let mode = Mode {
            id: "big".into(),
            name: "Big".into(),
            pages: vec![Page {
                name: "Home".into(),
                keys: (0..15).map(|i| Some(key(&format!("K{i}")))).collect(),
                dials: vec![],
            }],
            ..Mode::default()
        };
        let back: Mode = serde_json::from_str(&serde_json::to_string(&mode).unwrap()).unwrap();
        assert_eq!(back.home().unwrap().keys.len(), 15);
        assert!(back.strip.is_empty());
        assert_eq!(back, mode);
    }

    #[test]
    fn old_key_json_still_loads_with_defaults() {
        let k: KeyBinding =
            serde_json::from_str(r#"{"label":"A","action":{"do":"light_toggle"}}"#).unwrap();
        assert_eq!(k.face, Face::default());
        assert!(k.states.is_none() && k.follow.is_none() && k.hold_ms == 0);
        let v = serde_json::to_value(&k).unwrap();
        for f in ["face", "states", "follow", "hold_ms"] {
            assert!(v.get(f).is_none(), "{f} is skipped when unset");
        }
    }
}
