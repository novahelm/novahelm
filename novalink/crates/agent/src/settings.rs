//! The user's NovaLink settings, kept in `settings.json` beside the trust store. Later they
//! move into the user's NovaHelm account; the file keeps the same shape for that.

use novalink_proto::keys;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Keys from another computer's keyboard arrive with Ctrl and Cmd (Win) swapped, so a
    /// Windows keyboard copies with Ctrl+C on the Mac. On by default on the Mac only.
    pub swap_ctrl_cmd: bool,
}

/// Only a Mac swaps by default: a Windows PC keeps its keys as they arrive.
const SWAP_BY_DEFAULT: bool = cfg!(target_os = "macos");

impl Default for Settings {
    fn default() -> Self {
        Self {
            swap_ctrl_cmd: SWAP_BY_DEFAULT,
        }
    }
}

impl Settings {
    /// The saved settings, or the defaults when there are none or they can't be read.
    pub fn load(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!("ignoring unreadable {}: {e}", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Writes through a temporary file so a crash never leaves half a file.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)
    }
}

/// Ctrl and the Windows/Command key trade places; every other key is unchanged.
pub fn swap_ctrl_cmd(usage: u16) -> u16 {
    match usage {
        keys::LEFT_CTRL => keys::LEFT_GUI,
        keys::LEFT_GUI => keys::LEFT_CTRL,
        keys::RIGHT_CTRL => keys::RIGHT_GUI,
        keys::RIGHT_GUI => keys::RIGHT_CTRL,
        u => u,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_trades_ctrl_and_cmd_on_each_side_only() {
        assert_eq!(swap_ctrl_cmd(keys::LEFT_CTRL), keys::LEFT_GUI);
        assert_eq!(swap_ctrl_cmd(keys::LEFT_GUI), keys::LEFT_CTRL);
        assert_eq!(swap_ctrl_cmd(keys::RIGHT_CTRL), keys::RIGHT_GUI);
        assert_eq!(swap_ctrl_cmd(keys::RIGHT_GUI), keys::RIGHT_CTRL);
        assert_eq!(swap_ctrl_cmd(keys::LEFT_ALT), keys::LEFT_ALT);
        assert_eq!(swap_ctrl_cmd(keys::C), keys::C);
    }

    #[test]
    fn settings_round_trip_and_fall_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        assert_eq!(Settings::load(&p), Settings::default());
        let s = Settings {
            swap_ctrl_cmd: !Settings::default().swap_ctrl_cmd,
        };
        s.save(&p).unwrap();
        assert_eq!(Settings::load(&p), s);
        std::fs::write(&p, "not json").unwrap();
        assert_eq!(Settings::load(&p), Settings::default());
        std::fs::write(&p, "{}").unwrap();
        assert_eq!(Settings::load(&p), Settings::default());
    }
}
