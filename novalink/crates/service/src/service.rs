//! `novalink-service` under the Service Control Manager: keeps the SYSTEM helper
//! (`--helper`) running in whichever session is at the console.
//!
//! It re-evaluates [`decide::helper_action`] at start, on every session change and every
//! two seconds (to notice a helper that exited). The helper is started by duplicating
//! this process's own SYSTEM token into the console session, which is what lets it
//! reach the secure desktop there.

use crate::decide::{HelperAction, RestartBackoff, helper_action};
use crate::winutil;
use anyhow::Context;
use std::ffi::OsString;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TOKEN_ALL_ACCESS, TokenPrimary,
    TokenSessionId,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken,
    PROCESS_INFORMATION, ResumeThread, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};
use windows::core::{Owned, PCWSTR, PWSTR};
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
    ServiceType, SessionChangeReason,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

/// Service name registered with the SCM; the display name is `NovaLink`.
pub const SERVICE_NAME: &str = "NovaLinkService";

/// How often a helper that exited on its own is noticed.
const TICK: Duration = Duration::from_secs(2);
/// How long to wait for a terminated helper to be gone.
const REAP_TIMEOUT_MS: u32 = 5000;
/// `WTSGetActiveConsoleSessionId` when no session is attached to the console.
const NO_SESSION: u32 = 0xFFFF_FFFF;

define_windows_service!(ffi_service_main, service_main);

/// Runs as the service. Fails when not started by the SCM (for example from a console).
pub fn run() -> anyhow::Result<()> {
    let _guard = winutil::init_logging("novalink-service");
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .context("not started by the Service Control Manager")
}

enum Event {
    Stop,
    /// A user signed in or out of `session`.
    UserChanged(u32),
    /// Something else about the sessions changed.
    Sessions,
}

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service() {
        tracing::error!(error = %e, "service stopped with an error");
    }
}

fn status(state: ServiceState, accept: ServiceControlAccept, exit: u32) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accept,
        exit_code: ServiceExitCode::Win32(exit),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    }
}

fn run_service() -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel();
    let handler = service_control_handler::register(SERVICE_NAME, move |control| match control {
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = tx.send(Event::Stop);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::SessionChange(p) => {
            let _ = tx.send(match p.reason {
                SessionChangeReason::SessionLogon | SessionChangeReason::SessionLogoff => {
                    Event::UserChanged(p.notification.session_id)
                }
                _ => Event::Sessions,
            });
            ServiceControlHandlerResult::NoError
        }
        _ => ServiceControlHandlerResult::NotImplemented,
    })
    .context("registering the service control handler")?;

    let accept = ServiceControlAccept::STOP
        | ServiceControlAccept::SHUTDOWN
        | ServiceControlAccept::SESSION_CHANGE;
    let mut supervisor = Supervisor::new()?;
    handler.set_service_status(status(ServiceState::Running, accept, 0))?;
    tracing::info!("novalink service running");

    let mut user_changed = false;
    loop {
        supervisor.step(user_changed);
        user_changed = false;
        match rx.recv_timeout(TICK) {
            Ok(Event::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Event::UserChanged(session)) => user_changed = Some(session) == console_session(),
            Ok(Event::Sessions) | Err(RecvTimeoutError::Timeout) => {}
        }
    }

    supervisor.stop_helper();
    tracing::info!("novalink service stopped");
    handler.set_service_status(status(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        0,
    ))?;
    Ok(())
}

/// The session attached to the physical console, if any.
fn console_session() -> Option<u32> {
    // SAFETY: plain call, no arguments.
    match unsafe { WTSGetActiveConsoleSessionId() } {
        NO_SESSION => None,
        id => Some(id),
    }
}

struct Helper {
    session: u32,
    process: Owned<HANDLE>,
    started: Instant,
}

impl Helper {
    fn exited(&self) -> bool {
        // SAFETY: `process` is a live process handle owned by `self`.
        let wait = unsafe { WaitForSingleObject(*self.process, 0) };
        wait == WAIT_OBJECT_0
    }
}

/// A job object that kills whatever is in it when its last handle closes, so a stopped
/// or crashed service never leaves a helper holding the pipe name or locking the exe.
fn kill_on_close_job() -> anyhow::Result<Owned<HANDLE>> {
    // SAFETY: plain call; the new handle is owned by the returned `Owned`.
    let job =
        unsafe { Owned::new(CreateJobObjectW(None, PCWSTR::null()).context("CreateJobObjectW")?) };
    let info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
        BasicLimitInformation:
            windows::Win32::System::JobObjects::JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                ..Default::default()
            },
        ..Default::default()
    };
    // SAFETY: `job` is a valid job handle; `info` is a fully initialised structure of
    // the class and size passed.
    unsafe {
        SetInformationJobObject(
            *job,
            JobObjectExtendedLimitInformation,
            std::ptr::addr_of!(info).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    }
    .context("SetInformationJobObject")?;
    Ok(job)
}

/// Owns the helper process and decides when to (re)start it.
struct Supervisor {
    /// Every helper is put in this job; dropping it (service exit) kills the helper.
    job: Owned<HANDLE>,
    helper: Option<Helper>,
    backoff: RestartBackoff,
    /// No new helper before this instant (restart backoff).
    not_before: Instant,
}

impl Supervisor {
    fn new() -> anyhow::Result<Self> {
        Ok(Self {
            job: kill_on_close_job()?,
            helper: None,
            backoff: RestartBackoff::new(),
            not_before: Instant::now(),
        })
    }

    fn step(&mut self, user_changed: bool) {
        let console = console_session();
        let running = self.helper.as_ref().map(|h| h.session);
        let exited = self.helper.as_ref().is_some_and(Helper::exited);
        match helper_action(console, running, exited, user_changed) {
            HelperAction::Nothing => {}
            HelperAction::Stop => self.stop_helper(),
            HelperAction::Start(session) => self.start_helper(session),
            HelperAction::Restart(session) => {
                // Only a helper that died by itself is backed off; a restart the service
                // chose (new session or user) starts straight away.
                let died = exited && running == Some(session) && !user_changed;
                let uptime = self.helper.as_ref().map(|h| h.started.elapsed());
                self.stop_helper();
                if died {
                    let delay = self.backoff.after_exit(uptime.unwrap_or_default());
                    tracing::warn!(session, ?delay, "helper exited; restarting after a delay");
                    self.not_before = Instant::now() + delay;
                } else {
                    self.backoff.reset();
                    self.not_before = Instant::now();
                }
                self.start_helper(session);
            }
        }
    }

    fn start_helper(&mut self, session: u32) {
        if Instant::now() < self.not_before {
            return;
        }
        match spawn_helper(session, *self.job) {
            Ok(process) => {
                tracing::info!(session, "helper started");
                self.helper = Some(Helper {
                    session,
                    process,
                    started: Instant::now(),
                });
            }
            Err(e) => {
                let delay = self.backoff.after_exit(Duration::ZERO);
                tracing::error!(session, error = %e, ?delay, "could not start the helper");
                self.not_before = Instant::now() + delay;
            }
        }
    }

    /// Ends the helper (if it is still running) and waits for it to be gone.
    fn stop_helper(&mut self) {
        let Some(helper) = self.helper.take() else {
            return;
        };
        if !helper.exited() {
            // SAFETY: `process` is a live process handle owned by `helper`.
            if let Err(e) = unsafe { TerminateProcess(*helper.process, 1) } {
                tracing::warn!(error = %e, "could not terminate the helper");
            }
            // SAFETY: as above.
            unsafe { WaitForSingleObject(*helper.process, REAP_TIMEOUT_MS) };
        }
        tracing::info!(session = helper.session, "helper stopped");
    }
}

/// Starts `<this exe> --helper` as SYSTEM in `session` on the interactive desktop's
/// window station, by moving a copy of this process's token into that session.
fn spawn_helper(session: u32, job: HANDLE) -> anyhow::Result<Owned<HANDLE>> {
    let exe = std::env::current_exe().context("finding this executable")?;
    let exe_wide = winutil::to_wide(&exe.to_string_lossy());
    let mut cmdline = winutil::to_wide(&format!("\"{}\" --helper", exe.display()));
    let mut desktop = winutil::to_wide("winsta0\\default");

    let mut raw = HANDLE::default();
    // SAFETY: valid out-pointer; the pseudo-handle from `GetCurrentProcess` needs no close.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut raw) }
        .context("OpenProcessToken")?;
    // SAFETY: `raw` was just produced by `OpenProcessToken`.
    let own = unsafe { Owned::new(raw) };

    let mut raw = HANDLE::default();
    // SAFETY: `own` is a valid token; the out-pointer is valid.
    unsafe {
        DuplicateTokenEx(
            *own,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut raw,
        )
    }
    .context("DuplicateTokenEx")?;
    // SAFETY: `raw` was just produced by `DuplicateTokenEx`.
    let token = unsafe { Owned::new(raw) };

    // SAFETY: `token` is a valid primary token; `session` outlives the call and its size
    // is passed. Needs SeTcbPrivilege, which LocalSystem has.
    unsafe {
        SetTokenInformation(
            *token,
            TokenSessionId,
            std::ptr::addr_of!(session).cast(),
            std::mem::size_of::<u32>() as u32,
        )
    }
    .context("SetTokenInformation(TokenSessionId)")?;

    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: `exe_wide`, `cmdline` (mutable, as the API requires) and `desktop` are
    // NUL-terminated and outlive the call; `token` is a valid primary token; `startup`
    // and `info` are valid for the call.
    unsafe {
        CreateProcessAsUserW(
            Some(*token),
            PCWSTR(exe_wide.as_ptr()),
            Some(PWSTR(cmdline.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NO_WINDOW | CREATE_SUSPENDED,
            None,
            PCWSTR::null(),
            &startup,
            &mut info,
        )
    }
    .context("CreateProcessAsUserW")?;
    // SAFETY: both handles were just returned by `CreateProcessAsUserW` and are owned here.
    let (process, thread) = unsafe { (Owned::new(info.hProcess), Owned::new(info.hThread)) };

    // Started suspended so it is in the job before it runs a single instruction.
    // SAFETY: `job` and `process` are valid handles.
    if let Err(e) = unsafe { AssignProcessToJobObject(job, *process) } {
        // SAFETY: `process` is valid; never let a helper run outside the job.
        unsafe {
            let _ = TerminateProcess(*process, 1);
        }
        return Err(anyhow::Error::new(e).context("AssignProcessToJobObject"));
    }
    // SAFETY: `thread` is the valid, suspended initial thread of the new process.
    anyhow::ensure!(
        unsafe { ResumeThread(*thread) } != u32::MAX,
        "ResumeThread failed"
    );
    Ok(process)
}
