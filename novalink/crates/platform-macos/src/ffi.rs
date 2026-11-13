//! Raw CoreGraphics / ApplicationServices / IOKit declarations (only what we use).
#![allow(non_upper_case_globals, dead_code)]

use std::ffi::c_void;

pub type CGEventRef = *mut c_void;
pub type CGEventSourceRef = *mut c_void;
pub type CFMachPortRef = *mut c_void;
pub type CGDisplayModeRef = *mut c_void;
pub type CGError = i32;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CGPoint {
    pub x: f64,
    pub y: f64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CGSize {
    pub width: f64,
    pub height: f64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CGRect {
    pub origin: CGPoint,
    pub size: CGSize,
}

pub type CGEventTapCallBack =
    extern "C" fn(proxy: *mut c_void, ty: u32, event: CGEventRef, info: *mut c_void) -> CGEventRef;

// CGEventType
pub const LEFT_MOUSE_DOWN: u32 = 1;
pub const LEFT_MOUSE_UP: u32 = 2;
pub const RIGHT_MOUSE_DOWN: u32 = 3;
pub const RIGHT_MOUSE_UP: u32 = 4;
pub const MOUSE_MOVED: u32 = 5;
pub const LEFT_MOUSE_DRAGGED: u32 = 6;
pub const RIGHT_MOUSE_DRAGGED: u32 = 7;
pub const KEY_DOWN: u32 = 10;
pub const KEY_UP: u32 = 11;
pub const FLAGS_CHANGED: u32 = 12;
pub const SCROLL_WHEEL: u32 = 22;
pub const OTHER_MOUSE_DOWN: u32 = 25;
pub const OTHER_MOUSE_UP: u32 = 26;
pub const OTHER_MOUSE_DRAGGED: u32 = 27;
pub const TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
pub const TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;

// CGEventField
pub const F_MOUSE_CLICK_STATE: u32 = 1;
pub const F_MOUSE_BUTTON_NUMBER: u32 = 3;
pub const F_MOUSE_DELTA_X: u32 = 4;
pub const F_MOUSE_DELTA_Y: u32 = 5;
pub const F_KEY_AUTOREPEAT: u32 = 8;
pub const F_KEYCODE: u32 = 9;
pub const F_SCROLL_POINT_DELTA_1: u32 = 96;
pub const F_SCROLL_POINT_DELTA_2: u32 = 97;
pub const F_SOURCE_USER_DATA: u32 = 42;
pub const F_SOURCE_STATE_ID: u32 = 45;

// CGEventSourceStateID
pub const SOURCE_STATE_PRIVATE: i32 = -1;
pub const SOURCE_STATE_HID_SYSTEM: i64 = 1;
pub const SOURCE_STATE_HID_SYSTEM_I32: i32 = 1;

pub const HID_EVENT_TAP: u32 = 0; // kCGHIDEventTap
pub const HEAD_INSERT_EVENT_TAP: u32 = 0;
pub const TAP_OPTION_DEFAULT: u32 = 0;
pub const SCROLL_UNIT_PIXEL: u32 = 0;

// Device-dependent modifier flag bits (IOKit NX_DEVICE*KEYMASK)
pub const FLAG_ALPHA_SHIFT: u64 = 0x0001_0000;
pub const FLAG_SHIFT: u64 = 0x0002_0000;
pub const FLAG_CTRL: u64 = 0x0004_0000;
pub const FLAG_ALT: u64 = 0x0008_0000;
pub const FLAG_CMD: u64 = 0x0010_0000;

pub const IOHID_REQUEST_LISTEN_EVENT: u32 = 1;
pub const IOHID_ACCESS_GRANTED: u32 = 0;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    pub fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: CGEventTapCallBack,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    pub fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    pub fn CGEventGetIntegerValueField(ev: CGEventRef, field: u32) -> i64;
    pub fn CGEventSetIntegerValueField(ev: CGEventRef, field: u32, v: i64);
    pub fn CGEventGetLocation(ev: CGEventRef) -> CGPoint;
    pub fn CGEventGetFlags(ev: CGEventRef) -> u64;
    pub fn CGEventSetFlags(ev: CGEventRef, flags: u64);
    pub fn CGEventSetType(ev: CGEventRef, ty: u32);
    pub fn CGEventCreate(source: CGEventSourceRef) -> CGEventRef;
    pub fn CGEventCreateMouseEvent(
        source: CGEventSourceRef,
        ty: u32,
        pos: CGPoint,
        button: u32,
    ) -> CGEventRef;
    pub fn CGEventCreateKeyboardEvent(source: CGEventSourceRef, vk: u16, down: bool) -> CGEventRef;
    pub fn CGEventCreateScrollWheelEvent2(
        source: CGEventSourceRef,
        units: u32,
        wheel_count: u32,
        w1: i32,
        w2: i32,
        w3: i32,
    ) -> CGEventRef;
    pub fn CGEventPost(tap: u32, ev: CGEventRef);
    pub fn CGEventSourceCreate(state: i32) -> CGEventSourceRef;
    pub fn CGEventSourceSetUserData(src: CGEventSourceRef, v: i64);
    pub fn CGEventSourceSetLocalEventsSuppressionInterval(src: CGEventSourceRef, secs: f64);
    pub fn CGAssociateMouseAndMouseCursorPosition(connected: u32) -> CGError;
    pub fn CGWarpMouseCursorPosition(p: CGPoint) -> CGError;
    pub fn CGDisplayHideCursor(display: u32) -> CGError;
    pub fn CGDisplayShowCursor(display: u32) -> CGError;
    pub fn CGMainDisplayID() -> u32;
    pub fn CGGetActiveDisplayList(max: u32, ids: *mut u32, count: *mut u32) -> CGError;
    pub fn CGDisplayBounds(id: u32) -> CGRect;
    pub fn CGDisplayCopyDisplayMode(id: u32) -> CGDisplayModeRef;
    pub fn CGDisplayModeGetPixelWidth(mode: CGDisplayModeRef) -> usize;
    pub fn CGDisplayModeRelease(mode: CGDisplayModeRef);
    pub fn CGEventSourceFlagsState(state: i32) -> u64;
    pub fn AXIsProcessTrusted() -> bool;
    /// `options` is a CFDictionary; `kAXTrustedCheckOptionPrompt: true` shows the prompt.
    pub fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
    pub static kAXTrustedCheckOptionPrompt: *const c_void;
    /// Shows the Input Monitoring prompt (once) and lists the process in Settings.
    pub fn CGRequestListenEventAccess() -> bool;
    // Private but long-stable SkyLight/CGS symbols (exported via CoreGraphics).
    pub fn _CGSDefaultConnection() -> i32;
    pub fn CGSSetConnectionProperty(
        cid: i32,
        target: i32,
        key: *const c_void,
        value: *const c_void,
    ) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub fn CFRelease(cf: *const c_void);
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    pub fn IOHIDCheckAccess(request: u32) -> u32;
}

pub type IoObject = u32;
pub const IOHID_PARAM_CONNECT_TYPE: u32 = 1;
pub const IOHID_CAPS_LOCK_STATE: u32 = 1;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    pub fn IOServiceMatching(name: *const std::ffi::c_char) -> *mut c_void;
    pub fn IOServiceGetMatchingService(main_port: u32, matching: *mut c_void) -> IoObject;
    pub fn IOServiceOpen(service: IoObject, owning_task: u32, ty: u32, conn: *mut IoObject) -> i32;
    pub fn IOServiceClose(conn: IoObject) -> i32;
    pub fn IOObjectRelease(obj: IoObject) -> i32;
    pub fn IOHIDGetModifierLockState(conn: IoObject, selector: u32, state: *mut bool) -> i32;
    pub fn IOHIDSetModifierLockState(conn: IoObject, selector: u32, state: bool) -> i32;
}

unsafe extern "C" {
    pub static mach_task_self_: u32;
}
