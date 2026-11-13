use anyhow::{Context, Result};
use clap::Parser;
use novalink_agent::engine::{Engine, EngineOpts};
use novalink_agent::settings::Settings;
use novalink_agent::{api, channels};
use novalink_agent::{logging, paths};
use novalink_core::Platform;
use novalink_net::identity::{Identity, KeyringStore};
use novalink_net::manager::ManagerConfig;
use novalink_net::trust::TrustStore;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Parser)]
#[command(name = "novalink-agent", about = "NovaLink software KVM agent")]
struct Args {
    /// Device name shown to peers (default: the host name).
    #[arg(long)]
    name: Option<String>,
    /// UDP port to listen on.
    #[arg(long, default_value_t = 47474)]
    port: u16,
    /// Receive and track remote input but never inject it.
    #[arg(long)]
    dry_inject: bool,
    /// Do not advertise or browse over mDNS.
    #[arg(long)]
    no_discovery: bool,
}

fn host_name() -> String {
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
        .or_else(|| {
            let out = std::process::Command::new("hostname").output().ok()?;
            let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
            (!s.is_empty()).then_some(s)
        })
        .unwrap_or_else(|| "novalink".into())
}

#[cfg(target_os = "macos")]
fn real_platform() -> Result<Arc<dyn Platform>> {
    Ok(Arc::new(novalink_platform_macos::MacPlatform::new()?))
}

#[cfg(windows)]
fn real_platform() -> Result<Arc<dyn Platform>> {
    Ok(Arc::new(novalink_platform_windows::WinPlatform::new()?))
}

#[cfg(not(any(target_os = "macos", windows)))]
fn real_platform() -> Result<Arc<dyn Platform>> {
    anyhow::bail!("no input platform for this OS")
}

/// Resolves on Ctrl-C or any termination request the OS sends a user process.
#[cfg(unix)]
async fn wait_for_shutdown_signal() -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())?;
    let mut hup = signal(SignalKind::hangup())?;
    tokio::select! {
        r = tokio::signal::ctrl_c() => r?,
        _ = term.recv() => {}
        _ = hup.recv() => {}
    }
    Ok(())
}

#[cfg(windows)]
async fn wait_for_shutdown_signal() -> Result<()> {
    use tokio::signal::windows;
    let mut close = windows::ctrl_close()?;
    let mut shutdown = windows::ctrl_shutdown()?;
    let mut logoff = windows::ctrl_logoff()?;
    tokio::select! {
        r = tokio::signal::ctrl_c() => r?,
        _ = close.recv() => {}
        _ = shutdown.recv() => {}
        _ = logoff.recv() => {}
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
async fn wait_for_shutdown_signal() -> Result<()> {
    tokio::signal::ctrl_c().await?;
    Ok(())
}

/// True when 127.0.0.1:7979 is held by a NovaLink agent that proves it has our token.
async fn another_agent_running(token: &str) -> bool {
    let addr: SocketAddr = api::API_ADDR.parse().expect("literal address");
    let running = api::proven_agent_at(addr, token).await;
    if running {
        tracing::info!("another NovaLink agent is already running; exiting");
    }
    running
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    // Flushes the non-blocking log writer when dropped.
    let log_guard = logging::init(&paths::log_dir())?;
    let name = args.name.unwrap_or_else(host_name);
    tracing::info!(%name, port = args.port, dry_inject = args.dry_inject, "starting");

    let token = api::load_or_create_token(&paths::config_dir()).context("api token")?;
    // A second copy (e.g. launchd beside one started in Terminal) leaves quietly: exit 0
    // keeps launchd from restarting it over and over.
    if another_agent_running(&token).await {
        return Ok(());
    }
    let identity = Identity::load_or_create(&KeyringStore)?;
    let trust = TrustStore::open(&paths::config_dir().join("trusted-devices.json"))
        .context("open trust store")?;
    let cfg = ManagerConfig {
        identity,
        trust: Arc::new(Mutex::new(trust)),
        name,
        bind: SocketAddr::from(([0, 0, 0, 0], args.port)),
        hello_displays: vec![],
        discovery: !args.no_discovery,
    };
    let settings_path = paths::config_dir().join("settings.json");
    let opts = EngineOpts {
        dry_inject: args.dry_inject,
        recorder_span: Duration::from_secs(30),
        log_dir: paths::log_dir(),
        settings: Settings::load(&settings_path),
        settings_path,
    };
    let engine = Engine::start(real_platform()?, cfg, opts).await?;
    tracing::info!(id = %engine.status().me, "running");
    let release = engine.emergency_release();
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        release();
        prev(info);
    }));
    let api_listener = match api::bind().await {
        Ok(l) => l,
        Err(e) => {
            engine.shutdown().await;
            if another_agent_running(&token).await {
                return Ok(());
            }
            tracing::error!("local API unavailable: {e:#}");
            return Err(e.context("is another novalink-agent already running?"));
        }
    };
    let forward_task = match channels::bind().await {
        Ok(l) => Some(tokio::spawn(channels::serve_on(
            l,
            engine.clone(),
            token.clone(),
        ))),
        Err(e) => {
            tracing::error!("service forwarder unavailable: {e:#}");
            None
        }
    };
    let mut api_task = tokio::spawn(api::serve_on(api_listener, engine.clone(), token));
    tokio::select! {
        r = wait_for_shutdown_signal() => r?,
        _ = engine.shutdown_requested() => {
            tracing::info!("shutdown requested over the API");
            // The API finishes the reply to that request, then stops.
            let _ = tokio::time::timeout(Duration::from_secs(2), &mut api_task).await;
        }
        r = &mut api_task => {
            if engine.is_shutdown_requested() {
                tracing::info!("shutdown requested over the API");
            } else {
            tracing::error!("local API stopped: {r:?}");
            engine.shutdown().await;
            anyhow::bail!("local API stopped unexpectedly");
            }
        }
        exit = engine.supervise() => {
            // Everything is released; a dead engine must not leave a zombie process.
            tracing::error!(?exit, "engine died; exiting");
            // process::exit skips destructors: flush the log first so this line lands.
            drop(log_guard);
            std::process::exit(1);
        }
    }
    tracing::info!("shutting down");
    api_task.abort();
    if let Some(t) = forward_task {
        t.abort();
    }
    engine.shutdown().await;
    Ok(())
}
