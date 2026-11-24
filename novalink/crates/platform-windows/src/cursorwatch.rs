//! The Windows side of `cursorlog`: while a remote moves the pointer, a thread samples it
//! and logs when it hides and shows, with the app under it.

use crate::cursorlog::{Note, SAMPLE_EVERY, Watch};
use crate::inject;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use windows::Win32::Foundation::{CloseHandle, HWND, POINT};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSORINFO, GetClassNameW, GetCursorInfo, GetForegroundWindow, GetSystemMetrics,
    GetWindowThreadProcessId, SM_MOUSEPRESENT, WindowFromPoint,
};
use windows::core::PWSTR;

/// The stop flag of the running watch thread, if any.
#[derive(Default)]
pub struct CursorWatcher {
    stop: Mutex<Option<Arc<AtomicBool>>>,
}

impl CursorWatcher {
    pub fn set(&self, on: bool) {
        let mut cur = self.stop.lock().unwrap_or_else(|e| e.into_inner());
        if on {
            if cur.is_none() {
                let stop = Arc::new(AtomicBool::new(false));
                let flag = stop.clone();
                match std::thread::Builder::new()
                    .name("cursor-watch".into())
                    .spawn(move || run(&flag))
                {
                    Ok(_) => *cur = Some(stop),
                    Err(e) => tracing::warn!(event = "cursor_watch_failed", error = %e),
                }
            }
        } else if let Some(stop) = cur.take() {
            stop.store(true, Ordering::Release);
        }
    }
}

struct Sample {
    flags: u32,
    at: POINT,
}

fn sample() -> Option<Sample> {
    let mut ci = CURSORINFO {
        cbSize: size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: ci is a CURSORINFO with cbSize set.
    unsafe { GetCursorInfo(&mut ci) }.ok()?;
    Some(Sample {
        flags: ci.flags.0,
        at: ci.ptScreenPos,
    })
}

/// `name.exe (WindowClass)` for the window's process, or what could be read of it.
fn describe(hwnd: HWND) -> String {
    if hwnd.is_invalid() {
        return "none".into();
    }
    let mut class = [0u16; 128];
    // SAFETY: the buffer is ours; the length comes from the slice.
    let n = unsafe { GetClassNameW(hwnd, &mut class) }.max(0) as usize;
    let class = String::from_utf16_lossy(&class[..n]);
    let mut pid = 0u32;
    // SAFETY: valid out-pointer.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    format!(
        "{} ({class})",
        process_name(pid).unwrap_or_else(|| format!("pid {pid}"))
    )
}

fn process_name(pid: u32) -> Option<String> {
    // SAFETY: the handle is closed below; the buffer and its length describe `buf`.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let r =
            QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        r.ok()?;
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        Some(path.rsplit('\\').next().unwrap_or(&path).to_string())
    }
}

fn under(at: POINT) -> String {
    // SAFETY: plain call.
    describe(unsafe { WindowFromPoint(at) })
}

fn foreground() -> String {
    // SAFETY: plain call.
    describe(unsafe { GetForegroundWindow() })
}

fn run(stop: &AtomicBool) {
    let start = Instant::now();
    let ms = || start.elapsed().as_millis() as u64;
    // SAFETY: plain call.
    let mouse_present = unsafe { GetSystemMetrics(SM_MOUSEPRESENT) } != 0;
    let first = sample();
    tracing::info!(
        event = "remote_pointer_on",
        mouse_present,
        flags = first.as_ref().map(|s| s.flags),
        foreground = %foreground(),
    );
    let mut watch = Watch::default();
    let mut last_at = POINT::default();
    while !stop.load(Ordering::Acquire) {
        if let Some(s) = sample() {
            last_at = s.at;
            match watch.sample(ms(), s.flags, inject::moves()) {
                Some(Note::Hidden { flags }) => tracing::info!(
                    event = "cursor_hidden",
                    flags,
                    x = s.at.x,
                    y = s.at.y,
                    under = %under(s.at),
                    foreground = %foreground(),
                ),
                Some(Note::Shown { hidden_ms, moves }) => tracing::info!(
                    event = "cursor_shown",
                    hidden_ms,
                    moves,
                    under = %under(s.at),
                ),
                Some(Note::Quiet) => tracing::info!(event = "cursor_log_quiet"),
                None => {}
            }
        }
        std::thread::sleep(SAMPLE_EVERY);
    }
    if let Some((hidden_ms, moves)) = watch.end(ms(), inject::moves()) {
        tracing::warn!(
            event = "cursor_hidden_at_leave",
            hidden_ms,
            moves,
            under = %under(last_at),
        );
    }
    tracing::info!(event = "remote_pointer_off");
}
