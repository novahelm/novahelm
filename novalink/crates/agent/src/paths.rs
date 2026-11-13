//! Per-OS config and log directories.
//!
//! Set `NOVALINK_HOME` to redirect both under `<base>/config` and `<base>/logs`
//! (used by tests and portable setups). Directories are created on first use
//! (0700 on unix).
//!
//! The Windows log dir is per-user (`%LOCALAPPDATA%\NovaLink\logs`) because the agent
//! runs as the user in Plan 1; Plan 4 revisits this with a SYSTEM/Admins DACL.

use std::path::{Path, PathBuf};

fn home() -> PathBuf {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Pure path resolution (no filesystem access). `base` overrides the OS location.
pub fn resolve_config(base: Option<&Path>) -> PathBuf {
    if let Some(b) = base {
        return b.join("config");
    }
    #[cfg(target_os = "macos")]
    return home().join("Library/Application Support/NovaLink");
    #[cfg(windows)]
    return env_dir("APPDATA")
        .unwrap_or_else(|| home().join("AppData/Roaming"))
        .join("NovaLink");
    #[cfg(not(any(target_os = "macos", windows)))]
    return home().join(".config/novalink");
}

/// Pure path resolution (no filesystem access). `base` overrides the OS location.
pub fn resolve_log(base: Option<&Path>) -> PathBuf {
    if let Some(b) = base {
        return b.join("logs");
    }
    #[cfg(target_os = "macos")]
    return home().join("Library/Logs/NovaLink");
    #[cfg(windows)]
    return env_dir("LOCALAPPDATA")
        .unwrap_or_else(|| home().join("AppData/Local"))
        .join(r"NovaLink\logs");
    #[cfg(not(any(target_os = "macos", windows)))]
    return home().join(".local/state/novalink");
}

/// Create `p` (and parents) with mode 0700 on unix.
pub fn create_private_dir(p: &Path) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(p)
}

fn ensure(p: PathBuf) -> PathBuf {
    if let Err(e) = create_private_dir(&p) {
        tracing::warn!(path = %p.display(), error = %e, "could not create directory");
    }
    p
}

pub fn config_dir_in(base: Option<&Path>) -> PathBuf {
    ensure(resolve_config(base))
}

pub fn log_dir_in(base: Option<&Path>) -> PathBuf {
    ensure(resolve_log(base))
}

pub fn config_dir() -> PathBuf {
    config_dir_in(env_dir("NOVALINK_HOME").as_deref())
}

pub fn log_dir() -> PathBuf {
    log_dir_in(env_dir("NOVALINK_HOME").as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_product_specific() {
        // Pure resolution: touches no directories.
        let c = resolve_config(None);
        assert!(c.ends_with("NovaLink") || c.ends_with("novalink"));
        let l = resolve_log(None);
        assert!(l.ends_with("NovaLink") || l.ends_with("novalink") || l.ends_with("logs"));
    }

    #[test]
    fn override_base_is_used_and_dirs_created() {
        let t = tempfile::tempdir().unwrap();
        let c = config_dir_in(Some(t.path()));
        let l = log_dir_in(Some(t.path()));
        assert!(c.starts_with(t.path()) && c.is_dir());
        assert!(l.starts_with(t.path()) && l.is_dir());
        assert_ne!(c, l);
    }
}
