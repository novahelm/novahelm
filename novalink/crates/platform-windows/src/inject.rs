//! SendInput injection, tagged with NovaLink's magic in `dwExtraInfo`.
//!
//! Keys go by scancode (layout-independent); E0 keys add `KEYEVENTF_EXTENDEDKEY`.
//! Pause and Num Lock go by virtual key (see `keymap`). Auto-repeat downs are plain
//! key-downs: Windows marks a down of an already-down key as a repeat itself.
//! Caps Lock is a normal key here: SendInput toggles it natively.
//!
//! Motion is relative `MOUSEEVENTF_MOVE`, so Windows pointer acceleration
//! ("enhance pointer precision") applies to it. See the gate notes.

use crate::keymap;
use crate::logic::{self, MAGIC, PROBE_MAGIC};
use novalink_core::InputEvent;
use novalink_proto::MouseButton;
use std::sync::Mutex;
use windows::Win32::UI::Input::KeyboardAndMouse::*;

#[derive(Default)]
pub struct Injector {
    rem: Mutex<(f32, f32)>,
}

fn mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS, extra: u64) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: extra as usize,
            },
        },
    }
}

fn key(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: MAGIC as usize,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> anyhow::Result<()> {
    // SAFETY: slice of fully initialised INPUTs, correct element size.
    let n = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    anyhow::ensure!(
        n as usize == inputs.len(),
        "SendInput injected {n}/{} events (blocked by UIPI or a secure desktop?)",
        inputs.len()
    );
    Ok(())
}

/// The watchdog's self-test: a zero-length relative move tagged `PROBE_MAGIC`.
pub fn send_probe() -> anyhow::Result<()> {
    send(&[mouse(0, 0, 0, MOUSEEVENTF_MOVE, PROBE_MAGIC)])
}

impl Injector {
    pub fn inject(&self, ev: &InputEvent) -> anyhow::Result<()> {
        match *ev {
            InputEvent::Motion { dx, dy } => {
                let (x, y) = logic::take_whole(&mut self.rem.lock().unwrap(), dx, dy);
                if x == 0 && y == 0 {
                    return Ok(());
                }
                send(&[mouse(x, y, 0, MOUSEEVENTF_MOVE, MAGIC)])
            }
            InputEvent::Scroll { dx, dy } => {
                let (v, h) = (logic::units_to_wheel(dy), logic::units_to_wheel(dx));
                let mut batch = Vec::with_capacity(2);
                if v != 0 {
                    batch.push(mouse(0, 0, v as u32, MOUSEEVENTF_WHEEL, MAGIC));
                }
                if h != 0 {
                    batch.push(mouse(0, 0, h as u32, MOUSEEVENTF_HWHEEL, MAGIC));
                }
                if batch.is_empty() {
                    Ok(())
                } else {
                    send(&batch)
                }
            }
            InputEvent::Button { button, down } => {
                let (flags, data) = match (button, down) {
                    (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                    (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                    (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                    (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                    (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                    (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                    (b, d) => (
                        if d {
                            MOUSEEVENTF_XDOWN
                        } else {
                            MOUSEEVENTF_XUP
                        },
                        logic::xbutton_to_data(b).expect("X button"),
                    ),
                };
                send(&[mouse(0, 0, data, flags, MAGIC)])
            }
            InputEvent::Key { usage, down } => {
                let up = if down {
                    KEYBD_EVENT_FLAGS(0)
                } else {
                    KEYEVENTF_KEYUP
                };
                if let Some((vk, ext)) = keymap::special_vk(usage) {
                    let e = if ext {
                        KEYEVENTF_EXTENDEDKEY
                    } else {
                        KEYBD_EVENT_FLAGS(0)
                    };
                    return send(&[key(vk, 0, up | e)]);
                }
                let (sc, ext) = keymap::to_scancode(usage)
                    .ok_or_else(|| anyhow::anyhow!("no scancode for HID usage {usage:#x}"))?;
                let e = if ext {
                    KEYEVENTF_EXTENDEDKEY
                } else {
                    KEYBD_EVENT_FLAGS(0)
                };
                send(&[key(0, sc, KEYEVENTF_SCANCODE | up | e)])
            }
        }
    }
}
