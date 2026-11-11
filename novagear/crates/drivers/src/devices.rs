//! Device records: what we know about each piece of gear, kept as TOML data instead of
//! match arms. The built-in files are embedded; a user file layers on top and replaces a
//! record with the same id (`usb`, or `elgato_board` for Key Lights). Every record is
//! parsed on its own, so one bad record costs only itself.

use anyhow::Result;
use novagear_core::{DeviceKind, UsbId};
use serde::Deserialize;
use serde::de::IntoDeserializer;
use std::path::PathBuf;

const BUILTIN: [&str; 5] = [
    include_str!("../devices/elgato.toml"),
    include_str!("../devices/razer.toml"),
    include_str!("../devices/eightbitdo.toml"),
    include_str!("../devices/sony.toml"),
    include_str!("../devices/other.toml"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Family {
    #[serde(rename = "keylight")]
    KeyLight,
    #[serde(rename = "streamdeck")]
    StreamDeck,
    Razer90,
    Uvc,
    #[serde(rename = "displaylink")]
    DisplayLink,
    Hid,
    None,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FirmwarePolicy {
    Native,
    Fwupd,
    VendorTool,
    #[default]
    None,
}

impl FirmwarePolicy {
    /// The name the API shows.
    pub fn as_str(self) -> &'static str {
        match self {
            FirmwarePolicy::Native => "native",
            FirmwarePolicy::Fwupd => "fwupd",
            FirmwarePolicy::VendorTool => "vendor_tool",
            FirmwarePolicy::None => "none",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Firmware {
    #[serde(default)]
    pub policy: FirmwarePolicy,
    pub tool: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Quirks {
    pub interface: Option<i32>,
    pub transaction_id: Option<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub usb: Option<UsbId>,
    /// Key Lights: accessory-info `hardwareBoardType`.
    pub elgato_board: Option<u8>,
    pub name: String,
    pub kind: DeviceKind,
    pub family: Family,
    pub quirks: Quirks,
    pub firmware: Firmware,
    /// Loose; each family's driver reads what it needs.
    pub caps: toml::Table,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vendor {
    pub vid: u16,
    pub name: String,
    pub kind: DeviceKind,
}

#[derive(Deserialize)]
struct RawDevice {
    usb: Option<String>,
    elgato_board: Option<u8>,
    name: String,
    kind: String,
    family: String,
    #[serde(default)]
    quirks: Quirks,
    #[serde(default)]
    firmware: Firmware,
    #[serde(default)]
    caps: toml::Table,
}

#[derive(Deserialize)]
struct RawVendor {
    vid: u16,
    name: String,
    kind: String,
}

#[derive(Deserialize, Default)]
struct RawFile {
    #[serde(default)]
    vendor: Vec<toml::Value>,
    #[serde(default)]
    device: Vec<toml::Value>,
}

fn parse_usb(s: &str) -> Option<UsbId> {
    let (v, p) = s.split_once(':')?;
    Some(UsbId {
        vid: u16::from_str_radix(v, 16).ok()?,
        pid: u16::from_str_radix(p, 16).ok()?,
    })
}

fn word<T: for<'de> Deserialize<'de>>(what: &str, s: &str) -> Result<T, String> {
    T::deserialize(
        s.into_deserializer() as serde::de::value::StrDeserializer<'_, serde::de::value::Error>
    )
    .map_err(|_| format!("unknown {what} \"{s}\""))
}

impl RawDevice {
    fn into_record(self) -> Result<Record, String> {
        let usb = match &self.usb {
            Some(s) => Some(parse_usb(s).ok_or_else(|| format!("bad usb id \"{s}\""))?),
            None => None,
        };
        if usb.is_none() && self.elgato_board.is_none() {
            return Err("needs usb or elgato_board".into());
        }
        Ok(Record {
            usb,
            elgato_board: self.elgato_board,
            name: self.name,
            kind: word("kind", &self.kind)?,
            family: word("family", &self.family)?,
            quirks: self.quirks,
            firmware: self.firmware,
            caps: self.caps,
        })
    }
}

impl RawVendor {
    fn into_vendor(self) -> Result<Vendor, String> {
        Ok(Vendor {
            vid: self.vid,
            name: self.name,
            kind: word("kind", &self.kind)?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Records {
    devices: Vec<Record>,
    vendors: Vec<Vendor>,
}

impl Records {
    fn empty() -> Records {
        Records {
            devices: Vec::new(),
            vendors: Vec::new(),
        }
    }

    /// Merges one file in, replacing records with the same id. Returns the problems found.
    fn merge(&mut self, text: &str) -> Vec<String> {
        let mut problems = Vec::new();
        let file: RawFile = match toml::from_str(text) {
            Ok(f) => f,
            Err(e) => return vec![format!("devices.toml: {}", e.message())],
        };
        for v in file.vendor {
            match v
                .try_into::<RawVendor>()
                .map_err(|e| e.message().to_string())
                .and_then(RawVendor::into_vendor)
            {
                Ok(v) => {
                    self.vendors.retain(|o| o.vid != v.vid);
                    self.vendors.push(v);
                }
                Err(e) => problems.push(format!("devices.toml: vendor: {e}")),
            }
        }
        for d in file.device {
            let label = d
                .get("usb")
                .and_then(|u| u.as_str().map(str::to_string))
                .or_else(|| d.get("elgato_board").map(|b| format!("board {b}")))
                .unwrap_or_else(|| "?".into());
            match d
                .try_into::<RawDevice>()
                .map_err(|e| e.message().to_string())
                .and_then(RawDevice::into_record)
            {
                Ok(r) => {
                    self.devices.retain(|o| {
                        !((r.usb.is_some() && o.usb == r.usb)
                            || (r.elgato_board.is_some() && o.elgato_board == r.elgato_board))
                    });
                    self.devices.push(r);
                }
                Err(e) => problems.push(format!("devices.toml: device {label}: {e}")),
            }
        }
        problems
    }

    /// Parses the embedded files. They are checked by tests, so a problem is a bug.
    pub fn builtin() -> Records {
        let mut r = Records::empty();
        for file in BUILTIN {
            let problems = r.merge(file);
            debug_assert!(problems.is_empty(), "bad built-in records: {problems:?}");
        }
        r
    }

    /// The built-ins plus a user file. A bad record is dropped alone and reported.
    pub fn with_overrides(text: &str) -> (Records, Vec<String>) {
        let mut r = Records::builtin();
        let problems = r.merge(text);
        (r, problems)
    }

    pub fn by_usb(&self, id: UsbId) -> Option<&Record> {
        self.devices.iter().find(|r| r.usb == Some(id))
    }

    pub fn by_elgato_board(&self, board: u8) -> Option<&Record> {
        self.devices.iter().find(|r| r.elgato_board == Some(board))
    }

    pub fn vendor(&self, vid: u16) -> Option<&Vendor> {
        self.vendors.iter().find(|v| v.vid == vid)
    }
}

/// Keys a `[[device]]` and a `[[vendor]]` table may have. Anything else is most likely a typo
/// (serde ignores unknown keys, so `quirk = ...` would silently do nothing).
const DEVICE_KEYS: [&str; 8] = [
    "usb",
    "elgato_board",
    "name",
    "kind",
    "family",
    "quirks",
    "firmware",
    "caps",
];
const VENDOR_KEYS: [&str; 3] = ["vid", "name", "kind"];

/// Checks one record file the way a contribution is checked: every record must parse on its
/// own, ids must be unique within the file, and unknown keys are reported. Returns the
/// problems (empty means good) and, separately, notes such as "replaces the built-in record".
pub fn check(text: &str) -> (Vec<String>, Vec<String>) {
    let mut scratch = Records::empty();
    let mut problems = scratch.merge(text);
    let mut notes = Vec::new();
    let Ok(doc) = text.parse::<toml::Table>() else {
        return (problems, notes);
    };
    let builtin = Records::builtin();
    let mut seen = std::collections::HashSet::new();
    for (section, allowed) in [("device", &DEVICE_KEYS[..]), ("vendor", &VENDOR_KEYS[..])] {
        for t in doc
            .get(section)
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_table())
        {
            let id = t
                .get("usb")
                .and_then(|v| v.as_str())
                .map(|u| format!("usb {u}"))
                .or_else(|| t.get("elgato_board").map(|b| format!("board {b}")))
                .or_else(|| t.get("vid").map(|v| format!("vendor {v}")))
                .unwrap_or_else(|| "?".into());
            for key in t.keys().filter(|k| !allowed.contains(&k.as_str())) {
                problems.push(format!("{section} {id}: unknown key \"{key}\""));
            }
            if id != "?" && !seen.insert(id.clone()) {
                problems.push(format!("{section} {id}: appears twice in this file"));
            }
            let usb = t.get("usb").and_then(|v| v.as_str()).and_then(parse_usb);
            let board = t
                .get("elgato_board")
                .and_then(|v| v.as_integer())
                .and_then(|b| u8::try_from(b).ok());
            if usb.is_some_and(|u| builtin.by_usb(u).is_some())
                || board.is_some_and(|b| builtin.by_elgato_board(b).is_some())
            {
                notes.push(format!("{section} {id}: replaces the built-in record"));
            }
        }
    }
    (problems, notes)
}

fn toml_string(s: &str) -> String {
    // A JSON string is a valid TOML basic string.
    serde_json::Value::String(s.to_string()).to_string()
}

/// A starting point for a record, from what "Copy device details" sees of a device with no
/// record. Never includes the serial or the device path. `family = "none"` lists the device
/// without driving it; a contributor changes it when an existing driver family fits.
pub fn draft_record(u: &crate::catalog::UsbDevice) -> String {
    let name = match (u.vendor.trim(), u.product.trim()) {
        (_, "") => u.name.clone(),
        ("", p) => p.to_string(),
        (v, p) if p.starts_with(v) => p.to_string(),
        (v, p) => format!("{v} {p}"),
    };
    let kind = serde_json::to_value(u.kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "other".into());
    format!(
        "# Draft from \"Copy device details\". Check every value; see devices/README.md.\n\
         [[device]]\n\
         usb = \"{:04x}:{:04x}\"\n\
         name = {}\n\
         kind = \"{kind}\"\n\
         family = \"none\"\n",
        u.usb.vid,
        u.usb.pid,
        toml_string(&name),
    )
}

/// Where the user's own record file lives.
pub fn user_file() -> Result<PathBuf> {
    Ok(novagear_core::config::config_dir()?.join("devices.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::STREAM_DECK_PLUS;
    use crate::streamdeck;
    use novagear_core::{DeviceKind, UsbId};

    #[test]
    fn builtin_records_parse_and_are_unique() {
        let r = Records::builtin();
        assert_eq!(
            r.by_usb(UsbId {
                vid: 0x0fd9,
                pid: 0x0084
            })
            .unwrap()
            .family,
            Family::StreamDeck
        );
        let b = r
            .by_usb(UsbId {
                vid: 0x1532,
                pid: 0x0099,
            })
            .unwrap();
        assert_eq!(b.quirks.interface, Some(3));
        assert_eq!(b.quirks.transaction_id, Some(0x1f));
        assert_eq!(r.by_elgato_board(53).unwrap().name, "Key Light");
        assert_eq!(
            r.by_usb(UsbId {
                vid: 0x054c,
                pid: 0x0ce6
            })
            .unwrap()
            .kind,
            DeviceKind::Controller
        );
        assert_eq!(r.vendor(0x1532).unwrap().name, "Razer");
    }

    #[test]
    fn every_builtin_file_passes_the_contribution_check() {
        for file in BUILTIN {
            let (problems, _) = check(file);
            assert!(problems.is_empty(), "{problems:?}");
        }
    }

    #[test]
    fn check_reports_typos_duplicates_and_bad_records_and_notes_overrides() {
        let (problems, notes) = check(
            r#"
[[device]]
usb = "1532:00aa"
name = "Mouse"
kind = "mouse"
family = "razer90"
quirk = { interface = 3 }

[[device]]
usb = "1532:00aa"
name = "Mouse again"
kind = "mouse"
family = "razer90"

[[device]]
usb = "zz:1"
name = "Bad"
kind = "mouse"
family = "none"

[[device]]
usb = "0fd9:0084"
name = "Stream Deck +"
kind = "stream_deck"
family = "streamdeck"
"#,
        );
        let all = problems.join("\n");
        assert!(all.contains(r#"unknown key "quirk""#), "{all}");
        assert!(all.contains("usb 1532:00aa: appears twice"), "{all}");
        assert!(all.contains("bad usb id"), "{all}");
        assert_eq!(problems.len(), 3, "{all}");
        assert_eq!(
            notes,
            ["device usb 0fd9:0084: replaces the built-in record"]
        );
        assert_eq!(check("not = [toml").0.len(), 1);
    }

    #[test]
    fn a_draft_record_is_a_valid_record_without_the_serial() {
        let u = crate::catalog::UsbDevice {
            usb: UsbId {
                vid: 0x046d,
                pid: 0xc900,
            },
            key: "usb-046d-c900-SERIAL123".into(),
            vendor: "Logitech".into(),
            name: "Logitech Litra \"Glow\"".into(),
            serial: Some("SERIAL123".into()),
            kind: DeviceKind::Light,
            family: Family::None,
            device_version: Some("1.02".into()),
            unknown: true,
            product: "Litra \"Glow\"".into(),
            interfaces: vec![3],
            usage_pages: vec![0xff43],
        };
        let draft = draft_record(&u);
        assert!(!draft.contains("SERIAL123"), "{draft}");
        assert!(draft.contains("usb = \"046d:c900\""), "{draft}");
        assert!(draft.contains("kind = \"light\""), "{draft}");
        let (problems, notes) = check(&draft);
        assert!(
            problems.is_empty() && notes.is_empty(),
            "{problems:?} {notes:?}"
        );
        let (r, _) = Records::with_overrides(&draft);
        let rec = r
            .by_usb(UsbId {
                vid: 0x046d,
                pid: 0xc900,
            })
            .unwrap();
        assert_eq!(rec.name, "Logitech Litra \"Glow\"");
        assert_eq!(rec.family, Family::None);
    }

    #[test]
    fn every_builtin_file_has_no_duplicate_ids() {
        let mut ids = std::collections::HashSet::new();
        for file in BUILTIN {
            let doc: toml::Table = file.parse().unwrap();
            for d in doc["device"].as_array().unwrap() {
                let t = d.as_table().unwrap();
                let id = t
                    .get("usb")
                    .map(|v| format!("usb {v}"))
                    .or_else(|| t.get("elgato_board").map(|v| format!("board {v}")))
                    .expect("record has an id");
                assert!(ids.insert(id.clone()), "duplicate {id}");
            }
        }
    }

    #[test]
    fn plus_record_matches_the_driver() {
        let r = Records::builtin();
        let c = &r.by_usb(STREAM_DECK_PLUS).unwrap().caps;
        assert_eq!(
            c["keys"]["px"].as_integer(),
            Some(streamdeck::KEY_PX as i64)
        );
        assert_eq!(c["dials"].as_integer(), Some(streamdeck::DIALS as i64));
        assert_eq!(
            c["strip"]["w"].as_integer(),
            Some(streamdeck::STRIP_W as i64)
        );
    }

    #[test]
    fn override_replaces_and_adds() {
        let (r, p) = Records::with_overrides(
            r#"
            [[device]]
            usb = "1532:0099"
            name = "My Basilisk"
            kind = "mouse"
            family = "razer90"
            quirks = { interface = 3, transaction_id = 0x1F }
            [[device]]
            usb = "1234:5678"
            name = "Test Pad"
            kind = "controller"
            family = "none"
        "#,
        );
        assert!(p.is_empty());
        assert_eq!(
            r.by_usb(UsbId {
                vid: 0x1532,
                pid: 0x0099
            })
            .unwrap()
            .name,
            "My Basilisk"
        );
        assert_eq!(
            r.by_usb(UsbId {
                vid: 0x1234,
                pid: 0x5678
            })
            .unwrap()
            .name,
            "Test Pad"
        );
    }

    #[test]
    fn bad_override_record_is_dropped_alone() {
        let (r, p) = Records::with_overrides(
            r#"
            [[device]]
            usb = "1532:0099"
            name = "Broken"
            kind = "mouse"
            family = "warpdrive"
            [[device]]
            usb = "zz:01"
            name = "Bad id"
            kind = "mouse"
            family = "none"
            [[device]]
            usb = "1234:5678"
            name = "Fine"
            kind = "keyboard"
            family = "none"
        "#,
        );
        assert_eq!(p.len(), 2);
        assert!(p[0].starts_with("devices.toml: device 1532:0099: unknown family"));
        assert_eq!(
            r.by_usb(UsbId {
                vid: 0x1532,
                pid: 0x0099
            })
            .unwrap()
            .name,
            "Razer Basilisk V3"
        );
        assert_eq!(
            r.by_usb(UsbId {
                vid: 0x1234,
                pid: 0x5678
            })
            .unwrap()
            .name,
            "Fine"
        );
    }

    #[test]
    fn unparseable_override_file_keeps_builtins() {
        let (r, p) = Records::with_overrides("this is [not toml");
        assert_eq!(p.len(), 1);
        assert!(
            r.by_usb(UsbId {
                vid: 0x0fd9,
                pid: 0x0084
            })
            .is_some()
        );
    }
}
