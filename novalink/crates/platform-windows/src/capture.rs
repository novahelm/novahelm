//! Capture: `WH_KEYBOARD_LL` + `WH_MOUSE_LL` hooks and Raw Input on one thread that owns
//! a message-only window and a message loop.
//!
//! * Keys, buttons and wheel come from the LL hooks, which are also where physical input
//!   is swallowed while suppressing.
//! * Pointer deltas come from Raw Input (`RIDEV_INPUTSINK`); LL hooks only give absolute,
//!   clamped positions. Raw Input is unaffected by a hook swallowing the event.
//! * Hook procedures never block, allocate or log: they classify, push to the channel and
//!   return. Windows silently drops a hook whose callback is too slow.
//! * Modifier and other releases are reported before they are swallowed.
//! * Key auto-repeat downs are forwarded as plain downs (never dropped here).
//! * A 250 ms timer on this thread drives the watchdog (see `watchdog`).

use crate::logic::{
    AtomicBits, HookPoints, LLKHF_EXTENDED, PROBE_MAGIC, kbd_physical, mouse_physical,
    swallow_decision, wheel_delta, wheel_to_units, xbutton_from_data,
};
use crate::{inject, keymap, watchdog};
use novalink_core::{InputEvent, LocalEvent};
use novalink_proto::MouseButton;
use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT,
    RIDEV_INPUTSINK, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

/// Process-wide state reachable from the hook procedures (which have no user data).
pub(crate) struct Shared {
    tx: OnceLock<UnboundedSender<LocalEvent>>,
    pub suppress: AtomicBool,
    /// Set by the mouse hook when it sees the watchdog probe.
    probe_seen: AtomicBool,
    /// Set when the watchdog reinstalled the hooks; consumed by the agent.
    reinstalled: AtomicBool,
    pub thread_id: AtomicU32,
    /// `mono_us() + 1` of the last physical Raw Input packet / last hook call, per class
    /// (0 = none since the watchdog last looked). Drives passive hook-death detection.
    raw_kbd: AtomicU64,
    raw_mouse: AtomicU64,
    hook_kbd: AtomicU64,
    hook_mouse: AtomicU64,
    /// Physical downs (by VK / button index) that reached the local OS, so their ups
    /// are never swallowed while suppressing (which would leave them stuck down).
    keys_passed: AtomicBits,
    btns_passed: AtomicBits,
    /// `NOVALINK_WIN_MOTION=hook`: motion from the LL mouse hook instead of Raw Input.
    hook_motion: AtomicBool,
    /// Pinned point while suppressing, and the last hook-mode point.
    pub points: HookPoints,
}

pub(crate) static SHARED: Shared = Shared {
    tx: OnceLock::new(),
    suppress: AtomicBool::new(false),
    probe_seen: AtomicBool::new(false),
    reinstalled: AtomicBool::new(false),
    thread_id: AtomicU32::new(0),
    raw_kbd: AtomicU64::new(0),
    raw_mouse: AtomicU64::new(0),
    hook_kbd: AtomicU64::new(0),
    hook_mouse: AtomicU64::new(0),
    keys_passed: AtomicBits::new(),
    btns_passed: AtomicBits::new(),
    hook_motion: AtomicBool::new(false),
    points: HookPoints::new(),
};

fn stamp(a: &AtomicU64) {
    a.store(novalink_core::time::mono_us() + 1, Ordering::Release);
}

/// True once if the watchdog has reinstalled the hooks since the last call. The agent
/// turns this into a flight-recorder dump.
pub fn take_hook_reinstalled() -> bool {
    SHARED.reinstalled.swap(false, Ordering::AcqRel)
}

fn cursor() -> (f64, f64) {
    let mut p = POINT::default();
    // SAFETY: valid out-pointer. On failure p stays (0,0).
    let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut p) };
    (p.x as f64, p.y as f64)
}

fn emit(event: InputEvent, physical: bool, pos: (f64, f64)) {
    if let Some(tx) = SHARED.tx.get() {
        let _ = tx.send(LocalEvent {
            event,
            physical,
            t_us: novalink_core::time::mono_us(),
            pos,
        });
    }
}

const SWALLOW: LRESULT = LRESULT(1);

unsafe extern "system" fn kbd_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for HC_ACTION, lp points at a KBDLLHOOKSTRUCT valid for this call.
        let k = unsafe { &*(lp.0 as *const KBDLLHOOKSTRUCT) };
        let flags = k.flags.0;
        stamp(&SHARED.hook_kbd);
        let physical = kbd_physical(flags, k.dwExtraInfo as u64);
        let down = matches!(wp.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        let usage = keymap::usage_from_special_vk(k.vkCode as u16)
            .or_else(|| keymap::to_usage(k.scanCode as u16, flags & LLKHF_EXTENDED != 0));
        if let Some(usage) = usage {
            emit(InputEvent::Key { usage, down }, physical, cursor());
        }
        if physical {
            let vk = k.vkCode as u8;
            let (swallow, after) = swallow_decision(
                SHARED.suppress.load(Ordering::Acquire),
                down,
                SHARED.keys_passed.get(vk),
            );
            SHARED.keys_passed.set(vk, after);
            if swallow {
                return SWALLOW;
            }
        }
    }
    // SAFETY: plain pass-through.
    unsafe { CallNextHookEx(None, code, wp, lp) }
}

unsafe extern "system" fn mouse_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for HC_ACTION, lp points at an MSLLHOOKSTRUCT valid for this call.
        let m = unsafe { &*(lp.0 as *const MSLLHOOKSTRUCT) };
        let extra = m.dwExtraInfo as u64;
        if extra == PROBE_MAGIC {
            SHARED.probe_seen.store(true, Ordering::Release);
        } else {
            stamp(&SHARED.hook_mouse);
            let physical = mouse_physical(m.flags, extra);
            let pos = (m.pt.x as f64, m.pt.y as f64);
            let button = |b, down| Some(InputEvent::Button { button: b, down });
            let suppressed = SHARED.suppress.load(Ordering::Acquire);
            let mut ev = match wp.0 as u32 {
                WM_LBUTTONDOWN => button(MouseButton::Left, true),
                WM_LBUTTONUP => button(MouseButton::Left, false),
                WM_RBUTTONDOWN => button(MouseButton::Right, true),
                WM_RBUTTONUP => button(MouseButton::Right, false),
                WM_MBUTTONDOWN => button(MouseButton::Middle, true),
                WM_MBUTTONUP => button(MouseButton::Middle, false),
                WM_XBUTTONDOWN => xbutton_from_data(m.mouseData).and_then(|b| button(b, true)),
                WM_XBUTTONUP => xbutton_from_data(m.mouseData).and_then(|b| button(b, false)),
                WM_MOUSEWHEEL => Some(InputEvent::Scroll {
                    dx: 0.0,
                    dy: wheel_to_units(wheel_delta(m.mouseData)),
                }),
                WM_MOUSEHWHEEL => Some(InputEvent::Scroll {
                    dx: wheel_to_units(wheel_delta(m.mouseData)),
                    dy: 0.0,
                }),
                // Motion normally comes from Raw Input; the fallback mode uses the hook.
                WM_MOUSEMOVE if SHARED.hook_motion.load(Ordering::Relaxed) => {
                    let (dx, dy) = SHARED.points.on_move(suppressed, (m.pt.x, m.pt.y));
                    (dx != 0 || dy != 0).then_some(InputEvent::Motion {
                        dx: dx as f32,
                        dy: dy as f32,
                    })
                }
                _ => None,
            };
            if let Some(e) = ev.take() {
                emit(e, physical, pos);
            }
            if physical {
                let slot = match wp.0 as u32 {
                    WM_LBUTTONDOWN | WM_LBUTTONUP => Some((0u8, wp.0 as u32 == WM_LBUTTONDOWN)),
                    WM_RBUTTONDOWN | WM_RBUTTONUP => Some((1, wp.0 as u32 == WM_RBUTTONDOWN)),
                    WM_MBUTTONDOWN | WM_MBUTTONUP => Some((2, wp.0 as u32 == WM_MBUTTONDOWN)),
                    WM_XBUTTONDOWN | WM_XBUTTONUP => xbutton_from_data(m.mouseData).map(|b| {
                        (
                            if b == MouseButton::Back { 3 } else { 4 },
                            wp.0 as u32 == WM_XBUTTONDOWN,
                        )
                    }),
                    _ => None,
                };
                let swallow = match slot {
                    Some((i, down)) => {
                        let (s, after) =
                            swallow_decision(suppressed, down, SHARED.btns_passed.get(i));
                        SHARED.btns_passed.set(i, after);
                        s
                    }
                    None => suppressed,
                };
                if swallow {
                    return SWALLOW;
                }
            }
        }
    }
    // SAFETY: plain pass-through.
    unsafe { CallNextHookEx(None, code, wp, lp) }
}

fn on_raw_input(lp: LPARAM) {
    let mut buf = [0u64; 16]; // 128 bytes, 8-aligned; RAWINPUT is 48 on x64
    let mut size = std::mem::size_of_val(&buf) as u32;
    // SAFETY: buf is valid for `size` bytes; the header size is the documented constant.
    let n = unsafe {
        GetRawInputData(
            HRAWINPUT(lp.0 as *mut c_void),
            RID_INPUT,
            Some(buf.as_mut_ptr() as *mut c_void),
            &mut size,
            std::mem::size_of::<RAWINPUTHEADER>() as u32,
        )
    };
    if n == u32::MAX || (n as usize) < std::mem::size_of::<RAWINPUTHEADER>() {
        return;
    }
    // SAFETY: GetRawInputData filled a RAWINPUT into the 8-aligned buffer.
    let raw = unsafe { &*(buf.as_ptr() as *const RAWINPUT) };
    if raw.header.dwType == RIM_TYPEKEYBOARD.0 {
        if !raw.header.hDevice.0.is_null() {
            stamp(&SHARED.raw_kbd);
        }
        return;
    }
    if raw.header.dwType != RIM_TYPEMOUSE.0 {
        return;
    }
    // SAFETY: dwType says the union holds a RAWMOUSE.
    let m = unsafe { raw.data.mouse };
    const MOUSE_MOVE_ABSOLUTE: u16 = 1;
    if m.usFlags.0 & MOUSE_MOVE_ABSOLUTE != 0 {
        return; // tablets / remote sessions report absolute
    }
    // Injected input (ours or anyone's) has no source device.
    let physical = !raw.header.hDevice.0.is_null();
    if physical {
        stamp(&SHARED.raw_mouse);
    }
    if (m.lLastX == 0 && m.lLastY == 0) || SHARED.hook_motion.load(Ordering::Relaxed) {
        return; // zero deltas carry nothing; hook mode takes motion from the hook
    }
    emit(
        InputEvent::Motion {
            dx: m.lLastX as f32,
            dy: m.lLastY as f32,
        },
        physical,
        cursor(),
    );
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_INPUT {
        on_raw_input(lp);
    }
    // SAFETY: default handling (also required after WM_INPUT).
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

struct Hooks {
    kbd: HHOOK,
    mouse: HHOOK,
}

impl Hooks {
    fn install() -> anyhow::Result<Self> {
        // SAFETY: standard LL hook installation; the procs are `extern "system"` and static.
        unsafe {
            let hmod = GetModuleHandleW(None)?;
            let kbd = SetWindowsHookExW(WH_KEYBOARD_LL, Some(kbd_proc), Some(hmod.into()), 0)?;
            match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), Some(hmod.into()), 0) {
                Ok(mouse) => Ok(Self { kbd, mouse }),
                Err(e) => {
                    let _ = UnhookWindowsHookEx(kbd);
                    Err(e.into())
                }
            }
        }
    }

    fn remove(self) {
        // SAFETY: handles came from SetWindowsHookExW.
        unsafe {
            let _ = UnhookWindowsHookEx(self.kbd);
            let _ = UnhookWindowsHookEx(self.mouse);
        }
    }
}

const WATCHDOG_TIMER: usize = 1;

fn run(ready: std::sync::mpsc::Sender<anyhow::Result<()>>) {
    let setup = || -> anyhow::Result<(HWND, Hooks)> {
        // SAFETY: standard window-class + message-only window creation on this thread.
        unsafe {
            let hinst = GetModuleHandleW(None)?;
            let class = w!("NovaLinkCapture");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinst.into(),
                lpszClassName: class,
                ..Default::default()
            };
            // Registering twice (a retry in the same process) fails harmlessly.
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                w!(""),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(hinst.into()),
                None,
            )?;
            let dev = |usage| RAWINPUTDEVICE {
                usUsagePage: 1,
                usUsage: usage,
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: hwnd,
            };
            // Mouse (2) for deltas + liveness; keyboard (6) for liveness only.
            RegisterRawInputDevices(
                &[dev(2), dev(6)],
                std::mem::size_of::<RAWINPUTDEVICE>() as u32,
            )?;
            let hooks = Hooks::install()?;
            SetTimer(
                Some(hwnd),
                WATCHDOG_TIMER,
                watchdog::PROBE_INTERVAL.as_millis() as u32,
                None,
            );
            Ok((hwnd, hooks))
        }
    };
    let (hwnd, hooks) = match setup() {
        Ok(v) => v,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    // SAFETY: plain query.
    SHARED
        .thread_id
        .store(unsafe { GetCurrentThreadId() }, Ordering::Release);
    let _ = ready.send(Ok(()));

    let mut hooks = Some(hooks);
    let mut wd = Watch {
        probe: watchdog::State::default(),
        kbd: watchdog::Passive::default(),
        mouse: watchdog::Passive::default(),
    };
    let mut msg = MSG::default();
    // SAFETY: standard message loop; WM_QUIT (posted by `stop`) ends it.
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        if msg.message == WM_TIMER && msg.wParam.0 == WATCHDOG_TIMER {
            watchdog_tick(&mut wd, &mut hooks);
            continue;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    if let Some(h) = hooks {
        h.remove();
    }
    // SAFETY: hwnd is ours.
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

struct Watch {
    probe: watchdog::State,
    kbd: watchdog::Passive,
    mouse: watchdog::Passive,
}

fn take(a: &AtomicU64) -> Option<u64> {
    a.swap(0, Ordering::AcqRel).checked_sub(1)
}

fn watchdog_tick(w: &mut Watch, hooks: &mut Option<Hooks>) {
    let now = Instant::now();
    let now_us = novalink_core::time::mono_us();
    if SHARED.probe_seen.swap(false, Ordering::AcqRel) {
        w.probe.probe_seen(now);
    }
    if let Some(t) = take(&SHARED.raw_kbd) {
        w.kbd.raw_seen(t);
    }
    if let Some(t) = take(&SHARED.hook_kbd) {
        w.kbd.hook_seen(t);
    }
    if let Some(t) = take(&SHARED.raw_mouse) {
        w.mouse.raw_seen(t);
    }
    if let Some(t) = take(&SHARED.hook_mouse) {
        w.mouse.hook_seen(t);
    }
    let passive_kbd = w.kbd.needs_reinstall(now_us);
    let passive_mouse = w.mouse.needs_reinstall(now_us);
    if w.probe.needs_reinstall(now) || passive_kbd || passive_mouse || hooks.is_none() {
        if let Some(h) = hooks.take() {
            h.remove();
        }
        match Hooks::install() {
            Ok(h) => {
                *hooks = Some(h);
                SHARED.reinstalled.store(true, Ordering::Release);
                tracing::warn!(
                    event = "hook_reinstalled",
                    passive_kbd,
                    passive_mouse,
                    probe_timeout = w.probe.has_outstanding()
                );
            }
            Err(e) => tracing::error!(event = "hook_reinstall_failed", error = %e),
        }
        w.probe.reset();
        w.kbd.reset(now_us);
        w.mouse.reset(now_us);
        SHARED.probe_seen.store(false, Ordering::Release);
    }
    // Probes are injected input and would keep the PC awake, so only send them while
    // suppressing; otherwise liveness is judged passively from Raw Input.
    if SHARED.suppress.load(Ordering::Acquire)
        && !w.probe.has_outstanding()
        && hooks.is_some()
        && inject::send_probe().is_ok()
    {
        w.probe.probe_sent(now);
    }
}

/// Install the hooks and start the capture thread. Errors if already started in this
/// process or if the hooks / raw input cannot be set up.
pub fn start(tx: UnboundedSender<LocalEvent>) -> anyhow::Result<()> {
    SHARED
        .tx
        .set(tx)
        .map_err(|_| anyhow::anyhow!("capture already started"))?;
    let hook_mode =
        std::env::var("NOVALINK_WIN_MOTION").is_ok_and(|v| v.eq_ignore_ascii_case("hook"));
    SHARED.hook_motion.store(hook_mode, Ordering::Release);
    let (rtx, rrx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("novalink-hooks".into())
        .spawn(move || run(rtx))?;
    rrx.recv()
        .map_err(|_| anyhow::anyhow!("capture thread died"))?
}

/// Ask the capture thread to unhook and exit.
pub fn stop() {
    let id = SHARED.thread_id.swap(0, Ordering::AcqRel);
    if id != 0 {
        // SAFETY: posting WM_QUIT to our own thread id.
        unsafe {
            let _ = PostThreadMessageW(id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }
}
