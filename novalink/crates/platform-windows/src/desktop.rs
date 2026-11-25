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
use std::cell::RefCell;
use windows::Win32::Foundation::{HANDLE, POINT};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CREATEMENU, DESKTOP_CREATEWINDOW,
    DESKTOP_HOOKCONTROL, DESKTOP_JOURNALPLAYBACK, DESKTOP_JOURNALRECORD, DESKTOP_READOBJECTS,
    DESKTOP_SWITCHDESKTOP, DESKTOP_WRITEOBJECTS, GetUserObjectInformationW, HDESK,
    OpenInputDesktop, SetThreadDesktop, UOI_NAME,
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
///
/// Thread-affine: `SetThreadDesktop` applies to the thread that calls it, and the
/// assigned desktop handle is cached here, so create this on the one thread that will
/// call [`inject`](Self::inject) and keep it there. The `RefCell` makes it `!Sync`, and
/// the raw handle makes it `!Send`.
pub struct DesktopInjector {
    injector: Injector,
    /// The desktop the calling thread is currently assigned to. Kept open: closing the
    /// handle of a desktop a thread is assigned to fails (and would leak it).
    assigned: RefCell<Option<HDESK>>,
}

impl Drop for DesktopInjector {
    fn drop(&mut self) {
        if let Some(old) = self.assigned.get_mut().take() {
            // SAFETY: `old` is a handle this struct opened and still owns; the thread may
            // still be assigned to it, in which case this fails harmlessly at thread exit.
            unsafe {
                let _ = CloseDesktop(old);
            }
        }
    }
}

/// Name of the desktop behind `desk`, e.g. `"Default"` or `"Winlogon"`.
fn desktop_name(desk: HDESK) -> windows::core::Result<String> {
    let mut buf = [0u16; 256];
    let mut needed = 0u32;
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
}

/// Opens the desktop that currently has input.
fn open_input_desktop() -> windows::core::Result<HDESK> {
    // SAFETY: plain call; the caller owns the returned handle and must close it.
    unsafe { OpenInputDesktop(Default::default(), false, DESKTOP_RIGHTS) }
}

impl DesktopInjector {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            injector: Injector::default(),
            assigned: RefCell::new(None),
        })
    }

    pub fn current(&self) -> anyhow::Result<DesktopKind> {
        let desk = open_input_desktop()?;
        let name = desktop_name(desk);
        // SAFETY: `desk` was opened just above, is not assigned to any thread, and is not
        // used again.
        unsafe {
            let _ = CloseDesktop(desk);
        }
        Ok(secure::desktop_kind(&name?))
    }

    /// Injects `ev` on the input desktop, but only if that desktop is protected. The
    /// desktop is opened once and that same handle decides both the check and where the
    /// calling thread is moved, so the two cannot disagree.
    pub fn inject(&self, ev: &InputEvent) -> anyhow::Result<()> {
        self.inject_on_input_desktop(ev, true)
    }

    /// Injects a key-up or button-up on whichever desktop has input. Releasing is always
    /// safe, so unlike [`inject`](Self::inject) this has no desktop-kind check.
    pub fn release(&self, ev: &InputEvent) -> anyhow::Result<()> {
        self.inject_on_input_desktop(ev, false)
    }

    fn inject_on_input_desktop(&self, ev: &InputEvent, protected_only: bool) -> anyhow::Result<()> {
        let desk = open_input_desktop()?;
        if protected_only {
            let kind = desktop_name(desk).map(|n| secure::desktop_kind(&n));
            if !matches!(kind, Ok(DesktopKind::Protected)) {
                // SAFETY: `desk` was opened above and is not assigned to any thread.
                unsafe {
                    let _ = CloseDesktop(desk);
                }
                kind?;
                anyhow::bail!("input desktop is not protected");
            }
        }
        // SAFETY: `desk` is a valid desktop handle opened with `DESKTOP_RIGHTS`, and this
        // thread owns no windows or hooks (this type's documented precondition).
        let switched = unsafe { SetThreadDesktop(desk) };
        if let Err(e) = switched {
            // SAFETY: the switch failed, so no thread is assigned to `desk`.
            unsafe {
                let _ = CloseDesktop(desk);
            }
            return Err(e.into());
        }
        // The thread now uses `desk`, so keep it open and close the one it left, which no
        // thread uses any more.
        if let Some(old) = self.assigned.borrow_mut().replace(desk) {
            // SAFETY: `old` was opened by an earlier call and is no longer assigned.
            unsafe {
                let _ = CloseDesktop(old);
            }
        }
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
