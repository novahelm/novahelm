//! Doctor's checks for Windows' protected screens (the lock and PIN screens, and admin
//! prompts on the secure desktop). The facts come from the registry on Windows; the verdicts
//! are pure so they are tested anywhere. Each verdict is `(ok, detail, advice)`.

pub const SERVICE_NAME: &str = "NovaLinkService";

/// Whether the agent under Program Files is the one to run: the service must exist and the
/// installed copy must be this version, so a declined upgrade still runs the new code.
pub fn use_program_files_agent(
    service_exists: bool,
    pf_version: Option<&str>,
    own_version: &str,
) -> bool {
    service_exists && pf_version.is_some_and(|v| v.trim() == own_version)
}

/// Whether the SYSTEM helper is up, and whether the service that starts it is installed.
pub fn protected_screens(helper_up: bool, service_installed: bool) -> (bool, String, String) {
    let advice = if service_installed {
        "Restart the PC; if that doesn't help, run NovaLink-Setup again."
    } else {
        "Choose \"Allow on lock screen…\" in the NovaLink tray menu."
    };
    (
        helper_up,
        if helper_up {
            "helper running"
        } else {
            "helper not running"
        }
        .into(),
        advice.into(),
    )
}

/// `PromptOnSecureDesktop`: missing or 1 keeps admin prompts on the secure desktop, where
/// the helper can reach them; 0 shows them on the normal desktop, where it can't.
pub fn uac_secure_desktop(prompt_on_secure_desktop: Option<u32>) -> (bool, String, String) {
    if prompt_on_secure_desktop == Some(0) {
        (
            false,
            "admin prompts show on the normal desktop".into(),
            "Admin prompts show on the normal desktop here, so NovaLink can't click them. Turn the dimmed prompt back on in User Account Control settings.".into(),
        )
    } else {
        (
            true,
            "admin prompts show on the secure desktop".into(),
            String::new(),
        )
    }
}

/// `DisableCAD`: missing or 1 means signing in needs no Ctrl+Alt+Del; 0 means it does.
pub fn ctrl_alt_del(disable_cad: Option<u32>) -> (bool, String, String) {
    if disable_cad == Some(0) {
        (
            false,
            "signing in needs Ctrl+Alt+Del".into(),
            "This PC asks for Ctrl+Alt+Del before signing in, which NovaLink can't send. Turn off \"Require users to press Ctrl+Alt+Delete\" in netplwiz.".into(),
        )
    } else {
        (
            true,
            "signing in needs no Ctrl+Alt+Del".into(),
            String::new(),
        )
    }
}

#[cfg(windows)]
pub use reg::{
    ctrl_alt_del_policy, program_files_agent, program_files_agent_in_use, service_installed,
    uac_secure_desktop_policy,
};

#[cfg(windows)]
mod reg {
    use super::SERVICE_NAME;
    use std::path::PathBuf;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_DWORD, REG_VALUE_TYPE, RegCloseKey, RegOpenKeyExW,
        RegQueryValueExW,
    };
    use windows::core::PCWSTR;

    const POLICIES: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Opens `HKLM\<sub>` for reading; `None` when it isn't there.
    fn open(sub: &str) -> Option<HKEY> {
        let sub = wide(sub);
        let mut key = HKEY::default();
        // SAFETY: `sub` is NUL-terminated; the caller closes the key.
        let r = unsafe {
            RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(sub.as_ptr()),
                None,
                KEY_READ,
                &mut key,
            )
        };
        (r == ERROR_SUCCESS).then_some(key)
    }

    fn read_dword(sub: &str, name: &str) -> Option<u32> {
        let key = open(sub)?;
        let name = wide(name);
        let mut kind = REG_VALUE_TYPE::default();
        let mut data = [0u8; 4];
        let mut len = data.len() as u32;
        // SAFETY: the name is NUL-terminated; the buffer and length describe `data`.
        let r = unsafe {
            let r = RegQueryValueExW(
                key,
                PCWSTR(name.as_ptr()),
                None,
                Some(&mut kind),
                Some(data.as_mut_ptr()),
                Some(&mut len),
            );
            let _ = RegCloseKey(key);
            r
        };
        (r == ERROR_SUCCESS && kind == REG_DWORD && len == 4).then(|| u32::from_le_bytes(data))
    }

    pub fn uac_secure_desktop_policy() -> Option<u32> {
        read_dword(POLICIES, "PromptOnSecureDesktop")
    }

    pub fn ctrl_alt_del_policy() -> Option<u32> {
        read_dword(POLICIES, "DisableCAD")
    }

    /// Whether the Service Control Manager has `NovaLinkService` registered.
    pub fn service_installed() -> bool {
        open(&format!(
            r"SYSTEM\CurrentControlSet\Services\{SERVICE_NAME}"
        ))
        .is_some_and(|k| {
            // SAFETY: the key was just opened.
            unsafe {
                let _ = RegCloseKey(k);
            }
            true
        })
    }

    /// The Program Files agent when it is the one to run for a program of `own_version`.
    pub fn program_files_agent_in_use(own_version: &str) -> Option<PathBuf> {
        let exe = program_files_agent()?;
        let version = std::fs::read_to_string(exe.with_file_name("version.txt")).ok();
        super::use_program_files_agent(service_installed(), version.as_deref(), own_version)
            .then_some(exe)
    }

    /// `%ProgramFiles%\NovaLink\novalink-agent.exe` when that file exists.
    pub fn program_files_agent() -> Option<PathBuf> {
        let exe = PathBuf::from(std::env::var_os("ProgramFiles")?)
            .join("NovaLink")
            .join("novalink-agent.exe");
        exe.is_file().then_some(exe)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_files_agent_needs_service_and_matching_version() {
        assert!(use_program_files_agent(true, Some("0.2.0\r\n"), "0.2.0"));
        assert!(!use_program_files_agent(false, Some("0.2.0"), "0.2.0"));
        assert!(!use_program_files_agent(true, Some("0.1.0"), "0.2.0"));
        assert!(!use_program_files_agent(true, None, "0.2.0"));
    }

    #[test]
    fn protected_screens_follows_the_helper() {
        let (ok, detail, _) = protected_screens(true, true);
        assert!(ok && detail == "helper running");
        let (ok, detail, _) = protected_screens(false, true);
        assert!(!ok && detail == "helper not running");
    }

    #[test]
    fn protected_screens_advice_depends_on_the_service() {
        assert_eq!(
            protected_screens(false, false).2,
            "Choose \"Allow on lock screen…\" in the NovaLink tray menu."
        );
        assert_eq!(
            protected_screens(false, true).2,
            "Restart the PC; if that doesn't help, run NovaLink-Setup again."
        );
    }

    #[test]
    fn uac_secure_desktop_is_ok_unless_zero() {
        assert!(uac_secure_desktop(None).0);
        assert!(uac_secure_desktop(Some(1)).0);
        let (ok, _, advice) = uac_secure_desktop(Some(0));
        assert!(!ok && advice.starts_with("Admin prompts show on the normal desktop here"));
    }

    #[test]
    fn ctrl_alt_del_is_ok_unless_zero() {
        assert!(ctrl_alt_del(None).0);
        assert!(ctrl_alt_del(Some(1)).0);
        let (ok, _, advice) = ctrl_alt_del(Some(0));
        assert!(!ok && advice.contains("\"Require users to press Ctrl+Alt+Delete\" in netplwiz."));
    }
}
