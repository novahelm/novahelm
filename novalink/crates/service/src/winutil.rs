//! Windows plumbing shared by the service and its helper process: log-directory
//! preparation (both run as SYSTEM), security descriptors, SIDs and known folders.

use anyhow::Context;
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use tracing_appender::non_blocking::WorkerGuard;
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT, SetSecurityInfo,
};
use windows::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
    OPEN_EXISTING, READ_CONTROL, WRITE_DAC,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Shell::{FOLDERID_ProgramData, KNOWN_FOLDER_FLAG, SHGetKnownFolderPath};
use windows::core::{GUID, Owned, PCWSTR, PWSTR};

/// Protected DACL for `%ProgramData%\NovaLink`: SYSTEM and Administrators only.
const PROTECTED_DIR_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

/// SIDs allowed to own the log directories: SYSTEM and Administrators.
const TRUSTED_OWNERS: [&str; 2] = ["S-1-5-18", "S-1-5-32-544"];

/// Structured JSON-lines logging to `<file_prefix>.<date>.jsonl`, daily-rotated, mirroring
/// the agent's setup. The service and its helper each use their own prefix so they never
/// share a file. These processes run as SYSTEM, so it only writes to a log directory it has made or verified
/// (see [`prepare_log_dir`]); otherwise it logs to stderr and carries on.
pub fn init_logging(file_prefix: &str) -> Option<WorkerGuard> {
    let filter = tracing_subscriber::EnvFilter::try_from_env("NOVALINK_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt()
        .json()
        .with_ansi(false)
        .with_env_filter(filter);
    let appender = prepare_log_dir().and_then(|dir| {
        tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix(file_prefix)
            .filename_suffix("jsonl")
            .max_log_files(7)
            .build(&dir)
            .context("building the log appender")
    });
    match appender {
        Ok(appender) => {
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let _ = builder.with_writer(writer).try_init();
            Some(guard)
        }
        Err(e) => {
            eprintln!("{file_prefix}: not logging to disk: {e:#}");
            let _ = builder.with_writer(std::io::stderr).try_init();
            None
        }
    }
}

/// `%ProgramData%\NovaLink\logs`, created with a SYSTEM/Administrators-only DACL if it is
/// missing, and in either case verified (a real directory, not a junction or symlink,
/// owned by SYSTEM or Administrators) and then locked to that DACL. A normal user can
/// create entries under `%ProgramData%`, so anything else there could redirect what
/// SYSTEM writes.
fn prepare_log_dir() -> anyhow::Result<PathBuf> {
    let nova = known_folder(&FOLDERID_ProgramData)?.join("NovaLink");
    ensure_trusted_dir(&nova, Some(PROTECTED_DIR_SDDL))?;
    let logs = nova.join("logs");
    ensure_trusted_dir(&logs, Some(PROTECTED_DIR_SDDL))?;
    Ok(logs)
}

fn ensure_trusted_dir(path: &Path, sddl: Option<&str>) -> anyhow::Result<()> {
    if std::fs::symlink_metadata(path).is_err() {
        // Creation errors are ignored: if someone else made it first, the check below
        // judges what is there.
        let _ = create_dir(path, sddl);
    }
    secure_dir(path, PROTECTED_DIR_SDDL)
}

fn create_dir(path: &Path, sddl: Option<&str>) -> anyhow::Result<()> {
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut sd = sddl.map(SecurityDescriptor::from_sddl).transpose()?;
    let attrs = sd
        .as_mut()
        .map(|sd| sd.as_ptr().cast_const().cast::<SECURITY_ATTRIBUTES>());
    // SAFETY: `wide` is NUL-terminated; `attrs`, when set, points at `sd`'s attributes,
    // which stay alive until after the call.
    unsafe { CreateDirectoryW(PCWSTR(wide.as_ptr()), attrs) }
        .with_context(|| format!("creating {}", path.display()))
}

/// Opens `path` once and, through that one handle, checks it is a real directory (not a
/// reparse point) owned by a trusted SID, then replaces its DACL with the protected one
/// `sddl` describes. Using one handle for the check and the change means the directory
/// cannot be swapped for a junction in between. Inheritable entries are applied to
/// children created later; existing children keep what they have.
fn secure_dir(path: &Path, sddl: &str) -> anyhow::Result<()> {
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is NUL-terminated. Opens the entry itself (not what a reparse point
    // names) with only the rights needed to read and change its security.
    let dir = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            (READ_CONTROL | WRITE_DAC).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .with_context(|| format!("opening {}", path.display()))?;
    // SAFETY: `dir` was just returned by `CreateFileW`; closed via `Owned` when dropped.
    let dir = unsafe { Owned::new(dir) };

    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `dir` is a valid handle; `info` is a valid out-pointer.
    unsafe { GetFileInformationByHandle(*dir, &mut info) }
        .with_context(|| format!("reading {}", path.display()))?;
    anyhow::ensure!(
        info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0,
        "{} is not a directory",
        path.display()
    );
    anyhow::ensure!(
        info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0,
        "{} is a reparse point",
        path.display()
    );

    let mut owner = PSID::default();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `dir` is a valid handle; the out-pointers are valid. `owner` points into
    // `sd`, which is freed below only after the owner has been converted to a string.
    let err = unsafe {
        GetSecurityInfo(
            *dir,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            Some(&mut sd),
        )
    };
    err.ok().context("GetSecurityInfo")?;
    let owner = sid_to_string(owner);
    // SAFETY: `sd` was allocated by `GetSecurityInfo`, which documents `LocalFree`.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
    let owner = owner?;
    anyhow::ensure!(
        TRUSTED_OWNERS.contains(&owner.as_str()),
        "{} is owned by {owner}",
        path.display()
    );

    // A trusted owner says nothing about who else was granted access (a directory made
    // by an older install, or by an administrator, may let users write into it).
    let wanted = SecurityDescriptor::from_sddl(sddl)?;
    let mut present = windows::core::BOOL(0);
    let mut defaulted = windows::core::BOOL(0);
    let mut dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: `wanted` holds a valid descriptor for the call; the out-pointers are valid,
    // and `dacl` points into `wanted`, which outlives its use below.
    unsafe { GetSecurityDescriptorDacl(wanted.sd, &mut present, &mut dacl, &mut defaulted) }
        .context("GetSecurityDescriptorDacl")?;
    anyhow::ensure!(present.as_bool() && !dacl.is_null(), "SDDL carries no DACL");
    // SAFETY: `dir` is a valid handle with WRITE_DAC; `dacl` is valid (see above).
    let err = unsafe {
        SetSecurityInfo(
            *dir,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(dacl.cast_const()),
            None,
        )
    };
    err.ok()
        .with_context(|| format!("resetting the DACL of {}", path.display()))
}

/// The Windows session this process itself is running in.
pub fn own_session() -> anyhow::Result<u32> {
    // SAFETY: plain FFI calls; `session` is a valid out-pointer.
    let session = unsafe {
        let pid = GetCurrentProcessId();
        let mut session = 0u32;
        ProcessIdToSessionId(pid, &mut session)?;
        session
    };
    Ok(session)
}

/// NUL-terminated UTF-16, for passing to Win32 `W` APIs.
pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Owns the `SECURITY_ATTRIBUTES`/`PSECURITY_DESCRIPTOR` pair built from an SDDL string,
/// freeing the descriptor (via `LocalFree`, as `ConvertStringSecurityDescriptorToSecurityDescriptorW`
/// requires) when dropped.
pub(crate) struct SecurityDescriptor {
    attrs: Box<SECURITY_ATTRIBUTES>,
    sd: PSECURITY_DESCRIPTOR,
}

impl SecurityDescriptor {
    /// Pointer to pass as `lpSecurityAttributes`, valid for `self`'s lifetime.
    pub fn as_ptr(&mut self) -> *mut c_void {
        (&mut *self.attrs as *mut SECURITY_ATTRIBUTES).cast()
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: `sd.0` was allocated by `ConvertStringSecurityDescriptorToSecurityDescriptorW`,
        // which documents that the caller frees it with `LocalFree`.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.sd.0)));
        }
    }
}

impl SecurityDescriptor {
    pub fn from_sddl(sddl: &str) -> anyhow::Result<Self> {
        let wide = to_wide(sddl);
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `wide` is NUL-terminated and outlives the call; the descriptor it writes
        // into `sd` is owned by the returned `SecurityDescriptor` and freed by its `Drop`.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wide.as_ptr()),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
        }
        .context("ConvertStringSecurityDescriptorToSecurityDescriptorW")?;
        let attrs = Box::new(SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        });
        Ok(Self { attrs, sd })
    }
}

pub fn sid_to_string(sid: PSID) -> anyhow::Result<String> {
    let mut raw = PWSTR::null();
    // SAFETY: `sid` is valid for the call (it points into the still-live buffer in
    // `session_logon_sid`); `raw` receives a `LocalAlloc`'d string, freed below.
    unsafe { ConvertSidToStringSidW(sid, &mut raw) }.context("ConvertSidToStringSidW")?;
    // SAFETY: `raw` was just allocated above and is non-null and NUL-terminated.
    let s = unsafe { raw.to_string() }.context("logon SID was not valid UTF-16")?;
    // SAFETY: `raw` was allocated by `ConvertSidToStringSidW`, which documents `LocalFree`.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(raw.0.cast())));
    }
    Ok(s)
}

/// A known folder's path, read from the shell rather than the environment (which a
/// caller could otherwise spoof for itself).
pub fn known_folder(id: &GUID) -> anyhow::Result<PathBuf> {
    // SAFETY: `id` is a valid GUID; the returned string is freed with `CoTaskMemFree`
    // below, as `SHGetKnownFolderPath` documents.
    let raw = unsafe { SHGetKnownFolderPath(id, KNOWN_FOLDER_FLAG(0), None) }
        .context("SHGetKnownFolderPath")?;
    // SAFETY: `raw` is non-null and NUL-terminated per the call above succeeding.
    let s = unsafe { raw.to_string() }.context("known folder was not valid UTF-16")?;
    // SAFETY: `raw` was allocated by `SHGetKnownFolderPath`, which documents `CoTaskMemFree`.
    unsafe { CoTaskMemFree(Some(raw.0.cast())) };
    Ok(PathBuf::from(s))
}
