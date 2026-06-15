use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde_json::json;
use toffee_client::{Client, ConnectOptions};
use toffee_core::paths;

use crate::{format_for, stdout_is_tty, Effective, OutputFormat};

#[derive(Args, Debug)]
pub struct DaemonCmd {
    #[command(subcommand)]
    action: DaemonAction,
}

#[derive(Subcommand, Debug)]
enum DaemonAction {
    /// Spawn the daemon in the background. Idempotent if already running.
    Start {
        /// Run in the foreground (do not detach).
        #[arg(long)]
        foreground: bool,
    },
    /// Ask the daemon to shut down. Falls back to SIGTERM if RPC fails.
    Stop,
    /// Show daemon uptime and event count.
    Status,
    /// Print the daemon log file.
    Logs {
        /// Follow log output (like `tail -f`).
        #[arg(short, long)]
        follow: bool,
    },
    /// Drop and recompute the vector index from durable memories.
    RebuildIndexes,
    /// Pre-fetch embedding model weights. Today's HashEmbedder needs no
    /// weights, so this command reports the active model and exits. The
    /// command exists so the surface is stable once a real candle-backed
    /// BGE-small embedder ships (RFC §11, open question #2).
    PrefetchModels,
}

pub async fn run(cmd: DaemonCmd, fmt: OutputFormat) -> Result<()> {
    let effective = format_for(fmt, stdout_is_tty());
    match cmd.action {
        DaemonAction::Start { foreground } => start(foreground, effective).await,
        DaemonAction::Stop => stop(effective).await,
        DaemonAction::Status => status(effective).await,
        DaemonAction::Logs { follow } => logs(follow).await,
        DaemonAction::RebuildIndexes => rebuild_indexes(effective).await,
        DaemonAction::PrefetchModels => prefetch_models(effective).await,
    }
}

async fn prefetch_models(fmt: Effective) -> Result<()> {
    let client = Client::connect().await.context("connect to daemon")?;
    let info = client.hello("toffee-cli", env!("CARGO_PKG_VERSION")).await?;
    let model = info
        .supported_methods
        .iter()
        .find(|_| false)
        .cloned()
        .unwrap_or_else(|| "hash-feature-v1".to_string());
    // We don't have a direct `model_name()` RPC on the daemon today; the
    // hash embedder is the only backend, so report that explicitly.
    let _ = model; // silence unused-warning shape
    let active_model = "hash-feature-v1";
    match fmt {
        Effective::Human => {
            println!("active embedding model: {active_model}");
            println!(
                "no weights to prefetch — the hash embedder has no model artifacts.\n\
                 this command is a placeholder for the future candle-backed BGE-small\n\
                 backend (see RFC §11 open question #2)."
            );
        }
        Effective::Json => {
            println!(
                "{}",
                serde_json::json!({
                    "model": active_model,
                    "status": "noop",
                    "reason": "hash-embedder requires no model weights",
                })
            );
        }
    }
    Ok(())
}

async fn rebuild_indexes(fmt: Effective) -> Result<()> {
    let client = Client::connect().await.context("connect to daemon")?;
    let n = client.rebuild_indexes().await?;
    match fmt {
        Effective::Human => println!("reindexed {n} memories"),
        Effective::Json => println!("{}", json!({"reindexed": n})),
    }
    Ok(())
}

async fn start(foreground: bool, fmt: Effective) -> Result<()> {
    // Already up?
    if let Ok(client) = Client::connect_with(ConnectOptions {
        auto_spawn: false,
        connect_timeout: Duration::from_millis(50),
        ..Default::default()
    })
    .await
    {
        let info = client.hello("toffee-cli", env!("CARGO_PKG_VERSION")).await?;
        match fmt {
            Effective::Human => println!(
                "already running. uptime={}s, events={}",
                info.uptime_seconds, info.event_count
            ),
            Effective::Json => println!(
                "{}",
                json!({
                    "status": "already_running",
                    "uptime_seconds": info.uptime_seconds,
                    "event_count": info.event_count,
                })
            ),
        }
        return Ok(());
    }

    let bin = locate_toffeed().context("locate toffeed binary")?;
    if foreground {
        let mut child = Command::new(&bin).arg("--foreground").spawn()?;
        let status = child.wait()?;
        if !status.success() {
            anyhow::bail!("toffeed exited with status {}", status);
        }
        return Ok(());
    }

    // Detached: spawn and wait for the socket to appear.
    Command::new(&bin)
        .arg("--detached")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawn {:?}", bin))?;

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut delay = Duration::from_millis(25);
    loop {
        if let Ok(client) = Client::connect_with(ConnectOptions {
            auto_spawn: false,
            connect_timeout: Duration::from_millis(50),
            ..Default::default()
        })
        .await
        {
            let info = client.hello("toffee-cli", env!("CARGO_PKG_VERSION")).await?;
            match fmt {
                Effective::Human => println!(
                    "started. uptime={}s, events={}",
                    info.uptime_seconds, info.event_count
                ),
                Effective::Json => println!(
                    "{}",
                    json!({
                        "status": "started",
                        "uptime_seconds": info.uptime_seconds,
                        "event_count": info.event_count,
                    })
                ),
            }
            return Ok(());
        }
        if Instant::now() > deadline {
            anyhow::bail!("toffeed did not begin accepting connections within 5s");
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_millis(200));
    }
}

async fn stop(fmt: Effective) -> Result<()> {
    let client_result = Client::connect_with(ConnectOptions {
        auto_spawn: false,
        connect_timeout: Duration::from_millis(200),
        ..Default::default()
    })
    .await;

    match client_result {
        Ok(client) => {
            // Best-effort RPC shutdown. We ignore "connection closed" on the
            // response because the daemon may close the socket before we read.
            let _ = client.shutdown_daemon().await;
            // Wait briefly for the socket to disappear.
            let deadline = Instant::now() + Duration::from_secs(2);
            while paths::socket_path().exists() && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            match fmt {
                Effective::Human => println!("stopped"),
                Effective::Json => println!("{}", json!({"status": "stopped"})),
            }
            Ok(())
        }
        Err(_) => {
            // Fall back to SIGTERM via pid file.
            let pid_path = paths::pid_path();
            if !pid_path.exists() {
                match fmt {
                    Effective::Human => println!("not running"),
                    Effective::Json => println!("{}", json!({"status": "not_running"})),
                }
                return Ok(());
            }
            let pid_str = std::fs::read_to_string(&pid_path)
                .with_context(|| format!("read pid file {:?}", pid_path))?;
            let pid: i32 = pid_str
                .trim()
                .parse()
                .with_context(|| format!("parse pid from {:?}", pid_path))?;
            let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
            if rc != 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::ESRCH) {
                    match fmt {
                        Effective::Human => println!("not running (stale pid file)"),
                        Effective::Json => println!("{}", json!({"status": "not_running"})),
                    }
                    return Ok(());
                }
                anyhow::bail!("kill({pid}) failed: {err}");
            }
            match fmt {
                Effective::Human => println!("sent SIGTERM to pid {pid}"),
                Effective::Json => {
                    println!("{}", json!({"status": "signaled", "pid": pid}))
                }
            }
            Ok(())
        }
    }
}

async fn status(fmt: Effective) -> Result<()> {
    let client = match Client::connect_with(ConnectOptions {
        auto_spawn: false,
        connect_timeout: Duration::from_millis(200),
        ..Default::default()
    })
    .await
    {
        Ok(c) => c,
        Err(_) => {
            match fmt {
                Effective::Human => println!("daemon: not running"),
                Effective::Json => println!("{}", json!({"running": false})),
            }
            return Ok(());
        }
    };
    let info = client.hello("toffee-cli", env!("CARGO_PKG_VERSION")).await?;
    match fmt {
        Effective::Human => {
            println!("daemon: running ({})", info.server_version);
            println!("uptime: {}s", info.uptime_seconds);
            println!("events: {}", info.event_count);
            println!("db:     {}", info.db_path);
            println!("socket: {}", client.socket_path().display());
        }
        Effective::Json => {
            println!(
                "{}",
                json!({
                    "running": true,
                    "server_version": info.server_version,
                    "uptime_seconds": info.uptime_seconds,
                    "event_count": info.event_count,
                    "db_path": info.db_path,
                    "supported_methods": info.supported_methods,
                    "socket_path": client.socket_path().display().to_string(),
                })
            );
        }
    }
    Ok(())
}

async fn logs(follow: bool) -> Result<()> {
    let log_path = paths::log_dir().join("toffeed.log");
    if !log_path.exists() {
        eprintln!("no log file at {:?}", log_path);
        return Ok(());
    }
    if follow {
        let mut cmd = Command::new("tail");
        cmd.arg("-f").arg(&log_path);
        let status = cmd.status().context("invoke tail -f")?;
        if !status.success() {
            anyhow::bail!("tail exited with {status}");
        }
        return Ok(());
    }
    let content = std::fs::read_to_string(&log_path)
        .with_context(|| format!("read {:?}", log_path))?;
    print!("{content}");
    Ok(())
}

fn locate_toffeed() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("TOFFEE_DAEMON_BIN") {
        let path = PathBuf::from(p);
        if path.exists() {
            return Ok(path);
        }
    }
    let self_exe = std::env::current_exe().context("current_exe")?;
    if let Some(dir) = self_exe.parent() {
        let candidate = dir.join("toffeed");
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    if let Ok(path_env) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_env) {
            let candidate = dir.join("toffeed");
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    anyhow::bail!(
        "could not locate toffeed binary; set TOFFEE_DAEMON_BIN or place toffeed on PATH"
    )
}
