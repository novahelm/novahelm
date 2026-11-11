//! Text and image drawing for the Stream Deck: real fonts (`ab_glyph`), SVG/PNG/JPEG/GIF
//! decoding (`resvg`, `image`) and the icons and marks bundled into the binary.
//! `draw.rs` keeps the 5×7 font for the non-deck strip fallback; deck drawing uses `Pen`.

use anyhow::{Result, anyhow, bail};
use image::{ImageReader, RgbImage, RgbaImage};
use std::io::Cursor;
use std::sync::OnceLock;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use resvg::{tiny_skia, usvg};

const MAX_INPUT_BYTES: usize = 20 * 1024 * 1024;
const MAX_DIMENSION: u32 = 4096;
const MAX_GIF_FRAMES: usize = 120;
/// Canvas pixels summed over all source frames (500 megapixels); bounds decode time.
const MAX_GIF_PIXELS: u64 = 500_000_000;
/// More source frames than this is a hostile or broken file; decoding stops there.
const MAX_GIF_SOURCE_FRAMES: usize = 2000;
/// 15 fps: the fastest a key animates, whatever the file asks for.
/// Largest text size drawn; keeps a hostile size from allocating or looping for ages.
const MAX_TEXT_PX: f32 = 512.0;
const MIN_FRAME_DELAY_MS: u32 = 67;

/// Decoded image at key size. A still when `frames.len() == 1`.
#[derive(Clone)]
pub struct Img {
    pub frames: Vec<RgbaImage>,
    pub delay_ms: u32,
}

/// Decodes PNG, JPEG, SVG or GIF and scales to `size×size` (cover: fill, crop the overflow).
/// Refuses oversized input before decoding; SVG file and network references are never read.
pub fn load_image(bytes: &[u8], size: u32) -> Result<Img> {
    if bytes.len() > MAX_INPUT_BYTES {
        bail!("image is over 20 MB");
    }
    if size == 0 || size > MAX_DIMENSION {
        bail!("bad target size");
    }
    let reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let Some(format) = reader.format() else {
        return load_svg(bytes, size);
    };
    // The header says how big it is; check that before any pixel buffer exists.
    let (w, h) = reader.into_dimensions()?;
    if w == 0 || h == 0 || w > MAX_DIMENSION || h > MAX_DIMENSION {
        bail!("image is {w}x{h}, over the {MAX_DIMENSION}x{MAX_DIMENSION} limit");
    }
    if format == image::ImageFormat::Gif {
        return load_gif(bytes, size);
    }
    let img = image::load_from_memory_with_format(bytes, format)?.to_rgba8();
    Ok(Img {
        frames: vec![cover(&img, size)],
        delay_ms: 0,
    })
}

/// Scales so the shorter side is `size`, then crops the centre.
fn cover(src: &RgbaImage, size: u32) -> RgbaImage {
    let (w, h) = src.dimensions();
    let scale = size as f32 / w.min(h) as f32;
    let nw = ((w as f32 * scale).round() as u32).max(size);
    let nh = ((h as f32 * scale).round() as u32).max(size);
    let resized = image::imageops::resize(src, nw, nh, image::imageops::FilterType::Triangle);
    image::imageops::crop_imm(&resized, (nw - size) / 2, (nh - size) / 2, size, size).to_image()
}

fn load_gif(bytes: &[u8], size: u32) -> Result<Img> {
    load_gif_with_budget(bytes, size, MAX_GIF_PIXELS)
}

/// Two passes so memory never holds more than the frames that are kept. Pass one decodes
/// and discards, counting frames, delay and canvas pixels (over `budget` is an error, not a
/// partial animation); pass two decodes again and resizes and stores only the sampled ones.
fn load_gif_with_budget(bytes: &[u8], size: u32, budget: u64) -> Result<Img> {
    use image::AnimationDecoder;
    let decode = || image::codecs::gif::GifDecoder::new(Cursor::new(bytes));
    let (mut count, mut total_ms, mut pixels) = (0usize, 0u64, 0u64);
    for frame in decode()?.into_frames() {
        // A truncated file errors here; a half-animation would be worse than the fallback face.
        let frame = frame?;
        count += 1;
        if count > MAX_GIF_SOURCE_FRAMES {
            bail!("gif has too many frames");
        }
        pixels += frame.buffer().width() as u64 * frame.buffer().height() as u64;
        if pixels > budget {
            bail!("gif is too large to decode");
        }
        let (n, d) = frame.delay().numer_denom_ms();
        total_ms += (n / d.max(1)) as u64;
    }
    if count == 0 {
        bail!("gif has no frames");
    }
    let keep = count.min(MAX_GIF_FRAMES);
    let mut wanted = (0..keep).map(|i| i * count / keep).peekable();
    let mut frames = Vec::with_capacity(keep);
    for (idx, frame) in decode()?.into_frames().enumerate() {
        match wanted.peek() {
            Some(&w) if w == idx => {
                frames.push(cover(frame?.buffer(), size));
                wanted.next();
            }
            Some(_) => {
                frame?;
            }
            None => break,
        }
    }
    if frames.len() != keep {
        bail!("gif changed while decoding");
    }
    let avg = total_ms / count as u64;
    // Dropping frames shortens the loop; stretching the delay keeps the same overall pace.
    let delay = (avg * count as u64 / keep as u64) as u32;
    Ok(Img {
        frames,
        delay_ms: delay.max(MIN_FRAME_DELAY_MS),
    })
}

fn load_svg(bytes: &[u8], size: u32) -> Result<Img> {
    // usvg would gunzip svgz with no output limit (a 20 MB bomb is gigabytes), so svgz is
    // refused and only plain text goes to the parser, via `from_str`, which never decompresses.
    if bytes.starts_with(&[0x1f, 0x8b]) {
        bail!("compressed svg is not supported");
    }
    let text = std::str::from_utf8(bytes).map_err(|_| anyhow!("not an image"))?;
    // No resources_dir and a resolver that refuses everything: `<image href>` to a file or
    // URL is dropped (not drawn), so an uploaded SVG cannot read local files. Fonts are not
    // compiled in, so `<text>` is not drawn either.
    let mut opt = usvg::Options {
        resources_dir: None,
        ..usvg::Options::default()
    };
    opt.image_href_resolver = usvg::ImageHrefResolver {
        resolve_data: Box::new(|_, _, _| None),
        resolve_string: Box::new(|_, _| None),
    };
    let tree = usvg::Tree::from_str(text, &opt).map_err(|e| anyhow!("not an image: {e}"))?;
    let (tw, th) = (tree.size().width(), tree.size().height());
    if !(tw.is_finite() && th.is_finite() && tw > 0.0 && th > 0.0) {
        bail!("svg has no size");
    }
    let mut pixmap = tiny_skia::Pixmap::new(size, size).ok_or_else(|| anyhow!("bad size"))?;
    let s = (size as f32 / tw).max(size as f32 / th);
    let tx = (size as f32 - tw * s) / 2.0;
    let ty = (size as f32 - th * s) / 2.0;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(s, s).post_translate(tx, ty),
        &mut pixmap.as_mut(),
    );
    let mut out = RgbaImage::new(size, size);
    for (dst, px) in out.pixels_mut().zip(pixmap.pixels()) {
        let c = px.demultiply();
        *dst = image::Rgba([c.red(), c.green(), c.blue(), c.alpha()]);
    }
    Ok(Img {
        frames: vec![out],
        delay_ms: 0,
    })
}

static ASSETS: &[(&str, &[u8])] = &[
    (
        "lucide/layers.svg",
        include_bytes!("../assets/icons/lucide/layers.svg"),
    ),
    (
        "lucide/hand.svg",
        include_bytes!("../assets/icons/lucide/hand.svg"),
    ),
    (
        "lucide/hammer.svg",
        include_bytes!("../assets/icons/lucide/hammer.svg"),
    ),
    (
        "lucide/circle-check.svg",
        include_bytes!("../assets/icons/lucide/circle-check.svg"),
    ),
    (
        "lucide/cpu.svg",
        include_bytes!("../assets/icons/lucide/cpu.svg"),
    ),
    (
        "lucide/box.svg",
        include_bytes!("../assets/icons/lucide/box.svg"),
    ),
    (
        "lucide/pause.svg",
        include_bytes!("../assets/icons/lucide/pause.svg"),
    ),
    (
        "lucide/moon.svg",
        include_bytes!("../assets/icons/lucide/moon.svg"),
    ),
    (
        "lucide/info.svg",
        include_bytes!("../assets/icons/lucide/info.svg"),
    ),
    (
        "lucide/check.svg",
        include_bytes!("../assets/icons/lucide/check.svg"),
    ),
    (
        "lucide/x.svg",
        include_bytes!("../assets/icons/lucide/x.svg"),
    ),
    (
        "lucide/lightbulb.svg",
        include_bytes!("../assets/icons/lucide/lightbulb.svg"),
    ),
    (
        "lucide/align-left.svg",
        include_bytes!("../assets/icons/lucide/align-left.svg"),
    ),
    (
        "lucide/mic.svg",
        include_bytes!("../assets/icons/lucide/mic.svg"),
    ),
    (
        "lucide/mic-off.svg",
        include_bytes!("../assets/icons/lucide/mic-off.svg"),
    ),
    (
        "lucide/user.svg",
        include_bytes!("../assets/icons/lucide/user.svg"),
    ),
    (
        "lucide/monitor.svg",
        include_bytes!("../assets/icons/lucide/monitor.svg"),
    ),
    (
        "lucide/bookmark.svg",
        include_bytes!("../assets/icons/lucide/bookmark.svg"),
    ),
    (
        "lucide/clock.svg",
        include_bytes!("../assets/icons/lucide/clock.svg"),
    ),
    (
        "lucide/gamepad-2.svg",
        include_bytes!("../assets/icons/lucide/gamepad-2.svg"),
    ),
    (
        "lucide/message-circle.svg",
        include_bytes!("../assets/icons/lucide/message-circle.svg"),
    ),
    (
        "lucide/coffee.svg",
        include_bytes!("../assets/icons/lucide/coffee.svg"),
    ),
    (
        "lucide/scissors.svg",
        include_bytes!("../assets/icons/lucide/scissors.svg"),
    ),
    (
        "lucide/radio.svg",
        include_bytes!("../assets/icons/lucide/radio.svg"),
    ),
    (
        "lucide/play.svg",
        include_bytes!("../assets/icons/lucide/play.svg"),
    ),
    (
        "lucide/square.svg",
        include_bytes!("../assets/icons/lucide/square.svg"),
    ),
    (
        "lucide/rocket.svg",
        include_bytes!("../assets/icons/lucide/rocket.svg"),
    ),
    (
        "marks/novahelm.svg",
        include_bytes!("../assets/marks/novahelm.svg"),
    ),
    (
        "marks/novaguard.svg",
        include_bytes!("../assets/marks/novaguard.svg"),
    ),
    (
        "marks/novalink.svg",
        include_bytes!("../assets/marks/novalink.svg"),
    ),
    (
        "marks/novagear.svg",
        include_bytes!("../assets/marks/novagear.svg"),
    ),
    (
        "marks/novacompute.svg",
        include_bytes!("../assets/marks/novacompute.svg"),
    ),
];

/// An embedded asset by `lucide/<name>.svg` or `marks/<name>.svg`.
pub fn bundled(name: &str) -> Option<&'static [u8]> {
    ASSETS.iter().find(|(n, _)| *n == name).map(|(_, b)| *b)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontKind {
    /// Inter SemiBold, for captions.
    Label,
    /// Inter Bold, for titles.
    Bold,
    /// JetBrains Mono Bold, for numbers.
    Mono,
}

fn font(kind: FontKind) -> &'static FontRef<'static> {
    static FONTS: OnceLock<[FontRef<'static>; 3]> = OnceLock::new();
    let fonts = FONTS.get_or_init(|| {
        let parse = |b: &'static [u8]| FontRef::try_from_slice(b).expect("embedded font parses");
        [
            parse(include_bytes!("../assets/fonts/Inter-SemiBold.ttf")),
            parse(include_bytes!("../assets/fonts/Inter-Bold.ttf")),
            parse(include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf")),
        ]
    });
    &fonts[kind as usize]
}

/// Lays `s` out left to right: (glyph, x offset) pairs and the total advance. Characters the
/// font has no glyph for (emoji, most symbols) are skipped rather than drawn as boxes.
fn layout(kind: FontKind, px: f32, s: &str) -> (Vec<(ab_glyph::Glyph, f32)>, f32) {
    let px = px.min(MAX_TEXT_PX);
    let f = font(kind);
    let sf = f.as_scaled(PxScale::from(px));
    let (mut x, mut prev, mut out) = (0.0f32, None, Vec::new());
    for ch in s.chars().filter(|c| !c.is_control()) {
        let id = f.glyph_id(ch);
        if id.0 == 0 {
            continue;
        }
        if let Some(p) = prev {
            x += sf.kern(p, id);
        }
        out.push((id.with_scale(PxScale::from(px)), x));
        x += sf.h_advance(id);
        prev = Some(id);
    }
    (out, x)
}

/// Width in pixels of `s` at `px`.
pub fn text_width(px: f32, kind: FontKind, s: &str) -> f32 {
    layout(kind, px, s).1
}

fn coverage(v: f32) -> f32 {
    if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) }
}

/// An RGB canvas with anti-aliased text and shapes.
pub struct Pen {
    pub img: RgbImage,
}

impl Pen {
    pub fn new(w: u32, h: u32, bg: [u8; 3]) -> Pen {
        Pen {
            img: RgbImage::from_pixel(w, h, image::Rgb(bg)),
        }
    }

    fn blend(&mut self, x: i32, y: i32, color: [u8; 3], a: f32) {
        if x < 0 || y < 0 || x >= self.img.width() as i32 || y >= self.img.height() as i32 {
            return;
        }
        let a = coverage(a);
        if a <= 0.0 {
            return;
        }
        let p = self.img.get_pixel_mut(x as u32, y as u32);
        for (dst, src) in p.0.iter_mut().zip(color) {
            *dst = (*dst as f32 + (src as f32 - *dst as f32) * a).round() as u8;
        }
    }

    /// Draws `s` with its left edge at `x` and the top of the line box at `y`.
    pub fn text(&mut self, x: f32, y: f32, px: f32, kind: FontKind, color: [u8; 3], s: &str) {
        if !(px.is_finite() && px > 0.0 && x.is_finite() && y.is_finite()) {
            return;
        }
        let px = px.min(MAX_TEXT_PX);
        let ascent = font(kind).as_scaled(PxScale::from(px)).ascent();
        for (mut g, gx) in layout(kind, px, s).0 {
            g.position = ab_glyph::point(x + gx, y + ascent);
            if let Some(o) = font(kind).outline_glyph(g) {
                let b = o.px_bounds();
                o.draw(|gx, gy, c| {
                    self.blend(
                        b.min.x as i32 + gx as i32,
                        b.min.y as i32 + gy as i32,
                        color,
                        c,
                    )
                });
            }
        }
    }

    /// Centres `s` in the band `x0..x0+w`, ending with "…" if it doesn't fit. Returns the
    /// width used (0 when not even the ellipsis fits).
    #[allow(clippy::too_many_arguments)]
    pub fn text_centered(
        &mut self,
        x0: u32,
        w: u32,
        y: f32,
        px: f32,
        kind: FontKind,
        color: [u8; 3],
        s: &str,
    ) -> f32 {
        if !(px.is_finite() && px > 0.0 && y.is_finite()) {
            return 0.0;
        }
        let px = px.min(MAX_TEXT_PX);
        let avail = w as f32;
        let chars: Vec<char> = s.chars().collect();
        let width_of = |n: usize, ell: bool| {
            let mut t: String = chars[..n].iter().collect();
            if ell {
                t.push('…');
            }
            text_width(px, kind, &t)
        };
        let text: String = if width_of(chars.len(), false) <= avail {
            s.to_string()
        } else {
            // Longest prefix whose width plus the ellipsis fits; binary search keeps a
            // 5000-character value cheap.
            let (mut lo, mut hi) = (0usize, chars.len());
            while lo < hi {
                let mid = (lo + hi).div_ceil(2);
                if width_of(mid, true) <= avail {
                    lo = mid
                } else {
                    hi = mid - 1
                }
            }
            if width_of(lo, true) > avail {
                return 0.0;
            }
            let mut t: String = chars[..lo].iter().collect();
            t.push('…');
            t
        };
        let tw = text_width(px, kind, &text);
        self.text(x0 as f32 + (avail - tw) / 2.0, y, px, kind, color, &text);
        tw
    }

    pub fn rounded_rect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, color: [u8; 3]) {
        if !(x.is_finite() && y.is_finite() && w > 0.0 && h > 0.0 && w.is_finite() && h.is_finite())
        {
            return;
        }
        let r = if r.is_finite() {
            r.clamp(0.0, w.min(h) / 2.0)
        } else {
            0.0
        };
        let (cx, cy) = (x + w / 2.0, y + h / 2.0);
        let (iw, ih) = (self.img.width() as f32, self.img.height() as f32);
        for py in (y.max(0.0).floor() as i32)..(y + h).min(ih).ceil() as i32 {
            for px in (x.max(0.0).floor() as i32)..(x + w).min(iw).ceil() as i32 {
                // Signed distance to a rounded box; half a pixel of falloff anti-aliases the edge.
                let dx = (px as f32 + 0.5 - cx).abs() - (w / 2.0 - r);
                let dy = (py as f32 + 0.5 - cy).abs() - (h / 2.0 - r);
                let d = dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0) - r;
                self.blend(px, py, color, 0.5 - d);
            }
        }
    }

    /// A ring of radius `r` and `thickness` around `(cx, cy)`, filled clockwise from 12
    /// o'clock for `frac` of the circle. A NaN or non-positive `frac` draws nothing.
    pub fn ring(&mut self, cx: f32, cy: f32, r: f32, thickness: f32, frac: f32, color: [u8; 3]) {
        if !(cx.is_finite() && cy.is_finite() && r > 0.0 && r.is_finite() && thickness > 0.0)
            || frac.is_nan()
            || frac <= 0.0
        {
            return;
        }
        let frac = frac.min(1.0);
        let reach = r + thickness / 2.0 + 1.0;
        let (iw, ih) = (self.img.width() as f32, self.img.height() as f32);
        for py in ((cy - reach).max(0.0).floor() as i32)..=(cy + reach).min(ih).ceil() as i32 {
            for px in ((cx - reach).max(0.0).floor() as i32)..=(cx + reach).min(iw).ceil() as i32 {
                let (dx, dy) = (px as f32 + 0.5 - cx, py as f32 + 0.5 - cy);
                let d = dx.hypot(dy);
                let radial = coverage(r + thickness / 2.0 - d + 0.5)
                    * coverage(d - (r - thickness / 2.0) + 0.5);
                if radial <= 0.0 {
                    continue;
                }
                let turn = dx.atan2(-dy).rem_euclid(std::f32::consts::TAU) / std::f32::consts::TAU;
                if turn <= frac {
                    self.blend(px, py, color, radial);
                }
            }
        }
    }

    /// Draws `frame` into the box `x, y, w, h` (resized if it isn't that size), blending its
    /// own alpha times `alpha` over what is there.
    pub fn blit(&mut self, frame: &RgbaImage, x: i32, y: i32, w: u32, h: u32, alpha: f32) {
        if w == 0 || h == 0 || w > MAX_DIMENSION || h > MAX_DIMENSION {
            return;
        }
        let scaled;
        let src = if frame.dimensions() == (w, h) {
            frame
        } else {
            scaled = image::imageops::resize(frame, w, h, image::imageops::FilterType::Triangle);
            &scaled
        };
        for (sx, sy, p) in src.enumerate_pixels() {
            let a = p.0[3] as f32 / 255.0 * coverage(alpha);
            self.blend(
                x.saturating_add(sx as i32),
                y.saturating_add(sy as i32),
                [p.0[0], p.0[1], p.0[2]],
                a,
            );
        }
    }

    pub fn jpeg(&self, quality: u8) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality).encode(
            self.img.as_raw(),
            self.img.width(),
            self.img.height(),
            image::ExtendedColorType::Rgb8,
        )?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn loads_png_jpeg_svg_and_gif() {
        for (name, n) in [
            ("sample.png", 1),
            ("sample.jpg", 1),
            ("sample.svg", 1),
            ("sample.gif", 5),
        ] {
            let img = load_image(&fixture(name), 120).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(img.frames.len(), n, "{name}");
            assert!(
                img.frames.iter().all(|f| f.dimensions() == (120, 120)),
                "{name}"
            );
        }
    }

    #[test]
    fn rejects_oversized_and_external_svg() {
        assert!(load_image(&vec![0u8; 21 * 1024 * 1024], 120).is_err());

        // A PNG header alone claims 10000x10000; it must be refused before any decode. The
        // IHDR CRC is recomputed so only the dimension guard can be what refuses it.
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[0u8; 3], 1, 1, image::ExtendedColorType::Rgb8)
            .unwrap();
        png[16..20].copy_from_slice(&10000u32.to_be_bytes());
        png[20..24].copy_from_slice(&10000u32.to_be_bytes());
        let crc = crc32(&png[12..29]);
        png[29..33].copy_from_slice(&crc.to_be_bytes());
        let err = load_image(&png, 120).err().expect("refused").to_string();
        assert!(err.contains("limit"), "{err}");

        // Ruling R-P2: an SVG with a file reference loads, and the element is simply not drawn.
        let base = r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 24 24"><rect width="24" height="24" fill="#2a6"/>"##;
        let with = format!(
            r#"{base}<image width="24" height="24" xlink:href="file:///etc/passwd"/><image width="24" height="24" href="/etc/hosts"/></svg>"#
        );
        let without = format!("{base}</svg>");
        let a = load_image(with.as_bytes(), 64).expect("loads");
        let b = load_image(without.as_bytes(), 64).unwrap();
        assert_eq!(a.frames[0], b.frames[0]);
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for b in data {
            crc ^= *b as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB88320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    #[test]
    fn gzip_input_is_refused_not_inflated() {
        // A bomb is just a big gzip; a tiny one proves the prefix is refused unread.
        let gz = [
            0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3, 0x03, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert!(load_image(&gz, 64).is_err());
    }

    #[test]
    fn gif_over_the_pixel_budget_is_refused() {
        let gif = long_gif(300);
        // 300 frames of 8x8 is 19 200 px; a budget below that must refuse, one above must load.
        assert!(load_gif_with_budget(&gif, 32, 19_199).is_err());
        assert_eq!(
            load_gif_with_budget(&gif, 32, 19_200).unwrap().frames.len(),
            120
        );
    }

    #[test]
    fn truncated_gif_is_an_error_not_a_panic() {
        let gif = fixture("sample.gif");
        assert!(load_image(&gif[..gif.len() / 2], 64).is_err());
        assert!(load_image(&gif[..10], 64).is_err());
        assert!(load_image(b"", 64).is_err());
    }

    fn long_gif(frames: u32) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = image::codecs::gif::GifEncoder::new(&mut out);
            for i in 0..frames {
                let v = (i / 2) as u8;
                let buf = image::RgbaImage::from_pixel(8, 8, image::Rgba([v, 0, 255 - v, 255]));
                enc.encode_frame(image::Frame::from_parts(
                    buf,
                    0,
                    0,
                    image::Delay::from_numer_denom_ms(40, 1),
                ))
                .unwrap();
            }
        }
        out
    }

    #[test]
    fn long_gif_is_capped_at_120_frames() {
        let img = load_image(&long_gif(300), 32).unwrap();
        assert_eq!(img.frames.len(), 120);
        // 40 ms is faster than 15 fps, and sampling 300 down to 120 slows playback to match.
        assert!(img.delay_ms >= 67);
        // Sampling is even: the last kept frame is near the end, not the 120th source frame.
        let last = img.frames[119].get_pixel(0, 0)[0];
        assert_eq!(last, (297 / 2) as u8);
    }

    #[test]
    fn text_that_does_not_fit_ends_with_ellipsis() {
        let mut pen = Pen::new(120, 120, [0, 0, 0]);
        let long = "W".repeat(5000);
        let w = pen.text_centered(0, 120, 10.0, 20.0, FontKind::Bold, [255; 3], &long);
        assert!(w > 0.0 && w <= 120.0, "w={w}");
        // The ellipsis is what ends it: the same width as "W…" fitted, not a bare cut.
        let fits = pen.text_centered(0, 120, 40.0, 20.0, FontKind::Bold, [255; 3], "ok");
        assert!(fits < 120.0);
        assert_eq!(
            pen.text_centered(0, 120, 60.0, 20.0, FontKind::Label, [255; 3], ""),
            0.0
        );
        let _ = pen.text_centered(0, 120, 80.0, 20.0, FontKind::Mono, [255; 3], "🚀🎉👍");
        for bad in [f32::NAN, f32::INFINITY, 0.0, -4.0] {
            assert_eq!(
                pen.text_centered(0, 120, 0.0, bad, FontKind::Bold, [255; 3], "x"),
                0.0
            );
        }
        // A huge size is clamped, not looped over.
        let _ = pen.text_centered(0, 120, 0.0, 1e9, FontKind::Bold, [255; 3], "x");
        pen.rounded_rect(-1e9, -1e9, 2e9, 2e9, 4.0, [1, 2, 3]);
        pen.ring(60.0, 60.0, 1e9, 4.0, 1.0, [1, 2, 3]);
        // Narrower than the ellipsis itself: nothing drawn, nothing panics.
        assert_eq!(
            pen.text_centered(0, 2, 90.0, 20.0, FontKind::Mono, [255; 3], "abc"),
            0.0
        );
    }

    #[test]
    fn bundled_assets_exist() {
        assert!(bundled("lucide/rocket.svg").is_some());
        assert!(bundled("marks/novahelm.svg").is_some());
        assert!(bundled("lucide/nope.svg").is_none());
        // Every embedded asset must itself load.
        for (name, bytes) in ASSETS {
            load_image(bytes, 64).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }

    #[test]
    fn pen_primitives_draw_and_encode() {
        let mut pen = Pen::new(120, 120, [0, 0, 0]);
        pen.rounded_rect(10.0, 10.0, 100.0, 100.0, 16.0, [255, 0, 0]);
        assert_eq!(pen.img.get_pixel(60, 60).0, [255, 0, 0]);
        assert_eq!(
            pen.img.get_pixel(10, 10).0,
            [0, 0, 0],
            "corner is rounded away"
        );
        pen.ring(60.0, 60.0, 40.0, 6.0, 0.5, [0, 255, 0]);
        assert_eq!(
            pen.img.get_pixel(100, 60).0[1],
            255,
            "right side is inside a half ring"
        );
        assert_eq!(
            pen.img.get_pixel(20, 60).0[1],
            0,
            "left side is outside a half ring"
        );
        pen.ring(60.0, 60.0, 30.0, 4.0, f32::NAN, [0, 0, 255]);
        let icon = load_image(bundled("lucide/layers.svg").unwrap(), 48).unwrap();
        pen.blit(&icon.frames[0], 36, 36, 48, 48, 1.0);
        let jpg = pen.jpeg(90).unwrap();
        assert_eq!(&jpg[..2], &[0xff, 0xd8]);
    }

    #[test]
    fn text_matches_snapshot() {
        let mut pen = Pen::new(120, 120, [20, 24, 30]);
        pen.text_centered(0, 120, 8.0, 22.0, FontKind::Bold, [255; 3], "Deploy");
        pen.text_centered(0, 120, 50.0, 18.0, FontKind::Mono, [120, 220, 160], "42.5%");
        pen.text_centered(0, 120, 80.0, 16.0, FontKind::Label, [180; 3], "Label text");
        let path = format!("{}/tests/snapshots/text.png", env!("CARGO_MANIFEST_DIR"));
        if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
            pen.img.save(&path).unwrap();
        }
        let want = image::open(&path)
            .expect("snapshot (UPDATE_SNAPSHOTS=1 to create)")
            .to_rgb8();
        let diff: u64 = pen
            .img
            .as_raw()
            .iter()
            .zip(want.as_raw())
            .map(|(a, b)| a.abs_diff(*b) as u64)
            .sum();
        // Allow tiny rasteriser drift; a changed font or layout is far above this.
        assert!(diff < 120 * 120 * 3 / 20, "diff {diff}");
    }
}
