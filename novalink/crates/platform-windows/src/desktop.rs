//! Follows whichever desktop currently has input — the normal `Default` desktop, or a
//! secure desktop such as Winlogon's (lock screen, Ctrl+Alt+Del, a UAC consent prompt) —
//! and injects there.
//!
//! The thread that calls [`DesktopInjector::inject`] must own no windows or hooks: Win32
//! reassigns a thread's desktop with [`SetThreadDesktop`], and a thread that already owns
//! windows or hooks on its old desktop cannot be moved.

use crate::inject::Injector;
use crate::secure::{self, DesktopKind};
use novalink_core::InputEvent;
use std::sync::Mutex;
use windows::Win32::Foundation::{HANDLE, POINT};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CREATEMENU, DESKTOP_CREATEWINDOW,
    DESKTOP_HOOKCONTROL, DESKTOP_JOURNALPLAYBACK, DESKTOP_JOURNALRECORD, DESKTOP_READOBJECTS,
    DESKTOP_SWITCHDESKTOP, DESKTOP_WRITEOBJECTS, GetUserObjectInformationW, OpenInputDesktop,
    SetThreadDesktop, UOI_NAME,
};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

/// Rights a thread needs on a desktop to create windows/hooks and inject input there.
const DESKTOP_RIGHTS: DESKTOP_ACCESS_FLAGS = DESKTOP_ACCESS_FLAGS(
    DESKTOP_CREATEMENU.0
        | DESKTOP_CREATEWINDOW.0
        | DESKTOP_HOOKCONTROL.0
        | DESKTOP_JOURNALPLAYBACK.0
        | DESKTOP_JOURNALRECORD.0
        | DESKTOP_READOBJECTS.0
        | DESKTOP_SWITCHDESKTOP.0
        | DESKTOP_WRITEOBJECTS.0,
);

/// Switches the calling thread to whichever desktop has input, then injects with
/// [`Injector`].
pub struct DesktopInjector {
    injector: Injector,
    /// Name of the desktop the calling thread is currently assigned to, if any.
    assigned: Mutex<Option<String>>,
}

impl DesktopInjector {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            injector: Injector::default(),
            assigned: Mutex::new(None),
        })
    }

    /// Name of the desktop that currently has input, e.g. `"Default"` or `"Winlogon"`.
    fn input_desktop_name() -> anyhow::Result<String> {
        // SAFETY: `OpenInputDesktop` is called with rights the thread will need if it
        // switches onto this desktop; the handle is always closed below, after the name
        // is read and (if needed) the thread has switched onto it.
        let desk = unsafe { OpenInputDesktop(Default::default(), false, DESKTOP_RIGHTS) }?;

        let mut buf = [0u16; 256];
        let mut needed = 0u32;
        let name = (|| -> windows::core::Result<String> {
            // SAFETY: `buf` outlives the call; its byte length is passed as `nlength`.
            unsafe {
                GetUserObjectInformationW(
                    HANDLE(desk.0),
                    UOI_NAME,
                    Some(buf.as_mut_ptr().cast()),
                    std::mem::size_of_val(&buf) as u32,
                    Some(&mut needed),
                )
            }?;
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            Ok(String::from_utf16_lossy(&buf[..len]))
        })();

        // SAFETY: `desk` was opened above and is not used again after this call. The
        // system keeps its own reference once a thread is switched onto it with
        // `SetThreadDesktop`, so closing it here is safe even if that follows.
        unsafe {
            let _ = CloseDesktop(desk);
        }
        Ok(name?)
    }

    /// Switches the calling thread onto `desktop` if it is not already there.
    fn switch_if_needed(&self, name: &str) -> anyhow::Result<()> {
        let mut assigned = self.assigned.lock().unwrap();
        if assigned.as_deref() == Some(name) {
            return Ok(());
        }
        // SAFETY: opened with `DESKTOP_RIGHTS`, including `DESKTOP_SWITCHDESKTOP`; closed
        // right after the switch, since the thread holds its own reference once assigned.
        let desk = unsafe { OpenInputDesktop(Default::default(), false, DESKTOP_RIGHTS) }?;
        let result = unsafe { SetThreadDesktop(desk) };
        unsafe {
            let _ = CloseDesktop(desk);
        }
        result?;
        *assigned = Some(name.to_string());
        Ok(())
    }

    pub fn current(&self) -> anyhow::Result<DesktopKind> {
        let name = Self::input_desktop_name()?;
        Ok(secure::desktop_kind(&name))
    }

    /// Switches the calling thread to the current input desktop (if that changed since
    /// the last call) and injects `ev` there.
    pub fn inject(&self, ev: &InputEvent) -> anyhow::Result<()> {
        let name = Self::input_desktop_name()?;
        self.switch_if_needed(&name)?;
        self.injector.inject(ev)
    }

    pub fn cursor(&self) -> (f64, f64) {
        let mut p = POINT::default();
        // SAFETY: valid out-pointer; failure leaves `p` zeroed, which is an acceptable
        // fallback (there is no cursor position to report that would be more correct).
        let _ = unsafe { GetCursorPos(&mut p) };
        (p.x as f64, p.y as f64)
    }
}
