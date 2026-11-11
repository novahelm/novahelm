//! What NovaGear draws on the Stream Deck +: one image per key and a touch strip showing
//! each dial's label and current value.

use crate::feeds::{FeedItem, FeedSnapshot, FeedState, FeedStore, FeedValue, truthy};
use anyhow::Result;
use novagear_core::modes::{Face, Mode, PanelKind, Ring, StripPanel, TextSource};
use novagear_core::{Action, DialBinding, KeyBinding};
use novagear_drivers::draw::{Canvas, parse_color};
use novagear_drivers::render::{FontKind, Img, Pen, bundled, load_image, text_width};
use novagear_drivers::streamdeck::{DIALS, KEY_PX, STRIP_H, STRIP_W};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const WHITE: [u8; 3] = [0xf2, 0xf2, 0xf2];
const DIM: [u8; 3] = [0x8a, 0x90, 0xa6];
const STRIP_BG: [u8; 3] = [0x10, 0x12, 0x1a];

// ---- Key faces and strip panels, drawn from feeds ------------------------------------

const INK: [u8; 3] = [0xfa, 0xfa, 0xfa];
const INK_DARK: [u8; 3] = [0x1c, 0x19, 0x17];
const MUTED: [u8; 3] = [0xa1, 0xa1, 0xaa];
const TRACK: [u8; 3] = [0x27, 0x27, 0x2a];
const AMBER: [u8; 3] = [0xf5, 0x9e, 0x0b];
const GREEN: [u8; 3] = [0x34, 0xd3, 0x99];
const RED: [u8; 3] = [0xf8, 0x71, 0x71];
const BLUE: [u8; 3] = [0x60, 0xa5, 0xfa];
const PANEL_BG: [u8; 3] = [0x05, 0x05, 0x07];
const JPEG_QUALITY: u8 = 85;
/// Longest text we lay out; a hostile feed value is cut here before any measuring.
const MAX_TEXT_CHARS: usize = 64;

/// Resolves an image name to decoded frames at key size.
pub trait Images {
    fn get(&self, name: &str) -> Option<Arc<Img>>;
}

/// Looks a name up in the user's `images/` dir, then the legacy `icons/` dir, then the
/// bundled set. Decoded images are cached by name while something on screen uses them:
/// frames are RGBA at key size, so one long GIF is several MB.
pub struct FsImages {
    images: PathBuf,
    icons: PathBuf,
    cache: Mutex<HashMap<String, Option<Arc<Img>>>>,
    /// Names asked for since the last `begin_sweep`.
    used: Mutex<HashSet<String>>,
}

impl FsImages {
    pub fn new(images: PathBuf, icons: PathBuf) -> FsImages {
        FsImages {
            images,
            icons,
            cache: Mutex::new(HashMap::new()),
            used: Mutex::new(HashSet::new()),
        }
    }

    /// A whole-deck redraw is starting: from here, what it asks for is what is on screen.
    pub fn begin_sweep(&self) {
        self.used.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// The redraw is done: drops every cached image it did not ask for.
    pub fn evict_unused(&self) {
        let used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|name, _| used.contains(name));
    }

    /// Whether `name` is decoded and kept (or remembered as missing); for tests.
    #[cfg(test)]
    pub fn cached(&self, name: &str) -> bool {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(name)
    }

    /// Forgets what was cached, including names that failed or were missing, so an image
    /// that has since appeared (or changed) is read again on the next draw.
    pub fn invalidate(&self) {
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// The file's bytes: the user's `images/` dir, then the legacy `icons/` dir, then the
    /// bundled set. Plain file names inside the folders, never paths; bundled names carry
    /// one slash.
    pub fn raw(&self, name: &str) -> Option<Cow<'static, [u8]>> {
        let plain = !name.is_empty() && !name.contains(['/', '\\']) && !name.contains("..");
        let from_disk = plain
            .then(|| {
                std::fs::read(self.images.join(name))
                    .or_else(|_| std::fs::read(self.icons.join(name)))
                    .ok()
            })
            .flatten();
        match from_disk {
            Some(b) => Some(Cow::Owned(b)),
            None => bundled(name).map(Cow::Borrowed),
        }
    }

    fn load(&self, name: &str) -> Option<Img> {
        let bytes = self.raw(name)?;
        match load_image(&bytes, KEY_PX) {
            Ok(i) => Some(i),
            Err(_) => {
                // Never the name (a user's file name) or the decoder's text.
                tracing::warn!("an image could not be decoded");
                None
            }
        }
    }
}

impl Images for FsImages {
    fn get(&self, name: &str) -> Option<Arc<Img>> {
        self.used
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_string());
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hit) = cache.get(name) {
            return hit.clone();
        }
        let img = self.load(name).map(Arc::new);
        cache.insert(name.to_string(), img.clone());
        img
    }
}

/// One key to draw. `held` is the hold progress 0–1.
pub struct KeyLook<'a> {
    pub binding: Option<&'a KeyBinding>,
    pub accent: &'a str,
    pub held: Option<f32>,
    /// Which of a two-state key's looks to draw when it has no `follow` feed: the agent
    /// flips it on each press.
    pub state: u8,
    pub dimmed: bool,
    /// Which GIF frame to show.
    pub frame: usize,
}

fn state_color(s: Option<FeedState>) -> Option<[u8; 3]> {
    s.map(|s| match s {
        FeedState::Ok => GREEN,
        FeedState::Warn => AMBER,
        FeedState::Bad => RED,
    })
}

fn luma(c: [u8; 3]) -> f32 {
    0.299 * c[0] as f32 + 0.587 * c[1] as f32 + 0.114 * c[2] as f32
}

fn scale_color(c: [u8; 3], f: f32) -> [u8; 3] {
    c.map(|v| (v as f32 * f).round().clamp(0.0, 255.0) as u8)
}

/// How a feed reads as text, `None` when there is nothing sensible to show: NaN and
/// infinity, an empty text. Numbers are whole when they are whole.
fn format_feed(s: &FeedSnapshot) -> Option<String> {
    let t = match &s.value {
        FeedValue::Number(n) if n.is_finite() => {
            let body = if n.fract() == 0.0 && n.abs() < 1e12 {
                format!("{}", *n as i64)
            } else {
                format!("{n:.1}")
            };
            format!("{body}{}", s.unit.as_deref().unwrap_or(""))
        }
        FeedValue::Number(_) => return None,
        FeedValue::Text(t) => t.chars().take(MAX_TEXT_CHARS).collect(),
        FeedValue::List(l) => l.len().to_string(),
    };
    (!t.is_empty()).then_some(t)
}

/// What a feed looks like once drawn. Two snapshots with the same `Seen` draw the same
/// pixels, so a redraw is skipped when only something unseen changed: a number that rounds
/// to the same text, or the history of a feed nothing draws as a sparkline.
#[derive(Debug, Clone, PartialEq)]
pub struct Seen {
    value: SeenValue,
    state: Option<FeedState>,
    stale: bool,
    history: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq)]
enum SeenValue {
    /// The text it shows, and the whole percent a bar or gauge fills to.
    Number {
        text: Option<String>,
        pct: Option<i64>,
    },
    Text(String),
    /// How many there are, and the first 24 states: what the dots draw.
    List {
        len: usize,
        states: Vec<Option<FeedState>>,
    },
}

/// `s` as drawn. `history` keeps the last numbers too, for a sparkline.
pub fn seen(s: &FeedSnapshot, history: bool) -> Seen {
    let value = match &s.value {
        FeedValue::Number(n) => SeenValue::Number {
            text: format_feed(s),
            pct: n.is_finite().then(|| n.round() as i64),
        },
        FeedValue::Text(t) => SeenValue::Text(t.chars().take(MAX_TEXT_CHARS).collect()),
        FeedValue::List(l) => SeenValue::List {
            len: l.len(),
            states: l.iter().take(24).map(|i| i.state).collect(),
        },
    };
    Seen {
        value,
        state: s.state,
        stale: s.stale,
        history: if history {
            s.history.clone()
        } else {
            Vec::new()
        },
    }
}

impl Seen {
    pub fn stale(&self) -> bool {
        self.stale
    }
}

/// A feed's number when it has a finite one.
fn feed_number(s: &FeedSnapshot) -> Option<f64> {
    match &s.value {
        FeedValue::Number(n) if n.is_finite() => Some(*n),
        FeedValue::Text(t) => t
            .trim()
            .trim_end_matches('%')
            .parse()
            .ok()
            .filter(|n: &f64| n.is_finite()),
        _ => None,
    }
}

/// Text for a source: the text, whether it is stale/missing ("—" dimmed), and a colour hint.
struct Resolved {
    text: String,
    dim: bool,
    tint: Option<[u8; 3]>,
}

fn resolve(src: &TextSource, feeds: &FeedStore) -> Resolved {
    match src {
        TextSource::Literal(s) => Resolved {
            text: s.chars().take(MAX_TEXT_CHARS).collect(),
            dim: false,
            tint: None,
        },
        TextSource::Feed { feed } => match feeds.get(feed) {
            Some(s) if !s.stale => match format_feed(&s) {
                Some(text) => Resolved {
                    text,
                    dim: false,
                    tint: state_color(s.state),
                },
                None => Resolved {
                    text: "—".into(),
                    dim: true,
                    tint: None,
                },
            },
            _ => Resolved {
                text: "—".into(),
                dim: true,
                tint: None,
            },
        },
    }
}

pub(crate) fn feed_truthy(feeds: &FeedStore, key: &str) -> bool {
    feeds.get(key).is_some_and(|s| !s.stale && truthy(&s.value))
}

/// A 0–100 number as a fraction 0–1, in whole percents: a bar or ring is a pixel or two per
/// percent, so a fraction finer than that only makes a new image for the same picture
/// (and it is what `Seen` compares).
fn percent_fraction(n: f64) -> f32 {
    (n.round() / 100.0).clamp(0.0, 1.0) as f32
}

/// A fresh feed's 0–100 number as a fraction 0–1.
fn feed_fraction(feeds: &FeedStore, key: &str) -> Option<f32> {
    let s = feeds.get(key).filter(|s| !s.stale)?;
    feed_number(&s).map(percent_fraction)
}

/// Shrinks `px` until `s` fits `w`, but not below `min_px`.
fn fit_px(kind: FontKind, px: f32, min_px: f32, w: f32, s: &str) -> f32 {
    let mut px = px;
    while px > min_px && text_width(px, kind, s) > w {
        px -= 1.0;
    }
    px
}

/// The look a key has right now: its own, or one of its two states.
struct Shown<'a> {
    label: &'a str,
    color: &'a str,
    icon: Option<&'a str>,
    face: &'a Face,
}

fn shown<'a>(b: &'a KeyBinding, feeds: &FeedStore, local: u8) -> Shown<'a> {
    if let Some(states) = &b.states {
        // A followed feed decides; otherwise the key's own press count does.
        let on = match b.follow.as_deref() {
            Some(f) => feed_truthy(feeds, f),
            None => local == 1,
        };
        let s = &states[usize::from(on)];
        return Shown {
            label: &s.label,
            color: s.color.as_deref().unwrap_or(&b.color),
            icon: s.icon.as_deref().or(b.icon.as_deref()),
            face: &s.face,
        };
    }
    Shown {
        label: &b.label,
        color: &b.color,
        icon: b.icon.as_deref(),
        face: &b.face,
    }
}

fn frame_of(img: &Img, n: usize) -> &image::RgbaImage {
    &img.frames[n % img.frames.len().max(1)]
}

/// An icon's frame for drawing. The bundled line icons are black strokes, so they are
/// recoloured to the key's ink; the Nova marks and user images keep their own colours.
fn icon_frame(name: &str, img: &Img, frame: usize, ink: [u8; 3]) -> image::RgbaImage {
    let mut f = frame_of(img, frame).clone();
    if name.starts_with("lucide/") {
        for p in f.pixels_mut() {
            p.0[..3].copy_from_slice(&ink);
        }
    }
    f
}

/// Splits a label over two lines at the word break nearest the middle when it is too wide.
fn wrap2(px: f32, kind: FontKind, w: f32, s: &str) -> Vec<String> {
    let words: Vec<&str> = s.split_whitespace().collect();
    if words.len() < 2 || text_width(px, kind, s) <= w {
        return vec![s.to_string()];
    }
    let split = (1..words.len())
        .min_by_key(|i| {
            let a = text_width(px, kind, &words[..*i].join(" "));
            let b = text_width(px, kind, &words[*i..].join(" "));
            (a.max(b) * 100.0) as i64
        })
        .unwrap_or(1);
    vec![words[..split].join(" "), words[split..].join(" ")]
}

/// Draws one key onto a pen (before JPEG), so tests can compare exact pixels.
pub fn key_pen(look: &KeyLook, feeds: &FeedStore, images: &dyn Images) -> Pen {
    let Some(b) = look.binding else {
        return Pen::new(KEY_PX, KEY_PX, [0, 0, 0]);
    };
    let s = shown(b, feeds, look.state);
    let glow = s
        .face
        .glow
        .as_deref()
        .is_some_and(|g| feed_truthy(feeds, g));
    let bg = if glow { AMBER } else { parse_color(s.color) };
    let mut pen = Pen::new(KEY_PX, KEY_PX, bg);
    let bg_img = s
        .face
        .background_image
        .as_deref()
        .and_then(|n| images.get(n))
        .filter(|i| !i.frames.is_empty());
    let has_bg_img = bg_img.is_some();
    if let Some(img) = &bg_img {
        pen.blit(frame_of(img, look.frame), 0, 0, KEY_PX, KEY_PX, 1.0);
        // A scrim under the label keeps it readable over any picture.
        if !s.label.is_empty() {
            let scrim = image::RgbaImage::from_pixel(KEY_PX, 28, image::Rgba([8, 10, 20, 184]));
            pen.blit(&scrim, 0, KEY_PX as i32 - 28, KEY_PX, 28, 1.0);
        }
    }
    let ink = if glow || (!has_bg_img && luma(bg) > 170.0) {
        INK_DARK
    } else {
        INK
    };
    let value = s.face.value.as_ref().map(|v| resolve(v, feeds));
    let icon = s
        .icon
        .and_then(|n| images.get(n).map(|i| icon_frame(n, &i, look.frame, ink)))
        .filter(|f| f.width() > 0);
    let w = KEY_PX as f32 - 16.0;
    let label_px = 14.0;
    let label_y = if has_bg_img { 97.0 } else { 80.0 };

    if let Some(v) = &value {
        // Big number (or text) with the label under it; an icon shrinks to a header.
        let (icon_px, vy, vpx) = if icon.is_some() {
            (32u32, 46.0, 26.0)
        } else {
            (0, 24.0, 40.0)
        };
        if let Some(img) = &icon {
            let x = (KEY_PX - icon_px) as i32 / 2;
            pen.blit(img, x, 10, icon_px, icon_px, 1.0);
        }
        let px = fit_px(FontKind::Mono, vpx, 14.0, w, &v.text);
        let c = if v.dim {
            scale_color(ink, 0.45)
        } else {
            v.tint.unwrap_or(ink)
        };
        pen.text_centered(8, w as u32, vy, px, FontKind::Mono, c, &v.text);
        pen.text_centered(
            4,
            KEY_PX - 8,
            label_y + 4.0,
            12.0,
            FontKind::Label,
            ink,
            s.label,
        );
    } else if let Some(img) = &icon {
        let size = 44u32;
        let has_label = !s.label.is_empty();
        let top = if has_bg_img {
            14
        } else if has_label {
            24
        } else {
            (KEY_PX - size) as i32 / 2
        };
        pen.blit(img, (KEY_PX - size) as i32 / 2, top, size, size, 1.0);
        let y = if has_bg_img {
            label_y
        } else {
            top as f32 + size as f32 + 8.0
        };
        let px = fit_px(FontKind::Label, label_px, 10.0, w, s.label);
        pen.text_centered(8, w as u32, y, px, FontKind::Label, ink, s.label);
    } else if !s.label.is_empty() {
        let lines = wrap2(20.0, FontKind::Bold, w, s.label);
        let px = lines
            .iter()
            .map(|l| fit_px(FontKind::Bold, 20.0, 12.0, w, l))
            .fold(20.0f32, f32::min);
        let lh = px * 1.25;
        let top = if has_bg_img {
            label_y - 2.0
        } else {
            (KEY_PX as f32 - lh * lines.len() as f32) / 2.0
        };
        for (i, l) in lines.iter().enumerate() {
            pen.text_centered(8, w as u32, top + i as f32 * lh, px, FontKind::Bold, ink, l);
        }
    }

    if let Some(p) = s.face.progress.as_deref() {
        let frac = feed_fraction(feeds, p).unwrap_or(0.0);
        let (x, y, bw) = (8.0, KEY_PX as f32 - 12.0, KEY_PX as f32 - 16.0);
        pen.rounded_rect(x, y, bw, 6.0, 3.0, [0x3f, 0x3f, 0x46]);
        if frac > 0.0 {
            pen.rounded_rect(x, y, (bw * frac).max(6.0), 6.0, 3.0, BLUE);
        }
    }
    if let Some(badge) = &s.face.badge {
        let shown = match badge {
            TextSource::Feed { feed } => feed_truthy(feeds, feed).then(|| resolve(badge, feeds)),
            TextSource::Literal(t) => (!t.is_empty()).then(|| resolve(badge, feeds)),
        };
        if let Some(r) = shown {
            // Three characters at most, so the pill never leaves the top-right 40×24 corner.
            let mut t: String = r.text.chars().take(3).collect();
            if r.text.chars().count() > 3 {
                t.pop();
                t.push('…');
            }
            let tw = text_width(13.0, FontKind::Mono, &t);
            let pw = (tw + 12.0).min(36.0);
            let x = KEY_PX as f32 - 4.0 - pw;
            pen.rounded_rect(x, 4.0, pw, 20.0, 10.0, AMBER);
            pen.text(x + (pw - tw) / 2.0, 6.0, 13.0, FontKind::Mono, INK_DARK, &t);
        }
    }
    let ring = match &s.face.ring {
        Some(Ring::Always(c)) => Some(parse_color(c)),
        Some(Ring::When { color, feed }) => feed_truthy(feeds, feed).then(|| parse_color(color)),
        None => None,
    };
    if let Some(c) = ring {
        // Square edges: the glass is square, rounding would only show at the corners.
        let k = KEY_PX as f32;
        for (x, y, w, h) in [
            (0.0, 0.0, k, 4.0),
            (0.0, k - 4.0, k, 4.0),
            (0.0, 0.0, 4.0, k),
            (k - 4.0, 0.0, 4.0, k),
        ] {
            pen.rounded_rect(x, y, w, h, 0.0, c);
        }
    }
    if let Some(h) = look.held.filter(|h| h.is_finite() && *h > 0.0) {
        let k = KEY_PX as f32;
        pen.rounded_rect(0.0, k - 6.0, k, 6.0, 0.0, [0x3f, 0x3f, 0x46]);
        pen.rounded_rect(0.0, k - 6.0, k * h.min(1.0), 6.0, 0.0, [0xf4, 0xf4, 0xf5]);
    }
    if look.dimmed {
        for p in pen.img.pixels_mut() {
            p.0 = p.0.map(|v| (v as f32 * 0.35).round() as u8);
        }
    }
    pen
}

pub fn key_jpeg(look: &KeyLook, feeds: &FeedStore, images: &dyn Images) -> Result<Vec<u8>> {
    key_pen(look, feeds, images).jpeg(JPEG_QUALITY)
}

/// A key of the sleeping deck: the sleep image filling it, or black.
pub fn sleep_key_jpeg(img: Option<&Img>) -> Result<Vec<u8>> {
    let mut pen = Pen::new(KEY_PX, KEY_PX, [0, 0, 0]);
    if let Some(i) = img.filter(|i| !i.frames.is_empty()) {
        pen.blit(frame_of(i, 0), 0, 0, KEY_PX, KEY_PX, 1.0);
    }
    pen.jpeg(JPEG_QUALITY)
}

/// The sleeping deck's strip: black.
pub fn sleep_strip_jpeg() -> Result<Vec<u8>> {
    Pen::new(STRIP_W, STRIP_H, [0, 0, 0]).jpeg(JPEG_QUALITY)
}

/// A key's image from its icon file or colour only (no feeds); kept so the redraw path that
/// predates feeds still works.
pub fn key_image(binding: Option<&KeyBinding>, icons_dir: &Path) -> Result<Vec<u8>> {
    let images = FsImages::new(icons_dir.to_path_buf(), icons_dir.to_path_buf());
    key_jpeg(
        &KeyLook {
            binding,
            accent: "#2563eb",
            held: None,
            state: 0,
            dimmed: false,
            frame: 0,
        },
        &FeedStore::default(),
        &images,
    )
}

// ---- Strip panels --------------------------------------------------------------------

const SEG_W: u32 = STRIP_W / DIALS as u32;

fn level_color(frac: f32) -> [u8; 3] {
    if frac >= 0.9 {
        RED
    } else if frac >= 0.75 {
        AMBER
    } else {
        GREEN
    }
}

/// Draws one 200×100 segment onto a pen.
pub fn panel_pen(
    p: Option<&StripPanel>,
    feeds: &FeedStore,
    highlight: Option<usize>,
    accent: &str,
) -> Pen {
    let mut pen = Pen::new(SEG_W, STRIP_H, PANEL_BG);
    // The hairline between neighbours, on the right edge.
    pen.rounded_rect(
        SEG_W as f32 - 1.0,
        8.0,
        1.0,
        STRIP_H as f32 - 16.0,
        0.0,
        [0x1c, 0x1c, 0x20],
    );
    let Some(p) = p else {
        return pen;
    };
    let label = p.label.to_uppercase();
    let label: String = label.chars().take(MAX_TEXT_CHARS).collect();
    let main = p.value.as_ref().map(|v| resolve(v, feeds));
    let sub = p.sub.as_ref().map(|v| resolve(v, feeds));
    let fed = p
        .feed
        .as_deref()
        .and_then(|f| feeds.get(f))
        .filter(|s| !s.stale);
    let frac = fed.as_ref().and_then(feed_number).map(percent_fraction);
    let tint = fed.as_ref().and_then(|s| state_color(s.state));
    let text_of =
        |r: &Resolved, c: [u8; 3]| (r.text.clone(), if r.dim { scale_color(c, 0.45) } else { c });
    let w = SEG_W as f32 - 24.0;
    let label_at = |pen: &mut Pen, x: u32, w: u32| {
        pen.text_centered(x, w, 8.0, 11.0, FontKind::Label, MUTED, &label);
    };
    let sub_at = |pen: &mut Pen, x: u32, w: u32, y: f32| {
        if let Some(r) = &sub {
            let (t, c) = text_of(r, MUTED);
            pen.text_centered(x, w, y, 11.0, FontKind::Mono, c, &t);
        }
    };
    match p.kind {
        PanelKind::Value | PanelKind::Message => {
            label_at(&mut pen, 4, SEG_W - 8);
            let r = main.unwrap_or(Resolved {
                text: "—".into(),
                dim: true,
                tint: None,
            });
            let (t, c) = text_of(&r, r.tint.or(tint).unwrap_or(INK));
            if p.kind == PanelKind::Message {
                for (i, l) in wrap2(18.0, FontKind::Bold, w, &t)
                    .iter()
                    .take(2)
                    .enumerate()
                {
                    pen.text_centered(
                        12,
                        w as u32,
                        28.0 + i as f32 * 24.0,
                        fit_px(FontKind::Bold, 18.0, 11.0, w, l),
                        FontKind::Bold,
                        c,
                        l,
                    );
                }
            } else {
                let px = fit_px(FontKind::Mono, 34.0, 14.0, w, &t);
                pen.text_centered(12, w as u32, 32.0, px, FontKind::Mono, c, &t);
            }
            sub_at(&mut pen, 8, SEG_W - 16, 78.0);
        }
        PanelKind::Gauge => {
            label_at(&mut pen, 4, SEG_W - 8);
            let (cx, cy, r) = (48.0, 58.0, 28.0);
            pen.ring(cx, cy, r, 8.0, 1.0, TRACK);
            if let Some(f) = frac {
                pen.ring(cx, cy, r, 8.0, f, tint.unwrap_or_else(|| level_color(f)));
            }
            let text = match (&main, frac) {
                (Some(m), _) => text_of(m, m.tint.or(tint).unwrap_or(INK)),
                (None, Some(f)) => (format!("{}%", (f * 100.0).round() as i32), INK),
                (None, None) => ("—".into(), scale_color(INK, 0.45)),
            };
            let px = fit_px(FontKind::Bold, 22.0, 12.0, 92.0, &text.0);
            pen.text_centered(92, 100, 38.0, px, FontKind::Bold, text.1, &text.0);
            sub_at(&mut pen, 92, 100, 66.0);
        }
        PanelKind::Bar => {
            label_at(&mut pen, 4, SEG_W - 8);
            let text = match (&main, frac) {
                (Some(m), _) => text_of(m, m.tint.or(tint).unwrap_or(INK)),
                (None, Some(f)) => (format!("{}%", (f * 100.0).round() as i32), INK),
                (None, None) => ("—".into(), scale_color(INK, 0.45)),
            };
            pen.text_centered(
                12,
                w as u32,
                24.0,
                fit_px(FontKind::Mono, 26.0, 14.0, w, &text.0),
                FontKind::Mono,
                text.1,
                &text.0,
            );
            pen.rounded_rect(12.0, 62.0, w, 10.0, 5.0, TRACK);
            if let Some(f) = frac.filter(|f| *f > 0.0) {
                pen.rounded_rect(
                    12.0,
                    62.0,
                    (w * f).max(10.0),
                    10.0,
                    5.0,
                    tint.unwrap_or_else(|| level_color(f)),
                );
            }
            sub_at(&mut pen, 8, SEG_W - 16, 80.0);
        }
        PanelKind::Meter => {
            label_at(&mut pen, 4, SEG_W - 8);
            let lit = frac.map_or(0, |f| (f * 8.0).round() as usize);
            for i in 0..8usize {
                let h = 16.0 + i as f32 * 6.0;
                let x = 23.0 + i as f32 * 20.0;
                let c = if i < lit {
                    level_color((i as f32 + 1.0) / 8.0)
                } else {
                    TRACK
                };
                pen.rounded_rect(x, 88.0 - h, 14.0, h, 3.0, c);
            }
            sub_at(&mut pen, 100, 92, 6.0);
        }
        PanelKind::Choice => {
            label_at(&mut pen, 4, SEG_W - 8);
            let active = highlight.or(choice_current(p, feeds));
            let n = p.options.len().min(6);
            let cols = n.clamp(1, 3);
            let rows = n.div_ceil(cols).max(1);
            let (gap, pw) = (5.0, (176.0 - 5.0 * (cols as f32 - 1.0)) / cols as f32);
            for (i, o) in p.options.iter().take(n).enumerate() {
                let (cx, cy) = ((i % cols) as f32, (i / cols) as f32);
                let (x, y) = (12.0 + cx * (pw + gap), 30.0 + cy * 27.0);
                let on = active == Some(i);
                pen.rounded_rect(
                    x,
                    y,
                    pw,
                    22.0,
                    5.0,
                    if on { parse_color(accent) } else { TRACK },
                );
                let t: String = o.label.chars().take(MAX_TEXT_CHARS).collect();
                let (kind, c) = if on {
                    (FontKind::Bold, INK)
                } else {
                    (FontKind::Label, MUTED)
                };
                pen.text_centered(x as u32 + 2, pw as u32 - 4, y + 4.0, 12.0, kind, c, &t);
            }
            if rows == 1 {
                sub_at(&mut pen, 8, SEG_W - 16, 70.0);
            }
        }
        PanelKind::Dots => {
            label_at(&mut pen, 4, SEG_W - 8);
            let items: &[FeedItem] = match fed.as_ref().map(|s| &s.value) {
                Some(FeedValue::List(l)) => l,
                _ => &[],
            };
            for (i, it) in items.iter().take(24).enumerate() {
                let (col, row) = ((i % 8) as f32, (i / 8) as f32);
                let c = state_color(it.state).unwrap_or([0x52, 0x52, 0x5b]);
                pen.rounded_rect(14.0 + col * 22.0, 30.0 + row * 20.0, 14.0, 14.0, 3.0, c);
            }
            if items.is_empty() {
                pen.text_centered(
                    8,
                    SEG_W - 16,
                    48.0,
                    22.0,
                    FontKind::Mono,
                    scale_color(INK, 0.45),
                    "—",
                );
            }
            if let Some(m) = &main {
                let (t, c) = text_of(m, MUTED);
                pen.text_centered(100, 92, 6.0, 11.0, FontKind::Mono, c, &t);
            } else if items.len() > 24 {
                pen.text_centered(
                    100,
                    92,
                    6.0,
                    11.0,
                    FontKind::Mono,
                    MUTED,
                    &format!("+{}", items.len() - 24),
                );
            }
        }
        PanelKind::Sparkline => {
            label_at(&mut pen, 4, SEG_W - 8);
            let hist: Vec<f64> = fed
                .as_ref()
                .map(|s| {
                    s.history
                        .iter()
                        .copied()
                        .filter(|v| v.is_finite())
                        .collect()
                })
                .unwrap_or_default();
            if hist.len() >= 2 {
                let (lo, hi) = hist
                    .iter()
                    .fold((f64::MAX, f64::MIN), |(l, h), v| (l.min(*v), h.max(*v)));
                let span = (hi - lo).max(1e-9);
                let n = hist.len().min(60);
                let bw = 176.0 / n as f32;
                let tail = &hist[hist.len() - n..];
                for (i, v) in tail.iter().enumerate() {
                    let h = 4.0 + ((v - lo) / span) as f32 * 50.0;
                    let last = i + 1 == n;
                    let c = if last {
                        tint.unwrap_or(BLUE)
                    } else {
                        scale_color(tint.unwrap_or(BLUE), 0.45)
                    };
                    pen.rounded_rect(
                        12.0 + i as f32 * bw,
                        88.0 - h,
                        (bw - 1.0).max(1.0),
                        h,
                        1.0,
                        c,
                    );
                }
            } else {
                pen.text_centered(
                    8,
                    SEG_W - 16,
                    48.0,
                    22.0,
                    FontKind::Mono,
                    scale_color(INK, 0.45),
                    "—",
                );
            }
            let text = match (&main, fed.as_ref().and_then(format_feed)) {
                (Some(m), _) => Some(text_of(m, INK)),
                (None, Some(t)) => Some((t, INK)),
                _ => None,
            };
            if let Some((t, c)) = text {
                pen.text_centered(100, 92, 5.0, 14.0, FontKind::Mono, c, &t);
            }
        }
    }
    pen
}

/// The option a choice panel's feed says is current, matched by label.
pub fn choice_current(p: &StripPanel, feeds: &FeedStore) -> Option<usize> {
    let cur = p
        .feed
        .as_deref()
        .and_then(|f| feeds.get(f))
        .filter(|s| !s.stale)
        .and_then(|s| format_feed(&s))?
        .to_lowercase();
    p.options.iter().position(|o| o.label.to_lowercase() == cur)
}

/// One 200×100 strip segment; `highlight` is the choice the dial is pointing at and
/// `accent` the mode's colour for the lit choice.
pub fn panel_jpeg(
    p: Option<&StripPanel>,
    feeds: &FeedStore,
    highlight: Option<usize>,
    accent: &str,
) -> Result<Vec<u8>> {
    panel_pen(p, feeds, highlight, accent).jpeg(JPEG_QUALITY)
}

/// The full 800×100 strip listing modes, four to a page; `current` is lit.
pub fn picker_jpeg(
    modes: &[&Mode],
    current: &str,
    page: usize,
    images: &dyn Images,
) -> Result<Vec<u8>> {
    let mut pen = Pen::new(STRIP_W, STRIP_H, PANEL_BG);
    let pages = modes.len().div_ceil(DIALS).max(1);
    let page = page.min(pages - 1);
    for (slot, m) in modes.iter().skip(page * DIALS).take(DIALS).enumerate() {
        let x0 = slot as u32 * SEG_W;
        let accent = parse_color(&m.accent);
        let on = m.id == current;
        if on {
            pen.rounded_rect(
                x0 as f32 + 6.0,
                6.0,
                SEG_W as f32 - 12.0,
                82.0,
                10.0,
                scale_color(accent, 0.45),
            );
            pen.rounded_rect(x0 as f32 + 6.0, 84.0, SEG_W as f32 - 12.0, 4.0, 2.0, accent);
        }
        if let Some(img) = m
            .icon
            .as_deref()
            .and_then(|n| images.get(n))
            .filter(|i| !i.frames.is_empty())
        {
            pen.blit(
                &img.frames[0],
                (x0 + (SEG_W - 36) / 2) as i32,
                12,
                36,
                36,
                1.0,
            );
        }
        let name: String = m.name.chars().take(MAX_TEXT_CHARS).collect();
        pen.text_centered(
            x0 + 8,
            SEG_W - 16,
            56.0,
            15.0,
            FontKind::Bold,
            if on { INK } else { MUTED },
            &name,
        );
    }
    if pages > 1 {
        let start = (STRIP_W as f32 - pages as f32 * 10.0) / 2.0;
        for i in 0..pages {
            let c = if i == page { INK } else { [0x3f, 0x3f, 0x46] };
            pen.rounded_rect(start + i as f32 * 10.0 + 2.0, 93.0, 6.0, 4.0, 2.0, c);
        }
    }
    pen.jpeg(JPEG_QUALITY)
}

/// One approval, as the pop-up shows it on the strip.
pub struct ApprovalView {
    pub title: String,
    pub lines: Vec<String>,
    pub source: String,
    pub index: usize,
    pub total: usize,
}

/// Left-aligned text in a band; an overlong line ends in "…" and fills the band.
#[allow(clippy::too_many_arguments)]
fn band_text(pen: &mut Pen, x: u32, w: u32, y: f32, px: f32, kind: FontKind, c: [u8; 3], s: &str) {
    let s: String = s.chars().take(200).collect();
    if text_width(px, kind, &s) <= w as f32 {
        pen.text(x as f32, y, px, kind, c, &s);
    } else {
        pen.text_centered(x, w, y, px, kind, c, &s);
    }
}

pub fn popup_strip_jpeg(a: &ApprovalView) -> Result<Vec<u8>> {
    let mut pen = Pen::new(STRIP_W, STRIP_H, PANEL_BG);
    pen.rounded_rect(0.0, 0.0, 6.0, STRIP_H as f32, 0.0, AMBER);
    let tag = format!(
        "APPROVAL {} OF {}",
        a.index.saturating_add(1),
        a.total.max(1)
    );
    band_text(&mut pen, 22, 400, 9.0, 12.0, FontKind::Mono, AMBER, &tag);
    band_text(
        &mut pen,
        STRIP_W - 222,
        200,
        9.0,
        12.0,
        FontKind::Label,
        MUTED,
        &a.source.to_uppercase(),
    );
    band_text(
        &mut pen,
        22,
        STRIP_W - 44,
        28.0,
        22.0,
        FontKind::Bold,
        INK,
        &a.title,
    );
    for (i, l) in a.lines.iter().take(2).enumerate() {
        band_text(
            &mut pen,
            22,
            STRIP_W - 44,
            58.0 + i as f32 * 20.0,
            14.0,
            FontKind::Mono,
            MUTED,
            l,
        );
    }
    pen.jpeg(JPEG_QUALITY)
}

fn text_feed(s: &Option<TextSource>, out: &mut Vec<String>) {
    if let Some(TextSource::Feed { feed }) = s {
        out.push(feed.clone());
    }
}

fn dedup(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v.dedup();
    v
}

fn face_feeds(f: &Face, out: &mut Vec<String>) {
    text_feed(&f.value, out);
    text_feed(&f.badge, out);
    out.extend(f.progress.clone());
    out.extend(f.glow.clone());
    if let Some(Ring::When { feed, .. }) = &f.ring {
        out.push(feed.clone());
    }
}

/// The feeds a key reads, for `FeedStore::set_wanted`.
pub fn feeds_read(b: &KeyBinding) -> Vec<String> {
    let mut v = Vec::new();
    face_feeds(&b.face, &mut v);
    if let Some(states) = &b.states {
        for s in states {
            face_feeds(&s.face, &mut v);
        }
    }
    v.extend(b.follow.clone());
    dedup(v)
}

/// The feeds a panel reads.
pub fn panel_feeds(p: &StripPanel) -> Vec<String> {
    let mut v = Vec::new();
    text_feed(&p.value, &mut v);
    text_feed(&p.sub, &mut v);
    v.extend(p.feed.clone());
    dedup(v)
}

/// Live values the strip can show under each dial.
pub struct DialValues {
    pub brightness: Option<String>,
    pub kelvin: Option<String>,
    pub lights_on: bool,
    pub prompter_speed: u16,
    pub prompter_playing: bool,
    pub deck_brightness: u8,
    pub dpi: Option<u16>,
}

/// The value a dial shows, chosen from what turning it does.
pub fn dial_value(d: &DialBinding, v: &DialValues) -> String {
    let off = |s: Option<String>| {
        if v.lights_on {
            s.unwrap_or_else(|| "--".into())
        } else {
            "OFF".into()
        }
    };
    match &d.turn {
        Some(Action::LightBrightnessStep { .. }) => off(v.brightness.clone()),
        Some(Action::LightKelvinStep { .. }) => off(v.kelvin.clone()),
        Some(Action::PrompterSpeedStep { .. }) => {
            format!(
                "{}{}",
                v.prompter_speed,
                if v.prompter_playing { " >" } else { "" }
            )
        }
        Some(Action::DeckBrightnessStep { .. }) => format!("{}%", v.deck_brightness),
        Some(Action::MouseDpiStep { .. }) => v.dpi.map_or("--".into(), |d| d.to_string()),
        _ => String::new(),
    }
}

pub fn strip_image(dials: &[Option<DialBinding>], v: &DialValues) -> Result<Vec<u8>> {
    let mut c = Canvas::new(STRIP_W, STRIP_H, STRIP_BG);
    let zone = STRIP_W / DIALS as u32;
    for i in 0..DIALS {
        let x = i as u32 * zone;
        if i > 0 {
            c.fill_rect(x, 12, 2, STRIP_H - 24, [0x2a, 0x2e, 0x3d]);
        }
        let Some(Some(d)) = dials.get(i) else {
            continue;
        };
        c.text_centered(x, zone, 16, &d.label, 3, DIM);
        c.text_centered(x, zone, 50, &dial_value(d, v), 5, WHITE);
    }
    c.jpeg()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feeds::{FOREVER, FeedItem, FeedState, FeedValue};
    use novagear_core::modes::{ChoiceOption, Face, KeyState, PanelKind, Ring, StripPanel};
    use std::path::PathBuf;

    fn snap(v: FeedValue, history: &[f64]) -> FeedSnapshot {
        FeedSnapshot {
            value: v,
            unit: Some("%".into()),
            state: None,
            stale: false,
            history: history.to_vec(),
        }
    }

    #[test]
    fn seen_ignores_what_is_not_drawn() {
        let a = snap(FeedValue::Number(63.04), &[60.0, 63.04]);
        let b = snap(FeedValue::Number(63.01), &[60.0, 63.04, 63.01]);
        // Same text, same whole percent, only the history differs: the same picture.
        assert_eq!(seen(&a, false), seen(&b, false));
        // A sparkline does draw the history.
        assert_ne!(seen(&a, true), seen(&b, true));
        // A different text, or a different whole percent, is a different picture.
        let c = snap(FeedValue::Number(63.2), &[]);
        assert_ne!(seen(&a, false), seen(&c, false));
        let (d, e) = (
            snap(FeedValue::Number(63.46), &[]),
            snap(FeedValue::Number(63.54), &[]),
        );
        assert_ne!(
            seen(&d, false),
            seen(&e, false),
            "63.5 rounds up, 63.46 down"
        );
    }

    #[test]
    fn seen_follows_state_staleness_and_the_dots() {
        let base = snap(FeedValue::Number(5.0), &[]);
        let warn = FeedSnapshot {
            state: Some(FeedState::Warn),
            ..base.clone()
        };
        let stale = FeedSnapshot {
            stale: true,
            ..base.clone()
        };
        assert_ne!(seen(&base, false), seen(&warn, false));
        assert_ne!(seen(&base, false), seen(&stale, false));
        let item = |label: &str, state| FeedItem {
            label: label.into(),
            state,
        };
        let one = snap(FeedValue::List(vec![item("a", Some(FeedState::Ok))]), &[]);
        let renamed = snap(FeedValue::List(vec![item("b", Some(FeedState::Ok))]), &[]);
        let bad = snap(FeedValue::List(vec![item("a", Some(FeedState::Bad))]), &[]);
        assert_eq!(
            seen(&one, false),
            seen(&renamed, false),
            "dots draw no labels"
        );
        assert_ne!(seen(&one, false), seen(&bad, false));
    }

    #[test]
    fn a_gauge_ring_fills_to_whole_percents() {
        let feeds = FeedStore::default();
        feeds.set("g", FeedValue::Number(63.04), None, None, FOREVER);
        let p = StripPanel {
            kind: PanelKind::Gauge,
            label: "M".into(),
            feed: Some("g".into()),
            ..StripPanel::default()
        };
        let first = panel_pen(Some(&p), &feeds, None, "#2563eb");
        feeds.set("g", FeedValue::Number(63.01), None, None, FOREVER);
        let second = panel_pen(Some(&p), &feeds, None, "#2563eb");
        assert!(
            first.img.as_raw() == second.img.as_raw(),
            "the same text and percent must be the same pixels"
        );
    }

    fn values() -> DialValues {
        DialValues {
            brightness: Some("40%".into()),
            kelvin: Some("5300K".into()),
            lights_on: true,
            prompter_speed: 60,
            prompter_playing: true,
            deck_brightness: 70,
            dpi: Some(1600),
        }
    }

    #[test]
    fn dial_shows_the_value_it_turns() {
        let d = |turn| DialBinding {
            label: "X".into(),
            turn: Some(turn),
            turn_left: None,
            turn_right: None,
            press: None,
            alt_turn: None,
        };
        let v = values();
        assert_eq!(
            dial_value(&d(Action::LightBrightnessStep { id: None, step: 2 }), &v),
            "40%"
        );
        assert_eq!(
            dial_value(&d(Action::PrompterSpeedStep { step: 5 }), &v),
            "60 >"
        );
        assert_eq!(dial_value(&d(Action::MouseDpiStep { step: 1 }), &v), "1600");
        let off = DialValues {
            lights_on: false,
            ..values()
        };
        assert_eq!(
            dial_value(
                &d(Action::LightKelvinStep {
                    id: None,
                    step: 100
                }),
                &off
            ),
            "OFF"
        );
    }

    #[test]
    fn default_layout_draws() {
        let cfg = novagear_core::config::Config::default();
        let main = &cfg.modes[0].pages[0];
        let icons = std::env::temp_dir();
        assert!(key_image(main.keys[0].as_ref(), &icons).unwrap().len() > 100);
        assert!(key_image(None, &icons).unwrap().len() > 100);
        assert!(strip_image(&main.dials, &values()).unwrap().len() > 100);
    }

    fn images() -> FsImages {
        let none = PathBuf::from("/nonexistent-novagear-test");
        FsImages::new(none.clone(), none)
    }

    fn key(label: &str, color: &str) -> KeyBinding {
        KeyBinding {
            label: label.into(),
            color: color.into(),
            icon: None,
            face: Face::default(),
            states: None,
            follow: None,
            hold_ms: 0,
            action: Action::ModeNext,
        }
    }

    fn feed(f: &FeedStore, k: &str, v: FeedValue, state: Option<FeedState>) {
        f.set(k, v, None, state, FOREVER);
    }

    fn look(b: Option<&KeyBinding>) -> KeyLook<'_> {
        KeyLook {
            binding: b,
            accent: "#2563eb",
            held: None,
            state: 0,
            dimmed: false,
            frame: 0,
        }
    }

    fn dev_keys() -> Vec<KeyBinding> {
        let tf = |f: &str| TextSource::Feed { feed: f.into() };
        let mut k0 = key("DEV", "#1e2230");
        k0.face.background_image = Some("marks/novahelm.svg".into());
        let mut k1 = key("APPROVALS", "#1e2230");
        k1.face.value = Some(tf("dev.approvals"));
        k1.face.glow = Some("dev.approvals".into());
        let mut k2 = key("Build", "#18181b");
        k2.icon = Some("marks/novaguard.svg".into());
        k2.face.progress = Some("dev.build".into());
        let mut k3 = key("web · 4m ago", "#065f46");
        k3.icon = Some("lucide/circle-check.svg".into());
        let mut k4 = key("3 agents", "#312e81");
        k4.icon = Some("marks/novacompute.svg".into());
        k4.face.badge = Some(tf("dev.agents"));
        let mut k5 = key("12 up", "#0f172a");
        k5.icon = Some("lucide/box.svg".into());
        let mut k6 = key("Pause all", "#3f3f46");
        k6.icon = Some("lucide/pause.svg".into());
        let mut k7 = key("Focus off", "#1e1b4b");
        let st = |l: &str, ring: Option<Ring>| KeyState {
            label: l.into(),
            color: Some("#1e1b4b".into()),
            icon: Some("lucide/moon.svg".into()),
            face: Face {
                ring,
                ..Face::default()
            },
            action: Action::ModeNext,
        };
        k7.states = Some([
            st("Focus off", None),
            st("Focus on", Some(Ring::Always("#818cf8".into()))),
        ]);
        k7.follow = Some("dev.focus".into());
        vec![k0, k1, k2, k3, k4, k5, k6, k7]
    }

    fn dev_panels() -> Vec<StripPanel> {
        let lit = |s: &str| TextSource::Literal(s.into());
        vec![
            StripPanel {
                kind: PanelKind::Gauge,
                label: "Machine".into(),
                value: Some(lit("OK")),
                sub: Some(TextSource::Feed {
                    feed: "dev.memtext".into(),
                }),
                feed: Some("dev.cpu".into()),
                ..StripPanel::default()
            },
            StripPanel {
                kind: PanelKind::Choice,
                label: "Autonomy".into(),
                sub: Some(lit("asks before deploys")),
                feed: Some("dev.autonomy".into()),
                options: ["Ask", "Safe", "Full"]
                    .iter()
                    .map(|l| ChoiceOption {
                        label: (*l).into(),
                        action: Action::ModeNext,
                    })
                    .collect(),
                ..StripPanel::default()
            },
            StripPanel {
                kind: PanelKind::Dots,
                label: "Containers".into(),
                feed: Some("dev.containers".into()),
                ..StripPanel::default()
            },
            StripPanel {
                kind: PanelKind::Sparkline,
                label: "CPU".into(),
                feed: Some("dev.cpu".into()),
                ..StripPanel::default()
            },
        ]
    }

    fn dev_feeds() -> FeedStore {
        let f = FeedStore::default();
        feed(&f, "dev.approvals", FeedValue::Number(2.0), None);
        feed(&f, "dev.build", FeedValue::Number(62.0), None);
        feed(&f, "dev.agents", FeedValue::Number(3.0), None);
        feed(&f, "dev.focus", FeedValue::Number(0.0), None);
        feed(&f, "dev.memtext", FeedValue::Text("mem 35%".into()), None);
        feed(&f, "dev.autonomy", FeedValue::Text("Safe".into()), None);
        let items = (0..12)
            .map(|i| FeedItem {
                label: format!("c{i}"),
                state: match i {
                    4 => Some(FeedState::Warn),
                    9 => Some(FeedState::Bad),
                    _ => Some(FeedState::Ok),
                },
            })
            .collect();
        feed(&f, "dev.containers", FeedValue::List(items), None);
        for v in [
            20., 24., 31., 28., 40., 52., 47., 38., 35., 41., 55., 62., 58., 44., 39., 36.,
        ] {
            feed(&f, "dev.cpu", FeedValue::Number(v), Some(FeedState::Ok));
        }
        f
    }

    /// Compares `got` with `tests/snapshots/<name>.png`: each channel within 8, at most 1% of
    /// pixels off. `UPDATE_SNAPSHOTS=1` writes the file instead.
    fn check_snapshot(name: &str, got: &image::RgbImage) {
        let path = format!("{}/tests/snapshots/{name}.png", env!("CARGO_MANIFEST_DIR"));
        if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
            std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
            got.save(&path).unwrap();
            return;
        }
        let want = image::open(&path)
            .unwrap_or_else(|e| panic!("{path}: {e} (run with UPDATE_SNAPSHOTS=1)"))
            .to_rgb8();
        assert_eq!(want.dimensions(), got.dimensions(), "{name}: size");
        let off = want
            .pixels()
            .zip(got.pixels())
            .filter(|(a, b)| a.0.iter().zip(b.0).any(|(x, y)| x.abs_diff(y) > 8))
            .count();
        assert!(
            off * 100 <= (got.width() * got.height()) as usize,
            "{name}: {off} pixels differ"
        );
    }

    #[test]
    fn snapshot_dev_mode_keys_and_panels() {
        let feeds = dev_feeds();
        let imgs = images();
        for (i, k) in dev_keys().iter().enumerate() {
            check_snapshot(
                &format!("dev_key{i}"),
                &key_pen(&look(Some(k)), &feeds, &imgs).img,
            );
        }
        for (i, p) in dev_panels().iter().enumerate() {
            check_snapshot(
                &format!("dev_panel{i}"),
                &panel_pen(Some(p), &feeds, None, "#2563eb").img,
            );
        }
    }

    #[test]
    fn hostile_feed_values_draw() {
        let f = FeedStore::default();
        feed(&f, "long", FeedValue::Text("x".repeat(5000)), None);
        feed(&f, "nan", FeedValue::Number(f64::NAN), None);
        feed(&f, "inf", FeedValue::Number(f64::INFINITY), None);
        feed(&f, "emoji", FeedValue::Text("🚀🔥 ok".into()), None);
        feed(&f, "empty", FeedValue::List(vec![]), None);
        for name in ["long", "nan", "inf", "emoji", "empty", "missing"] {
            for kind in [
                PanelKind::Value,
                PanelKind::Gauge,
                PanelKind::Bar,
                PanelKind::Meter,
                PanelKind::Dots,
                PanelKind::Sparkline,
                PanelKind::Message,
            ] {
                let p = StripPanel {
                    kind,
                    label: "L".repeat(300),
                    value: Some(TextSource::Feed { feed: name.into() }),
                    feed: Some(name.into()),
                    ..StripPanel::default()
                };
                assert!(
                    panel_jpeg(Some(&p), &f, None, "#2563eb").unwrap().len() > 100,
                    "{name}"
                );
            }
            let mut k = key("L", "#000000");
            k.face.value = Some(TextSource::Feed { feed: name.into() });
            k.face.badge = Some(TextSource::Feed { feed: name.into() });
            k.face.progress = Some(name.into());
            assert!(key_jpeg(&look(Some(&k)), &f, &images()).unwrap().len() > 100);
        }
        // NaN and infinity read as "no value".
        assert_eq!(format_feed(&f.get("nan").unwrap()), None);
        assert_eq!(format_feed(&f.get("inf").unwrap()), None);
    }

    #[test]
    fn badge_hidden_when_zero() {
        let f = FeedStore::default();
        let mut k = key("Agents", "#312e81");
        k.face.badge = Some(TextSource::Feed { feed: "n".into() });
        feed(&f, "n", FeedValue::Number(0.0), None);
        let a = key_pen(&look(Some(&k)), &f, &images()).img;
        feed(&f, "n", FeedValue::Number(2.0), None);
        let b = key_pen(&look(Some(&k)), &f, &images()).img;
        let mut inside = 0;
        for (x, y, p) in a.enumerate_pixels() {
            if p != b.get_pixel(x, y) {
                assert!(
                    x >= 80 && y < 24,
                    "pixel ({x},{y}) changed outside the corner"
                );
                inside += 1;
            }
        }
        assert!(inside > 20, "the badge shows when the feed is 2");
    }

    #[test]
    fn follow_picks_state() {
        let f = FeedStore::default();
        let imgs = images();
        let mut k = key("base", "#000000");
        let st = |l: &str| KeyState {
            label: l.into(),
            color: None,
            icon: None,
            face: Face::default(),
            action: Action::ModeNext,
        };
        k.states = Some([st("OFF"), st("ON")]);
        k.follow = Some("obs.recording".into());
        let plain = |label: &str| {
            let mut p = key(label, "#000000");
            p.color = "#000000".into();
            key_pen(&look(Some(&p)), &f, &imgs).img
        };
        assert_eq!(key_pen(&look(Some(&k)), &f, &imgs).img, plain("OFF"));
        feed(&f, "obs.recording", FeedValue::Number(1.0), None);
        assert_eq!(key_pen(&look(Some(&k)), &f, &imgs).img, plain("ON"));
        assert_ne!(plain("OFF"), plain("ON"));
    }

    #[test]
    fn missing_image_falls_back_to_colour() {
        let mut k = key("X", "#102030");
        k.face.background_image = Some("nope".into());
        k.icon = Some("../etc/passwd".into());
        let f = FeedStore::default();
        assert!(key_jpeg(&look(Some(&k)), &f, &images()).unwrap().len() > 100);
        let p = key_pen(&look(Some(&k)), &f, &images());
        assert_eq!(p.img.get_pixel(2, 2).0, [0x10, 0x20, 0x30]);
    }

    #[test]
    fn dimmed_held_and_stale_look_different() {
        let f = dev_feeds();
        let imgs = images();
        let ks = dev_keys();
        let base = key_pen(&look(Some(&ks[3])), &f, &imgs).img;
        let dim = key_pen(
            &KeyLook {
                dimmed: true,
                ..look(Some(&ks[3]))
            },
            &f,
            &imgs,
        )
        .img;
        assert!(dim.get_pixel(2, 2).0[1] < base.get_pixel(2, 2).0[1] / 2);
        let held = key_pen(
            &KeyLook {
                held: Some(0.5),
                ..look(Some(&ks[3]))
            },
            &f,
            &imgs,
        )
        .img;
        assert_ne!(held.get_pixel(30, 117), base.get_pixel(30, 117));
        assert_eq!(
            held.get_pixel(30, 117).0,
            [0xf4, 0xf4, 0xf5],
            "filled from the left"
        );
        assert_eq!(
            held.get_pixel(100, 117).0,
            [0x3f, 0x3f, 0x46],
            "empty part is a track"
        );
    }

    #[test]
    fn picker_and_popup_draw_and_feeds_are_named() {
        let imgs = images();
        let modes: Vec<Mode> = (0..6)
            .map(|i| Mode {
                id: format!("m{i}"),
                name: format!("Mode {i}"),
                icon: Some("lucide/rocket.svg".into()),
                ..Mode::default()
            })
            .collect();
        let refs: Vec<&Mode> = modes.iter().collect();
        assert!(picker_jpeg(&refs, "m1", 0, &imgs).unwrap().len() > 100);
        assert!(picker_jpeg(&refs, "m5", 1, &imgs).unwrap().len() > 100);
        assert!(picker_jpeg(&[], "", 3, &imgs).unwrap().len() > 100);
        let a = ApprovalView {
            title: "Deploy web to production ".repeat(10),
            lines: vec!["rm -rf".repeat(50), "second".into(), "third".into()],
            source: "claude".into(),
            index: 0,
            total: 2,
        };
        assert!(popup_strip_jpeg(&a).unwrap().len() > 100);
        let keys = dev_keys();
        assert_eq!(feeds_read(&keys[1]), vec!["dev.approvals".to_string()]);
        assert_eq!(feeds_read(&keys[7]), vec!["dev.focus".to_string()]);
        assert_eq!(
            panel_feeds(&dev_panels()[0]),
            vec!["dev.cpu".to_string(), "dev.memtext".to_string()]
        );
    }
}
