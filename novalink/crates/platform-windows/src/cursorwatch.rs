//! The Windows side of `cursorlog`: a thread samples the pointer and logs when it hides and
//! shows while a remote moves it, with the app under it, and when it hides while away.

use crate::cursorlog::{Away, Note, SAMPLE_EVERY, Watch};
use crate::inject;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use windows::Win32::Foundation::{CloseHandle, HWND, POINT};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSORINFO, GetClassNameW, GetCursorInfo, GetForegroundWindow, GetSystemMetrics,
    GetWindowThreadProcessId, SM_MOUSEPRESENT, SPI_GETMOUSEVANISH,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW, WindowFromPoint,
};
use windows::core::PWSTR;

/// Whether a remote is moving the pointer. One thread watches for the life of the
/// process: closely during a visit, and between visits for the moment the cursor hides.
#[derive(Default)]
pub struct CursorWatcher {
    remote: Arc<AtomicBool>,
    started: AtomicBool,
}

impl CursorWatcher {
    pub fn set(&self, on: bool) {
        self.remote.store(on, Ordering::Release);
        if on && !self.started.swap(true, Ordering::AcqRel) {
            let remote = self.remote.clone();
            if let Err(e) = std::thread::Builder::new()
                .name("cursor-watch".into())
                .spawn(move || run(&remote))
            {
                tracing::warn!(event = "cursor_watch_failed", error = %e);
                self.started.store(false, Ordering::Release);
            }
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

/// Windows' "Hide pointer while typing" (Mouse properties, Pointer Options).
fn mouse_vanish() -> Option<bool> {
    let mut on = 0i32;
    // SAFETY: SPI_GETMOUSEVANISH writes a BOOL (i32) to the pointer, which outlives the call.
    unsafe {
        SystemParametersInfoW(
            SPI_GETMOUSEVANISH,
            0,
            Some(&mut on as *mut i32 as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .ok()?;
    Some(on != 0)
}

fn run(remote: &AtomicBool) {
    let start = Instant::now();
    let ms = || start.elapsed().as_millis() as u64;
    // The visit in progress (and where the cursor last was), or how it fares while away.
    let mut visit: Option<(Watch, POINT)> = None;
    let mut away: Option<Away> = None;
    loop {
        let on = remote.load(Ordering::Acquire);
        let s = sample();
        let flags = s.as_ref().map(|s| s.flags);
        let at = s.as_ref().map_or(POINT::default(), |s| s.at);
        if on && visit.is_none() {
            // SAFETY: plain call.
            let mouse_present = unsafe { GetSystemMetrics(SM_MOUSEPRESENT) } != 0;
            tracing::info!(
                event = "remote_pointer_on",
                mouse_present,
                hide_while_typing = mouse_vanish(),
                flags,
                x = at.x,
                y = at.y,
                under = %under(at),
                foreground = %foreground(),
            );
            visit = Some((Watch::default(), at));
            away = None;
        } else if !on && let Some((watch, last_at)) = visit.take() {
            if let Some((hidden_ms, moves)) = watch.end(ms(), inject::moves()) {
                tracing::warn!(
                    event = "cursor_hidden_at_leave",
                    hidden_ms,
                    moves,
                    under = %under(last_at),
                );
            }
            tracing::info!(event = "remote_pointer_off", flags);
            away = flags.map(|f| Away::new(ms(), f));
        }
        match (&mut visit, &s) {
            (Some((watch, last_at)), Some(s)) => {
                *last_at = s.at;
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
            (None, Some(s)) => {
                if let Some(after_ms) = away.as_mut().and_then(|a| a.sample(ms(), s.flags)) {
                    tracing::info!(
                        event = "cursor_hidden_while_away",
                        after_ms,
                        flags = s.flags,
                        x = s.at.x,
                        y = s.at.y,
                        under = %under(s.at),
                        foreground = %foreground(),
                        hide_while_typing = mouse_vanish(),
                    );
                }
            }
            _ => {}
        }
        std::thread::sleep(SAMPLE_EVERY);
    }
}
