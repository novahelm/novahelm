//! Imports Elgato Stream Deck profiles (ProfilesV3) as NovaGear pages, so the Stream Deck
//! app can be quit without rebuilding a layout by hand. Every Elgato page and folder becomes
//! a page; actions NovaGear runs are mapped, the rest become labelled placeholders.

use anyhow::{Context, Result};
use novagear_core::{Action, DialBinding, KeyBinding, Page};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Stream Deck +: the only model NovaGear drives so far.
const SD_PLUS_MODEL: &str = "20GBD9901";
const COLS: usize = 4;
const ROWS: usize = 2;
const DIALS: usize = 4;

const NAV_COLOR: &str = "#2c2c2e";
const PLACEHOLDER_COLOR: &str = "#48484a";
const KEY_COLOR: &str = "#1e2230";

pub fn default_profiles_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let mac = home.join("Library/Application Support/com.elgato.StreamDeck/ProfilesV3");
    if mac.is_dir() {
        return Some(mac);
    }
    let win = std::env::var_os("APPDATA")
        .map(|a| PathBuf::from(a).join("Elgato/StreamDeck/ProfilesV3"))?;
    win.is_dir().then_some(win)
}

#[derive(Debug, Default)]
pub struct Imported {
    pub pages: Vec<Page>,
    pub start_page: String,
    /// Elgato action names NovaGear can't run yet, with how many keys use each.
    pub placeholders: Vec<(String, usize)>,
    pub skipped_profiles: Vec<String>,
    pub icons: usize,
}

struct Profile {
    id: String,
    name: String,
    dir: PathBuf,
    /// Page folder ids (lowercase) in the order Elgato lists them; folders aren't listed.
    pages: Vec<String>,
}

struct PageSrc {
    profile: usize,
    id: String,
    dir: PathBuf,
    manifest: Value,
}

fn read_json(p: &Path) -> Result<Value> {
    serde_json::from_slice(&std::fs::read(p).with_context(|| format!("reading {}", p.display()))?)
        .with_context(|| format!("parsing {}", p.display()))
}

fn title_of(a: &Value) -> String {
    let state = a["State"].as_u64().unwrap_or(0) as usize;
    let states = a["States"].as_array();
    let t = states
        .and_then(|s| s.get(state).or(s.first()))
        .and_then(|s| s["Title"].as_str())
        .unwrap_or("");
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn image_of(a: &Value) -> Option<String> {
    let state = a["State"].as_u64().unwrap_or(0) as usize;
    let states = a["States"].as_array()?;
    states.get(state).or(states.first())?["Image"]
        .as_str()
        .map(str::to_string)
}

/// Unique page names: Elgato lets two folders share a title.
fn unique(name: String, taken: &mut Vec<String>) -> String {
    let mut n = name.clone();
    let mut i = 2;
    while taken.contains(&n) {
        n = format!("{name} {i}");
        i += 1;
    }
    taken.push(n.clone());
    n
}

fn app_name(path: &str) -> String {
    let p = path.trim().trim_matches('"');
    Path::new(p)
        .file_stem()
        .map_or_else(|| p.to_string(), |s| s.to_string_lossy().to_string())
}

fn url_host(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    rest.split('/')
        .next()
        .unwrap_or(rest)
        .trim_start_matches("www.")
        .to_string()
}

fn hotkey_string(h: &Value) -> Option<String> {
    let code = h["NativeCode"].as_i64().filter(|&c| c >= 0)?;
    let mut parts = Vec::new();
    for (key, name) in [
        ("KeyCmd", "cmd"),
        ("KeyCtrl", "ctrl"),
        ("KeyOption", "alt"),
        ("KeyShift", "shift"),
    ] {
        if h[key].as_bool() == Some(true) {
            parts.push(name.to_string());
        }
    }
    parts.push(crate::system::mac_key_name(u16::try_from(code).ok()?));
    Some(parts.join("+"))
}

/// Reads every profile under `dir` and converts the Stream Deck + ones.
pub fn import(dir: &Path, icons_dir: &Path) -> Result<Imported> {
    let mut out = Imported::default();
    let mut profiles = Vec::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "sdProfile"))
        .collect();
    entries.sort();
    for p in entries {
        let m = read_json(&p.join("manifest.json"))?;
        let name = m["Name"].as_str().unwrap_or("Profile").to_string();
        if m["Device"]["Model"].as_str() != Some(SD_PLUS_MODEL) {
            out.skipped_profiles.push(name);
            continue;
        }
        let id = p
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();
        let pages = m["Pages"]["Pages"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_lowercase))
                    .collect()
            })
            .unwrap_or_default();
        profiles.push(Profile {
            id,
            name,
            dir: p,
            pages,
        });
    }

    // Every page folder, listed pages first in Elgato's order, then folders.
    let mut srcs: Vec<PageSrc> = Vec::new();
    for (pi, prof) in profiles.iter().enumerate() {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(prof.dir.join("Profiles"))
            .map(|r| {
                r.filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.join("manifest.json").is_file())
                    .collect()
            })
            .unwrap_or_default();
        let order = |p: &PathBuf| {
            let id = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase();
            prof.pages
                .iter()
                .position(|x| *x == id)
                .unwrap_or(usize::MAX)
        };
        dirs.sort_by_key(|p| (order(p), p.clone()));
        for d in dirs {
            let id = d
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase();
            let manifest = read_json(&d.join("manifest.json"))?;
            srcs.push(PageSrc {
                profile: pi,
                id,
                dir: d,
                manifest,
            });
        }
    }

    // Folder titles come from the keys that open them.
    let mut folder_titles: HashMap<String, String> = HashMap::new();
    for s in &srcs {
        for c in s.manifest["Controllers"].as_array().into_iter().flatten() {
            for a in c["Actions"]
                .as_object()
                .into_iter()
                .flat_map(|o| o.values())
            {
                if a["UUID"] == "com.elgato.streamdeck.profile.openchild"
                    && let Some(target) = a["Settings"]["ProfileUUID"].as_str()
                {
                    let t = title_of(a);
                    if !t.is_empty() {
                        folder_titles.entry(target.to_lowercase()).or_insert(t);
                    }
                }
            }
        }
    }

    // Page names, keyed by (profile, page id).
    let mut taken = Vec::new();
    let mut names: HashMap<(usize, String), String> = HashMap::new();
    for s in &srcs {
        let prof = &profiles[s.profile];
        let listed = prof.pages.iter().position(|x| *x == s.id);
        let base = match listed {
            Some(0) => prof.name.clone(),
            Some(i) => format!("{} {}", prof.name, i + 1),
            None => format!(
                "{} / {}",
                prof.name,
                folder_titles
                    .get(&s.id)
                    .cloned()
                    .unwrap_or_else(|| "Folder".into())
            ),
        };
        names.insert((s.profile, s.id.clone()), unique(base, &mut taken));
    }
    let profile_page = |id: &str, index: usize| -> Option<String> {
        let pi = profiles.iter().position(|p| p.id == id.to_lowercase())?;
        let page = profiles[pi]
            .pages
            .get(index)
            .or(profiles[pi].pages.first())?;
        names.get(&(pi, page.clone())).cloned()
    };

    std::fs::create_dir_all(icons_dir)?;
    let mut missing: HashMap<String, usize> = HashMap::new();
    let mut main_votes: HashMap<String, usize> = HashMap::new();

    for s in &srcs {
        let mut keys: Vec<Option<KeyBinding>> = vec![None; COLS * ROWS];
        let mut dials: Vec<Option<DialBinding>> = vec![None; DIALS];
        for c in s.manifest["Controllers"].as_array().into_iter().flatten() {
            let encoder = c["Type"] == "Encoder";
            for (pos, a) in c["Actions"].as_object().into_iter().flatten() {
                let mut it = pos.split(',').filter_map(|n| n.parse::<usize>().ok());
                let (Some(col), Some(row)) = (it.next(), it.next()) else {
                    continue;
                };
                let uuid = a["UUID"].as_str().unwrap_or("");
                let set = &a["Settings"];
                let title = title_of(a);
                let target_page = |key: &str| -> Option<String> {
                    let id = set[key].as_str()?;
                    if uuid == "com.elgato.streamdeck.profile.openchild" {
                        names.get(&(s.profile, id.to_lowercase())).cloned()
                    } else {
                        let idx = set["PageIndex"].as_u64().unwrap_or(1).saturating_sub(1) as usize;
                        profile_page(id, idx)
                    }
                };

                if encoder {
                    if col >= DIALS {
                        continue;
                    }
                    let step = |d: i64| {
                        set["volumeStep"]
                            .as_str()
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(d) as i16
                    };
                    let (turn, left, right, press, label) = match uuid {
                        "com.elgato.volume-controller.output-device-control" => (
                            Some(Action::VolumeStep { step: step(3) }),
                            None,
                            None,
                            Some(Action::MuteToggle),
                            "VOLUME",
                        ),
                        "com.elgato.volume-controller.input-device-control" => (
                            Some(Action::MicVolumeStep { step: step(3) }),
                            None,
                            None,
                            Some(Action::MicMuteToggle),
                            "MIC",
                        ),
                        "com.elgato.streamdeck.system.multimedia" => (
                            Some(Action::VolumeStep { step: 3 }),
                            None,
                            None,
                            Some(Action::MuteToggle),
                            "VOLUME",
                        ),
                        "com.elgato.streamdeck.system.keybrightness" => (
                            Some(Action::DeckBrightnessStep { step: 5 }),
                            None,
                            None,
                            None,
                            "DECK",
                        ),
                        "com.elgato.streamdeck.system.hotkey" => {
                            let hk: Vec<Option<String>> = set["Hotkeys"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .map(hotkey_string)
                                .collect();
                            let as_action = |i: usize| {
                                hk.get(i)
                                    .cloned()
                                    .flatten()
                                    .map(|keys| Action::Hotkey { keys })
                            };
                            (None, as_action(1), as_action(0), None, "KEYS")
                        }
                        _ => {
                            *missing
                                .entry(a["Name"].as_str().unwrap_or(uuid).to_string())
                                .or_default() += 1;
                            (None, None, None, None, "")
                        }
                    };
                    if turn.is_none() && left.is_none() && right.is_none() && press.is_none() {
                        continue;
                    }
                    dials[col] = Some(DialBinding {
                        label: if title.is_empty() {
                            label.into()
                        } else {
                            title.to_uppercase()
                        },
                        turn,
                        turn_left: left,
                        turn_right: right,
                        press,
                        alt_turn: None,
                    });
                    continue;
                }

                if col >= COLS || row >= ROWS {
                    continue;
                }
                let (action, fallback_label, color) = match uuid {
                    "com.elgato.streamdeck.profile.backtoparent" => {
                        (Some(Action::PageBack), "BACK".to_string(), NAV_COLOR)
                    }
                    "com.elgato.streamdeck.profile.rotate"
                    | "com.elgato.streamdeck.profile.openchild" => {
                        match target_page("ProfileUUID") {
                            Some(page) => {
                                if title.eq_ignore_ascii_case("main") {
                                    *main_votes.entry(page.clone()).or_default() += 1;
                                }
                                let label = page.rsplit(" / ").next().unwrap_or(&page).to_string();
                                (Some(Action::PageGo { page }), label, NAV_COLOR)
                            }
                            None => (None, String::new(), NAV_COLOR),
                        }
                    }
                    "com.elgato.streamdeck.system.website"
                        if set["path"].as_str().unwrap_or("").trim().is_empty() =>
                    {
                        // A website key with no address does nothing in Elgato's app either.
                        continue;
                    }
                    "com.elgato.streamdeck.system.website" => {
                        let raw = set["path"].as_str().unwrap_or("").trim().to_string();
                        let url = if raw.contains("://") || raw.starts_with("mailto:") {
                            raw
                        } else {
                            format!("https://{raw}")
                        };
                        let host = url_host(&url);
                        (Some(Action::OpenUrl { url }), host, KEY_COLOR)
                    }
                    "com.elgato.streamdeck.system.open" => {
                        let path = set["path"]
                            .as_str()
                            .unwrap_or("")
                            .trim()
                            .trim_matches('"')
                            .to_string();
                        let label = app_name(&path);
                        (Some(Action::OpenApp { path }), label, KEY_COLOR)
                    }
                    "com.elgato.streamdeck.system.text" => {
                        let text = set["pastedText"].as_str().unwrap_or("").to_string();
                        let enter = set["isSendingEnter"].as_bool().unwrap_or(false);
                        let label: String = text.chars().take(8).collect();
                        (Some(Action::TypeText { text, enter }), label, KEY_COLOR)
                    }
                    "com.elgato.streamdeck.system.hotkey" => {
                        let keys = set["Hotkeys"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .find_map(hotkey_string);
                        let label = keys.clone().unwrap_or_default();
                        (keys.map(|keys| Action::Hotkey { keys }), label, KEY_COLOR)
                    }
                    "com.elgato.obsstudio.scene" => {
                        let scene = set["scene"].as_str().unwrap_or("").to_string();
                        (
                            Some(Action::ObsScene {
                                scene: scene.clone(),
                            }),
                            scene,
                            "#2a2f45",
                        )
                    }
                    "com.elgato.obsstudio.record" => {
                        (Some(Action::ObsRecordToggle), "REC".into(), "#8e1b1b")
                    }
                    "com.elgato.obsstudio.stream" => {
                        (Some(Action::ObsStreamToggle), "LIVE".into(), "#8e1b1b")
                    }
                    "com.elgato.camerahub.prompterdisplaysettings" => {
                        (Some(Action::PrompterPlayPause), "PROMPT".into(), "#2a6fdb")
                    }
                    _ => (None, String::new(), PLACEHOLDER_COLOR),
                };
                let (action, color) = match action {
                    Some(a) => (a, color),
                    None => {
                        let name = a["Name"].as_str().unwrap_or(uuid).to_string();
                        *missing.entry(name.clone()).or_default() += 1;
                        (Action::Placeholder { note: name }, PLACEHOLDER_COLOR)
                    }
                };
                let icon = image_of(a).and_then(|rel| {
                    let src = s.dir.join(&rel);
                    let file = format!(
                        "{}-{}",
                        profiles[s.profile].id.get(..8).unwrap_or("p"),
                        Path::new(&rel).file_name()?.to_string_lossy()
                    );
                    std::fs::copy(&src, icons_dir.join(&file)).ok()?;
                    out.icons += 1;
                    Some(file)
                });
                let label = if title.is_empty() {
                    if icon.is_some() {
                        String::new()
                    } else {
                        fallback_label
                    }
                } else {
                    title
                };
                keys[row * COLS + col] = Some(KeyBinding {
                    label,
                    color: color.into(),
                    icon,
                    face: Default::default(),
                    states: None,
                    follow: None,
                    hold_ms: 0,
                    action,
                });
            }
        }
        let name = names[&(s.profile, s.id.clone())].clone();
        out.pages.push(Page { name, keys, dials });
    }

    // Elgato leaves empty folder pages behind that no key opens; they'd only clutter the list.
    let referenced: Vec<String> = out
        .pages
        .iter()
        .flat_map(|p| p.keys.iter().flatten().map(|k| &k.action))
        .filter_map(|a| match a {
            Action::PageGo { page } => Some(page.clone()),
            _ => None,
        })
        .collect();
    let listed: Vec<String> = srcs
        .iter()
        .filter(|s| profiles[s.profile].pages.contains(&s.id))
        .map(|s| names[&(s.profile, s.id.clone())].clone())
        .collect();
    out.pages.retain(|p| {
        let empty = p.keys.iter().all(Option::is_none) && p.dials.iter().all(Option::is_none);
        !(empty && !referenced.contains(&p.name) && !listed.contains(&p.name))
    });

    out.start_page = main_votes
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(p, _)| p)
        .or_else(|| out.pages.first().map(|p| p.name.clone()))
        .unwrap_or_default();
    let mut missing: Vec<(String, usize)> = missing.into_iter().collect();
    missing.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out.placeholders = missing;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(p: &Path, v: &Value) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, serde_json::to_vec(v).unwrap()).unwrap();
    }

    fn action(uuid: &str, title: &str, settings: Value) -> Value {
        json!({ "UUID": uuid, "Name": uuid.rsplit('.').next().unwrap(), "Settings": settings, "State": 0, "States": [{ "Title": title }] })
    }

    /// Two profiles shaped like a real setup's: "Edit" with a folder of apps, and "Stream" with OBS
    /// and volume dials, linked by profile-switch keys.
    #[test]
    fn imports_pages_folders_and_actions() {
        let root = std::env::temp_dir().join(format!("novagear-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let profiles = root.join("ProfilesV3");
        let edit = profiles.join("AAAAAAAA-0000-0000-0000-000000000000.sdProfile");
        let stream = profiles.join("BBBBBBBB-0000-0000-0000-000000000000.sdProfile");
        write(
            &edit.join("manifest.json"),
            &json!({ "Name": "Edit", "Device": { "Model": SD_PLUS_MODEL }, "Pages": { "Pages": ["11111111-0000-0000-0000-000000000000"] } }),
        );
        write(
            &stream.join("manifest.json"),
            &json!({ "Name": "Stream", "Device": { "Model": SD_PLUS_MODEL }, "Pages": { "Pages": ["33333333-0000-0000-0000-000000000000"] } }),
        );
        write(
            &profiles.join("CCCC.sdProfile/manifest.json"),
            &json!({ "Name": "Old deck", "Device": { "Model": "20GAA9902" }, "Pages": { "Pages": [] } }),
        );

        let edit_page = edit.join("Profiles/11111111-0000-0000-0000-000000000000");
        write(
            &edit_page.join("manifest.json"),
            &json!({ "Controllers": [
                { "Type": "Keypad", "Actions": {
                    "0,0": action("com.elgato.streamdeck.profile.openchild", "Apps", json!({ "ProfileUUID": "22222222-0000-0000-0000-000000000000" })),
                    "1,0": action("com.elgato.streamdeck.profile.rotate", "Main", json!({ "ProfileUUID": "bbbbbbbb-0000-0000-0000-000000000000", "PageIndex": 1 })),
                    "1,1": { "UUID": "com.elgato.streamdeck.system.hotkey", "Name": "Hotkey", "State": 0, "States": [{ "Title": "Screen\n\nShot", "Image": "Images/shot.png" }],
                             "Settings": { "Hotkeys": [{ "KeyCmd": true, "KeyCtrl": false, "KeyOption": false, "KeyShift": true, "NativeCode": 21 }, { "NativeCode": -1 }] } },
                    "2,0": action("com.elgato.cpu.cpu", "", json!({})),
                    "3,1": action("com.elgato.streamdeck.system.website", "", json!({ "path": "youtube.com" })),
                }},
                { "Type": "Encoder", "Actions": {
                    "0,0": { "UUID": "com.elgato.streamdeck.system.hotkey", "Name": "Hotkey", "State": 0, "States": [{ "Title": "Undo/Redo" }],
                             "Settings": { "Hotkeys": [{ "KeyCmd": true, "NativeCode": 6, "KeyShift": true }, { "KeyCmd": true, "NativeCode": 6 }] } },
                }},
            ]}),
        );
        std::fs::create_dir_all(edit_page.join("Images")).unwrap();
        std::fs::write(edit_page.join("Images/shot.png"), b"png").unwrap();
        write(
            &edit.join("Profiles/22222222-0000-0000-0000-000000000000/manifest.json"),
            &json!({ "Controllers": [
                { "Type": "Keypad", "Actions": {
                    "0,0": action("com.elgato.streamdeck.profile.backtoparent", "", json!({})),
                    "1,0": action("com.elgato.streamdeck.system.open", "", json!({ "path": "\"/System/Applications/Notes.app\"" })),
                    "2,0": action("com.elgato.streamdeck.system.text", "", json!({ "pastedText": "👋 hi", "isSendingEnter": true })),
                }},
            ]}),
        );
        write(
            &stream.join("Profiles/33333333-0000-0000-0000-000000000000/manifest.json"),
            &json!({ "Controllers": [
                { "Type": "Keypad", "Actions": {
                    "0,0": action("com.elgato.obsstudio.scene", "", json!({ "scene": "Fullscreen" })),
                    "3,1": action("com.elgato.camerahub.prompterdisplaysettings", "Prompt", json!({})),
                }},
                { "Type": "Encoder", "Actions": {
                    "2,0": action("com.elgato.volume-controller.output-device-control", "Headphones", json!({ "volumeStep": "3" })),
                    "3,0": action("com.elgato.volume-controller.input-device-control", "Mic", json!({ "volumeStep": "5" })),
                }},
            ]}),
        );

        write(
            &edit.join("Profiles/99999999-0000-0000-0000-000000000000/manifest.json"),
            &json!({ "Controllers": [] }),
        );

        let icons = root.join("icons");
        let r = import(&profiles, &icons).unwrap();
        let names: Vec<&str> = r.pages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            ["Edit", "Edit / Apps", "Stream"],
            "the empty, unreferenced folder is dropped"
        );
        assert_eq!(r.skipped_profiles, ["Old deck"]);
        assert_eq!(
            r.start_page, "Stream",
            "the page the \"Main\" keys point at"
        );

        let edit = &r.pages[0];
        let key = |p: &Page, i: usize| p.keys[i].clone().unwrap();
        assert_eq!(
            key(edit, 0).action,
            Action::PageGo {
                page: "Edit / Apps".into()
            }
        );
        assert_eq!(
            key(edit, 1).action,
            Action::PageGo {
                page: "Stream".into()
            }
        );
        let shot = key(edit, 5);
        assert_eq!(
            shot.action,
            Action::Hotkey {
                keys: "cmd+shift+4".into()
            }
        );
        assert_eq!(shot.label, "Screen Shot");
        assert!(shot.icon.is_some() && icons.join(shot.icon.unwrap()).is_file());
        assert_eq!(
            key(edit, 2).action,
            Action::Placeholder { note: "cpu".into() }
        );
        assert_eq!(
            key(edit, 7).action,
            Action::OpenUrl {
                url: "https://youtube.com".into()
            }
        );
        assert_eq!(key(edit, 7).label, "youtube.com");
        let undo = edit.dials[0].clone().unwrap();
        assert_eq!(
            undo.turn_right,
            Some(Action::Hotkey {
                keys: "cmd+shift+z".into()
            })
        );
        assert_eq!(
            undo.turn_left,
            Some(Action::Hotkey {
                keys: "cmd+z".into()
            })
        );

        let apps = &r.pages[1];
        assert_eq!(key(apps, 0).action, Action::PageBack);
        assert_eq!(
            key(apps, 1).action,
            Action::OpenApp {
                path: "/System/Applications/Notes.app".into()
            }
        );
        assert_eq!(key(apps, 1).label, "Notes");
        assert_eq!(
            key(apps, 2).action,
            Action::TypeText {
                text: "👋 hi".into(),
                enter: true
            }
        );

        let stream = &r.pages[2];
        assert_eq!(
            key(stream, 0).action,
            Action::ObsScene {
                scene: "Fullscreen".into()
            }
        );
        assert_eq!(key(stream, 7).action, Action::PrompterPlayPause);
        assert_eq!(
            stream.dials[2].clone().unwrap().turn,
            Some(Action::VolumeStep { step: 3 })
        );
        assert_eq!(
            stream.dials[3].clone().unwrap().turn,
            Some(Action::MicVolumeStep { step: 5 })
        );
        assert_eq!(r.placeholders, [("cpu".to_string(), 1)]);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
