//! Starts `novalink-agent.exe` and keeps it running, unless the user turned NovaLink off.
//! The restart pacing is `supervisor::Backoff`.

use novalink_tray::supervisor::{Backoff, STABLE_AFTER};
use std::net::{SocketAddr, TcpStream};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const TICK: Duration = Duration::from_millis(250);

/// What the rest of the tray knows about the agent process.
pub struct Agent {
    off: AtomicBool,
    restarting: AtomicBool,
    spawn_error: Mutex<Option<String>>,
    tx: Sender<()>,
}

impl Agent {
    /// Starts the supervising thread. `off` is the persisted Off state.
    pub fn start(off: bool) -> std::sync::Arc<Agent> {
        let (tx, rx) = channel();
        let agent = std::sync::Arc::new(Agent {
            off: AtomicBool::new(off),
            restarting: AtomicBool::new(false),
            spawn_error: Mutex::new(None),
            tx,
        });
        let a = agent.clone();
        std::thread::spawn(move || supervise(&a, rx));
        agent
    }

    pub fn is_off(&self) -> bool {
        self.off.load(Ordering::SeqCst)
    }

    /// True from the moment the agent is known to be gone until its port answers again.
    pub fn is_restarting(&self) -> bool {
        self.restarting.load(Ordering::SeqCst)
    }

    /// Why the last attempt to start the agent failed, until a later attempt works.
    pub fn spawn_error(&self) -> Option<String> {
        self.spawn_error.lock().ok().and_then(|e| e.clone())
    }

    fn set_spawn_error(&self, e: Option<String>) {
        if let Ok(mut g) = self.spawn_error.lock() {
            *g = e;
        }
    }

    pub fn set_off(&self) {
        self.off.store(true, Ordering::SeqCst);
        self.restarting.store(false, Ordering::SeqCst);
    }

    /// Clears Off and starts the agent now.
    pub fn turn_on(&self) {
        self.set_spawn_error(None);
        self.off.store(false, Ordering::SeqCst);
        let _ = self.tx.send(());
    }
}

fn agent_exe() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().ok_or("the tray has no folder")?;
    Ok(dir.join("novalink-agent.exe"))
}

fn port_open() -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], 7979));
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// Waits up to `limit` for the agent's API port to close; true once it has.
pub fn gone_within(limit: Duration) -> bool {
    let end = Instant::now() + limit;
    while port_open() {
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(TICK);
    }
    true
}

fn spawn() -> Result<Child, String> {
    let exe = agent_exe()?;
    if !exe.exists() {
        return Err(format!("{} is missing", exe.display()));
    }
    Command::new(exe)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())
}

fn supervise(agent: &Agent, wake: Receiver<()>) {
    let mut child: Option<(Child, Instant)> = None;
    let mut backoff = Backoff::new();
    let mut retry_at: Option<Instant> = None;
    // The first start happens at once; later ones wait out the backoff.
    let mut first = true;
    loop {
        if wake.recv_timeout(TICK).is_ok() {
            backoff.reset();
            retry_at = None;
            first = true;
        }
        if agent.is_off() {
            if let Some((c, _)) = child.as_mut()
                && !matches!(c.try_wait(), Ok(None))
            {
                child = None;
            }
            continue;
        }
        if let Some((c, since)) = child.as_mut() {
            match c.try_wait() {
                Ok(None) => {
                    if since.elapsed() >= STABLE_AFTER {
                        backoff.reset();
                    }
                    if agent.is_restarting() && port_open() {
                        agent.restarting.store(false, Ordering::SeqCst);
                    }
                    continue;
                }
                _ => {
                    child = None;
                    agent.restarting.store(true, Ordering::SeqCst);
                    retry_at = Some(Instant::now() + backoff.next_delay());
                    first = false;
                }
            }
        }
        if port_open() {
            agent.restarting.store(false, Ordering::SeqCst);
            retry_at = None;
            continue;
        }
        let due = match retry_at {
            Some(t) => Instant::now() >= t,
            None if first => true,
            None => {
                // The agent vanished without being ours; pace its restart the same way.
                agent.restarting.store(true, Ordering::SeqCst);
                retry_at = Some(Instant::now() + backoff.next_delay());
                false
            }
        };
        if !due {
            continue;
        }
        first = false;
        match spawn() {
            Ok(c) => {
                agent.set_spawn_error(None);
                child = Some((c, Instant::now()));
                retry_at = None;
                agent.restarting.store(true, Ordering::SeqCst);
            }
            Err(e) => {
                agent.set_spawn_error(Some(e));
                agent.restarting.store(false, Ordering::SeqCst);
                retry_at = Some(Instant::now() + backoff.next_delay());
            }
        }
    }
}
