//! Small OS helpers: message boxes, the Run registry value, the off-marker, Explorer.

use novalink_tray::model::os_failure_text;
use std::path::PathBuf;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ, RegCloseKey, RegDeleteValueW,
    RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    IDYES, MB_DEFBUTTON2, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
    MB_YESNO, MESSAGEBOX_STYLE, MessageBoxW, SW_SHOWNORMAL,
};
use windows::core::PCWSTR;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn message_box(title: &str, text: &str, style: MESSAGEBOX_STYLE) -> bool {
    let (t, c) = (wide(title), wide(text));
    // SAFETY: both buffers are NUL-terminated and outlive the call.
    let r = unsafe {
        MessageBoxW(
            None,
            PCWSTR(c.as_ptr()),
            PCWSTR(t.as_ptr()),
            style | MB_TOPMOST | MB_SETFOREGROUND,
        )
    };
    r == IDYES
}

/// Blocks until dismissed.
pub fn info(title: &str, text: &str) {
    message_box(title, text, MB_OK | MB_ICONINFORMATION);
}

/// Blocks until answered; true for Yes.
pub fn ask(title: &str, text: &str) -> bool {
    message_box(title, text, MB_YESNO | MB_ICONQUESTION)
}

/// Like `ask`, but Enter answers No: a pairing prompt must refuse unless the user chooses Yes.
pub fn ask_default_no(title: &str, text: &str) -> bool {
    message_box(title, text, MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2)
}

fn off_marker() -> PathBuf {
    novalink_agent::paths::config_dir().join("off")
}

pub fn is_off() -> bool {
    off_marker().exists()
}

pub fn set_off(off: bool) -> Result<(), String> {
    let p = off_marker();
    let r = if off {
        std::fs::write(&p, b"")
    } else {
        match std::fs::remove_file(&p) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    };
    r.map_err(|e| os_failure_text("save the Off setting", &format!("{e} ({})", p.display())))
}

pub fn open_logs() -> Result<(), String> {
    let dir = wide(&novalink_agent::paths::log_dir().to_string_lossy());
    let verb = wide("open");
    // SAFETY: the strings are NUL-terminated and outlive the call.
    let r = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(dir.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecute reports success with a value above 32.
    if r.0 as isize > 32 {
        Ok(())
    } else {
        Err(os_failure_text(
            "open the logs folder",
            &format!("Windows error {}", r.0 as isize),
        ))
    }
}

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "NovaLink";

fn with_run_key<T>(f: impl FnOnce(HKEY) -> T) -> Option<T> {
    let sub = wide(RUN_KEY);
    let mut key = HKEY::default();
    // SAFETY: `sub` is NUL-terminated; `key` is closed below.
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            None,
            KEY_READ | KEY_WRITE,
            &mut key,
        )
    };
    if opened != ERROR_SUCCESS {
        return None;
    }
    let out = f(key);
    // SAFETY: `key` was opened above.
    unsafe {
        let _ = RegCloseKey(key);
    }
    Some(out)
}

pub fn start_at_login() -> bool {
    let name = wide(RUN_VALUE);
    with_run_key(|key| {
        // SAFETY: `name` is NUL-terminated; no output buffers are requested.
        let r = unsafe { RegQueryValueExW(key, PCWSTR(name.as_ptr()), None, None, None, None) };
        r == ERROR_SUCCESS
    })
    .unwrap_or(false)
}

pub fn set_start_at_login(on: bool) -> Result<(), String> {
    let fail = |why: String| os_failure_text("change Start at login", &why);
    let name = wide(RUN_VALUE);
    let value = if on {
        let exe = std::env::current_exe().map_err(|e| fail(e.to_string()))?;
        let v = wide(&format!("\"{}\"", exe.display()));
        Some(v.iter().flat_map(|u| u.to_le_bytes()).collect::<Vec<u8>>())
    } else {
        None
    };
    with_run_key(|key| {
        // SAFETY: `name` and the value buffer are NUL-terminated and outlive the calls.
        let r = unsafe {
            match &value {
                Some(bytes) => {
                    RegSetValueExW(key, PCWSTR(name.as_ptr()), None, REG_SZ, Some(bytes))
                }
                None => RegDeleteValueW(key, PCWSTR(name.as_ptr())),
            }
        };
        // Removing a value that isn't there is already the state asked for.
        if r == ERROR_SUCCESS || (value.is_none() && r == ERROR_FILE_NOT_FOUND) {
            Ok(())
        } else {
            Err(fail(format!("Windows error {}", r.0)))
        }
    })
    .unwrap_or_else(|| Err(fail("couldn't open the Windows startup list".into())))
}
