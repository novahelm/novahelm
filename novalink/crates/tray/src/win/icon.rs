//! The four tray icons, drawn at runtime so the repo carries no binary assets.

use novalink_tray::model::IconState;
use tray_icon::Icon;

const SIZE: u32 = 32;

pub fn icon(state: IconState) -> Icon {
    let (color, mark) = match state {
        IconState::InControl => ([46, 160, 67], Mark::None),
        IconState::Remote => ([47, 111, 214], Mark::Dot),
        IconState::Disconnected => ([128, 128, 128], Mark::Hollow),
        IconState::Problem => ([214, 69, 65], Mark::Bang),
    };
    Icon::from_rgba(draw(color, mark), SIZE, SIZE).expect("32x32 rgba")
}

#[derive(Clone, Copy)]
enum Mark {
    None,
    /// A white dot in the middle.
    Dot,
    /// Only the outline is filled.
    Hollow,
    /// A white exclamation mark.
    Bang,
}

fn draw(color: [u8; 3], mark: Mark) -> Vec<u8> {
    let mut px = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    let c = SIZE as f32 / 2.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let (dx, dy) = (x as f32 + 0.5 - c, y as f32 + 0.5 - c);
            let d = (dx * dx + dy * dy).sqrt();
            // One pixel of soft edge on the outer circle.
            let mut alpha = (c - 1.0 - d + 0.5).clamp(0.0, 1.0);
            let mut rgb = color;
            let white = match mark {
                Mark::None => false,
                Mark::Dot => d < 5.0,
                Mark::Hollow => d < c - 5.0,
                Mark::Bang => {
                    let bar = dx.abs() < 2.0 && (-9.0..4.0).contains(&dy);
                    let dot = dx.abs() < 2.0 && (7.0..11.0).contains(&dy);
                    bar || dot
                }
            };
            if white {
                if matches!(mark, Mark::Hollow) {
                    alpha = 0.0;
                } else {
                    rgb = [255, 255, 255];
                }
            }
            px.extend_from_slice(&[rgb[0], rgb[1], rgb[2], (alpha * 255.0) as u8]);
        }
    }
    px
}
