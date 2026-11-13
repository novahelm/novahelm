//! Event-tap capture on a dedicated thread with its own CFRunLoop.

use crate::ffi::*;
use crate::keymap;
use core_foundation::base::TCFType;
use core_foundation::mach_port::CFMachPort;
use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes};
use novalink_core::{InputEvent, LocalEvent};
use novalink_proto::MouseButton;
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use tokio::sync::mpsc::UnboundedSender;

pub const MAGIC: i64 = 0x4E4F_5641_4C49_4E4B;

/// Physical = from the HID system state and not tagged by us.
pub fn is_physical(source_state_id: i64, user_data: i64) -> bool {
    source_state_id == SOURCE_STATE_HID_SYSTEM && user_data != MAGIC
}

/// Window after an injected Caps toggle in which the resulting HID-state
/// flagsChanged (which carries no MAGIC tag) is treated as non-physical.
pub const CAPS_INJECT_WINDOW_US: u64 = 250_000;

/// Mono-us (+1, so 0 = none) of our last injected Caps toggle.
static CAPS_INJECTED_AT: AtomicU64 = AtomicU64::new(0);
static TAP_REENABLED: AtomicBool = AtomicBool::new(false);

/// True once if the tap was re-enabled since the last call.
pub fn take_tap_reenabled() -> bool {
    TAP_REENABLED.swap(false, Ordering::AcqRel)
}

/// Called by the injector just before it toggles the Caps Lock state.
pub fn mark_caps_injected() {
    CAPS_INJECTED_AT.store(novalink_core::time::mono_us() + 1, Ordering::Release);
}

/// Pure: is a Caps flagsChanged at `now_us` the echo of our injection marked at `mark`?
pub fn caps_echo(mark: u64, now_us: u64) -> bool {
    mark != 0 && now_us.saturating_sub(mark - 1) < CAPS_INJECT_WINDOW_US
}

/// Consume the injection mark; true if this Caps event is our own echo.
fn take_caps_echo() -> bool {
    let mark = CAPS_INJECTED_AT.swap(0, Ordering::AcqRel);
    caps_echo(mark, novalink_core::time::mono_us())
}

/// Down/up of a modifier key derived from the flags of a flagsChanged event.
/// `None` if `vk` is not a modifier we track (Caps Lock is handled separately:
/// it fires once per toggle with no release, see `flags_events`).
pub fn modifier_down(vk: u16, flags: u64) -> Option<bool> {
    // Device-dependent bits distinguish left from right.
    const L_CTRL: u64 = 0x1;
    const L_SHIFT: u64 = 0x2;
    const R_SHIFT: u64 = 0x4;
    const L_CMD: u64 = 0x8;
    const R_CMD: u64 = 0x10;
    const L_ALT: u64 = 0x20;
    const R_ALT: u64 = 0x40;
    const R_CTRL: u64 = 0x2000;
    let bit = match vk {
        0x3B => L_CTRL,
        0x3E => R_CTRL,
        0x38 => L_SHIFT,
        0x3C => R_SHIFT,
        0x37 => L_CMD,
        0x36 => R_CMD,
        0x3A => L_ALT,
        0x3D => R_ALT,
        _ => return None,
    };
    Some(flags & bit != 0)
}

/// What the tap does with a physical key/button event while input may be suppressed.
/// `passed` = its down already reached the local OS. Returns `(swallow, passed_after)`.
/// A down seen while suppressing is swallowed; its up is swallowed only if its down was,
/// otherwise the up must pass or the key/button stays stuck down locally.
pub fn swallow_decision(suppress: bool, down: bool, passed: bool) -> (bool, bool) {
    match (down, suppress) {
        (true, false) => (false, true),
        (true, true) => (true, passed),
        (false, false) => (false, false),
        (false, true) => (!passed, false),
    }
}

/// Which key/button slot an event toggles, and whether it is a down.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Slot {
    Key(u8),
    Button(u8),
}

struct Ctx {
    /// Keys (by keycode) and buttons (by number, capped at 7) whose down was passed through.
    keys_passed: [AtomicBool; 256],
    buttons_passed: [AtomicBool; 8],
    tx: UnboundedSender<LocalEvent>,
    suppress: Arc<AtomicBool>,
    port: AtomicPtr<c_void>,
}

/// Key events for a flagsChanged event. Caps Lock fires once per toggle with
/// no physical release, so it becomes a synthetic down+up pair.
pub fn flags_events(vk: u16, flags: u64) -> [Option<InputEvent>; 2] {
    let Some(usage) = keymap::to_usage(vk) else {
        return [None, None];
    };
    if vk == 0x39 {
        return [
            Some(InputEvent::Key { usage, down: true }),
            Some(InputEvent::Key { usage, down: false }),
        ];
    }
    match modifier_down(vk, flags) {
        Some(down) => [Some(InputEvent::Key { usage, down }), None],
        None => [None, None],
    }
}

fn mask_of(types: &[u32]) -> u64 {
    types.iter().fold(0, |m, &t| m | (1u64 << t))
}

/// Translate a raw event into an `InputEvent` (None = not something we forward).
///
/// # Safety
/// `ev` must be a valid CGEventRef.
unsafe fn translate(ty: u32, ev: CGEventRef) -> [Option<InputEvent>; 2] {
    if ty == FLAGS_CHANGED {
        // SAFETY: caller guarantees ev is valid.
        let (vk, flags) = unsafe {
            (
                CGEventGetIntegerValueField(ev, F_KEYCODE) as u16,
                CGEventGetFlags(ev),
            )
        };
        return flags_events(vk, flags);
    }
    [unsafe { translate_one(ty, ev) }, None]
}

/// # Safety
/// `ev` must be a valid CGEventRef.
unsafe fn translate_one(ty: u32, ev: CGEventRef) -> Option<InputEvent> {
    unsafe {
        let field = |f| CGEventGetIntegerValueField(ev, f);
        match ty {
            MOUSE_MOVED | LEFT_MOUSE_DRAGGED | RIGHT_MOUSE_DRAGGED | OTHER_MOUSE_DRAGGED => {
                Some(InputEvent::Motion {
                    dx: field(F_MOUSE_DELTA_X) as f32,
                    dy: field(F_MOUSE_DELTA_Y) as f32,
                })
            }
            LEFT_MOUSE_DOWN | LEFT_MOUSE_UP => Some(InputEvent::Button {
                button: MouseButton::Left,
                down: ty == LEFT_MOUSE_DOWN,
            }),
            RIGHT_MOUSE_DOWN | RIGHT_MOUSE_UP => Some(InputEvent::Button {
                button: MouseButton::Right,
                down: ty == RIGHT_MOUSE_DOWN,
            }),
            OTHER_MOUSE_DOWN | OTHER_MOUSE_UP => {
                let button = match field(F_MOUSE_BUTTON_NUMBER) {
                    2 => MouseButton::Middle,
                    3 => MouseButton::Back,
                    4 => MouseButton::Forward,
                    _ => return None,
                };
                Some(InputEvent::Button {
                    button,
                    down: ty == OTHER_MOUSE_DOWN,
                })
            }
            SCROLL_WHEEL => Some(InputEvent::Scroll {
                dx: field(F_SCROLL_POINT_DELTA_2) as f32,
                dy: field(F_SCROLL_POINT_DELTA_1) as f32,
            }),
            KEY_DOWN | KEY_UP => {
                // Auto-repeat downs are forwarded as plain downs; the injector marks repeats.
                let usage = keymap::to_usage(field(F_KEYCODE) as u16)?;
                Some(InputEvent::Key {
                    usage,
                    down: ty == KEY_DOWN,
                })
            }
            _ => None,
        }
    }
}

extern "C" fn callback(
    _proxy: *mut c_void,
    ty: u32,
    ev: CGEventRef,
    info: *mut c_void,
) -> CGEventRef {
    // SAFETY: `info` is the leaked Ctx created in `start`, valid for the process lifetime.
    let ctx = unsafe { &*(info as *const Ctx) };
    if ty == TAP_DISABLED_BY_TIMEOUT || ty == TAP_DISABLED_BY_USER_INPUT {
        let port = ctx.port.load(Ordering::Acquire);
        if !port.is_null() {
            // SAFETY: port is the live tap port.
            unsafe { CGEventTapEnable(port, true) };
        }
        TAP_REENABLED.store(true, Ordering::Release);
        tracing::warn!(
            event = "tap_reenabled",
            by_timeout = ty == TAP_DISABLED_BY_TIMEOUT
        );
        return ev;
    }
    // SAFETY: ev is valid for the duration of the callback.
    unsafe {
        let mut physical = is_physical(
            CGEventGetIntegerValueField(ev, F_SOURCE_STATE_ID),
            CGEventGetIntegerValueField(ev, F_SOURCE_USER_DATA),
        );
        if ty == FLAGS_CHANGED
            && CGEventGetIntegerValueField(ev, F_KEYCODE) == 0x39
            && take_caps_echo()
        {
            physical = false;
        }
        for event in translate(ty, ev).into_iter().flatten() {
            let p = CGEventGetLocation(ev);
            let _ = ctx.tx.send(LocalEvent {
                event,
                physical,
                t_us: novalink_core::time::mono_us(),
                pos: (p.x, p.y),
            });
        }
        if physical {
            let suppressed = ctx.suppress.load(Ordering::Acquire);
            let button = |n: i64| Slot::Button(n.clamp(0, 7) as u8);
            let slot = match ty {
                KEY_DOWN | KEY_UP => Some((
                    Slot::Key(CGEventGetIntegerValueField(ev, F_KEYCODE) as u8),
                    ty == KEY_DOWN,
                )),
                FLAGS_CHANGED => {
                    let vk = CGEventGetIntegerValueField(ev, F_KEYCODE) as u16;
                    modifier_down(vk, CGEventGetFlags(ev)).map(|d| (Slot::Key(vk as u8), d))
                }
                LEFT_MOUSE_DOWN | LEFT_MOUSE_UP => Some((button(0), ty == LEFT_MOUSE_DOWN)),
                RIGHT_MOUSE_DOWN | RIGHT_MOUSE_UP => Some((button(1), ty == RIGHT_MOUSE_DOWN)),
                OTHER_MOUSE_DOWN | OTHER_MOUSE_UP => Some((
                    button(CGEventGetIntegerValueField(ev, F_MOUSE_BUTTON_NUMBER)),
                    ty == OTHER_MOUSE_DOWN,
                )),
                _ => None,
            };
            let swallow = match slot {
                Some((s, down)) => {
                    let flag = match s {
                        Slot::Key(k) => &ctx.keys_passed[k as usize],
                        Slot::Button(b) => &ctx.buttons_passed[b as usize],
                    };
                    let (sw, after) =
                        swallow_decision(suppressed, down, flag.load(Ordering::Acquire));
                    flag.store(after, Ordering::Release);
                    sw
                }
                None => suppressed,
            };
            if swallow {
                return std::ptr::null_mut();
            }
        }
    }
    ev
}

/// Create the tap and run its loop on a new thread. Errors if the tap cannot be
/// created (usually missing Accessibility / Input Monitoring permission).
///
/// The context lives as long as the tap. It is freed when no tap was made from it, so
/// retrying while permissions are missing doesn't leak; once the tap runs it is never freed.
pub fn start(tx: UnboundedSender<LocalEvent>, suppress: Arc<AtomicBool>) -> anyhow::Result<()> {
    let ctx: *mut Ctx = Box::into_raw(Box::new(Ctx {
        keys_passed: std::array::from_fn(|_| AtomicBool::new(false)),
        buttons_passed: std::array::from_fn(|_| AtomicBool::new(false)),
        tx,
        suppress,
        port: AtomicPtr::new(std::ptr::null_mut()),
    }));
    let ctx_addr = ctx as usize;
    // SAFETY: only called where no tap holds the context any more (or never did).
    let free = move || drop(unsafe { Box::from_raw(ctx_addr as *mut Ctx) });
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<anyhow::Result<()>>();
    let spawned = std::thread::Builder::new()
        .name("novalink-tap".into())
        .spawn(move || {
            let mask = mask_of(&[
                MOUSE_MOVED, LEFT_MOUSE_DOWN, LEFT_MOUSE_UP, LEFT_MOUSE_DRAGGED, RIGHT_MOUSE_DOWN,
                RIGHT_MOUSE_UP, RIGHT_MOUSE_DRAGGED, OTHER_MOUSE_DOWN, OTHER_MOUSE_UP,
                OTHER_MOUSE_DRAGGED, SCROLL_WHEEL, KEY_DOWN, KEY_UP, FLAGS_CHANGED,
            ]);
            // SAFETY: callback/ctx outlive the tap (ctx is freed only if the tap never ran).
            let port = unsafe {
                CGEventTapCreate(HID_EVENT_TAP, HEAD_INSERT_EVENT_TAP, TAP_OPTION_DEFAULT, mask, callback, ctx_addr as *mut c_void)
            };
            if port.is_null() {
                // No tap refers to the context.
                free();
                let _ = ready_tx.send(Err(anyhow::anyhow!(
                    "CGEventTapCreate failed: grant Accessibility and Input Monitoring to this process"
                )));
                return;
            }
            // SAFETY: the context stays alive while the tap exists.
            let ctx = unsafe { &*(ctx_addr as *const Ctx) };
            ctx.port.store(port, Ordering::Release);
            // SAFETY: port is a fresh +1 CFMachPort we now own.
            let mach = unsafe { CFMachPort::wrap_under_create_rule(port as _) };
            let Ok(source) = mach.create_runloop_source(0) else {
                // Without a run loop source the callback never runs; release the tap first.
                drop(mach);
                free();
                let _ = ready_tx.send(Err(anyhow::anyhow!("failed to create run loop source")));
                return;
            };
            let rl = CFRunLoop::get_current();
            // SAFETY: kCFRunLoopCommonModes is a valid static constant.
            rl.add_source(&source, unsafe { kCFRunLoopCommonModes });
            unsafe { CGEventTapEnable(port, true) };
            let _ = ready_tx.send(Ok(()));
            CFRunLoop::run_current();
            drop(mach);
        });
    if let Err(e) = spawned {
        // The closure never ran, so no tap was made.
        free();
        return Err(e.into());
    }
    ready_rx
        .recv()
        .map_err(|_| anyhow::anyhow!("tap thread died"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_requires_hid_state_and_no_magic() {
        assert!(is_physical(1, 0));
        assert!(!is_physical(1, MAGIC));
        assert!(!is_physical(-1, 0));
        assert!(!is_physical(0, 0));
    }

    #[test]
    fn modifier_flags() {
        assert_eq!(modifier_down(0x38, 0x2), Some(true));
        assert_eq!(modifier_down(0x38, 0x0), Some(false));
        // Right shift held while left shift released:
        assert_eq!(modifier_down(0x38, 0x4), Some(false));
        assert_eq!(modifier_down(0x3C, 0x4), Some(true));
        assert_eq!(modifier_down(0x3E, 0x2000), Some(true));
        assert_eq!(modifier_down(0x39, FLAG_ALPHA_SHIFT), None);
        assert_eq!(modifier_down(0x00, 0xFFFF), None);
    }

    #[test]
    fn up_of_a_passed_down_passes_while_suppressed() {
        // (suppress, down, passed) -> (swallow, passed_after)
        assert_eq!(swallow_decision(false, true, false), (false, true));
        assert_eq!(swallow_decision(false, false, true), (false, false));
        assert_eq!(swallow_decision(true, true, false), (true, false));
        assert_eq!(swallow_decision(true, false, false), (true, false));
        assert_eq!(swallow_decision(true, false, true), (false, false)); // would stick otherwise
        assert_eq!(swallow_decision(true, true, true), (true, true)); // repeat keeps it tracked
    }

    #[test]
    fn mask_bits() {
        assert_eq!(mask_of(&[1, 5]), (1 << 1) | (1 << 5));
    }

    #[test]
    fn caps_lock_emits_synthetic_down_up_each_toggle() {
        use novalink_proto::keys::CAPS_LOCK;
        let want = [
            Some(InputEvent::Key {
                usage: CAPS_LOCK,
                down: true,
            }),
            Some(InputEvent::Key {
                usage: CAPS_LOCK,
                down: false,
            }),
        ];
        // Same pair whether the toggle turned the lock on or off.
        assert_eq!(flags_events(0x39, FLAG_ALPHA_SHIFT), want);
        assert_eq!(flags_events(0x39, 0), want);
    }

    #[test]
    fn modifier_flags_events() {
        use novalink_proto::keys::LEFT_SHIFT;
        assert_eq!(
            flags_events(0x38, 0x2),
            [
                Some(InputEvent::Key {
                    usage: LEFT_SHIFT,
                    down: true
                }),
                None
            ]
        );
        assert_eq!(
            flags_events(0x38, 0),
            [
                Some(InputEvent::Key {
                    usage: LEFT_SHIFT,
                    down: false
                }),
                None
            ]
        );
        assert_eq!(flags_events(0x00, 0), [None, None]);
    }

    #[test]
    fn caps_echo_window() {
        assert!(!caps_echo(0, 1_000)); // no mark
        assert!(caps_echo(1_001, 1_000 + 100_000)); // mark=1000us (+1)
        assert!(caps_echo(1_001, 1_000 + 249_999));
        assert!(!caps_echo(1_001, 1_000 + 250_000));
    }
}
