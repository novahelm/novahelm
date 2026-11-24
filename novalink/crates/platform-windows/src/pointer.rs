//! Keeps the pointer visible while another machine moves it. Windows draws no pointer
//! when no mouse is plugged in, and injected motion does not change that; Mouse Keys
//! being on does. We turn it on for this sign-in only (never saved to the profile) and
//! put the user's own setting back when the remote leaves.

use crate::logic;
use std::sync::Mutex;
use windows::Win32::UI::Accessibility::MOUSEKEYS;
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_MOUSEPRESENT, SPI_GETMOUSEKEYS, SPI_SETMOUSEKEYS, SPIF_SENDCHANGE,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
};

/// The user's Mouse Keys setting while we hold Mouse Keys on, else `None`.
#[derive(Default)]
pub struct ForcedPointer {
    saved: Mutex<Option<MOUSEKEYS>>,
}

fn get_mouse_keys() -> windows::core::Result<MOUSEKEYS> {
    let mut mk = MOUSEKEYS {
        cbSize: size_of::<MOUSEKEYS>() as u32,
        ..Default::default()
    };
    // SAFETY: mk is a valid MOUSEKEYS with cbSize set, and outlives the call.
    unsafe {
        SystemParametersInfoW(
            SPI_GETMOUSEKEYS,
            mk.cbSize,
            Some(&mut mk as *mut MOUSEKEYS as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }?;
    Ok(mk)
}

/// Applies `mk` for this sign-in only: SPIF_UPDATEINIFILE is left out on purpose.
fn set_mouse_keys(mut mk: MOUSEKEYS) -> windows::core::Result<()> {
    // SAFETY: as above.
    unsafe {
        SystemParametersInfoW(
            SPI_SETMOUSEKEYS,
            mk.cbSize,
            Some(&mut mk as *mut MOUSEKEYS as *mut _),
            SPIF_SENDCHANGE,
        )
    }
}

impl ForcedPointer {
    pub fn set(&self, on: bool) {
        let mut saved = self.saved.lock().unwrap_or_else(|e| e.into_inner());
        if !on {
            if let Some(old) = saved.take()
                && let Err(e) = set_mouse_keys(old)
            {
                tracing::warn!(event = "pointer_restore_failed", error = %e);
            }
            return;
        }
        // SAFETY: plain call.
        let mouse_present = unsafe { GetSystemMetrics(SM_MOUSEPRESENT) } != 0;
        if saved.is_some() || mouse_present {
            return;
        }
        let old = match get_mouse_keys() {
            Ok(old) => old,
            Err(e) => {
                tracing::warn!(event = "pointer_force_failed", error = %e);
                return;
            }
        };
        let Some(flags) = logic::force_mouse_keys(old.dwFlags) else {
            return; // Mouse Keys is already on, so the pointer already shows.
        };
        match set_mouse_keys(MOUSEKEYS {
            dwFlags: flags,
            ..old
        }) {
            Ok(()) => {
                tracing::info!(event = "pointer_forced_visible", reason = "no_mouse");
                *saved = Some(old);
            }
            Err(e) => tracing::warn!(event = "pointer_force_failed", error = %e),
        }
    }
}
