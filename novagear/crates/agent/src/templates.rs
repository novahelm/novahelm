//! Templates: shareable sets of modes (and, if asked, the rules that go with them) with
//! their images inline. The built-in "Developer + Creator" is the maintainer's own setup, shipped
//! so others can copy it and change what they like.
//!
//! Notes for anyone editing a template file:
//! - A value written `<pick:scene>`, `<pick:mic>` or `<pick:game>` is a blank the user fills
//!   in (an OBS scene, an audio input); `apply` lists every one in `needs`.
//! - A choice panel shows an option as active when its label equals the panel's feed text,
//!   ignoring case. The Autonomy panel's feed `nova.autonomy` should say "Ask", "Safe"
//!   or "Full".
//! - Feeds nothing pushes (`nova.*`, `ship.*`, `live.*`) draw a dash until a client does.

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use novagear_core::config::Config;
use novagear_core::modes::{Mode, ModeRule, Template};
use novagear_core::{Action, DialBinding};
use novagear_drivers::render::load_image;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

const DEVELOPER_CREATOR: &str = include_str!("../templates/developer-creator.json");

/// What an uploaded or template image may weigh; `load_image` enforces the same limit.
pub const IMAGE_MAX_BYTES: usize = 20 * 1024 * 1024;

const PICK: &str = "<pick:";

/// The templates that ship with the agent.
pub fn builtin() -> Vec<Template> {
    [DEVELOPER_CREATOR]
        .into_iter()
        .map(|s| serde_json::from_str(s).expect("built-in template parses (tested)"))
        .collect()
}

/// `Developer + Creator` → `developer-creator`, so a URL path can name a template.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

/// A built-in template by its name or its slug.
pub fn find(name: &str) -> Option<Template> {
    let want = slug(name);
    builtin().into_iter().find(|t| slug(&t.name) == want)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ApplyReport {
    /// Ids of the modes that were added (after any renaming).
    pub added: Vec<String>,
    /// `(old, new)` for each id or name that clashed with one already there.
    pub renamed: Vec<(String, String)>,
    pub rules_replaced: bool,
    /// What the user still has to choose: OBS scenes and audio inputs.
    pub needs: Vec<String>,
}

/// The blanks in a template, one line per key or panel and kind, for the panel to list.
pub fn needs(t: &Template) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |mode: &Mode, place: &str, v: serde_json::Value| {
        let mut kinds = Vec::new();
        picks(&v, &mut kinds);
        for k in kinds {
            let line = format!("{} · {place}: pick {k}", mode.name);
            if !out.contains(&line) {
                out.push(line);
            }
        }
    };
    for m in &t.modes {
        for p in &m.pages {
            for k in p.keys.iter().flatten() {
                add(m, &k.label, serde_json::to_value(k).unwrap_or_default());
            }
            for d in p.dials.iter().flatten() {
                add(m, &d.label, serde_json::to_value(d).unwrap_or_default());
            }
        }
        for p in m.strip.iter().flatten() {
            add(m, &p.label, serde_json::to_value(p).unwrap_or_default());
        }
    }
    out
}

/// Every `<pick:kind>` in the strings of `v`, as `kind`.
fn picks(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::String(s) => {
            let mut rest = s.as_str();
            while let Some(i) = rest.find(PICK) {
                rest = &rest[i + PICK.len()..];
                let end = rest.find('>').unwrap_or(rest.len());
                out.push(rest[..end].to_string());
                rest = &rest[end..];
            }
        }
        serde_json::Value::Array(a) => a.iter().for_each(|x| picks(x, out)),
        serde_json::Value::Object(o) => o.values().for_each(|x| picks(x, out)),
        _ => {}
    }
}

/// The file name for an image's bytes: its sha256 and an extension from what it is, so the
/// same picture always has the same name.
pub fn content_name(bytes: &[u8]) -> String {
    let ext = image::guess_format(bytes)
        .ok()
        .and_then(|f| f.extensions_str().first().copied())
        .unwrap_or("svg");
    let sum: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{sum}.{ext}")
}

/// Checks `bytes` the way the deck will read them, then writes them to `dir/name` unless
/// the same file is already there. Never replaces a different file of that name.
pub fn write_image(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let plain = !name.is_empty()
        && !name.starts_with('.')
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !plain {
        bail!("image names are plain file names");
    }
    load_image(bytes, 64).context("that image can't be used")?;
    let path = dir.join(name);
    match std::fs::read(&path) {
        Ok(have) if have == bytes => return Ok(()),
        Ok(_) => bail!("an image called {name} is already there with different contents"),
        Err(_) => {}
    }
    std::fs::create_dir_all(dir)?;
    // A name of its own, so two uploads of the same picture at once don't share a file.
    static NONCE: AtomicU64 = AtomicU64::new(0);
    let tmp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        NONCE.fetch_add(1, Ordering::Relaxed)
    ));
    let done = std::fs::write(&tmp, bytes).and_then(|_| std::fs::rename(&tmp, &path));
    if done.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(done?)
}

/// Points every `ModeGo` in `a` (and in a sequence's steps) at the mode's new id.
fn remap_action(a: &mut Action, ids: &BTreeMap<String, String>) {
    match a {
        Action::ModeGo { mode } => {
            if let Some(new) = ids.get(mode.as_str()) {
                *mode = new.clone();
            }
        }
        Action::Sequence { steps } => steps
            .iter_mut()
            .for_each(|s| remap_action(&mut s.action, ids)),
        _ => {}
    }
}

fn remap_dial(d: &mut DialBinding, ids: &BTreeMap<String, String>) {
    for a in [
        &mut d.turn,
        &mut d.turn_left,
        &mut d.turn_right,
        &mut d.press,
        &mut d.alt_turn,
    ]
    .into_iter()
    .flatten()
    {
        remap_action(a, ids);
    }
}

/// An added mode's own jumps to its siblings must follow the renames too, or they would
/// land in the user's older mode of the same id.
fn remap_mode(m: &mut Mode, ids: &BTreeMap<String, String>) {
    for p in &mut m.pages {
        for k in p.keys.iter_mut().flatten() {
            remap_action(&mut k.action, ids);
            for st in k.states.iter_mut().flatten() {
                remap_action(&mut st.action, ids);
            }
        }
        p.dials
            .iter_mut()
            .flatten()
            .for_each(|d| remap_dial(d, ids));
    }
    for panel in m.strip.iter_mut().flatten() {
        if let Some(d) = &mut panel.dial {
            remap_dial(d, ids);
        }
        if let Some(a) = &mut panel.press {
            remap_action(a, ids);
        }
        for o in &mut panel.options {
            remap_action(&mut o.action, ids);
        }
    }
}

fn unique(taken: impl Fn(&str) -> bool, first: &str, make: impl Fn(u32) -> String) -> String {
    if !taken(first) {
        return first.to_string();
    }
    (2..)
        .map(make)
        .find(|c| !taken(c))
        .expect("a free name exists")
}

/// Adds the template's modes to `cfg` and writes its images into `images_dir`. Nothing
/// already there is removed or replaced: a clashing id or name is renamed. The rules are
/// replaced only when asked, because they decide when the deck changes mode by itself.
/// The images are checked and written first, so a bad one leaves `cfg` as it was.
pub fn apply(
    cfg: &mut Config,
    t: &Template,
    replace_rules: bool,
    images_dir: &Path,
) -> Result<ApplyReport> {
    let engine = base64::engine::general_purpose::STANDARD;
    let mut files = BTreeMap::new();
    for (name, b64) in &t.images {
        let bytes = engine
            .decode(b64)
            .with_context(|| format!("image {name} isn't base64"))?;
        files.insert(name.clone(), bytes);
    }
    for (name, bytes) in &files {
        write_image(images_dir, name, bytes).with_context(|| format!("image {name}"))?;
    }

    let mut report = ApplyReport {
        needs: needs(t),
        ..ApplyReport::default()
    };
    let mut ids: BTreeMap<String, String> = BTreeMap::new();
    let mut added = Vec::new();
    for m in &t.modes {
        let mut m = m.clone();
        let id = unique(
            |c| cfg.modes.iter().chain(&added).any(|x: &Mode| x.id == c),
            &m.id,
            |n| format!("{}-{n}", m.id),
        );
        let name = unique(
            |c| cfg.modes.iter().chain(&added).any(|x: &Mode| x.name == c),
            &m.name,
            |n| format!("{} {n}", m.name),
        );
        if id != m.id {
            report.renamed.push((m.id.clone(), id.clone()));
        }
        if name != m.name {
            report.renamed.push((m.name.clone(), name.clone()));
        }
        ids.insert(m.id.clone(), id.clone());
        (m.id, m.name) = (id, name);
        report.added.push(m.id.clone());
        added.push(m);
    }
    added.iter_mut().for_each(|m| remap_mode(m, &ids));
    cfg.modes.extend(added);
    if replace_rules {
        cfg.mode_rules = t
            .mode_rules
            .iter()
            .map(|r| ModeRule {
                when: r.when.clone(),
                mode: ids.get(&r.mode).cloned().unwrap_or_else(|| r.mode.clone()),
            })
            .collect();
        report.rules_replaced = true;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deck::Images;
    use novagear_core::modes::{RuleWhen, TextSource};
    use novagear_core::{Action, Page};
    use novagear_drivers::render::bundled;
    use std::sync::Arc;

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("novagear-tpl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn template() -> Template {
        builtin().remove(0)
    }

    fn png() -> Vec<u8> {
        let mut out = Vec::new();
        image::RgbaImage::from_pixel(4, 4, image::Rgba([9, 99, 199, 255]))
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    /// Every image name a mode draws: its icon, and each key's icon, background and states.
    fn image_names(t: &Template) -> Vec<String> {
        let mut v = Vec::new();
        for m in &t.modes {
            v.extend(m.icon.clone());
            for k in m.pages.iter().flat_map(|p| &p.keys).flatten() {
                v.extend(k.icon.clone());
                v.extend(k.face.background_image.clone());
                for s in k.states.iter().flatten() {
                    v.extend(s.icon.clone());
                    v.extend(s.face.background_image.clone());
                }
            }
        }
        v
    }

    #[test]
    fn builtin_template_parses_and_every_image_resolves() {
        let ts = builtin();
        assert_eq!(ts.len(), 1);
        let t = &ts[0];
        assert_eq!(t.name, "Developer + Creator");
        assert_eq!(
            t.modes.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["dev", "create", "live"]
        );
        let names = image_names(t);
        assert!(names.len() > 20);
        for n in names {
            let bytes = bundled(&n)
                .map(<[u8]>::to_vec)
                .or_else(|| {
                    t.images
                        .get(&n)
                        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
                })
                .unwrap_or_else(|| panic!("{n} is neither bundled nor in the template"));
            load_image(&bytes, 64).unwrap_or_else(|e| panic!("{n}: {e}"));
        }
        // The spec'd shape: eight keys per mode, four panels, pop-ups only in Dev.
        for m in &t.modes {
            assert_eq!(m.pages[0].keys.len(), 8, "{}", m.id);
            assert_eq!(m.strip.len(), 4, "{}", m.id);
        }
        let dev = &t.modes[0];
        assert!(dev.popups && !t.modes[1].popups && !t.modes[2].popups);
        assert_eq!(dev.approval_slots.as_ref().unwrap().deny, 7);
        let mode_key = dev.pages[0].keys[0].as_ref().unwrap();
        assert_eq!(mode_key.action, Action::ModePicker);
        assert_eq!(
            mode_key.face.badge,
            Some(TextSource::Feed {
                feed: "approvals.pending".into()
            })
        );
        let on_air = t.modes[2].pages[0].keys[1].as_ref().unwrap();
        assert_eq!(on_air.hold_ms, 1000);
        assert_eq!(on_air.follow.as_deref(), Some("obs.streaming"));
        assert_eq!(
            t.mode_rules.iter().map(|r| &r.when).collect::<Vec<_>>(),
            [&RuleWhen::ObsStreaming, &RuleWhen::ObsRecording]
        );
    }

    #[test]
    fn dev_mode_reads_the_feeds_novaguard_pushes() {
        let feed = |name: &str| {
            Some(TextSource::Feed {
                feed: name.to_string(),
            })
        };
        let t = template();
        let dev = &t.modes[0];
        let key = |label: &str| {
            dev.pages[0]
                .keys
                .iter()
                .flatten()
                .find(|k| k.label == label)
                .unwrap()
        };
        assert_eq!(key("Builds").face.value, feed("novaguard.builds.running"));
        assert_eq!(
            key("Builds").face.progress,
            None,
            "NovaGuard has no percentage"
        );
        assert_eq!(key("Deploy").face.value, feed("novaguard.deploy.last"));
        assert_eq!(
            key("AI runners").face.value,
            feed("novaguard.sessions.working")
        );
        let sessions = dev
            .strip
            .iter()
            .flatten()
            .find(|p| p.label == "SESSIONS")
            .unwrap();
        assert_eq!(sessions.value, feed("novaguard.sessions.open"));
        assert_eq!(sessions.sub, feed("novaguard.sessions.working"));
        // The old guesses are gone from the whole template.
        let raw = include_str!("../templates/developer-creator.json");
        for old in [
            "nova.agents.active",
            "nova.sessions.count",
            "novaguard.builds.progress",
        ] {
            assert!(!raw.contains(old), "{old}");
        }
    }

    #[test]
    fn template_pause_key_targets_novaguard() {
        let t = template();
        let key = t.modes[0]
            .pages
            .iter()
            .flat_map(|p| p.keys.iter().flatten())
            .find(|k| k.label == "Pause all")
            .unwrap();
        let pause = Action::ClientEvent {
            client: "novaguard".into(),
            name: "pause_all".into(),
            value: None,
        };
        assert_eq!(key.action, pause);
        assert_eq!(key.follow.as_deref(), Some("novaguard.paused"));
        let states = key.states.as_ref().expect("two states");
        assert_eq!(states[0].label, "Pause all");
        assert_eq!(states[0].icon.as_deref(), Some("lucide/pause.svg"));
        assert_eq!(states[1].label, "Paused");
        assert_eq!(states[1].icon.as_deref(), Some("lucide/play.svg"));
        assert_eq!(states[1].color.as_deref(), Some("#d97706"));
        // Each state's action says what the key is showing, so NovaGuard can tell a stale
        // label from a real press.
        let showing = |v: i32| Action::ClientEvent {
            client: "novaguard".into(),
            name: "pause_all".into(),
            value: Some(serde_json::json!(v)),
        };
        assert_eq!(states[0].action, showing(0));
        assert_eq!(states[1].action, showing(1));
    }

    fn with_dev() -> Config {
        Config {
            modes: vec![Mode {
                id: "dev".into(),
                name: "Development".into(),
                pages: vec![Page {
                    name: "Mine".into(),
                    keys: vec![],
                    dials: vec![],
                }],
                ..Mode::default()
            }],
            mode_rules: vec![ModeRule {
                when: RuleWhen::ObsRecording,
                mode: "dev".into(),
            }],
            ..Config::default()
        }
    }

    #[test]
    fn apply_adds_and_renames_without_deleting() {
        let d = dir("rename");
        let mut cfg = with_dev();
        let r = apply(&mut cfg, &template(), false, &d).unwrap();
        let ids: Vec<_> = cfg.modes.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["dev", "dev-2", "create", "live"]);
        assert_eq!(
            cfg.modes[0].pages[0].name, "Mine",
            "the old mode is untouched"
        );
        assert_eq!(cfg.modes[1].name, "Development 2");
        assert_eq!(r.added, ["dev-2", "create", "live"]);
        assert_eq!(
            r.renamed,
            [
                ("dev".to_string(), "dev-2".to_string()),
                ("Development".to_string(), "Development 2".to_string())
            ]
        );
        // Applying again renames again rather than piling onto one name.
        apply(&mut cfg, &template(), false, &d).unwrap();
        assert!(cfg.modes.iter().any(|m| m.id == "dev-3"));
        assert!(cfg.modes.iter().any(|m| m.id == "create-2"));
        let mut names: Vec<_> = cfg.modes.iter().map(|m| &m.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), cfg.modes.len());
    }

    #[test]
    fn rules_only_replaced_when_asked() {
        let d = dir("rules");
        let mut cfg = with_dev();
        let before = cfg.mode_rules.clone();
        let r = apply(&mut cfg, &template(), false, &d).unwrap();
        assert!(!r.rules_replaced);
        assert_eq!(cfg.mode_rules, before);

        let mut cfg = with_dev();
        let r = apply(&mut cfg, &template(), true, &d).unwrap();
        assert!(r.rules_replaced);
        assert_eq!(cfg.mode_rules.len(), 2);
        assert_eq!(cfg.mode_rules[0].mode, "live");
        // A rule follows its mode when the mode was renamed.
        let mut cfg = Config {
            modes: vec![Mode {
                id: "live".into(),
                name: "Live".into(),
                ..Mode::default()
            }],
            ..Config::default()
        };
        apply(&mut cfg, &template(), true, &d).unwrap();
        assert_eq!(cfg.mode_rules[0].mode, "live-2");
    }

    #[test]
    fn needs_lists_scene_picks() {
        let n = needs(&template());
        let count = |k: &str| {
            n.iter()
                .filter(|l| l.ends_with(&format!("pick {k}")))
                .count()
        };
        assert_eq!(count("scene"), 6, "{n:?}");
        assert_eq!(count("mic"), 1, "{n:?}");
        assert_eq!(count("game"), 1, "{n:?}");
        assert_eq!(n.len(), 8);
        assert!(n.iter().any(|l| l.starts_with("Live · BRB")));
        let r = apply(&mut Config::default(), &template(), false, &dir("needs")).unwrap();
        assert_eq!(r.needs, n);
    }

    #[test]
    fn apply_writes_template_images_and_a_bad_one_changes_nothing() {
        let d = dir("imgs");
        let mut t = template();
        let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
        t.images.insert("mine.png".into(), b64(&png()));
        let mut cfg = Config::default();
        apply(&mut cfg, &t, false, &d).unwrap();
        assert_eq!(std::fs::read(d.join("mine.png")).unwrap(), png());

        let mut cfg = Config::default();
        let before = cfg.clone();
        t.images
            .insert("zz-bad.gif".into(), b64(b"GIF89a-truncated"));
        assert!(apply(&mut cfg, &t, true, &d).is_err());
        assert_eq!(cfg, before);
        assert!(!d.join("zz-bad.gif").exists());
        // Names that are paths are refused, and a different file is never overwritten.
        t.images.remove("zz-bad.gif");
        t.images.insert("../evil.png".into(), b64(&png()));
        assert!(apply(&mut Config::default(), &t, false, &d).is_err());
        t.images.remove("../evil.png");
        std::fs::write(d.join("other.png"), png()).unwrap();
        let mut other = png();
        other[20] ^= 1;
        t.images.insert("other.png".into(), b64(&other));
        // (a flipped pixel byte may or may not still decode; either way it must not replace.)
        let _ = apply(&mut Config::default(), &t, false, &d);
        assert_eq!(std::fs::read(d.join("other.png")).unwrap(), png());
    }

    #[test]
    fn mode_go_follows_a_renamed_sibling() {
        let go = |to: &str| Action::ModeGo { mode: to.into() };
        let key = |a: Action| novagear_core::KeyBinding {
            label: "K".into(),
            color: "#000000".into(),
            icon: None,
            face: Default::default(),
            states: None,
            follow: None,
            hold_ms: 0,
            action: a,
        };
        let seq = Action::Sequence {
            steps: vec![novagear_core::Step {
                action: go("live"),
                delay_ms: 0,
            }],
        };
        let dial = DialBinding {
            label: "D".into(),
            turn: None,
            turn_left: Some(go("live")),
            turn_right: None,
            press: Some(go("live")),
            alt_turn: Some(go("live")),
        };
        let panel = novagear_core::modes::StripPanel {
            press: Some(go("live")),
            dial: Some(dial.clone()),
            options: vec![novagear_core::modes::ChoiceOption {
                label: "o".into(),
                action: go("live"),
            }],
            ..Default::default()
        };
        let t = Template {
            modes: vec![
                Mode {
                    id: "live".into(),
                    name: "Live".into(),
                    ..Mode::default()
                },
                Mode {
                    id: "dev".into(),
                    name: "Dev".into(),
                    pages: vec![Page {
                        name: "Home".into(),
                        keys: vec![Some(key(go("live"))), Some(key(seq))],
                        dials: vec![Some(dial)],
                    }],
                    strip: vec![Some(panel)],
                    ..Mode::default()
                },
            ],
            ..Template::default()
        };
        let mut cfg = Config {
            modes: vec![Mode {
                id: "live".into(),
                name: "Live".into(),
                ..Mode::default()
            }],
            ..Config::default()
        };
        apply(&mut cfg, &t, false, &dir("modego")).unwrap();
        let json = serde_json::to_string(&cfg.modes[2]).unwrap();
        assert!(!json.contains(r#""mode":"live""#), "{json}");
        assert_eq!(json.matches(r#""mode":"live-2""#).count(), 10, "{json}");
    }

    #[test]
    fn slugs_find_the_template() {
        assert_eq!(slug("Developer + Creator"), "developer-creator");
        assert!(find("developer-creator").is_some());
        assert!(find("Developer + Creator").is_some());
        assert!(find("nope").is_none());
    }

    #[tokio::test]
    async fn uploaded_image_is_content_named_and_validated() {
        use crate::api::put_image;
        use crate::clients::Caller;
        use axum::body::Bytes;
        use axum::extract::{Extension, State};
        use axum::http::StatusCode;
        use axum::response::IntoResponse;

        let d = dir("upload");
        let a = crate::Agent::new(d.join("config.json"), "t".repeat(48)).unwrap();
        let bytes = png();
        let name = content_name(&bytes);
        assert!(name.ends_with(".png") && name.len() == 64 + 4);
        // A miss is cached until the upload says the images changed.
        assert!(a.images.get(&name).is_none());

        let call = |b: Vec<u8>, who: Caller| {
            let a: Arc<crate::Agent> = a.clone();
            async move {
                put_image(State(a), Extension(who), Bytes::from(b))
                    .await
                    .into_response()
            }
        };
        let r = call(bytes.clone(), Caller::Panel).await;
        assert_eq!(r.status(), StatusCode::OK);
        let body = axum::body::to_bytes(r.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["name"], name.as_str());
        assert_eq!(std::fs::read(a.images_dir().join(&name)).unwrap(), bytes);
        assert!(a.images.get(&name).is_some(), "shows up without a restart");
        // The same bytes name the same file.
        let again = call(bytes.clone(), Caller::Agent).await;
        let body = axum::body::to_bytes(again.into_body(), 4096).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["name"],
            name.as_str()
        );

        // A truncated GIF, and nothing at all, are 400s that leave no file behind.
        let gif = {
            let mut out = Vec::new();
            let mut enc = image::codecs::gif::GifEncoder::new(&mut out);
            enc.encode_frame(image::Frame::new(image::RgbaImage::from_pixel(
                8,
                8,
                image::Rgba([1, 2, 3, 255]),
            )))
            .unwrap();
            drop(enc);
            out
        };
        let before = std::fs::read_dir(a.images_dir()).unwrap().count();
        for bad in [gif[..gif.len() / 2].to_vec(), Vec::new()] {
            assert_eq!(
                call(bad, Caller::Panel).await.status(),
                StatusCode::BAD_REQUEST
            );
        }
        assert_eq!(std::fs::read_dir(a.images_dir()).unwrap().count(), before);
        // A client app can't upload.
        assert_eq!(
            call(bytes, Caller::Client("ship".into())).await.status(),
            StatusCode::FORBIDDEN
        );
    }
}
