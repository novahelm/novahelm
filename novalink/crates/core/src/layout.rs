use novalink_proto::{DeviceId, DisplayInfo, Edge, EntryPoint, Link};
use serde::{Deserialize, Serialize};

/// Distance (in units) from a display side that counts as "at the edge".
const EDGE_TOLERANCE: f64 = 1.0;
/// How far inside the receiving machine's edge the cursor lands.
const ENTRY_INSET: f64 = 2.0;

/// The mesh layout: which machine sits beyond which edge of which machine.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    pub version: u64,
    pub links: Vec<Link>,
}

impl Layout {
    /// The device beyond `edge` of `dev`, resolving reverse links too.
    pub fn neighbor(&self, dev: DeviceId, edge: Edge) -> Option<DeviceId> {
        self.links.iter().find_map(|l| {
            if l.from == dev && l.from_edge == edge {
                Some(l.to)
            } else if l.to == dev && l.from_edge.opposite() == edge {
                Some(l.from)
            } else {
                None
            }
        })
    }

    /// Link `from`'s `edge` to `to`, replacing any link occupying either side.
    pub fn set_link(&mut self, from: DeviceId, edge: Edge, to: DeviceId) {
        let back = edge.opposite();
        self.links.retain(|l| {
            let from_taken =
                (l.from == from && l.from_edge == edge) || (l.to == from && l.from_edge == back);
            let to_taken =
                (l.from == to && l.from_edge == back) || (l.to == to && l.from_edge == edge);
            !(from_taken || to_taken)
        });
        self.links.push(Link {
            from,
            from_edge: edge,
            to,
        });
        self.version += 1;
    }

    /// Adopt a remote layout if its version is newer. Returns whether it was adopted.
    pub fn merge(&mut self, version: u64, links: Vec<Link>) -> bool {
        if version > self.version {
            self.version = version;
            self.links = links;
            true
        } else {
            false
        }
    }
}

fn contains(d: &DisplayInfo, x: f64, y: f64) -> bool {
    x >= d.x && x < d.x + d.width && y >= d.y && y < d.y + d.height
}

fn bounding_box(displays: &[DisplayInfo]) -> Option<(f64, f64, f64, f64)> {
    let first = displays.first()?;
    let init = (
        first.x,
        first.y,
        first.x + first.width,
        first.y + first.height,
    );
    let (x0, y0, x1, y1) = displays.iter().fold(init, |a, d| {
        (
            a.0.min(d.x),
            a.1.min(d.y),
            a.2.max(d.x + d.width),
            a.3.max(d.y + d.height),
        )
    });
    Some((x0, y0, x1 - x0, y1 - y0))
}

/// If `pos` is at an outer edge of the machine's display set, which edge, and
/// how far along the bounding box (0..=1). Left/Right win at corners.
pub fn hit_edge(displays: &[DisplayInfo], pos: (f64, f64)) -> Option<(Edge, f32)> {
    let (bx, by, bw, bh) = bounding_box(displays)?;
    let (x, y) = pos;
    let outside = |px: f64, py: f64| displays.iter().any(|d| contains(d, px, py));
    for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
        let hit = displays.iter().any(|d| {
            let (right, bottom) = (d.x + d.width, d.y + d.height);
            let in_x = x >= d.x && x <= right;
            let in_y = y >= d.y && y <= bottom;
            match edge {
                Edge::Left => in_y && (x - d.x).abs() <= EDGE_TOLERANCE && !outside(d.x - 0.5, y),
                Edge::Right => {
                    in_y && (x - right).abs() <= EDGE_TOLERANCE && !outside(right + 0.5, y)
                }
                Edge::Top => in_x && (y - d.y).abs() <= EDGE_TOLERANCE && !outside(x, d.y - 0.5),
                Edge::Bottom => {
                    in_x && (y - bottom).abs() <= EDGE_TOLERANCE && !outside(x, bottom + 0.5)
                }
            }
        });
        if hit {
            let frac = match edge {
                Edge::Left | Edge::Right => (y - by) / bh,
                Edge::Top | Edge::Bottom => (x - bx) / bw,
            };
            return Some((edge, frac.clamp(0.0, 1.0) as f32));
        }
    }
    None
}

/// Where the cursor lands for `entry`: `frac` along the bounding box edge,
/// 2 units inward, snapped onto the nearest real display.
pub fn entry_position(displays: &[DisplayInfo], entry: EntryPoint) -> (f64, f64) {
    let Some((bx, by, bw, bh)) = bounding_box(displays) else {
        return (0.0, 0.0);
    };
    let frac = f64::from(entry.frac).clamp(0.0, 1.0);
    let (x, y) = match entry.edge {
        Edge::Left => (bx + ENTRY_INSET, by + frac * bh),
        Edge::Right => (bx + bw - ENTRY_INSET, by + frac * bh),
        Edge::Top => (bx + frac * bw, by + ENTRY_INSET),
        Edge::Bottom => (bx + frac * bw, by + bh - ENTRY_INSET),
    };
    // Snap onto the nearest display, at least ENTRY_INSET inside on every side
    // so the landing point is never within EDGE_TOLERANCE of any edge.
    let inset = |v: f64, lo: f64, len: f64| {
        if len > 2.0 * ENTRY_INSET {
            v.clamp(lo + ENTRY_INSET, lo + len - ENTRY_INSET)
        } else {
            lo + len / 2.0
        }
    };
    displays
        .iter()
        .map(|d| {
            let (cx, cy) = (inset(x, d.x, d.width), inset(y, d.y, d.height));
            ((cx - x).powi(2) + (cy - y).powi(2), (cx, cy))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, p)| p)
        .unwrap_or((x, y))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(id: u32, x: f64, y: f64, w: f64, h: f64) -> DisplayInfo {
        DisplayInfo {
            id,
            x,
            y,
            width: w,
            height: h,
            scale: 1.0,
        }
    }
    #[test]
    fn right_edge_middle() {
        assert_eq!(
            hit_edge(&[d(1, 0., 0., 1920., 1080.)], (1919.5, 540.)),
            Some((Edge::Right, 0.5))
        );
    }
    #[test]
    fn interior_is_not_edge() {
        assert_eq!(hit_edge(&[d(1, 0., 0., 1920., 1080.)], (900., 500.)), None);
    }
    #[test]
    fn seam_between_side_by_side_displays_is_not_edge() {
        let ds = [d(1, 0., 0., 1920., 1080.), d(2, 1920., 0., 1920., 1080.)];
        assert_eq!(hit_edge(&ds, (1919.5, 500.)), None);
        assert_eq!(hit_edge(&ds, (3839.5, 540.)), Some((Edge::Right, 0.5)));
    }
    #[test]
    fn corner_prefers_horizontal_edge() {
        assert_eq!(
            hit_edge(&[d(1, 0., 0., 100., 100.)], (99.5, 0.)).unwrap().0,
            Edge::Right
        );
    }
    #[test]
    fn entry_lands_two_units_inside() {
        let p = entry_position(
            &[d(1, 0., 0., 2560., 1440.)],
            EntryPoint {
                edge: Edge::Left,
                frac: 0.5,
            },
        );
        assert_eq!(p, (2.0, 720.0));
    }
    #[test]
    fn entry_snaps_onto_real_display_in_l_shape() {
        let ds = [d(1, 0., 0., 1000., 2000.), d(2, 1000., 0., 1000., 500.)];
        let (x, y) = entry_position(
            &ds,
            EntryPoint {
                edge: Edge::Right,
                frac: 0.9,
            },
        );
        assert!(
            ds.iter()
                .any(|q| x >= q.x && x < q.x + q.width && y >= q.y && y < q.y + q.height)
        );
    }
    #[test]
    fn entry_never_lands_on_an_edge() {
        let layouts: [Vec<DisplayInfo>; 4] = [
            vec![d(1, 0., 0., 1920., 1080.)],
            vec![d(1, 0., 0., 1000., 2000.), d(2, 1000., 0., 1000., 500.)],
            vec![d(1, 0., 0., 500., 500.), d(2, 500., 500., 1000., 500.)],
            vec![d(1, -1440., 0., 1440., 900.), d(2, 0., -200., 2560., 1440.)],
        ];
        for ds in &layouts {
            for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
                for frac in [0.0, 0.5, 0.9, 1.0] {
                    let p = entry_position(ds, EntryPoint { edge, frac });
                    assert!(
                        ds.iter().any(|q| p.0 >= q.x
                            && p.0 < q.x + q.width
                            && p.1 >= q.y
                            && p.1 < q.y + q.height),
                        "{edge:?} {frac}: {p:?} off display"
                    );
                    assert_eq!(hit_edge(ds, p), None, "{edge:?} {frac}: {p:?} on an edge");
                }
            }
        }
    }
    #[test]
    fn layout_reverse_link_and_merge() {
        let (a, b) = (DeviceId([1; 32]), DeviceId([2; 32]));
        let mut l = Layout::default();
        l.set_link(a, Edge::Right, b);
        assert_eq!(l.neighbor(b, Edge::Left), Some(a));
        assert!(!l.merge(0, vec![]));
        assert!(l.merge(l.version + 1, vec![]));
        assert_eq!(l.neighbor(a, Edge::Right), None);
    }
    #[test]
    fn set_link_replaces_both_sides() {
        let (a, b, c) = (DeviceId([1; 32]), DeviceId([2; 32]), DeviceId([3; 32]));
        let mut l = Layout::default();
        l.set_link(a, Edge::Right, b);
        l.set_link(c, Edge::Right, a); // a's Left now taken by c; a-b link on a.Right stays
        assert_eq!(l.neighbor(a, Edge::Left), Some(c));
        assert_eq!(l.neighbor(a, Edge::Right), Some(b));
        l.set_link(c, Edge::Left, b); // b's Right occupied? no: b.Left taken by a -> unaffected; c.Left new, b.Right new
        l.set_link(a, Edge::Right, c); // replaces a.Right->b and c.Left->b
        assert_eq!(l.neighbor(a, Edge::Right), Some(c));
        assert_eq!(l.neighbor(b, Edge::Left), None);
    }
}
