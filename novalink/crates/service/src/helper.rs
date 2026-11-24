//! `novalink-service --helper`: the SYSTEM process that follows whichever desktop has
//! input and injects whatever NovaLink's user agent sends it over a named pipe.
//!
//! Only the installed agent, running in this helper's own session, may drive it: each
//! client's image path is read from its process (never trusted from the wire) and
//! checked with [`secure::caller_allowed`], and the pipe's DACL grants access only to
//! SYSTEM and the signed-in user's logon SID. Input is only ever injected while the
//! input desktop is [`DesktopKind::Protected`] — the whole point of this helper is to
//! reach the secure desktop, not to act as a general-purpose SYSTEM input backdoor on
//! the user's own desktop.

use anyhow::Context;
use novalink_core::{HeldInputs, InputEvent};
use novalink_platform_windows::desktop::DesktopInjector;
use novalink_platform_windows::secure::{self, DesktopKind, FrameReader, FromHelper, HELPER_PROTO, ToHelper};
use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{NamedPipeServer, PipeMode, ServerOptions};
use tracing_appender::non_blocking::WorkerGuard;
use windows::Win32::Foundation::{HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_GROUPS,
    TokenGroups,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Pipes::GetNamedPipeClientProcessId;
use windows::Win32::System::SystemServices::SE_GROUP_LOGON_ID;
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::Win32::UI::Shell::{FOLDERID_ProgramFiles, KNOWN_FOLDER_FLAG, SHGetKnownFolderPath};
use windows::core::{Owned, PCWSTR, PWSTR};

/// How often the current input desktop is polled while a client is connected.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

pub fn run() -> anyhow::Result<()> {
    let _guard = init_logging()?;
    let session = own_session()?;
    tracing::info!(session, "novalink-service helper starting");
    let rt = tokio::runtime::Runtime::new().context("building the tokio runtime")?;
    let result = rt.block_on(serve(session));
    if let Err(e) = &result {
        tracing::error!(error = %e, "helper stopped");
    }
    result
}

fn log_dir() -> PathBuf {
    let base = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("NovaLink").join("logs")
}

/// Structured JSON-lines logging, daily-rotated, mirroring the agent's setup.
fn init_logging() -> anyhow::Result<WorkerGuard> {
    let dir = log_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("novalink-service")
        .filename_suffix("jsonl")
        .max_log_files(7)
        .build(&dir)
        .context("building the log appender")?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = tracing_subscriber::EnvFilter::try_from_env("NOVALINK_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .json()
        .with_ansi(false)
        .with_writer(writer)
        .with_env_filter(filter)
        .try_init();
    Ok(guard)
}

/// The Windows session this process itself is running in.
fn own_session() -> anyhow::Result<u32> {
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
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Owns the `SECURITY_ATTRIBUTES`/`PSECURITY_DESCRIPTOR` pair built from an SDDL string,
/// freeing the descriptor (via `LocalFree`, as `ConvertStringSecurityDescriptorToSecurityDescriptorW`
/// requires) when dropped.
struct SecurityDescriptor {
    attrs: Box<SECURITY_ATTRIBUTES>,
    sd: PSECURITY_DESCRIPTOR,
}

impl SecurityDescriptor {
    /// Pointer to pass as `lpSecurityAttributes`, valid for `self`'s lifetime.
    fn as_ptr(&mut self) -> *mut c_void {
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

/// Builds the pipe's DACL: SYSTEM full access, plus read/write for `session`'s signed-in
/// user (identified by their logon SID, not their user SID, so it is specific to this
/// logon). SYSTEM only when no user is signed into `session`.
fn build_security_descriptor(session: u32) -> anyhow::Result<SecurityDescriptor> {
    let sddl = match session_logon_sid(session) {
        Ok(Some(sid)) => format!("O:SYG:SYD:(A;;GA;;;SY)(A;;GRGW;;;{sid})"),
        Ok(None) => "O:SYG:SYD:(A;;GA;;;SY)".to_string(),
        Err(e) => {
            tracing::warn!(session, error = %e, "could not read the session's logon SID; SYSTEM-only pipe");
            "O:SYG:SYD:(A;;GA;;;SY)".to_string()
        }
    };
    let wide = to_wide(&sddl);
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
    Ok(SecurityDescriptor { attrs, sd })
}

/// The string SID of the logon session of whichever user is signed into `session`, or
/// `None` when nobody is.
fn session_logon_sid(session: u32) -> anyhow::Result<Option<String>> {
    let mut raw_token = HANDLE::default();
    // SAFETY: valid out-pointer; this fails when no user is signed into `session`, which
    // the caller treats as "no logon SID" rather than an error.
    if unsafe { WTSQueryUserToken(session, &mut raw_token) }.is_err() {
        return Ok(None);
    }
    // SAFETY: `raw_token` was just produced by `WTSQueryUserToken` above.
    let token = unsafe { Owned::new(raw_token) };

    let mut needed = 0u32;
    // SAFETY: a null buffer with zero length only asks for the required size.
    let _ = unsafe { GetTokenInformation(*token, TokenGroups, None, 0, &mut needed) };
    anyhow::ensure!(needed > 0, "GetTokenInformation reported no size for TokenGroups");
    let mut buf = vec![0u8; needed as usize];
    // SAFETY: `buf` is exactly `needed` bytes, matching `tokeninformationlength`.
    unsafe {
        GetTokenInformation(*token, TokenGroups, Some(buf.as_mut_ptr().cast()), needed, &mut needed)
    }
    .context("GetTokenInformation(TokenGroups)")?;
    // SAFETY: the kernel just wrote a `TOKEN_GROUPS` header into `buf`, followed by
    // `GroupCount` `SID_AND_ATTRIBUTES` entries, all within `buf`'s bounds.
    let (count, groups_ptr) = unsafe {
        let header = &*(buf.as_ptr() as *const TOKEN_GROUPS);
        (header.GroupCount as usize, header.Groups.as_ptr())
    };
    // SAFETY: `groups_ptr` and `count` describe entries that live inside `buf`, which is
    // still in scope.
    let groups = unsafe { std::slice::from_raw_parts(groups_ptr, count) };
    let Some(sid) = groups
        .iter()
        .find(|g| g.Attributes & (SE_GROUP_LOGON_ID as u32) != 0)
        .map(|g| g.Sid)
    else {
        return Ok(None);
    };
    // `sid` points inside `buf`, which is still alive here.
    Ok(Some(sid_to_string(sid)?))
}

fn sid_to_string(sid: PSID) -> anyhow::Result<String> {
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

/// `%ProgramFiles%`, read from the shell rather than the environment (which a caller
/// could otherwise spoof for itself).
fn program_files() -> anyhow::Result<PathBuf> {
    // SAFETY: `FOLDERID_ProgramFiles` is a valid, static GUID; the returned string is
    // freed with `CoTaskMemFree` below, as `SHGetKnownFolderPath` documents.
    let raw = unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramFiles, KNOWN_FOLDER_FLAG(0), None) }
        .context("SHGetKnownFolderPath(FOLDERID_ProgramFiles)")?;
    // SAFETY: `raw` is non-null and NUL-terminated per the call above succeeding.
    let s = unsafe { raw.to_string() }.context("%ProgramFiles% was not valid UTF-16")?;
    // SAFETY: `raw` was allocated by `SHGetKnownFolderPath`, which documents `CoTaskMemFree`.
    unsafe { CoTaskMemFree(Some(raw.0.cast())) };
    Ok(PathBuf::from(s))
}

/// The image path of the process on the other end of `pipe`, read from the kernel (never
/// a path the client itself claims).
fn caller_image(pipe: &NamedPipeServer) -> anyhow::Result<PathBuf> {
    let handle = HANDLE(pipe.as_raw_handle());
    let mut pid = 0u32;
    // SAFETY: `handle` is this process's own pipe server handle, valid for the call.
    unsafe { GetNamedPipeClientProcessId(handle, &mut pid) }
        .context("GetNamedPipeClientProcessId")?;
    // SAFETY: opened with the minimum access needed to read the image path; closed via
    // `Owned` when it drops.
    let proc = unsafe {
        Owned::new(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).context("OpenProcess")?)
    };
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` outlives the call; `len` starts as its capacity in characters.
    unsafe {
        QueryFullProcessImageNameW(*proc, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len)
    }
    .context("QueryFullProcessImageNameW")?;
    Ok(PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
}

async fn serve(session: u32) -> anyhow::Result<()> {
    let injector = Arc::new(DesktopInjector::new()?);
    let name = secure::helper_pipe_name(session);

    // The first instance carries the DACL (Windows applies the security descriptor only
    // when the named pipe object itself is created); `first_pipe_instance` refuses to
    // start if something has already squatted this name.
    let mut server = {
        let mut sd = build_security_descriptor(session)?;
        // SAFETY: `sd` is a live `SECURITY_ATTRIBUTES`/`PSECURITY_DESCRIPTOR` pair used
        // only for the duration of this call, matching this method's safety contract.
        unsafe {
            ServerOptions::new()
                .pipe_mode(PipeMode::Byte)
                .reject_remote_clients(true)
                .first_pipe_instance(true)
                .create_with_security_attributes_raw(&name, sd.as_ptr())
        }
        .context("creating the helper pipe's first instance")?
    };

    loop {
        server.connect().await.context("waiting for a client")?;
        // Built before handling the connection, so a client is never left with no
        // pending server instance to connect to next.
        let next = ServerOptions::new()
            .pipe_mode(PipeMode::Byte)
            .reject_remote_clients(true)
            .create(&name)
            .context("creating the next helper pipe instance")?;
        if let Err(e) = handle_client(&mut server, &injector).await {
            tracing::debug!(error = %e, "helper client session ended");
        }
        server = next;
    }
}

/// One client's session: checks it is the installed agent, then serves it until it
/// disconnects or sends something it shouldn't.
async fn handle_client(pipe: &mut NamedPipeServer, injector: &Arc<DesktopInjector>) -> anyhow::Result<()> {
    let image = caller_image(pipe)?;
    let program_files = program_files()?;
    if !secure::caller_allowed(&image, &program_files) {
        tracing::warn!(image = %image.display(), "rejected helper client: not the installed agent");
        return Ok(());
    }

    let mut reader = FrameReader::default();
    let mut buf = [0u8; 4096];

    match read_message::<ToHelper>(pipe, &mut reader, &mut buf).await? {
        Some(ToHelper::Hello { proto }) if proto == HELPER_PROTO => {}
        _ => {
            tracing::debug!("rejected helper client: no (or wrong) Hello");
            return Ok(());
        }
    }

    let mut held = HeldInputs::default();
    let mut current: Option<DesktopKind> = None;
    let mut poll = tokio::time::interval(POLL_INTERVAL);

    loop {
        tokio::select! {
            _ = poll.tick() => {
                match injector.current() {
                    Ok(kind) if current != Some(kind) => {
                        current = Some(kind);
                        release_all(injector, &mut held);
                        if write_message(pipe, &FromHelper::Desktop(kind)).await.is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(e) => tracing::debug!(error = %e, "reading the current desktop failed"),
                }
            }
            msg = read_message::<ToHelper>(pipe, &mut reader, &mut buf) => {
                match msg? {
                    None => break,
                    Some(ToHelper::Hello { .. }) => {
                        tracing::debug!("unexpected second Hello; closing");
                        break;
                    }
                    Some(ToHelper::Inject(ev)) => {
                        if current == Some(DesktopKind::Protected) {
                            held.record(&ev);
                            match injector.inject(&ev) {
                                Ok(()) if matches!(ev, InputEvent::Motion { .. }) => {
                                    let (x, y) = injector.cursor();
                                    let _ = write_message(pipe, &FromHelper::Cursor { x, y }).await;
                                }
                                Ok(()) => {}
                                Err(e) => tracing::debug!(error = %e, "inject failed"),
                            }
                        } else {
                            tracing::debug!("dropped Inject: desktop is not protected");
                        }
                    }
                    Some(ToHelper::ReleaseAll) => release_all(injector, &mut held),
                }
            }
        }
    }

    release_all(injector, &mut held);
    Ok(())
}

fn release_all(injector: &DesktopInjector, held: &mut HeldInputs) {
    for ev in held.release_all() {
        if let Err(e) = injector.inject(&ev) {
            tracing::debug!(error = %e, "releasing held input failed");
        }
    }
}

async fn read_message<T: serde::de::DeserializeOwned>(
    pipe: &mut NamedPipeServer,
    reader: &mut FrameReader,
    buf: &mut [u8],
) -> anyhow::Result<Option<T>> {
    loop {
        if let Some(m) = reader.next::<T>().map_err(|e| anyhow::anyhow!(e.to_string()))? {
            return Ok(Some(m));
        }
        let n = pipe.read(buf).await.context("reading from the pipe")?;
        if n == 0 {
            return Ok(None);
        }
        reader.push(&buf[..n]);
    }
}

async fn write_message<T: serde::Serialize>(pipe: &mut NamedPipeServer, m: &T) -> anyhow::Result<()> {
    pipe.write_all(&secure::encode(m)).await.context("writing to the pipe")
}
