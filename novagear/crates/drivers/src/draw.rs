//! Tiny RGB canvas for Stream Deck keys and the touch strip: fills, a 5×7 bitmap font and
//! JPEG output. Enough for labels and values without shipping a font rasterizer.

use anyhow::Result;

pub struct Canvas {
    pub w: u32,
    pub h: u32,
    px: Vec<u8>,
}

/// `#rrggbb` to bytes; anything unparseable is dark grey.
pub fn parse_color(s: &str) -> [u8; 3] {
    let h = s.trim_start_matches('#');
    let byte = |i: usize| h.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok());
    match (h.len(), byte(0), byte(2), byte(4)) {
        (6, Some(r), Some(g), Some(b)) => [r, g, b],
        _ => [0x30, 0x30, 0x30],
    }
}

impl Canvas {
    pub fn new(w: u32, h: u32, bg: [u8; 3]) -> Canvas {
        let px = bg
            .iter()
            .copied()
            .cycle()
            .take((w * h * 3) as usize)
            .collect();
        Canvas { w, h, px }
    }

    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, c: [u8; 3]) {
        for yy in y..(y + h).min(self.h) {
            for xx in x..(x + w).min(self.w) {
                let i = ((yy * self.w + xx) * 3) as usize;
                self.px[i..i + 3].copy_from_slice(&c);
            }
        }
    }

    /// Width in pixels of `text` at `scale`.
    pub fn text_width(text: &str, scale: u32) -> u32 {
        let n = text.chars().count() as u32;
        if n == 0 { 0 } else { n * 6 * scale - scale }
    }

    pub fn text(&mut self, x: u32, y: u32, text: &str, scale: u32, c: [u8; 3]) {
        for (i, ch) in text.chars().enumerate() {
            let gx = x + i as u32 * 6 * scale;
            for (row, bits) in glyph(ch).iter().enumerate() {
                for col in 0..5 {
                    if bits & (0x10 >> col) != 0 {
                        self.fill_rect(gx + col * scale, y + row as u32 * scale, scale, scale, c);
                    }
                }
            }
        }
    }

    /// Text centred horizontally in the band `x..x+w`, shrinking the scale until it fits.
    pub fn text_centered(
        &mut self,
        x: u32,
        w: u32,
        y: u32,
        text: &str,
        max_scale: u32,
        c: [u8; 3],
    ) {
        let mut scale = max_scale;
        while scale > 1 && Canvas::text_width(text, scale) > w.saturating_sub(2) {
            scale -= 1;
        }
        let tw = Canvas::text_width(text, scale);
        self.text(x + w.saturating_sub(tw) / 2, y, text, scale, c);
    }

    /// Draws an image scaled to fit the box `x, y, w, h` (aspect kept, centred), blending
    /// its alpha over what's there.
    pub fn draw_image(&mut self, img: &image::RgbaImage, x: u32, y: u32, w: u32, h: u32) {
        let (iw, ih) = img.dimensions();
        if iw == 0 || ih == 0 || w == 0 || h == 0 {
            return;
        }
        let scale = (w as f32 / iw as f32).min(h as f32 / ih as f32);
        let (tw, th) = (
            ((iw as f32 * scale) as u32).max(1),
            ((ih as f32 * scale) as u32).max(1),
        );
        let scaled = image::imageops::resize(img, tw, th, image::imageops::FilterType::Triangle);
        let (ox, oy) = (x + (w - tw) / 2, y + (h - th) / 2);
        for (sx, sy, p) in scaled.enumerate_pixels() {
            let (dx, dy) = (ox + sx, oy + sy);
            if dx >= self.w || dy >= self.h {
                continue;
            }
            let i = ((dy * self.w + dx) * 3) as usize;
            let a = u32::from(p[3]);
            for c in 0..3 {
                let under = u32::from(self.px[i + c]);
                self.px[i + c] = ((u32::from(p[c]) * a + under * (255 - a)) / 255) as u8;
            }
        }
    }

    pub fn jpeg(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90).encode(
            &self.px,
            self.w,
            self.h,
            image::ExtendedColorType::Rgb8,
        )?;
        Ok(out)
    }
}

/// Rows of a 5×7 glyph, high bit on the left. Lowercase draws as uppercase.
fn glyph(c: char) -> [u8; 7] {
    match c.to_ascii_uppercase() {
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        'D' => [0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F],
        'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        'J' => [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C],
        'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x11, 0x19, 0x15, 0x13, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        'Q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x15, 0x0A],
        'X' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
        'Y' => [0x11, 0x11, 0x11, 0x0A, 0x04, 0x04, 0x04],
        'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        '%' => [0x18, 0x19, 0x02, 0x04, 0x08, 0x13, 0x03],
        '-' => [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00],
        '+' => [0x00, 0x04, 0x04, 0x1F, 0x04, 0x04, 0x00],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C],
        ':' => [0x00, 0x0C, 0x0C, 0x00, 0x0C, 0x0C, 0x00],
        '/' => [0x00, 0x01, 0x02, 0x04, 0x08, 0x10, 0x00],
        '>' => [0x08, 0x04, 0x02, 0x01, 0x02, 0x04, 0x08],
        '!' => [0x04, 0x04, 0x04, 0x04, 0x04, 0x00, 0x04],
        '?' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x00, 0x04],
        _ => [0; 7],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_images_scaled_with_alpha() {
        let mut c = Canvas::new(10, 10, [0, 0, 0]);
        let mut img = image::RgbaImage::new(2, 2);
        img.put_pixel(0, 0, image::Rgba([255, 255, 255, 255]));
        img.put_pixel(1, 1, image::Rgba([255, 255, 255, 0]));
        c.draw_image(&img, 0, 0, 10, 10);
        assert_eq!(&c.px[..3], &[255, 255, 255], "opaque corner drawn");
        let last = ((9 * 10 + 9) * 3) as usize;
        assert_eq!(
            &c.px[last..last + 3],
            &[0, 0, 0],
            "transparent corner leaves the background"
        );
    }

    #[test]
    fn parses_colors() {
        assert_eq!(parse_color("#c8871e"), [0xc8, 0x87, 0x1e]);
        assert_eq!(parse_color("nope"), [0x30, 0x30, 0x30]);
    }

    #[test]
    fn centred_text_shrinks_to_fit_and_encodes() {
        let mut c = Canvas::new(120, 120, [0, 0, 0]);
        c.text_centered(0, 120, 50, "BRIGHTNESS", 4, [255, 255, 255]);
        let jpg = c.jpeg().unwrap();
        assert_eq!(&jpg[..2], &[0xFF, 0xD8], "JPEG start-of-image marker");
        assert!(
            Canvas::text_width("BRIGHTNESS", 2) <= 118,
            "a ten-letter label still fits a key at scale 2"
        );
    }
}
