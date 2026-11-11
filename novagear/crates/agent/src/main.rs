use anyhow::{Context, Result};
use clap::Parser;
use novagear_agent::{Agent, api};
use novagear_core::API_PORT;
use novagear_core::config::{config_dir, token_path};
use std::net::SocketAddr;

#[derive(Parser)]
#[command(
    name = "novagear-agent",
    about = "NovaGear device agent: lights, Stream Deck, prompter, capture, mice"
)]
struct Args {
    /// Config file (default: config.json in the NovaGear config dir).
    #[arg(long)]
    config: Option<std::path::PathBuf>,
    /// Internal: run one firmware check, print its result as JSON and exit. The agent runs
    /// itself this way so the check's memory never joins the agent's.
    #[arg(long = "firmware-check", hide = true)]
    firmware_check: bool,
    /// Internal: the ETag the last check saw (with `--firmware-check`).
    #[arg(long = "firmware-etag", hide = true)]
    firmware_etag: Option<String>,
}

/// Reuses the token across restarts so an open panel keeps working; makes one on first run.
fn load_or_make_token() -> Result<String> {
    let path = token_path()?;
    if let Ok(t) = std::fs::read_to_string(&path)
        && t.trim().len() >= 32
    {
        return Ok(t.trim().to_string());
    }
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    write_private(&path, &token).with_context(|| format!("writing {}", path.display()))?;
    Ok(token)
}

/// Writes `contents` so no other user can ever read it: the dir is 0700, and the file is
/// created 0600 under a temp name and then renamed into place. On Windows the per-user
/// config dir's ACL already does this.
fn write_private(path: &std::path::Path, contents: &str) -> Result<()> {
    use std::io::Write;
    let dir = path.parent().context("token path has no parent")?;
    let tmp = path.with_extension("tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        // An existing dir keeps its old mode; tighten it.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        opts.mode(0o600);
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)?;
    // A leftover temp file from a crash would make create_new fail.
    let _ = std::fs::remove_file(&tmp);
    opts.open(&tmp)?.write_all(contents.as_bytes())?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.firmware_check {
        let outcome = novagear_agent::elgato_fw::child_outcome(args.firmware_etag.as_deref());
        println!("{}", serde_json::to_string(&outcome)?);
        return Ok(());
    }
    // On macOS this thread serves the main run loop (the app-switch and display-sleep
    // notifications arrive through it) while the agent runs on another thread.
    let (done, result) = std::sync::mpsc::channel();
    novagear_agent::macos_events::run_main_loop(move || {
        let _ = done.send(run(args));
    });
    result.recv()?
}

fn run(args: Args) -> Result<()> {
    // Logs go to stderr and, for the app's log viewer, to a rotating file. A log directory
    // that can't be written doesn't stop the agent: the file layer just drops lines.
    match novagear_core::config::log_dir() {
        Ok(dir) => novagear_agent::logfile::init(&dir)?,
        Err(e) => {
            eprintln!("NovaGear: no log directory ({e:#}); logging to stderr only");
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "info".into()),
                )
                .init();
        }
    }
    tracing::info!("NovaGear agent {} starting", env!("CARGO_PKG_VERSION"));
    let cfg_path = match args.config {
        Some(p) => p,
        None => config_dir()?.join("config.json"),
    };
    let token = load_or_make_token()?;
    // One thread for everything async: the agent is I/O-bound and tiny.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(4)
        .build()?;
    rt.block_on(async move {
        let addr = SocketAddr::from(([127, 0, 0, 1], API_PORT));
        let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| {
            format!("port {API_PORT} is taken (is another novagear-agent running?)")
        })?;
        let agent = Agent::new(cfg_path, token)?;
        agent.start();
        let flush = agent.clone();
        tracing::info!("NovaGear agent on http://{addr}");
        // Graceful shutdown waits for every connection to close, but event streams (the
        // tray, an open panel) never do on their own, so give them a moment and then go.
        let (stopping, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = axum::serve(listener, api::router(agent)).with_graceful_shutdown(async move {
            shutdown().await;
            let _ = stopping.send(());
        });
        tokio::select! {
            r = server => {
                // A server that fails still owes the last second's changes to disk.
                flush.flush_saves();
                r?
            }
            _ = async {
                let _ = stopped.await;
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            } => tracing::info!("event streams still open after 2 s; exiting"),
        }
        // A knob turn or mode change from the last second is still waiting on its timer.
        flush.flush_saves();
        Ok::<_, anyhow::Error>(())
    })?;
    tracing::info!("NovaGear agent {} stopped", env!("CARGO_PKG_VERSION"));
    // Don't wait for blocking-pool work (a USB scan, a mouse call) on the way out.
    rt.shutdown_background();
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
