//! Polls the agent from a background thread and hands the UI thread a `UiState`.

use super::agent::Agent;
use novalink_client::{Check, Client, ClientError, Discovered, PendingPair};
use novalink_tray::model::{StatusErrors, UiState};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

const EVERY: Duration = Duration::from_secs(1);
const DISCOVER_EVERY: Duration = Duration::from_secs(5);

/// Starts the poller. A message on `wake` polls at once; `true` also refreshes the doctor
/// checks (the menu is opening). `notify` tells the UI thread a new state is waiting.
pub fn start(
    agent: Arc<Agent>,
    wake: Receiver<bool>,
    out: Sender<UiState>,
    notify: impl Fn() + Send + 'static,
) {
    std::thread::spawn(move || {
        let mut cache = Cache::default();
        loop {
            let doctor_now = match wake.recv_timeout(EVERY) {
                Ok(d) => d,
                Err(RecvTimeoutError::Timeout) => false,
                Err(RecvTimeoutError::Disconnected) => return,
            };
            if let Some(state) = poll(&agent, &mut cache, doctor_now) {
                if out.send(state).is_err() {
                    return;
                }
                notify();
            }
        }
    });
}

#[derive(Default)]
struct Cache {
    pending: Vec<PendingPair>,
    discovered: Vec<Discovered>,
    discovered_at: Option<Instant>,
    checks: Option<Vec<Check>>,
    /// Polls in a row that found the agent answering although NovaLink is off.
    still_up: u32,
    status_errors: StatusErrors,
}

/// A few polls of grace: the agent needs a moment to go after its shutdown reply.
const OFF_GRACE: u32 = 3;

/// Off is only shown if nothing answers; an agent that ignored the shutdown stays visible.
fn off_state(cache: &mut Cache) -> UiState {
    let Ok(client) = Client::local() else {
        return UiState::Off;
    };
    match client.status() {
        Err(ClientError::NotRunning | ClientError::TokenMissing) => {
            cache.still_up = 0;
            UiState::Off
        }
        Err(ClientError::Impostor) => UiState::Impostor,
        Ok(_) | Err(_) => {
            cache.still_up += 1;
            if cache.still_up > OFF_GRACE {
                UiState::OffButRunning
            } else {
                UiState::Off
            }
        }
    }
}

/// `None` keeps whatever the UI shows now (a slow or odd reply isn't worth a flicker).
fn poll(agent: &Agent, cache: &mut Cache, doctor_now: bool) -> Option<UiState> {
    if agent.is_off() {
        return Some(off_state(cache));
    }
    cache.still_up = 0;
    let down = || match agent.spawn_error() {
        Some(reason) => UiState::StartFailed { reason },
        None => UiState::AgentDown {
            restarting: agent.is_restarting(),
        },
    };
    let Ok(client) = Client::local() else {
        cache.status_errors.reset();
        return Some(down());
    };
    let status = match client.status() {
        Ok(s) => s,
        Err(ClientError::NotRunning | ClientError::TokenMissing) => {
            cache.status_errors.reset();
            return Some(down());
        }
        Err(ClientError::Impostor) => {
            cache.status_errors.reset();
            return Some(UiState::Impostor);
        }
        Err(e) => return cache.status_errors.other(e.to_string()),
    };
    cache.status_errors.reset();
    if let Ok(p) = client.pending() {
        cache.pending = p;
    }
    if cache
        .discovered_at
        .is_none_or(|t| t.elapsed() >= DISCOVER_EVERY)
        && let Ok(d) = client.discovered()
    {
        cache.discovered = d;
        cache.discovered_at = Some(Instant::now());
    }
    if (doctor_now || cache.checks.is_none())
        && let Ok(c) = client.doctor()
    {
        cache.checks = Some(c);
    }
    Some(UiState::Up {
        status,
        pending: cache.pending.clone(),
        discovered: cache.discovered.clone(),
        checks: cache.checks.clone().unwrap_or_default(),
        start_at_login: super::system::start_at_login(),
    })
}
