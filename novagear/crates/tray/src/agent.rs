//! Keeps the agent running while the tray is up, and turns its event stream into fresh
//! snapshots for the menu. Event-driven: nothing polls while the desk is quiet.

use crate::model::Snapshot;
use novagear_core::client;
use std::process::{Child, Command};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The agent the tray started, if it had to start one. An agent that was already running
/// (from a terminal or a service) is left alone on quit.
#[derive(Default, Clone)]
pub struct Supervisor {
    child: Arc<Mutex<Option<Child>>>,
}

fn agent_path() -> Option<std::path::PathBuf> {
    let name = if cfg!(windows) {
        "novagear-agent.exe"
    } else {
        "novagear-agent"
    };
    let p = std::env::current_exe().ok()?.parent()?.join(name);
    p.exists().then_some(p)
}

fn agent_answers() -> bool {
    std::net::TcpStream::connect_timeout(
        &([127, 0, 0, 1], novagear_core::API_PORT).into(),
        Duration::from_millis(500),
    )
    .is_ok()
}

impl Supervisor {
    /// Starts the agent next to the tray binary unless one already answers.
    pub fn ensure_running(&self) {
        if agent_answers() {
            return;
        }
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = child.as_mut()
            && matches!(c.try_wait(), Ok(None))
        {
            return;
        }
        let Some(path) = agent_path() else {
            eprintln!("novagear-tray: no novagear-agent next to the tray; start it yourself");
            return;
        };
        match Command::new(&path).spawn() {
            Ok(c) => *child = Some(c),
            Err(e) => eprintln!("novagear-tray: starting {}: {e}", path.display()),
        }
    }

    pub fn stop(&self) {
        if let Some(mut c) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Runs forever on its own threads, sending a snapshot whenever the agent reports a change
/// (and `None` while it can't be reached).
pub fn watch(sup: Supervisor, send: impl Fn(Option<Snapshot>) + Send + Sync + 'static) {
    let send = Arc::new(send);
    let (poke, pokes) = mpsc::channel::<()>();
    let fetch_send = send.clone();
    std::thread::Builder::new()
        .name("snapshot".into())
        .spawn(move || {
            while pokes.recv().is_ok() {
                // Let a burst (a dial being turned) settle, then fetch once.
                std::thread::sleep(Duration::from_millis(150));
                while pokes.try_recv().is_ok() {}
                fetch_send(Snapshot::fetch().ok());
            }
        })
        .expect("spawning snapshot thread");
    std::thread::Builder::new()
        .name("events".into())
        .spawn(move || {
            loop {
                sup.ensure_running();
                let _ = poke.send(());
                let ended = client::follow_events(|ev| {
                    // Live key and dial presses and log lines don't change what the menu shows.
                    if !ev.contains(r#""type":"input""#) && !ev.contains(r#""type":"log""#) {
                        let _ = poke.send(());
                    }
                });
                if ended.is_err() {
                    send(None);
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        })
        .expect("spawning events thread");
}
