//! "Start at login" for the tray, the way each OS expects it: a LaunchAgent on macOS, an
//! XDG autostart entry on Linux (SteamOS desktop mode included), the Run key on Windows.

use anyhow::{Context, Result};
use std::path::PathBuf;

const LABEL: &str = "com.novahelm.novagear";

fn exe() -> Result<PathBuf> {
    std::env::current_exe().context("finding the tray's own path")
}

#[cfg(target_os = "macos")]
fn entry() -> Result<PathBuf> {
    Ok(dirs_home()?
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn entry() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or(dirs_home()?.join(".config"));
    Ok(base.join("autostart/novagear.desktop"))
}

/// Where the Linux icon goes: the user's icon theme, which desktop entries find by name.
#[cfg(all(unix, not(target_os = "macos")))]
fn icon_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or(dirs_home()?.join(".local/share"));
    Ok(base.join("icons/hicolor/256x256/apps/novagear.png"))
}

#[cfg(all(unix, not(target_os = "macos")))]
const ICON_PNG: &[u8] = include_bytes!("../../../../brand/icons/novagear-256.png");

#[cfg(unix)]
fn dirs_home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The file contents that start `exe` at login.
pub fn entry_contents(exe: &str) -> String {
    if cfg!(target_os = "macos") {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{}</string></array>
  <key>RunAtLoad</key><true/>
  <key>ProcessType</key><string>Interactive</string>
</dict>
</plist>
"#,
            xml_escape(exe)
        )
    } else {
        // Exec takes a quoted path so spaces survive.
        format!(
            "[Desktop Entry]\nType=Application\nName=NovaGear\nIcon=novagear\nExec=\"{exe}\"\nX-GNOME-Autostart-enabled=true\n"
        )
    }
}

#[cfg(unix)]
pub fn enabled() -> bool {
    entry().is_ok_and(|p| p.exists())
}

#[cfg(unix)]
pub fn set(on: bool) -> Result<()> {
    let path = entry()?;
    if on {
        std::fs::create_dir_all(path.parent().context("autostart path has no parent")?)?;
        let exe = exe()?;
        #[cfg(not(target_os = "macos"))]
        {
            let icon = icon_path()?;
            std::fs::create_dir_all(icon.parent().context("icon path has no parent")?)?;
            std::fs::write(&icon, ICON_PNG)?;
        }
        std::fs::write(&path, entry_contents(&exe.to_string_lossy()))?;
    } else if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

#[cfg(windows)]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

#[cfg(windows)]
pub fn enabled() -> bool {
    std::process::Command::new("reg")
        .args(["query", RUN_KEY, "/v", "NovaGear"])
        .output()
        .is_ok_and(|o| o.status.success())
}

#[cfg(windows)]
pub fn set(on: bool) -> Result<()> {
    let status = if on {
        let exe = format!("\"{}\"", exe()?.display());
        std::process::Command::new("reg")
            .args(["add", RUN_KEY, "/v", "NovaGear", "/d", &exe, "/f"])
            .status()?
    } else {
        std::process::Command::new("reg")
            .args(["delete", RUN_KEY, "/v", "NovaGear", "/f"])
            .status()?
    };
    anyhow::ensure!(status.success(), "reg.exe failed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_names_the_tray_and_escapes_paths() {
        let e = entry_contents("/Applications/Nova & Co/novagear-tray");
        if cfg!(target_os = "macos") {
            assert!(e.contains("<string>/Applications/Nova &amp; Co/novagear-tray</string>"));
            assert!(e.contains("<key>RunAtLoad</key><true/>"));
        } else {
            assert!(e.contains("Exec=\"/Applications/Nova & Co/novagear-tray\""));
            assert!(e.contains("Icon=novagear\n"));
        }
    }
}
