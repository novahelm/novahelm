//! The user agent's side of the protected-screen helper: [`ProtectedPlatform`] wraps the
//! normal [`Platform`] and, while Windows shows a protected screen (the lock/PIN screen,
//! a UAC prompt), sends input to the SYSTEM helper over its pipe instead of injecting
//! directly, which Windows would refuse.
//!
//! Everything here must stay off the engine's back: the pipe lives on its own thread,
//! and [`Platform::inject`] only does a non-blocking push onto a bounded queue. A full or
//! closed queue means the helper is gone, so input goes back to the direct path.
//!
//! The routing and bookkeeping ([`Shared`]) is portable so it can be tested anywhere;
//! only the pipe thread and the access-denied probe are Windows-only.

use crate::secure::{FromHelper, Route, ToHelper};
use novalink_core::{InputEvent, LocalEvent, Platform};
use novalink_proto::DisplayInfo;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::mpsc::{Sender, UnboundedSender};

/// Messages that may wait for the pipe writer before the helper is treated as stuck.
#[cfg_attr(not(windows), allow(dead_code))]
const WRITE_QUEUE: usize = 256;

#[derive(Default)]
struct State {
    route: Route,
    /// The helper has said something on the current connection.
    up: bool,
    /// Queue to the connection's writer; `None` while there is no connection.
    writer: Option<Sender<ToHelper>>,
    /// The helper's last reported cursor on the current protected visit.
    cursor: Option<(f64, f64)>,
    /// The engine's capture channel, once `start_capture` has run.
    capture: Option<UnboundedSender<LocalEvent>>,
    /// The effective desktop changed and `take_desktop_switch` has not yet said so.
    switched: bool,
}

impl State {
    fn note_switch(&mut self, switched: bool) {
        if switched {
            self.switched = true;
            // A position from an earlier visit says nothing about this one.
            self.cursor = None;
        }
    }
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    stop: AtomicBool,
}

// The pipe thread drives the connection methods; elsewhere only the tests do.
#[cfg_attr(not(windows), allow(dead_code))]
impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    fn is_up(&self) -> bool {
        self.lock().up
    }

    /// A pipe connection is open and `writer` feeds it.
    fn link_up(&self, writer: Sender<ToHelper>) {
        self.lock().writer = Some(writer);
    }

    /// The connection ended or was given up on.
    fn link_down(&self) {
        let mut st = self.lock();
        st.writer = None;
        if st.up {
            st.up = false;
            let switched = st.route.on_helper(false);
            st.note_switch(switched);
        }
    }

    fn on_message(&self, m: FromHelper) {
        let mut st = self.lock();
        if st.writer.is_none() {
            // Given up on (or between connections): ignore stragglers.
            return;
        }
        if !st.up {
            // Trusted only once it has spoken, so a client the helper turns away
            // never counts as a working helper.
            st.up = true;
            let switched = st.route.on_helper(true);
            st.note_switch(switched);
        }
        match m {
            FromHelper::Desktop(kind) => {
                let switched = st.route.on_desktop(kind);
                st.note_switch(switched);
            }
            FromHelper::Cursor { x, y } => {
                if !st.route.use_helper() {
                    return;
                }
                st.cursor = Some((x, y));
                // The helper moved the cursor on the engine's behalf; tell the engine
                // as injected motion so its edge detection runs, as it does for the
                // direct path.
                if let Some(tx) = &st.capture {
                    let _ = tx.send(LocalEvent {
                        event: InputEvent::Motion { dx: 0.0, dy: 0.0 },
                        physical: false,
                        t_us: novalink_core::time::mono_us(),
                        pos: (x, y),
                    });
                }
            }
        }
    }

    /// `None` when input should go the direct way; otherwise the result of queueing
    /// it for the helper. A queue that is full or closed marks the helper down.
    fn route_inject(&self, ev: &InputEvent) -> Option<anyhow::Result<()>> {
        let mut st = self.lock();
        if !st.route.use_helper() {
            return None;
        }
        let sent = st
            .writer
            .as_ref()
            .map(|w| w.try_send(ToHelper::Inject(*ev)));
        if let Some(Ok(())) = sent {
            return Some(Ok(()));
        }
        // Dropping the writer ends the connection once its queue drains.
        st.writer = None;
        st.up = false;
        let switched = st.route.on_helper(false);
        st.note_switch(switched);
        Some(Err(anyhow::anyhow!(
            "the protected-screen helper is not accepting input"
        )))
    }
}

/// Wraps `P`, adding the protected-screen path.
pub struct ProtectedPlatform<P: Platform> {
    inner: P,
    shared: Arc<Shared>,
}

impl<P: Platform> ProtectedPlatform<P> {
    /// Wraps `inner` and, on Windows, starts looking for this session's helper.
    pub fn new(inner: P) -> Self {
        let me = Self::detached(inner);
        #[cfg(windows)]
        pipe::spawn(me.shared.clone());
        me
    }

    /// No pipe thread: the helper never connects unless the caller drives `Shared`.
    fn detached(inner: P) -> Self {
        Self {
            inner,
            shared: Arc::new(Shared::default()),
        }
    }
}

impl<P: Platform> Drop for ProtectedPlatform<P> {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
    }
}

impl<P: Platform> Platform for ProtectedPlatform<P> {
    fn displays(&self) -> Vec<DisplayInfo> {
        self.inner.displays()
    }

    fn start_capture(&self, tx: UnboundedSender<LocalEvent>) -> anyhow::Result<()> {
        self.inner.start_capture(tx.clone())?;
        self.shared.lock().capture = Some(tx);
        Ok(())
    }

    fn set_suppress(&self, on: bool) {
        self.inner.set_suppress(on);
    }

    fn inject(&self, ev: &InputEvent) -> anyhow::Result<()> {
        match self.shared.route_inject(ev) {
            Some(r) => r,
            None => self.inner.inject(ev),
        }
    }

    fn warp(&self, pos: (f64, f64)) {
        self.inner.warp(pos);
    }

    fn set_remote_pointer(&self, on: bool) {
        self.inner.set_remote_pointer(on);
    }

    fn cursor_pos(&self) -> (f64, f64) {
        let st = self.shared.lock();
        match st.cursor {
            Some(c) if st.route.use_helper() => c,
            _ => {
                drop(st);
                self.inner.cursor_pos()
            }
        }
    }

    fn permissions(&self) -> Vec<(&'static str, bool)> {
        let mut perms = self.inner.permissions();
        perms.push(("protected_screens", self.shared.is_up()));
        perms
    }

    fn take_health_events(&self) -> Vec<&'static str> {
        self.inner.take_health_events()
    }

    fn take_desktop_switch(&self) -> bool {
        // Both are drained, so the inner platform's switch is never left pending.
        let inner = self.inner.take_desktop_switch();
        let ours = std::mem::take(&mut self.shared.lock().switched);
        inner || ours
    }

    fn request_permissions(&self) {
        self.inner.request_permissions();
    }
}

/// Lets something happen once per protected-screen visit, until it is reset.
#[derive(Default)]
#[cfg_attr(not(windows), allow(dead_code))] // the pipe thread uses it; elsewhere only tests
struct OncePerVisit(bool);

#[cfg_attr(not(windows), allow(dead_code))]
impl OncePerVisit {
    /// True the first time since the last reset.
    fn first(&mut self) -> bool {
        !std::mem::replace(&mut self.0, true)
    }

    fn reset(&mut self) {
        self.0 = false;
    }
}

#[cfg(windows)]
mod pipe {
    use super::*;
    use crate::secure::{FrameReader, HELPER_PROTO, encode, helper_pipe_name};
    use anyhow::Context;
    use std::os::windows::io::AsRawHandle;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
    use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, HANDLE, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
    use windows::Win32::Security::{
        IsWellKnownSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, WinLocalSystemSid,
    };
    use windows::Win32::System::Pipes::GetNamedPipeServerSessionId;
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, DESKTOP_READOBJECTS, OpenInputDesktop,
    };
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::core::HRESULT;

    /// How often to look for a helper, and so how long a gap between pipe attempts.
    const RETRY: Duration = Duration::from_secs(2);
    /// How often to check for a protected screen while no helper is connected.
    const PROBE: Duration = Duration::from_millis(500);
    /// How long one write to the helper may take before it is treated as stuck.
    const WRITE_TIMEOUT: Duration = Duration::from_secs(1);

    pub(super) fn spawn(shared: Arc<Shared>) {
        let session = match own_session() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "protected screens unavailable: unknown session");
                return;
            }
        };
        let name = helper_pipe_name(session);
        let started = std::thread::Builder::new()
            .name("protected-pipe".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::warn!(error = %e, "protected screens unavailable: no runtime");
                        return;
                    }
                };
                rt.block_on(run(shared, name, session));
            });
        if let Err(e) = started {
            tracing::warn!(error = %e, "protected screens unavailable: no pipe thread");
        }
    }

    fn own_session() -> anyhow::Result<u32> {
        let mut session = 0u32;
        // SAFETY: plain FFI calls; `session` is a valid out-pointer.
        unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) }
            .context("ProcessIdToSessionId")?;
        Ok(session)
    }

    async fn run(shared: Arc<Shared>, name: String, session: u32) {
        // Reported once per protected visit: reset when the helper really comes up or the
        // desktop is back to normal, never merely because the pipe opened.
        let mut missing = OncePerVisit::default();
        // Why the server was last turned away, so a persistent rejection is logged once
        // per reason per run, however often the pipe is retried.
        let mut rejected: Option<String> = None;
        while !shared.stopped() {
            match ClientOptions::new().open(&name) {
                Ok(client) => match verify_server(&client, session) {
                    Ok(()) => {
                        let mut spoke = false;
                        if let Err(e) = serve(&shared, client, &mut spoke).await {
                            tracing::debug!(error = %e, "helper connection ended");
                        }
                        if spoke {
                            missing.reset();
                            rejected = None;
                        }
                    }
                    Err(e) => {
                        // Dropping `client` closes the pipe before anything was sent.
                        let why = e.to_string();
                        if rejected.as_deref() != Some(why.as_str()) {
                            tracing::warn!(error = %why, "not using the helper pipe: its server is not the NovaLink service");
                            rejected = Some(why);
                        }
                    }
                },
                Err(e) => tracing::trace!(error = %e, "helper pipe not available"),
            }
            for _ in 0..(RETRY.as_millis() / PROBE.as_millis()) {
                if shared.stopped() {
                    return;
                }
                if shared.is_up() || !input_desktop_denied() {
                    missing.reset();
                } else if missing.first() {
                    tracing::warn!("protected screen, helper not running");
                }
                tokio::time::sleep(PROBE).await;
            }
        }
    }

    /// Confirms the pipe's server is a SYSTEM process in this session, before anything
    /// (a PIN, say) is sent to it. The helper creates its pipe only after sign-in, so
    /// another process could otherwise get there first and impersonate it.
    ///
    /// The check is the pipe's owner plus the session the server runs in, both read from
    /// the connected handle. Only SYSTEM can make a SYSTEM-owned pipe, and any SYSTEM
    /// process is already fully trusted, so no more is needed; the server's own process
    /// is deliberately not opened, as an ordinary user cannot open a SYSTEM process.
    /// Any failure rejects the server.
    fn verify_server(client: &NamedPipeClient, session: u32) -> anyhow::Result<()> {
        let pipe = HANDLE(client.as_raw_handle());
        let mut server_session = 0u32;
        // SAFETY: `pipe` is this process's own connected pipe handle; valid out-pointer.
        unsafe { GetNamedPipeServerSessionId(pipe, &mut server_session) }
            .context("GetNamedPipeServerSessionId")?;
        anyhow::ensure!(
            server_session == session,
            "pipe server runs in session {server_session}, not {session}"
        );
        ensure_pipe_owned_by_system(pipe)
    }

    /// The pipe object itself must be owned by LocalSystem. The helper creates its pipe
    /// with `O:SY`, and an ordinary user cannot make an object owned by SYSTEM, so a
    /// squatter cannot pass this. Read from
    /// the connected handle (not by name), so it is the very pipe we are about to use.
    /// Any failure to read it rejects the server.
    fn ensure_pipe_owned_by_system(pipe: HANDLE) -> anyhow::Result<()> {
        let mut owner = PSID::default();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `pipe` is this process's own connected pipe handle (opened with
        // GENERIC_READ, which includes READ_CONTROL); the out-pointers are valid. `owner`
        // points into `sd`, which is freed only after the check below.
        let err = unsafe {
            GetSecurityInfo(
                pipe,
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION,
                Some(&mut owner),
                None,
                None,
                None,
                Some(&mut sd),
            )
        };
        err.ok().context("GetSecurityInfo on the pipe")?;
        let valid = !owner.0.is_null();
        // SAFETY: `owner` is a valid SID inside `sd` (checked non-null above).
        let system = valid && unsafe { IsWellKnownSid(owner, WinLocalSystemSid).as_bool() };
        // SAFETY: `sd` was allocated by `GetSecurityInfo`, which documents `LocalFree`.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
        }
        anyhow::ensure!(system, "the pipe is not owned by LocalSystem");
        Ok(())
    }

    /// True when this process may not open the desktop that has input, which is what
    /// a protected screen looks like to an ordinary user process.
    fn input_desktop_denied() -> bool {
        // SAFETY: plain call; a returned handle is closed below.
        match unsafe { OpenInputDesktop(Default::default(), false, DESKTOP_READOBJECTS) } {
            Ok(desk) => {
                // SAFETY: `desk` was just opened here and is not used again.
                unsafe {
                    let _ = CloseDesktop(desk);
                }
                false
            }
            Err(e) => e.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0),
        }
    }

    /// Writes one frame, giving up if the helper has stopped reading.
    async fn write_frame<W: AsyncWriteExt + Unpin>(wr: &mut W, bytes: &[u8]) -> anyhow::Result<()> {
        match tokio::time::timeout(WRITE_TIMEOUT, wr.write_all(bytes)).await {
            Ok(r) => r.context("writing to the helper"),
            Err(_) => anyhow::bail!("the helper stopped reading"),
        }
    }

    /// One connection's lifetime: says `Hello`, then forwards queued messages out and
    /// helper messages in until either side ends it. `spoke` is set once the helper has
    /// said something.
    async fn serve(
        shared: &Shared,
        client: NamedPipeClient,
        spoke: &mut bool,
    ) -> anyhow::Result<()> {
        let (mut rd, mut wr) = tokio::io::split(client);
        write_frame(
            &mut wr,
            &encode(&ToHelper::Hello {
                proto: HELPER_PROTO,
            }),
        )
        .await?;
        let (tx, mut rx) = tokio::sync::mpsc::channel(WRITE_QUEUE);
        shared.link_up(tx);

        let mut reader = FrameReader::default();
        let mut buf = [0u8; 4096];
        let mut tick = tokio::time::interval(PROBE);
        let result: anyhow::Result<()> = 'conn: loop {
            tokio::select! {
                queued = rx.recv() => match queued {
                    Some(m) => {
                        if let Err(e) = write_frame(&mut wr, &encode(&m)).await {
                            break 'conn Err(e);
                        }
                    }
                    // `inject` gave up on this connection.
                    None => break 'conn Ok(()),
                },
                read = rd.read(&mut buf) => match read {
                    Ok(0) => break 'conn Ok(()),
                    Ok(n) => {
                        reader.push(&buf[..n]);
                        loop {
                            match reader.next::<FromHelper>() {
                                Ok(Some(m)) => {
                                    shared.on_message(m);
                                    *spoke |= shared.is_up();
                                }
                                Ok(None) => break,
                                Err(e) => break 'conn Err(anyhow::anyhow!(e.to_string())),
                            }
                        }
                    }
                    Err(e) => break 'conn Err(anyhow::Error::new(e).context("reading from the helper")),
                },
                _ = tick.tick() => {
                    if shared.stopped() {
                        break 'conn Ok(());
                    }
                }
            }
        };
        shared.link_down();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secure::DesktopKind;
    use novalink_core::FakePlatform;
    use novalink_proto::keys;
    use tokio::sync::mpsc::{channel, unbounded_channel};

    fn key(down: bool) -> InputEvent {
        InputEvent::Key {
            usage: keys::A,
            down,
        }
    }

    /// A platform whose helper has connected and reported `kind`.
    fn linked(
        kind: DesktopKind,
    ) -> (
        ProtectedPlatform<FakePlatform>,
        FakePlatform,
        tokio::sync::mpsc::Receiver<ToHelper>,
    ) {
        let fake = FakePlatform::new();
        let p = ProtectedPlatform::detached(fake.clone());
        let (tx, rx) = channel(WRITE_QUEUE);
        p.shared.link_up(tx);
        p.shared.on_message(FromHelper::Desktop(kind));
        (p, fake, rx)
    }

    #[test]
    fn once_per_visit_repeats_only_after_a_reset() {
        let mut o = OncePerVisit::default();
        assert!(o.first());
        assert!(!o.first());
        assert!(!o.first());
        o.reset();
        assert!(o.first());
    }

    #[test]
    fn without_a_helper_everything_goes_direct() {
        let fake = FakePlatform::new();
        let p = ProtectedPlatform::detached(fake.clone());
        p.shared.lock().route.on_desktop(DesktopKind::Protected);
        p.inject(&key(true)).unwrap();
        assert!(fake.injected_contains(&key(true)));
        assert!(!p.take_desktop_switch());
        assert_eq!(p.permissions().last(), Some(&("protected_screens", false)));
    }

    #[test]
    fn protected_desktop_sends_input_to_the_helper() {
        let (p, fake, mut rx) = linked(DesktopKind::Protected);
        p.inject(&key(true)).unwrap();
        assert_eq!(rx.try_recv().unwrap(), ToHelper::Inject(key(true)));
        assert!(!fake.injected_contains(&key(true)));
        assert_eq!(p.permissions().last(), Some(&("protected_screens", true)));
    }

    #[test]
    fn normal_desktop_stays_direct_even_with_a_helper() {
        let (p, fake, mut rx) = linked(DesktopKind::Normal);
        p.inject(&key(true)).unwrap();
        assert!(fake.injected_contains(&key(true)));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_full_queue_marks_the_helper_down() {
        let fake = FakePlatform::new();
        let p = ProtectedPlatform::detached(fake.clone());
        let (tx, mut rx) = channel(1);
        p.shared.link_up(tx);
        p.shared
            .on_message(FromHelper::Desktop(DesktopKind::Protected));
        p.inject(&key(true)).unwrap();
        assert!(p.inject(&key(false)).is_err(), "queue of one is full");
        // Back to the direct path, and the engine is told the desktop changed.
        p.inject(&key(false)).unwrap();
        assert!(fake.injected_contains(&key(false)));
        assert_eq!(p.permissions().last(), Some(&("protected_screens", false)));
        assert_eq!(rx.try_recv().unwrap(), ToHelper::Inject(key(true)));
        assert!(rx.try_recv().is_err());
        assert!(p.take_desktop_switch());
    }

    #[test]
    fn a_closed_queue_marks_the_helper_down() {
        let (p, fake, rx) = linked(DesktopKind::Protected);
        drop(rx);
        assert!(p.inject(&key(true)).is_err());
        p.inject(&key(true)).unwrap();
        assert!(fake.injected_contains(&key(true)));
    }

    #[test]
    fn desktop_switch_is_reported_once() {
        let (p, _fake, _rx) = linked(DesktopKind::Protected);
        assert!(p.take_desktop_switch());
        assert!(!p.take_desktop_switch());
        p.shared
            .on_message(FromHelper::Desktop(DesktopKind::Protected));
        assert!(!p.take_desktop_switch(), "no change, no switch");
        p.shared
            .on_message(FromHelper::Desktop(DesktopKind::Normal));
        assert!(p.take_desktop_switch());
        assert!(!p.take_desktop_switch());
    }

    #[test]
    fn losing_the_helper_while_protected_is_a_switch() {
        let (p, _fake, _rx) = linked(DesktopKind::Protected);
        p.take_desktop_switch();
        p.shared.link_down();
        assert!(p.take_desktop_switch());
        assert!(!p.shared.lock().route.use_helper());
    }

    #[test]
    fn the_inner_platforms_own_switch_is_passed_through() {
        let fake = FakePlatform::new();
        let p = ProtectedPlatform::detached(fake.clone());
        fake.push_desktop_switch();
        assert!(p.take_desktop_switch());
        assert!(!p.take_desktop_switch());
    }

    #[test]
    fn helper_cursor_becomes_injected_zero_motion_while_protected() {
        let (p, fake, _rx) = linked(DesktopKind::Protected);
        fake.warp((5.0, 6.0));
        let (tx, mut cap) = unbounded_channel();
        p.start_capture(tx).unwrap();
        p.shared
            .on_message(FromHelper::Cursor { x: 300.0, y: 200.0 });
        let e = cap.try_recv().unwrap();
        assert_eq!(e.event, InputEvent::Motion { dx: 0.0, dy: 0.0 });
        assert!(!e.physical);
        assert_eq!(e.pos, (300.0, 200.0));
        assert_eq!(p.cursor_pos(), (300.0, 200.0));
    }

    #[test]
    fn cursor_reports_are_ignored_off_the_protected_desktop_or_before_capture() {
        let (p, fake, _rx) = linked(DesktopKind::Normal);
        fake.warp((5.0, 6.0));
        let (tx, mut cap) = unbounded_channel();
        p.start_capture(tx).unwrap();
        p.shared
            .on_message(FromHelper::Cursor { x: 300.0, y: 200.0 });
        assert!(cap.try_recv().is_err());
        assert_eq!(p.cursor_pos(), (5.0, 6.0));

        // Protected but capture never started: nothing to push into, and no panic.
        let (p, _fake, _rx) = linked(DesktopKind::Protected);
        p.shared.on_message(FromHelper::Cursor { x: 1.0, y: 2.0 });
        assert_eq!(p.cursor_pos(), (1.0, 2.0));
    }

    #[test]
    fn a_stale_cursor_is_not_reused_on_the_next_visit() {
        let (p, fake, _rx) = linked(DesktopKind::Protected);
        fake.warp((5.0, 6.0));
        p.shared
            .on_message(FromHelper::Cursor { x: 300.0, y: 200.0 });
        p.shared
            .on_message(FromHelper::Desktop(DesktopKind::Normal));
        p.shared
            .on_message(FromHelper::Desktop(DesktopKind::Protected));
        assert_eq!(p.cursor_pos(), (5.0, 6.0));
    }

    #[test]
    fn messages_after_giving_up_are_ignored() {
        let (p, _fake, rx) = linked(DesktopKind::Protected);
        drop(rx);
        let _ = p.inject(&key(true)); // gives up on the connection
        p.shared
            .on_message(FromHelper::Desktop(DesktopKind::Protected));
        assert!(!p.shared.lock().route.use_helper());
    }

    #[test]
    fn capture_failure_leaves_no_channel_behind() {
        let fake = FakePlatform::new();
        fake.deny_capture(true);
        let p = ProtectedPlatform::detached(fake);
        let (tx, _rx) = unbounded_channel();
        assert!(p.start_capture(tx).is_err());
        assert!(p.shared.lock().capture.is_none());
    }
}
