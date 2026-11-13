//! Event injection via CGEventPost, tagged with NovaLink's magic user data.

use crate::capture::MAGIC;
use crate::ffi::*;
use crate::{displays, keymap};
use novalink_core::InputEvent;
use novalink_proto::{DisplayInfo, MouseButton};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Re-sync the predicted cursor from the OS after this long without injected motion.
const RESYNC_AFTER: Duration = Duration::from_millis(50);
/// How long cached display bounds stay valid.
const BOUNDS_TTL: Duration = Duration::from_secs(2);

/// Max gap and distance for counting consecutive clicks.
const MULTI_CLICK: Duration = Duration::from_millis(500);

/// Clamp `new` into the displays: unchanged if inside one, otherwise clamped to
/// the display containing `old` (or the nearest display if `old` is in a gap).
pub fn clamp_to_displays(old: (f64, f64), new: (f64, f64), ds: &[DisplayInfo]) -> (f64, f64) {
    let inside = |d: &DisplayInfo, p: (f64, f64)| {
        p.0 >= d.x && p.0 < d.x + d.width && p.1 >= d.y && p.1 < d.y + d.height
    };
    if ds.is_empty() || ds.iter().any(|d| inside(d, new)) {
        return new;
    }
    let clamp_into = |d: &DisplayInfo, p: (f64, f64)| {
        (
            p.0.clamp(d.x, d.x + d.width - 1.0),
            p.1.clamp(d.y, d.y + d.height - 1.0),
        )
    };
    let dist = |d: &DisplayInfo, p: (f64, f64)| {
        let c = clamp_into(d, p);
        (c.0 - p.0).powi(2) + (c.1 - p.1).powi(2)
    };
    let home = ds
        .iter()
        .find(|d| inside(d, old))
        .or_else(|| {
            ds.iter()
                .min_by(|a, b| dist(a, old).total_cmp(&dist(b, old)))
        })
        .expect("non-empty");
    clamp_into(home, new)
}

/// Where the next injected motion starts: our predicted position while injected
/// motion is flowing (the OS position lags behind posted events), otherwise the
/// real OS position.
pub fn base_position(
    pred: Option<(f64, f64)>,
    since_last_motion: Option<Duration>,
    os: impl FnOnce() -> (f64, f64),
) -> (f64, f64) {
    match (pred, since_last_motion) {
        (Some(p), Some(d)) if d < RESYNC_AFTER => p,
        _ => os(),
    }
}

/// Track Caps in the down set; returns whether it was already down before this event.
pub fn caps_transition(down_set: &mut Vec<u16>, down: bool) -> bool {
    let caps = novalink_proto::keys::CAPS_LOCK;
    let was = down_set.contains(&caps);
    down_set.retain(|&u| u != caps);
    if down {
        down_set.push(caps);
    }
    was
}

/// Injected flags: held modifiers plus the live Caps Lock bit (CGEventSetFlags
/// replaces all flags, so the lock state must be re-added or letters go lowercase).
pub fn compose_flags(live_lock_bits: u64, modifiers: u64) -> u64 {
    modifiers | (live_lock_bits & FLAG_ALPHA_SHIFT)
}

/// CGEventType for motion given which buttons are held.
pub fn motion_type(left: bool, right: bool, other: bool) -> u32 {
    if left {
        LEFT_MOUSE_DRAGGED
    } else if right {
        RIGHT_MOUSE_DRAGGED
    } else if other {
        OTHER_MOUSE_DRAGGED
    } else {
        MOUSE_MOVED
    }
}

/// Modifier flag mask bit for a HID usage (None if not a modifier).
pub fn flag_for_usage(usage: u16) -> Option<u64> {
    use novalink_proto::keys as k;
    match usage {
        k::LEFT_SHIFT | k::RIGHT_SHIFT => Some(FLAG_SHIFT),
        k::LEFT_CTRL | k::RIGHT_CTRL => Some(FLAG_CTRL),
        k::LEFT_ALT | k::RIGHT_ALT => Some(FLAG_ALT),
        k::LEFT_GUI | k::RIGHT_GUI => Some(FLAG_CMD),
        _ => None,
    }
}

#[derive(Default)]
struct State {
    left: bool,
    right: bool,
    other: u32, // count of other buttons held
    /// Per-side modifier usages currently down (so left+right of one kind work).
    mods_down: Vec<u16>,
    /// Non-modifier usages currently down (a repeated down is an auto-repeat).
    keys_down: Vec<u16>,
    /// Predicted cursor position after our own injected motion / warps.
    pred: Option<(f64, f64)>,
    last_motion: Option<Instant>,
    bounds: Option<(Instant, Vec<DisplayInfo>)>,
    last_click: Option<(MouseButton, Instant, i64)>,
}

impl State {
    fn flags(&self, live_lock: u64) -> u64 {
        compose_flags(
            live_lock,
            self.mods_down
                .iter()
                .filter_map(|&u| flag_for_usage(u))
                .fold(0, |a, b| a | b),
        )
    }

    fn base(&self, os: impl FnOnce() -> (f64, f64)) -> (f64, f64) {
        base_position(self.pred, self.last_motion.map(|t| t.elapsed()), os)
    }

    fn bounds(&mut self) -> &[DisplayInfo] {
        let stale = self
            .bounds
            .as_ref()
            .is_none_or(|(t, _)| t.elapsed() > BOUNDS_TTL);
        if stale {
            self.bounds = Some((Instant::now(), displays::bounds()));
        }
        &self.bounds.as_ref().expect("just set").1
    }
}

pub struct Injector {
    source: CGEventSourceRef,
    st: Mutex<State>,
}

// SAFETY: CGEventSourceRef is only used under `st`'s lock or via thread-safe CG calls.
unsafe impl Send for Injector {}
unsafe impl Sync for Injector {}

impl Injector {
    pub fn new() -> anyhow::Result<Self> {
        // SAFETY: plain CF object creation.
        let source = unsafe { CGEventSourceCreate(SOURCE_STATE_PRIVATE) };
        anyhow::ensure!(!source.is_null(), "CGEventSourceCreate failed");
        unsafe {
            CGEventSourceSetUserData(source, MAGIC);
            CGEventSourceSetLocalEventsSuppressionInterval(source, 0.0);
        }
        Ok(Self {
            source,
            st: Mutex::new(State::default()),
        })
    }

    pub fn cursor_pos(&self) -> (f64, f64) {
        // SAFETY: CGEventCreate(NULL) yields an event at the current location.
        unsafe {
            let e = CGEventCreate(std::ptr::null_mut());
            if e.is_null() {
                return (0.0, 0.0);
            }
            let p = CGEventGetLocation(e);
            CFRelease(e);
            (p.x, p.y)
        }
    }

    /// Warp the cursor. Callers that are not suppressing must re-associate the
    /// mouse afterwards (see `MacPlatform::warp`) to avoid the post-warp freeze.
    pub fn warp(&self, pos: (f64, f64)) {
        {
            let mut st = self.st.lock().unwrap();
            st.pred = Some(pos);
            st.last_motion = Some(Instant::now());
        }
        // SAFETY: plain call.
        unsafe { CGWarpMouseCursorPosition(CGPoint { x: pos.0, y: pos.1 }) };
    }

    /// Toggle the system Caps Lock state (posting a Caps key event does not).
    fn toggle_caps_lock(&self) -> anyhow::Result<()> {
        // SAFETY: standard IOHIDSystem open/get/set/close sequence; handles released on all paths.
        unsafe {
            let svc = IOServiceGetMatchingService(0, IOServiceMatching(c"IOHIDSystem".as_ptr()));
            anyhow::ensure!(svc != 0, "IOHIDSystem not found");
            let mut conn = 0;
            let rc = IOServiceOpen(svc, mach_task_self_, IOHID_PARAM_CONNECT_TYPE, &mut conn);
            IOObjectRelease(svc);
            anyhow::ensure!(rc == 0, "IOServiceOpen failed: {rc:#x}");
            let mut on = false;
            let mut rc = IOHIDGetModifierLockState(conn, IOHID_CAPS_LOCK_STATE, &mut on);
            if rc == 0 {
                rc = IOHIDSetModifierLockState(conn, IOHID_CAPS_LOCK_STATE, !on);
            }
            IOServiceClose(conn);
            anyhow::ensure!(rc == 0, "caps lock toggle failed: {rc:#x}");
        }
        Ok(())
    }

    /// Tag, flag and post an owned event, then release it.
    unsafe fn post(&self, e: CGEventRef, flags: u64) -> anyhow::Result<()> {
        anyhow::ensure!(!e.is_null(), "failed to create CGEvent");
        unsafe {
            CGEventSetIntegerValueField(e, F_SOURCE_USER_DATA, MAGIC);
            CGEventSetFlags(e, flags);
            CGEventPost(HID_EVENT_TAP, e);
            CFRelease(e);
        }
        Ok(())
    }

    pub fn inject(&self, ev: &InputEvent) -> anyhow::Result<()> {
        let mut st = self.st.lock().unwrap();
        // SAFETY: plain query.
        let live = unsafe { CGEventSourceFlagsState(SOURCE_STATE_HID_SYSTEM_I32) };
        match *ev {
            InputEvent::Motion { dx, dy } => {
                let old = st.base(|| self.cursor_pos());
                let new =
                    clamp_to_displays(old, (old.0 + dx as f64, old.1 + dy as f64), st.bounds());
                st.pred = Some(new);
                st.last_motion = Some(Instant::now());
                let ty = motion_type(st.left, st.right, st.other > 0);
                let btn = if st.left {
                    0
                } else if st.right {
                    1
                } else {
                    2
                };
                // SAFETY: source is valid; event ownership passes to `post`.
                unsafe {
                    let e = CGEventCreateMouseEvent(
                        self.source,
                        ty,
                        CGPoint { x: new.0, y: new.1 },
                        btn,
                    );
                    if !e.is_null() {
                        CGEventSetIntegerValueField(e, F_MOUSE_DELTA_X, dx.round() as i64);
                        CGEventSetIntegerValueField(e, F_MOUSE_DELTA_Y, dy.round() as i64);
                    }
                    self.post(e, st.flags(live))
                }
            }
            InputEvent::Button { button, down } => {
                let (ty, num) = match (button, down) {
                    (MouseButton::Left, true) => (LEFT_MOUSE_DOWN, 0),
                    (MouseButton::Left, false) => (LEFT_MOUSE_UP, 0),
                    (MouseButton::Right, true) => (RIGHT_MOUSE_DOWN, 1),
                    (MouseButton::Right, false) => (RIGHT_MOUSE_UP, 1),
                    (MouseButton::Middle, d) => {
                        (if d { OTHER_MOUSE_DOWN } else { OTHER_MOUSE_UP }, 2)
                    }
                    (MouseButton::Back, d) => {
                        (if d { OTHER_MOUSE_DOWN } else { OTHER_MOUSE_UP }, 3)
                    }
                    (MouseButton::Forward, d) => {
                        (if d { OTHER_MOUSE_DOWN } else { OTHER_MOUSE_UP }, 4)
                    }
                };
                let pos = st.base(|| self.cursor_pos());
                // Click count: consecutive downs of the same button within MULTI_CLICK.
                let count = if down {
                    let c = match st.last_click {
                        Some((b, t, n)) if b == button && t.elapsed() < MULTI_CLICK => n + 1,
                        _ => 1,
                    };
                    st.last_click = Some((button, Instant::now(), c));
                    c
                } else {
                    st.last_click.map(|(_, _, n)| n).unwrap_or(1)
                };
                match button {
                    MouseButton::Left => st.left = down,
                    MouseButton::Right => st.right = down,
                    _ => {
                        st.other = if down {
                            st.other + 1
                        } else {
                            st.other.saturating_sub(1)
                        }
                    }
                }
                // SAFETY: as above.
                unsafe {
                    let e = CGEventCreateMouseEvent(
                        self.source,
                        ty,
                        CGPoint { x: pos.0, y: pos.1 },
                        num,
                    );
                    if !e.is_null() {
                        CGEventSetIntegerValueField(e, F_MOUSE_CLICK_STATE, count);
                        CGEventSetIntegerValueField(e, F_MOUSE_BUTTON_NUMBER, num as i64);
                    }
                    self.post(e, st.flags(live))
                }
            }
            InputEvent::Scroll { dx, dy } => {
                // SAFETY: as above.
                unsafe {
                    let e = CGEventCreateScrollWheelEvent2(
                        self.source,
                        SCROLL_UNIT_PIXEL,
                        2,
                        dy.round() as i32,
                        dx.round() as i32,
                        0,
                    );
                    self.post(e, st.flags(live))
                }
            }
            InputEvent::Key { usage, down } => {
                let vk = keymap::to_vk(usage)
                    .ok_or_else(|| anyhow::anyhow!("no macOS keycode for HID usage {usage:#x}"))?;
                let is_mod = flag_for_usage(usage).is_some();
                if usage == novalink_proto::keys::CAPS_LOCK {
                    // Toggle only on a not-already-down transition (ignore repeats and up).
                    let was_down = caps_transition(&mut st.keys_down, down);
                    return if down && !was_down {
                        crate::capture::mark_caps_injected();
                        self.toggle_caps_lock()
                    } else {
                        Ok(())
                    };
                }
                let repeat = down && !is_mod && st.keys_down.contains(&usage);
                if !is_mod {
                    st.keys_down.retain(|&u| u != usage);
                    if down {
                        st.keys_down.push(usage);
                    }
                }
                if is_mod {
                    st.mods_down.retain(|&u| u != usage);
                    if down {
                        st.mods_down.push(usage);
                    }
                }
                // SAFETY: as above.
                unsafe {
                    let e = CGEventCreateKeyboardEvent(self.source, vk, down);
                    if is_mod && !e.is_null() {
                        CGEventSetType(e, FLAGS_CHANGED);
                    }
                    if repeat && !e.is_null() {
                        CGEventSetIntegerValueField(e, F_KEY_AUTOREPEAT, 1);
                    }
                    self.post(e, st.flags(live))
                }
            }
        }
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        // SAFETY: we own the +1 source.
        unsafe { CFRelease(self.source) };
    }
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
    fn clamp_inside_is_unchanged() {
        let ds = [d(1, 0.0, 0.0, 100.0, 100.0)];
        assert_eq!(
            clamp_to_displays((10.0, 10.0), (50.5, 20.0), &ds),
            (50.5, 20.0)
        );
    }

    #[test]
    fn clamp_to_home_display_edges() {
        let ds = [d(1, 0.0, 0.0, 100.0, 100.0)];
        assert_eq!(
            clamp_to_displays((90.0, 90.0), (150.0, -20.0), &ds),
            (99.0, 0.0)
        );
    }

    #[test]
    fn clamp_stays_on_home_display_not_neighbor() {
        let ds = [d(1, 0.0, 0.0, 100.0, 100.0), d(2, 200.0, 0.0, 100.0, 100.0)];
        // Gap between displays: stay on the one the cursor came from.
        assert_eq!(
            clamp_to_displays((90.0, 50.0), (150.0, 50.0), &ds),
            (99.0, 50.0)
        );
        // Landing inside the neighbor is allowed.
        assert_eq!(
            clamp_to_displays((90.0, 50.0), (250.0, 50.0), &ds),
            (250.0, 50.0)
        );
    }

    #[test]
    fn clamp_no_displays_passthrough() {
        assert_eq!(clamp_to_displays((0.0, 0.0), (5.0, 5.0), &[]), (5.0, 5.0));
    }

    #[test]
    fn motion_types() {
        assert_eq!(motion_type(false, false, false), MOUSE_MOVED);
        assert_eq!(motion_type(true, false, false), LEFT_MOUSE_DRAGGED);
        assert_eq!(motion_type(false, true, false), RIGHT_MOUSE_DRAGGED);
        assert_eq!(motion_type(false, false, true), OTHER_MOUSE_DRAGGED);
    }

    #[test]
    fn modifier_flags_for_usage() {
        use novalink_proto::keys as k;
        assert_eq!(flag_for_usage(k::LEFT_GUI), Some(FLAG_CMD));
        assert_eq!(flag_for_usage(k::RIGHT_SHIFT), Some(FLAG_SHIFT));
        assert_eq!(flag_for_usage(k::A), None);
    }

    #[test]
    fn both_shifts_keep_flag_until_both_up() {
        use novalink_proto::keys as k;
        let mut s = State {
            mods_down: vec![k::LEFT_SHIFT, k::RIGHT_SHIFT],
            ..Default::default()
        };
        s.mods_down.retain(|&u| u != k::LEFT_SHIFT);
        assert_eq!(s.flags(0), FLAG_SHIFT);
        s.mods_down.clear();
        assert_eq!(s.flags(0), 0);
    }

    #[test]
    fn base_uses_prediction_during_burst_and_resyncs_after_pause() {
        let os = (500.0, 500.0);
        let p = Some((10.0, 10.0));
        let ms = Duration::from_millis;
        assert_eq!(base_position(p, Some(ms(1)), || os), (10.0, 10.0));
        assert_eq!(base_position(p, Some(ms(49)), || os), (10.0, 10.0));
        assert_eq!(base_position(p, Some(ms(50)), || os), os);
        assert_eq!(base_position(None, None, || os), os);
    }

    #[test]
    fn burst_accumulates_without_os_feedback() {
        // OS position is stale (never updates); 100 +1 moves must still add up.
        let ds = [d(1, 0.0, 0.0, 1000.0, 1000.0)];
        let mut pred = None;
        for _ in 0..100 {
            let base = base_position(pred, pred.map(|_| Duration::from_millis(1)), || (0.0, 0.0));
            pred = Some(clamp_to_displays(base, (base.0 + 1.0, base.1), &ds));
        }
        assert_eq!(pred, Some((100.0, 0.0)));
    }

    #[test]
    fn caps_lock_is_not_a_flag_modifier() {
        assert_eq!(flag_for_usage(novalink_proto::keys::CAPS_LOCK), None);
    }

    #[test]
    fn compose_flags_keeps_live_lock_and_mods() {
        assert_eq!(
            compose_flags(FLAG_ALPHA_SHIFT | FLAG_CMD, FLAG_SHIFT),
            FLAG_SHIFT | FLAG_ALPHA_SHIFT
        );
        assert_eq!(compose_flags(0, FLAG_CMD), FLAG_CMD);
        assert_eq!(compose_flags(FLAG_ALPHA_SHIFT, 0), FLAG_ALPHA_SHIFT);
    }

    #[test]
    fn caps_toggles_once_per_press() {
        let mut set = Vec::new();
        assert!(!caps_transition(&mut set, true)); // first down: toggle
        assert!(caps_transition(&mut set, true)); // repeat: no toggle
        assert!(caps_transition(&mut set, false)); // up
        assert!(!caps_transition(&mut set, true)); // next press toggles again
    }
}
