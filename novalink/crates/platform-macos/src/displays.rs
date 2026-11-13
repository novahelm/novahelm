//! Active display enumeration (points, with backing scale).

use crate::ffi::*;
use novalink_proto::DisplayInfo;

/// Bounds only (scale = 1.0); cheap enough to call per injected motion event.
pub fn bounds() -> Vec<DisplayInfo> {
    list(false)
}

/// Full info: `scale` = pixel width / point width.
pub fn displays() -> Vec<DisplayInfo> {
    list(true)
}

fn list(with_scale: bool) -> Vec<DisplayInfo> {
    let mut ids = [0u32; 32];
    let mut n = 0u32;
    // SAFETY: buffers are valid for the sizes passed.
    let rc = unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut n) };
    let mut ids: Vec<u32> = if rc == 0 {
        ids[..n as usize].to_vec()
    } else {
        Vec::new()
    };
    if ids.is_empty() {
        // The active list is empty while displays are asleep / the session is
        // locked; fall back to the main display so callers always have geometry.
        // SAFETY: plain query.
        ids.push(unsafe { CGMainDisplayID() });
    }
    ids.into_iter()
        .filter_map(|id| {
            // SAFETY: id came from CoreGraphics.
            let r = unsafe { CGDisplayBounds(id) };
            if r.size.width <= 0.0 || r.size.height <= 0.0 {
                return None;
            }
            let scale = if with_scale {
                scale_of(id, r.size.width)
            } else {
                1.0
            };
            Some(DisplayInfo {
                id,
                x: r.origin.x,
                y: r.origin.y,
                width: r.size.width,
                height: r.size.height,
                scale,
            })
        })
        .collect()
}

fn scale_of(id: u32, point_width: f64) -> f64 {
    if point_width <= 0.0 {
        return 1.0;
    }
    // SAFETY: mode is released below; null is handled.
    unsafe {
        let mode = CGDisplayCopyDisplayMode(id);
        if mode.is_null() {
            return 1.0;
        }
        let px = CGDisplayModeGetPixelWidth(mode) as f64;
        CGDisplayModeRelease(mode);
        if px > 0.0 { px / point_width } else { 1.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_least_one_sane_display() {
        // Headless CI may have none; only assert sanity when present.
        for d in displays() {
            assert!(d.width > 0.0 && d.height > 0.0);
            assert!(d.scale >= 1.0);
        }
    }
}
