//! The tray icon, drawn in code: a ring (a key light seen face-on) with a dot in the middle.
//! On macOS it's a template image, so the menu bar tints it; elsewhere it's drawn white for
//! dark taskbars.

pub const SIZE: u32 = 32;

pub fn rgba() -> Vec<u8> {
    let rgb: [u8; 3] = if cfg!(target_os = "macos") {
        [0, 0, 0]
    } else {
        [255, 255, 255]
    };
    let c = (SIZE as f32 - 1.0) / 2.0;
    let mut px = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
            // Ring 10–14 px from the centre and a 4 px dot, with a 1 px soft edge.
            let ring = 1.0 - ((d - 12.0).abs() - 2.0).clamp(0.0, 1.0);
            let dot = 1.0 - (d - 4.0).clamp(0.0, 1.0);
            let a = (ring.max(dot) * 255.0) as u8;
            px.extend_from_slice(&[rgb[0], rgb[1], rgb[2], a]);
        }
    }
    px
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_and_dot_are_opaque_gap_is_clear() {
        let px = rgba();
        let alpha = |x: u32, y: u32| px[((y * SIZE + x) * 4 + 3) as usize];
        assert_eq!(px.len(), (SIZE * SIZE * 4) as usize);
        assert_eq!(alpha(16, 16), 255, "centre dot");
        assert_eq!(alpha(16 + 8, 16), 0, "gap between dot and ring");
        assert_eq!(alpha(16 + 12, 16), 255, "ring");
        assert_eq!(alpha(0, 0), 0, "corner");
    }
}
