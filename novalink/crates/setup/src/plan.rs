//! What setup does, as an ordered list. Pure, so the order can be tested anywhere.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Install,
    Uninstall,
}

/// What is on this machine before setup runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub dir_exists: bool,
    pub version: Option<String>,
    pub agent_running: bool,
    /// The "Start with Windows" Run value exists.
    pub run_key_set: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    StopAgent,
    StopTray,
    WriteFiles,
    WriteVersion,
    SetRunKey,
    CreateStartMenuShortcut,
    WriteUninstallEntry,
    /// Offers to add a Windows Firewall rule when none lets the agent in. Never fails.
    AllowFirewall,
    LaunchTray,
    /// Deletes a "Turn Off" left behind by an earlier, uninstalled copy.
    ClearOffMarker,
    RemoveRunKey,
    RemoveShortcut,
    RemoveUninstallEntry,
    RemoveFiles,
    KeepUserData,
}

/// Installing is the same whether the version is older, equal or newer: every file is
/// rewritten, so running setup again repairs a damaged install. The Run value is only
/// rewritten when it exists, so an upgrade never undoes a user's "Start with Windows" off.
/// Without version.txt nothing finished installing before (a fresh machine, or a first
/// install that failed part way), so the Run value is set and an old "off" is cleared.
pub fn plan(current: &Installed, _payload_version: &str, mode: Mode) -> Vec<Step> {
    use Step::*;
    match mode {
        Mode::Install => {
            let mut steps = Vec::new();
            // A tray can be up while the agent is off, and the tray respawns the agent while
            // it shuts down, so an existing install is always stopped, agent last.
            if current.dir_exists || current.agent_running {
                steps.extend([StopTray, StopAgent]);
            }
            let fresh = current.version.is_none();
            steps.push(WriteFiles);
            if fresh || current.run_key_set {
                steps.push(SetRunKey);
            }
            if fresh {
                steps.push(ClearOffMarker);
            }
            // version.txt marks a finished install, so it goes after every other step.
            steps.extend([
                CreateStartMenuShortcut,
                WriteUninstallEntry,
                AllowFirewall,
                WriteVersion,
                LaunchTray,
            ]);
            steps
        }
        // Stopping is harmless when nothing is running, and the executor tolerates it.
        Mode::Uninstall => vec![
            StopTray,
            StopAgent,
            RemoveRunKey,
            RemoveShortcut,
            RemoveUninstallEntry,
            RemoveFiles,
            KeepUserData,
        ],
    }
}

/// Shown when this setup has no programs inside it (built without NOVALINK_SETUP_PAYLOAD).
pub const EMPTY_PAYLOAD: &str =
    "This NovaLink-Setup.exe was built without the NovaLink programs inside it.";

/// Whether an install can go ahead with these embedded files (name, size).
pub fn payload_problem(files: &[(&str, usize)]) -> Option<&'static str> {
    files.iter().any(|(_, n)| *n == 0).then_some(EMPTY_PAYLOAD)
}

/// What to say when NovaLink is still answering after setup asked it to stop. Without
/// version.txt there is no tray to turn it off from: it is a copy started by hand.
pub fn still_running_text(current: &Installed) -> &'static str {
    if current.version.is_none() {
        "An older NovaLink is running in a window. Close that window (or the Command Prompt \
         running novalink-agent), then run setup again."
    } else {
        "NovaLink is still running. Turn it off from the tray (or restart Windows), then run \
         setup again."
    }
}

#[cfg(test)]
mod tests {
    use super::Step::*;
    use super::*;

    fn installed(dir_exists: bool, version: Option<&str>, agent_running: bool) -> Installed {
        Installed {
            dir_exists,
            version: version.map(String::from),
            agent_running,
            run_key_set: dir_exists,
        }
    }

    #[test]
    fn plan_fresh_install() {
        let p = plan(&installed(false, None, false), "0.1.0", Mode::Install);
        assert_eq!(
            p,
            [
                WriteFiles,
                SetRunKey,
                ClearOffMarker,
                CreateStartMenuShortcut,
                WriteUninstallEntry,
                AllowFirewall,
                WriteVersion,
                LaunchTray
            ]
        );
    }

    #[test]
    fn plan_writes_version_only_after_every_other_install_step() {
        for cur in [
            installed(false, None, false),
            installed(true, Some("0.1.0"), true),
            installed(true, Some("0.1.0"), false),
        ] {
            let p = plan(&cur, "0.2.0", Mode::Install);
            assert_eq!(&p[p.len() - 2..], [WriteVersion, LaunchTray], "{p:?}");
        }
    }

    #[test]
    fn plan_upgrade_stops_running_agent_first() {
        let p = plan(
            &installed(true, Some("0.1.0"), true),
            "0.2.0",
            Mode::Install,
        );
        assert_eq!(
            p,
            [
                StopTray,
                StopAgent,
                WriteFiles,
                SetRunKey,
                CreateStartMenuShortcut,
                WriteUninstallEntry,
                AllowFirewall,
                WriteVersion,
                LaunchTray
            ]
        );
    }

    #[test]
    fn plan_same_version_repairs() {
        let p = plan(
            &installed(true, Some("0.2.0"), false),
            "0.2.0",
            Mode::Install,
        );
        for s in [
            WriteFiles,
            WriteVersion,
            SetRunKey,
            WriteUninstallEntry,
            LaunchTray,
        ] {
            assert!(p.contains(&s), "{s:?} missing from {p:?}");
        }
        // The tray may respawn the agent while it quits, so it is stopped regardless.
        assert_eq!(&p[..2], [StopTray, StopAgent]);
    }

    #[test]
    fn plan_upgrade_keeps_start_with_windows_off() {
        let mut cur = installed(true, Some("0.1.0"), false);
        cur.run_key_set = false;
        let p = plan(&cur, "0.2.0", Mode::Install);
        assert!(!p.contains(&SetRunKey), "{p:?}");
        cur.run_key_set = true;
        assert!(plan(&cur, "0.2.0", Mode::Install).contains(&SetRunKey));
    }

    #[test]
    fn plan_fresh_install_sets_run_key_even_if_flag_false() {
        let mut cur = installed(false, None, false);
        cur.run_key_set = false;
        assert!(plan(&cur, "0.1.0", Mode::Install).contains(&SetRunKey));
    }

    #[test]
    fn plan_rerun_after_failed_first_install_sets_run_key() {
        // WriteFiles made the folder, then a later step failed: no version.txt, no Run value.
        let mut cur = installed(true, None, false);
        cur.run_key_set = false;
        let p = plan(&cur, "0.1.0", Mode::Install);
        assert!(p.contains(&SetRunKey), "{p:?}");
    }

    #[test]
    fn plan_clears_off_marker_only_on_a_fresh_install() {
        // An uninstall keeps %APPDATA%\NovaLink, including a Turn Off marker.
        assert!(
            plan(&installed(false, None, false), "0.1.0", Mode::Install).contains(&ClearOffMarker)
        );
        assert!(
            plan(&installed(true, None, true), "0.1.0", Mode::Install).contains(&ClearOffMarker)
        );
        // An upgrade keeps the user's Turn Off.
        let p = plan(
            &installed(true, Some("0.1.0"), false),
            "0.2.0",
            Mode::Install,
        );
        assert!(!p.contains(&ClearOffMarker), "{p:?}");
        let u = plan(
            &installed(true, Some("0.1.0"), false),
            "0.1.0",
            Mode::Uninstall,
        );
        assert!(!u.contains(&ClearOffMarker));
    }

    #[test]
    fn empty_payload_is_refused() {
        assert_eq!(
            payload_problem(&[("novalink-agent.exe", 0), ("novalink.exe", 10)]),
            Some(EMPTY_PAYLOAD)
        );
        assert_eq!(
            payload_problem(&[("novalink-agent.exe", 5), ("novalink.exe", 10)]),
            None
        );
    }

    #[test]
    fn still_running_text_depends_on_a_previous_install() {
        // A hand-started Plan-1 agent: no version.txt, no tray to turn it off from.
        let t = still_running_text(&installed(false, None, true));
        assert!(
            t.contains("running in a window") && !t.contains("tray"),
            "{t}"
        );
        let t = still_running_text(&installed(true, Some("0.1.0"), true));
        assert!(t.contains("from the tray"), "{t}");
    }

    #[test]
    fn plan_install_never_removes_anything() {
        let p = plan(
            &installed(true, Some("0.1.0"), true),
            "0.2.0",
            Mode::Install,
        );
        assert!(!p.iter().any(|s| matches!(
            s,
            RemoveRunKey | RemoveShortcut | RemoveUninstallEntry | RemoveFiles | KeepUserData
        )));
    }

    #[test]
    fn plan_uninstall_keeps_user_data() {
        let p = plan(
            &installed(true, Some("0.1.0"), true),
            "0.1.0",
            Mode::Uninstall,
        );
        assert_eq!(
            p,
            [
                StopTray,
                StopAgent,
                RemoveRunKey,
                RemoveShortcut,
                RemoveUninstallEntry,
                RemoveFiles,
                KeepUserData
            ]
        );
    }
}
