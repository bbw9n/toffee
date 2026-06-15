use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use async_trait::async_trait;
use clap::Parser;
use tokio::net::UnixListener;
use tokio::sync::broadcast;
use toffee_core::{paths, EventInput};
use toffee_rpc::methods::{
    method_names, AppendEventRequest, AppendEventResponse, HelloRequest, HelloResponse,
    ServerInfo,
};
use toffee_rpc::server::{serve, Handler, RpcError};
use toffee_store::Store;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "toffeed", version, about = "Toffee memory daemon")]
struct Args {
    /// Run in the foreground, logging to stderr.
    #[arg(long)]
    foreground: bool,

    /// Sentinel used by clients that auto-spawn; behaves the same as default
    /// background launch (logs go to a file).
    #[arg(long)]
    detached: bool,

    /// Override the Unix socket path.
    #[arg(long, env = "TOFFEE_SOCKET")]
    socket: Option<PathBuf>,

    /// Override the database file path.
    #[arg(long, env = "TOFFEE_DB")]
    db: Option<PathBuf>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    init_tracing(args.foreground)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;

    runtime.block_on(async move { run(args).await })
}

async fn run(args: Args) -> Result<()> {
    let runtime_dir = paths::runtime_dir();
    std::fs::create_dir_all(&runtime_dir)
        .with_context(|| format!("create runtime dir {:?}", runtime_dir))?;

    let pid_path = paths::pid_path();
    let _pid_lock = acquire_pid_lock(&pid_path)
        .context("another toffeed instance appears to be running")?;

    let socket_path = args.socket.unwrap_or_else(paths::socket_path);
    // Stale socket from a previous crash — remove before binding.
    if socket_path.exists() {
        std::fs::remove_file(&socket_path).ok();
    }
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("bind unix socket {:?}", socket_path))?;
    set_socket_mode(&socket_path, 0o600)?;
    tracing::info!(?socket_path, "listening");

    let db_path = args.db.unwrap_or_else(paths::db_path);
    let store = Arc::new(
        Store::open(&db_path).with_context(|| format!("open store {:?}", db_path))?,
    );
    tracing::info!(?db_path, "store ready");

    let (shutdown_tx, _) = broadcast::channel::<()>(8);
    let handler = Arc::new(DaemonHandler {
        store: store.clone(),
        started_at: Instant::now(),
        shutdown: shutdown_tx.clone(),
    });

    // Spawn the server task.
    let server_handle = {
        let handler = handler.clone();
        let shutdown_rx = shutdown_tx.subscribe();
        tokio::spawn(async move {
            if let Err(e) = serve(listener, handler, shutdown_rx).await {
                tracing::error!(error = ?e, "server task ended with error");
            }
        })
    };

    wait_for_shutdown_signal(shutdown_tx.clone()).await;
    tracing::info!("shutting down");

    // Drop the server task and clean up.
    let _ = server_handle.await;
    std::fs::remove_file(&socket_path).ok();
    // pid file is unlinked when the lock guard drops.
    Ok(())
}

async fn wait_for_shutdown_signal(shutdown_tx: broadcast::Sender<()>) {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = ?e, "failed to install SIGTERM handler");
            return;
        }
    };
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = ?e, "failed to install SIGINT handler");
            return;
        }
    };
    let mut shutdown_rx = shutdown_tx.subscribe();

    tokio::select! {
        _ = sigterm.recv() => tracing::info!("received SIGTERM"),
        _ = sigint.recv() => tracing::info!("received SIGINT"),
        _ = shutdown_rx.recv() => tracing::info!("received in-process shutdown"),
    }
    let _ = shutdown_tx.send(());
}

struct DaemonHandler {
    store: Arc<Store>,
    started_at: Instant,
    shutdown: broadcast::Sender<()>,
}

#[async_trait]
impl Handler for DaemonHandler {
    async fn hello(&self, _req: HelloRequest) -> Result<HelloResponse, RpcError> {
        let store = self.store.clone();
        let event_count = tokio::task::spawn_blocking(move || store.event_count())
            .await
            .map_err(|e| RpcError::Internal(e.to_string()))?
            .unwrap_or(0);
        Ok(ServerInfo {
            server_name: "toffeed".to_string(),
            server_version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_seconds: self.started_at.elapsed().as_secs(),
            supported_methods: method_names::all(),
            db_path: self.store.path().display().to_string(),
            event_count,
        })
    }

    async fn append_event(
        &self,
        req: AppendEventRequest,
    ) -> Result<AppendEventResponse, RpcError> {
        let input: EventInput = req;
        let store = self.store.clone();
        let event = tokio::task::spawn_blocking(move || store.append_event(input))
            .await
            .map_err(|e| RpcError::Internal(e.to_string()))?
            .map_err(|e| RpcError::Internal(e.to_string()))?;
        Ok(AppendEventResponse { event_id: event.id })
    }

    async fn daemon_shutdown(&self) -> Result<(), RpcError> {
        let _ = self.shutdown.send(());
        Ok(())
    }
}

/// Owns the pid file and its flock. Drop unlinks the file.
struct PidLock {
    path: PathBuf,
    _file: File,
}

impl Drop for PidLock {
    fn drop(&mut self) {
        std::fs::remove_file(&self.path).ok();
    }
}

fn acquire_pid_lock(pid_path: &PathBuf) -> Result<PidLock> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(pid_path)
        .with_context(|| format!("open pid file {:?}", pid_path))?;
    // Non-blocking exclusive lock. If another daemon holds it, we bail.
    let fd = file.as_raw_fd();
    let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("flock on pid file failed: {err}");
    }
    // Truncate then write our pid.
    let pid = std::process::id();
    // Truncate by re-opening with truncate true — easier than the unsafe ftruncate dance.
    let mut writer = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(pid_path)
        .with_context(|| format!("truncate pid file {:?}", pid_path))?;
    writeln!(writer, "{pid}")?;
    writer.sync_all().ok();
    drop(writer);
    Ok(PidLock {
        path: pid_path.clone(),
        _file: file,
    })
}

fn set_socket_mode(path: &PathBuf, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(path).with_context(|| format!("stat {:?}", path))?;
    let mut perms = metadata.permissions();
    perms.set_mode(mode);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

fn init_tracing(foreground: bool) -> Result<()> {
    let filter = EnvFilter::try_from_env("TOFFEE_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,toffee=debug"));

    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true);

    if foreground {
        builder.with_writer(std::io::stderr).try_init().ok();
    } else {
        let log_dir = paths::log_dir();
        std::fs::create_dir_all(&log_dir)
            .with_context(|| format!("create log dir {:?}", log_dir))?;
        let log_path = log_dir.join("toffeed.log");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("open log file {:?}", log_path))?;
        builder.with_writer(move || file.try_clone().unwrap()).try_init().ok();
    }
    Ok(())
}
