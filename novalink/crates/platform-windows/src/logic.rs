//! Platform-independent pieces of the Windows backend: event classification,
//! wheel/scroll unit conversion and DPI scale. Kept separate so they are testable
//! off Windows.

use novalink_proto::MouseButton;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

/// NovaLink's tag in `dwExtraInfo` on injected input ("NOVALINK").
pub const MAGIC: u64 = 0x4E4F_5641_4C49_4E4B;
/// Tag on the watchdog's self-test mouse move.
pub const PROBE_MAGIC: u64 = 0x4E4C_5F50_4947_4E47;

pub const LLKHF_EXTENDED: u32 = 0x01;
pub const LLKHF_LOWER_IL_INJECTED: u32 = 0x02;
pub const LLKHF_ALTDOWN: u32 = 0x20;
pub const LLKHF_INJECTED: u32 = 0x10;
pub const LLMHF_INJECTED: u32 = 0x01;
pub const LLMHF_LOWER_IL_INJECTED: u32 = 0x02;

/// One mouse wheel notch in Windows units.
pub const WHEEL_DELTA: i32 = 120;
/// Protocol scroll units (pixel-like) per wheel notch. Tune at the gate.
pub const SCROLL_UNITS_PER_NOTCH: f32 = 40.0;

/// Real keyboard hardware: not flagged injected (either level) and not ours.
pub fn kbd_physical(flags: u32, extra: u64) -> bool {
    flags & (LLKHF_INJECTED | LLKHF_LOWER_IL_INJECTED) == 0 && extra != MAGIC
}

/// Real mouse hardware. The watchdog probe is never physical either.
pub fn mouse_physical(flags: u32, extra: u64) -> bool {
    flags & (LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED) == 0
        && extra != MAGIC
        && extra != PROBE_MAGIC
}

/// Signed wheel delta from the high word of `mouseData`.
pub fn wheel_delta(mouse_data: u32) -> i32 {
    ((mouse_data >> 16) as u16 as i16) as i32
}

pub fn wheel_to_units(delta: i32) -> f32 {
    delta as f32 / WHEEL_DELTA as f32 * SCROLL_UNITS_PER_NOTCH
}

pub fn units_to_wheel(units: f32) -> i32 {
    (units / SCROLL_UNITS_PER_NOTCH * WHEEL_DELTA as f32).round() as i32
}

/// Which X button the high word of `mouseData` names.
pub fn xbutton_from_data(mouse_data: u32) -> Option<MouseButton> {
    match mouse_data >> 16 {
        1 => Some(MouseButton::Back),
        2 => Some(MouseButton::Forward),
        _ => None,
    }
}

/// `mouseData` value for SendInput (XBUTTON1 = 1, XBUTTON2 = 2).
pub fn xbutton_to_data(b: MouseButton) -> Option<u32> {
    match b {
        MouseButton::Back => Some(1),
        MouseButton::Forward => Some(2),
        _ => None,
    }
}

pub fn scale_from_dpi(dpi: u32) -> f64 {
    if dpi == 0 { 1.0 } else { dpi as f64 / 96.0 }
}

/// Add `(dx, dy)` to the carried sub-pixel remainder and return the whole pixels
/// to send, keeping the fraction for next time (SendInput motion is integral).
pub fn take_whole(rem: &mut (f32, f32), dx: f32, dy: f32) -> (i32, i32) {
    rem.0 += dx;
    rem.1 += dy;
    let (wx, wy) = (rem.0.round(), rem.1.round());
    rem.0 -= wx;
    rem.1 -= wy;
    (wx as i32, wy as i32)
}

/// What a hook does with a physical key/button event while input may be suppressed.
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

/// 256 lock-free flags (virtual keys, button numbers).
#[derive(Default)]
pub struct AtomicBits([std::sync::atomic::AtomicU64; 4]);

impl AtomicBits {
    pub const fn new() -> Self {
        use std::sync::atomic::AtomicU64 as A;
        Self([A::new(0), A::new(0), A::new(0), A::new(0)])
    }

    pub fn get(&self, i: u8) -> bool {
        self.0[(i >> 6) as usize].load(std::sync::atomic::Ordering::Acquire) >> (i & 63) & 1 != 0
    }

    pub fn set(&self, i: u8, on: bool) {
        let m = 1u64 << (i & 63);
        let w = &self.0[(i >> 6) as usize];
        if on {
            w.fetch_or(m, std::sync::atomic::Ordering::AcqRel);
        } else {
            w.fetch_and(!m, std::sync::atomic::Ordering::AcqRel);
        }
    }
}

/// Fallback motion source (`NOVALINK_WIN_MOTION=hook`): delta of the hook's point from
/// the pinned point while suppressed (the swallowed move leaves the cursor there), or
/// from the previous point otherwise.
pub fn hook_motion_delta(
    suppressed: bool,
    pin: (i32, i32),
    last: (i32, i32),
    pt: (i32, i32),
) -> (i32, i32) {
    let base = if suppressed { pin } else { last };
    (pt.0 - base.0, pt.1 - base.1)
}

/// Hook-mode motion state shared by the mouse hook and the platform calls: the point
/// the cursor is pinned to while suppressing, and the last point seen otherwise.
///
/// Ordering matters because the hook runs concurrently: the pin is published *before*
/// suppression turns on (so the first swallowed move is measured from it), and `last`
/// is rebased on the cursor when suppression ends or the cursor is warped.
pub struct HookPoints {
    pin_x: AtomicI32,
    pin_y: AtomicI32,
    last_x: AtomicI32,
    last_y: AtomicI32,
}

impl Default for HookPoints {
    fn default() -> Self {
        Self::new()
    }
}

impl HookPoints {
    pub const fn new() -> Self {
        HookPoints {
            pin_x: AtomicI32::new(0),
            pin_y: AtomicI32::new(0),
            last_x: AtomicI32::new(0),
            last_y: AtomicI32::new(0),
        }
    }

    /// Publishes the pinned point (and the baseline, which equals it while pinned).
    pub fn pin(&self, p: (i32, i32)) {
        self.pin_x.store(p.0, Ordering::Release);
        self.pin_y.store(p.1, Ordering::Release);
        self.rebase(p);
    }

    pub fn pin_point(&self) -> (i32, i32) {
        (
            self.pin_x.load(Ordering::Acquire),
            self.pin_y.load(Ordering::Acquire),
        )
    }

    /// Sets the unsuppressed baseline (after unsuppressing or warping).
    pub fn rebase(&self, p: (i32, i32)) {
        self.last_x.store(p.0, Ordering::Release);
        self.last_y.store(p.1, Ordering::Release);
    }

    /// Pins at `p`, then turns suppression on.
    pub fn suppress_on(&self, flag: &AtomicBool, p: (i32, i32)) {
        self.pin(p);
        flag.store(true, Ordering::Release);
    }

    /// Turns suppression off, then rebases on the cursor `p`. Returns whether it was on.
    pub fn suppress_off(&self, flag: &AtomicBool, p: (i32, i32)) -> bool {
        let was = flag.swap(false, Ordering::AcqRel);
        self.rebase(p);
        was
    }

    /// The hook saw the cursor at `pt`: the motion delta to report.
    pub fn on_move(&self, suppressed: bool, pt: (i32, i32)) -> (i32, i32) {
        let last = (
            self.last_x.load(Ordering::Acquire),
            self.last_y.load(Ordering::Acquire),
        );
        let pin = self.pin_point();
        let d = hook_motion_delta(suppressed, pin, last, pt);
        // A swallowed move leaves the cursor at the pin.
        self.rebase(if suppressed { pin } else { pt });
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_requires_no_injected_flag_and_no_magic() {
        assert!(kbd_physical(0, 0));
        assert!(!kbd_physical(LLKHF_INJECTED, 0));
        assert!(!kbd_physical(LLKHF_LOWER_IL_INJECTED, 0));
        assert!(!kbd_physical(0, MAGIC));
        assert!(kbd_physical(LLKHF_EXTENDED | LLKHF_ALTDOWN, 0));
        assert!(mouse_physical(0, 0));
        assert!(!mouse_physical(LLMHF_INJECTED, 0));
        assert!(!mouse_physical(LLMHF_LOWER_IL_INJECTED, 0));
        assert!(!mouse_physical(0, MAGIC));
        assert!(!mouse_physical(0, PROBE_MAGIC));
    }

    #[test]
    fn magic_values_match_the_spec() {
        assert_eq!(MAGIC, 0x4E4F56414C494E4B);
        assert_eq!(PROBE_MAGIC, 0x4E4C5F5049474E47);
    }

    #[test]
    fn wheel_units_roundtrip() {
        assert_eq!(wheel_to_units(120), SCROLL_UNITS_PER_NOTCH);
        assert_eq!(wheel_to_units(-120), -SCROLL_UNITS_PER_NOTCH);
        assert_eq!(units_to_wheel(SCROLL_UNITS_PER_NOTCH), 120);
        assert_eq!(units_to_wheel(-SCROLL_UNITS_PER_NOTCH), -120);
        assert_eq!(units_to_wheel(0.0), 0);
        assert_eq!(units_to_wheel(1.0), 3);
    }

    #[test]
    fn xbutton_mapping() {
        use novalink_proto::MouseButton;
        assert_eq!(xbutton_from_data(0x0001_0000), Some(MouseButton::Back));
        assert_eq!(xbutton_from_data(0x0002_0000), Some(MouseButton::Forward));
        assert_eq!(xbutton_from_data(0x0003_0000), None);
        assert_eq!(xbutton_to_data(MouseButton::Back), Some(1));
        assert_eq!(xbutton_to_data(MouseButton::Forward), Some(2));
        assert_eq!(xbutton_to_data(MouseButton::Left), None);
    }

    #[test]
    fn wheel_delta_is_the_signed_high_word() {
        assert_eq!(wheel_delta(120u32 << 16), 120);
        assert_eq!(wheel_delta(((-120i16) as u16 as u32) << 16), -120);
    }

    #[test]
    fn dpi_to_scale() {
        assert_eq!(scale_from_dpi(96), 1.0);
        assert_eq!(scale_from_dpi(144), 1.5);
        assert_eq!(scale_from_dpi(192), 2.0);
        assert_eq!(scale_from_dpi(0), 1.0);
    }

    #[test]
    fn motion_remainder_carries_subpixels() {
        let mut rem = (0.0, 0.0);
        let mut total = (0i32, 0i32);
        for _ in 0..10 {
            let (x, y) = take_whole(&mut rem, 0.3, -0.3);
            total.0 += x;
            total.1 += y;
        }
        assert_eq!(total, (3, -3));
    }

    #[test]
    fn up_decision_table() {
        // (suppress, down, passed) -> (swallow, passed_after)
        assert_eq!(swallow_decision(false, true, false), (false, true));
        assert_eq!(swallow_decision(false, false, true), (false, false));
        assert_eq!(swallow_decision(true, true, false), (true, false)); // swallowed down
        assert_eq!(swallow_decision(true, false, false), (true, false)); // its up is swallowed too
        assert_eq!(swallow_decision(true, false, true), (false, false)); // passed down: up passes
        assert_eq!(swallow_decision(true, true, true), (true, true)); // repeat while suppressed
    }

    #[test]
    fn atomic_bits_set_clear() {
        let b = AtomicBits::default();
        assert!(!b.get(0x41));
        b.set(0x41, true);
        b.set(0xFF, true);
        assert!(b.get(0x41) && b.get(0xFF) && !b.get(0x42));
        b.set(0x41, false);
        assert!(!b.get(0x41));
    }

    #[test]
    fn first_swallowed_move_after_suppress_uses_the_new_pin() {
        let pts = HookPoints::new();
        let flag = AtomicBool::new(false);
        // Cursor last seen at (10, 10) before the crossing.
        assert_eq!(pts.on_move(false, (10, 10)), (10, 10));
        pts.suppress_on(&flag, (1919, 540));
        assert!(flag.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(pts.pin_point(), (1919, 540));
        // The very first swallowed move is measured from the new pin.
        assert_eq!(pts.on_move(true, (1922, 541)), (3, 1));
        assert_eq!(pts.on_move(true, (1920, 540)), (1, 0));
    }

    #[test]
    fn unsuppress_rebases_last_on_the_cursor() {
        let pts = HookPoints::new();
        let flag = AtomicBool::new(false);
        pts.suppress_on(&flag, (1919, 540));
        let _ = pts.on_move(true, (1925, 540));
        // Returned: the engine parked the cursor at the entry point and unsuppressed.
        pts.suppress_off(&flag, (1, 300));
        assert!(!flag.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(pts.on_move(false, (3, 300)), (2, 0));
    }

    #[test]
    fn warp_while_suppressed_moves_the_pin_and_baseline() {
        let pts = HookPoints::new();
        let flag = AtomicBool::new(false);
        pts.suppress_on(&flag, (1919, 540));
        pts.pin((960, 540));
        assert_eq!(pts.pin_point(), (960, 540));
        assert_eq!(pts.on_move(true, (961, 540)), (1, 0));
        // Unsuppressed warp: next move is relative to the warp target.
        pts.suppress_off(&flag, (960, 540));
        pts.rebase((100, 100));
        assert_eq!(pts.on_move(false, (101, 102)), (1, 2));
    }

    #[test]
    fn hook_delta_uses_pin_when_suppressed_else_last() {
        assert_eq!(
            hook_motion_delta(true, (100, 100), (7, 7), (103, 98)),
            (3, -2)
        );
        assert_eq!(
            hook_motion_delta(false, (100, 100), (50, 60), (53, 58)),
            (3, -2)
        );
        assert_eq!(
            hook_motion_delta(true, (100, 100), (7, 7), (100, 100)),
            (0, 0)
        );
    }
}
