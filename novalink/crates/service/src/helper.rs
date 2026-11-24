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
use novalink_platform_windows::secure::{
    self, DesktopKind, FrameReader, FromHelper, HELPER_PROTO, ToHelper,
};
use std::collections::HashSet;
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{NamedPipeServer, PipeMode, ServerOptions};
use tokio::sync::{mpsc, oneshot};
use tracing_appender::non_blocking::WorkerGuard;
use windows::Win32::Foundation::{HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    GetTokenInformation, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_ATTRIBUTES, SID_AND_ATTRIBUTES, TOKEN_GROUPS, TokenGroups,
};
use windows::Win32::Storage::FileSystem::{
    CreateDirectoryW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, GetFileAttributesW,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Pipes::GetNamedPipeClientProcessId;
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSQueryUserToken};
use windows::Win32::System::SystemServices::SE_GROUP_LOGON_ID;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::Win32::UI::Shell::{
    FOLDERID_ProgramData, FOLDERID_ProgramFiles, KNOWN_FOLDER_FLAG, SHGetKnownFolderPath,
};
use windows::core::{GUID, Owned, PCWSTR, PWSTR};

/// How often the current input desktop is polled while a client is connected.
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// How long a new client has to send its `Hello`.
const HELLO_TIMEOUT: Duration = Duration::from_secs(2);
/// How often to check whether a user has signed in yet.
const SIGN_IN_POLL: Duration = Duration::from_secs(2);

/// Protected DACL for `%ProgramData%\NovaLink`: SYSTEM and Administrators only.
const PROTECTED_DIR_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

/// SIDs allowed to own the log directories: SYSTEM and Administrators.
const TRUSTED_OWNERS: [&str; 2] = ["S-1-5-18", "S-1-5-32-544"];

pub fn run() -> anyhow::Result<()> {
    let _guard = init_logging();
    let session = own_session()?;
    tracing::info!(session, "novalink-service helper starting");
    let rt = tokio::runtime::Runtime::new().context("building the tokio runtime")?;
    let result = rt.block_on(serve(session));
    if let Err(e) = &result {
        tracing::error!(error = %e, "helper stopped");
    }
    result
}

/// Structured JSON-lines logging, daily-rotated, mirroring the agent's setup. This
/// process runs as SYSTEM, so it only writes to a log directory it has made or verified
/// (see [`prepare_log_dir`]); otherwise it logs to stderr and carries on.
fn init_logging() -> Option<WorkerGuard> {
    let filter = tracing_subscriber::EnvFilter::try_from_env("NOVALINK_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt()
        .json()
        .with_ansi(false)
        .with_env_filter(filter);
    let appender = prepare_log_dir().and_then(|dir| {
        tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix("novalink-service")
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
            eprintln!("novalink-service: not logging to disk: {e:#}");
            let _ = builder.with_writer(std::io::stderr).try_init();
            None
        }
    }
}

/// `%ProgramData%\NovaLink\logs`, created with a SYSTEM/Administrators-only DACL if it is
/// missing, or else verified to be a real directory (not a junction or symlink) owned by
/// SYSTEM or Administrators. A normal user can create entries under `%ProgramData%`, so
/// anything else there could redirect what SYSTEM writes.
fn prepare_log_dir() -> anyhow::Result<PathBuf> {
    let nova = known_folder(&FOLDERID_ProgramData)?.join("NovaLink");
    ensure_trusted_dir(&nova, Some(PROTECTED_DIR_SDDL))?;
    let logs = nova.join("logs");
    // Inherits the protected DACL from `NovaLink`.
    ensure_trusted_dir(&logs, None)?;
    Ok(logs)
}

fn ensure_trusted_dir(path: &Path, sddl: Option<&str>) -> anyhow::Result<()> {
    if std::fs::symlink_metadata(path).is_err() {
        // Creation errors are ignored: if someone else made it first, the check below
        // judges what is there.
        let _ = create_dir(path, sddl);
    }
    check_trusted_dir(path)
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

fn check_trusted_dir(path: &Path) -> anyhow::Result<()> {
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is NUL-terminated. Reads the attributes of the entry itself without
    // following a reparse point.
    let attrs = unsafe { GetFileAttributesW(PCWSTR(wide.as_ptr())) };
    anyhow::ensure!(attrs != u32::MAX, "{} cannot be read", path.display());
    anyhow::ensure!(
        attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0,
        "{} is not a directory",
        path.display()
    );
    anyhow::ensure!(
        attrs & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0,
        "{} is a reparse point",
        path.display()
    );
    let mut owner = PSID::default();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `wide` is NUL-terminated; the out-pointers are valid. `owner` points into
    // `sd`, which is freed below only after the owner has been converted to a string.
    let err = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            &mut sd,
        )
    };
    err.ok().context("GetNamedSecurityInfoW")?;
    let owner = sid_to_string(owner);
    // SAFETY: `sd` was allocated by `GetNamedSecurityInfoW`, which documents `LocalFree`.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
    let owner = owner?;
    anyhow::ensure!(
        TRUSTED_OWNERS.contains(&owner.as_str()),
        "{} is owned by {owner}",
        path.display()
    );
    Ok(())
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

impl SecurityDescriptor {
    fn from_sddl(sddl: &str) -> anyhow::Result<Self> {
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

/// Access the signed-in user gets on the pipe: FILE_READ_DATA, FILE_WRITE_DATA,
/// FILE_READ_EA, FILE_WRITE_EA, FILE_READ_ATTRIBUTES, FILE_WRITE_ATTRIBUTES,
/// READ_CONTROL and SYNCHRONIZE. Deliberately not the generic read/write mapping
/// (`GRGW`), which includes FILE_CREATE_PIPE_INSTANCE (0x4) and would let a user process
/// create its own server instance of this pipe.
const CLIENT_PIPE_MASK: &str = "0x0012019B";

/// Builds the pipe's DACL: SYSTEM full access, plus the access above for the signed-in
/// user, identified by their logon SID (not their user SID) so it is specific to this
/// logon.
fn build_security_descriptor(logon_sid: &str) -> anyhow::Result<SecurityDescriptor> {
    SecurityDescriptor::from_sddl(&format!(
        "O:SYG:SYD:(A;;GA;;;SY)(A;;{CLIENT_PIPE_MASK};;;{logon_sid})"
    ))
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
    anyhow::ensure!(
        needed > 0,
        "GetTokenInformation reported no size for TokenGroups"
    );
    // `u64` elements give the buffer the 8-byte alignment `TOKEN_GROUPS` needs.
    let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: `buf` holds at least `needed` bytes, matching `tokeninformationlength`.
    unsafe {
        GetTokenInformation(
            *token,
            TokenGroups,
            Some(buf.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )
    }
    .context("GetTokenInformation(TokenGroups)")?;
    let base = buf.as_ptr().cast::<TOKEN_GROUPS>();
    // SAFETY: the kernel wrote a `TOKEN_GROUPS` into the aligned `buf`; fields are read
    // through raw pointers (no reference to the one-element `Groups` array is made).
    let (count, groups) = unsafe {
        (
            std::ptr::addr_of!((*base).GroupCount).read() as usize,
            std::ptr::addr_of!((*base).Groups).cast::<SID_AND_ATTRIBUTES>(),
        )
    };
    let first = std::mem::offset_of!(TOKEN_GROUPS, Groups);
    anyhow::ensure!(
        first + count * std::mem::size_of::<SID_AND_ATTRIBUTES>() <= needed as usize,
        "TokenGroups claims more groups than it holds"
    );
    let mut logon = None;
    for i in 0..count {
        // SAFETY: the check above shows entry `i` lies within the bytes the kernel wrote.
        let g = unsafe { groups.add(i).read() };
        if g.Attributes & (SE_GROUP_LOGON_ID as u32) == SE_GROUP_LOGON_ID as u32 {
            logon = Some(g.Sid);
            break;
        }
    }
    let Some(sid) = logon else {
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

/// A known folder's path, read from the shell rather than the environment (which a
/// caller could otherwise spoof for itself).
fn known_folder(id: &GUID) -> anyhow::Result<PathBuf> {
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
        Owned::new(
            OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).context("OpenProcess")?,
        )
    };
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` outlives the call; `len` starts as its capacity in characters.
    unsafe {
        QueryFullProcessImageNameW(*proc, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len)
    }
    .context("QueryFullProcessImageNameW")?;
    Ok(PathBuf::from(String::from_utf16_lossy(
        &buf[..len as usize],
    )))
}

/// Work for the injector thread.
enum Cmd {
    Current(oneshot::Sender<anyhow::Result<DesktopKind>>),
    Inject(InputEvent, oneshot::Sender<anyhow::Result<()>>),
    Release(Vec<InputEvent>),
    Cursor(oneshot::Sender<(f64, f64)>),
}

/// Handle to the one OS thread that owns the `DesktopInjector`. `SetThreadDesktop`
/// applies to the calling thread, so every desktop call must come from that same
/// thread, which creates no windows or hooks; the async side only sends it commands.
struct Injector {
    tx: mpsc::UnboundedSender<Cmd>,
}

impl Injector {
    fn spawn() -> anyhow::Result<Self> {
        let (tx, mut rx) = mpsc::unbounded_channel::<Cmd>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<anyhow::Result<()>>();
        std::thread::Builder::new()
            .name("injector".into())
            .spawn(move || {
                let inj = match DesktopInjector::new() {
                    Ok(inj) => {
                        let _ = ready_tx.send(Ok(()));
                        inj
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                while let Some(cmd) = rx.blocking_recv() {
                    match cmd {
                        Cmd::Current(r) => {
                            let _ = r.send(inj.current());
                        }
                        Cmd::Inject(ev, r) => {
                            let _ = r.send(inj.inject(&ev));
                        }
                        Cmd::Release(events) => {
                            for ev in events {
                                if let Err(e) = inj.release(&ev) {
                                    tracing::debug!(error = %e, "releasing held input failed");
                                }
                            }
                        }
                        Cmd::Cursor(r) => {
                            let _ = r.send(inj.cursor());
                        }
                    }
                }
            })
            .context("starting the injector thread")?;
        ready_rx.recv().context("the injector thread died")??;
        Ok(Self { tx })
    }

    async fn ask<T>(&self, make: impl FnOnce(oneshot::Sender<T>) -> Cmd) -> anyhow::Result<T> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(make(reply))
            .map_err(|_| anyhow::anyhow!("the injector thread stopped"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("the injector thread stopped"))
    }

    async fn current(&self) -> anyhow::Result<DesktopKind> {
        self.ask(Cmd::Current).await?
    }

    async fn inject(&self, ev: InputEvent) -> anyhow::Result<()> {
        self.ask(|r| Cmd::Inject(ev, r)).await?
    }

    async fn cursor(&self) -> anyhow::Result<(f64, f64)> {
        self.ask(Cmd::Cursor).await
    }

    /// Queued behind everything already sent, so it runs in order.
    fn release_all(&self, held: &mut HeldInputs) {
        let events = held.release_all();
        if !events.is_empty() {
            let _ = self.tx.send(Cmd::Release(events));
        }
    }
}

async fn serve(session: u32) -> anyhow::Result<()> {
    let injector = Injector::spawn()?;
    let name = secure::helper_pipe_name(session);

    // Started before anyone has signed in, the helper would otherwise build a
    // SYSTEM-only pipe that the user's agent could never open. Wait for the logon SID.
    let mut said = false;
    let logon_sid = loop {
        match session_logon_sid(session) {
            Ok(Some(sid)) => break sid,
            Ok(None) => {
                if !std::mem::replace(&mut said, true) {
                    tracing::info!(
                        session,
                        "waiting for a user to sign in before opening the pipe"
                    );
                }
            }
            Err(e) => tracing::warn!(session, error = %e, "could not read the session's logon SID"),
        }
        tokio::time::sleep(SIGN_IN_POLL).await;
    };

    // The first instance carries the DACL (Windows applies the security descriptor only
    // when the named pipe object itself is created); `first_pipe_instance` refuses to
    // start if something has already squatted this name.
    let mut server = {
        let mut sd = build_security_descriptor(&logon_sid)?;
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

    let mut rejected = HashSet::new();
    loop {
        server.connect().await.context("waiting for a client")?;
        // Built before handling the connection, so a client is never left with no
        // pending server instance to connect to next.
        let next = ServerOptions::new()
            .pipe_mode(PipeMode::Byte)
            .reject_remote_clients(true)
            .create(&name)
            .context("creating the next helper pipe instance")?;
        if let Err(e) = handle_client(&mut server, &injector, &mut rejected).await {
            tracing::debug!(error = %e, "helper client session ended");
        }
        server = next;
    }
}

/// One client's session: checks it is the installed agent, then serves it until it
/// disconnects or sends something it shouldn't. Whatever it injected is released on
/// every way out.
async fn handle_client(
    pipe: &mut NamedPipeServer,
    injector: &Injector,
    rejected: &mut HashSet<String>,
) -> anyhow::Result<()> {
    let image = caller_image(pipe)?;
    let program_files = known_folder(&FOLDERID_ProgramFiles)?;
    if !secure::caller_allowed(&image, &program_files) {
        // Once per image, so a misbehaving process can't fill the log by reconnecting.
        if rejected.insert(image.to_string_lossy().to_lowercase()) {
            tracing::warn!(image = %image.display(), "rejected helper client: not the installed agent");
        }
        return Ok(());
    }

    let mut reader = FrameReader::default();
    let mut buf = [0u8; 4096];

    let hello = tokio::time::timeout(
        HELLO_TIMEOUT,
        read_message::<ToHelper>(pipe, &mut reader, &mut buf),
    )
    .await;
    match hello {
        Ok(Ok(Some(ToHelper::Hello { proto }))) if proto == HELPER_PROTO => {}
        _ => {
            tracing::debug!("rejected helper client: no (or wrong) Hello in time");
            return Ok(());
        }
    }

    let mut held = HeldInputs::default();
    let mut current: Option<DesktopKind> = None;
    let mut poll = tokio::time::interval(POLL_INTERVAL);

    let result: anyhow::Result<()> = loop {
        tokio::select! {
            _ = poll.tick() => {
                match injector.current().await {
                    Ok(kind) if current != Some(kind) => {
                        current = Some(kind);
                        injector.release_all(&mut held);
                        if let Err(e) = write_message(pipe, &FromHelper::Desktop(kind)).await {
                            break Err(e);
                        }
                    }
                    Ok(_) => {}
                    Err(e) => tracing::debug!(error = %e, "reading the current desktop failed"),
                }
            }
            msg = read_message::<ToHelper>(pipe, &mut reader, &mut buf) => {
                match msg {
                    Err(e) => {
                        tracing::warn!(error = %e, "closing helper client: unreadable message");
                        break Err(e);
                    }
                    Ok(None) => break Ok(()),
                    Ok(Some(ToHelper::Hello { .. })) => {
                        tracing::debug!("unexpected second Hello; closing");
                        break Ok(());
                    }
                    Ok(Some(ToHelper::Inject(ev))) => {
                        if current == Some(DesktopKind::Protected) {
                            held.record(&ev);
                            // The injector thread re-checks the desktop it opens itself.
                            match injector.inject(ev).await {
                                Ok(()) if matches!(ev, InputEvent::Motion { .. }) => {
                                    if let Ok((x, y)) = injector.cursor().await {
                                        let _ = write_message(pipe, &FromHelper::Cursor { x, y }).await;
                                    }
                                }
                                Ok(()) => {}
                                Err(e) => tracing::debug!(error = %e, "inject refused or failed"),
                            }
                        } else {
                            tracing::debug!("dropped Inject: desktop is not protected");
                        }
                    }
                    Ok(Some(ToHelper::ReleaseAll)) => injector.release_all(&mut held),
                }
            }
        }
    };

    injector.release_all(&mut held);
    result
}

async fn read_message<T: serde::de::DeserializeOwned>(
    pipe: &mut NamedPipeServer,
    reader: &mut FrameReader,
    buf: &mut [u8],
) -> anyhow::Result<Option<T>> {
    loop {
        if let Some(m) = reader
            .next::<T>()
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
        {
            return Ok(Some(m));
        }
        let n = pipe.read(buf).await.context("reading from the pipe")?;
        if n == 0 {
            return Ok(None);
        }
        reader.push(&buf[..n]);
    }
}

async fn write_message<T: serde::Serialize>(
    pipe: &mut NamedPipeServer,
    m: &T,
) -> anyhow::Result<()> {
    pipe.write_all(&secure::encode(m))
        .await
        .context("writing to the pipe")
}
