use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use toffee_core::paths;

#[derive(Debug, Clone)]
pub struct ConnectOptions {
    pub socket_path: Option<PathBuf>,
    pub auto_spawn: bool,
    pub daemon_bin: Option<PathBuf>,
    pub connect_timeout: Duration,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        ConnectOptions {
            socket_path: None,
            auto_spawn: true,
            daemon_bin: None,
            connect_timeout: Duration::from_secs(3),
        }
    }
}

/// Locate the `toffeed` binary and spawn it if no daemon is already running.
///
/// Resolution order:
/// 1. `opts.daemon_bin`
/// 2. `$TOFFEE_DAEMON_BIN`
/// 3. Sibling of the current executable
/// 4. `PATH` lookup (`toffeed`)
pub fn spawn_daemon_if_needed(opts: &ConnectOptions) -> std::io::Result<()> {
    let bin = resolve_daemon_bin(opts.daemon_bin.clone());
    let bin = match bin {
        Some(b) => b,
        None => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "toffeed binary not found; set TOFFEE_DAEMON_BIN or place toffeed on PATH",
            ));
        }
    };

    // Make sure runtime dir exists so the daemon can write the socket / pid.
    let rt = paths::runtime_dir();
    std::fs::create_dir_all(&rt)?;

    // Detached child: don't inherit stdio, don't bind to parent's process
    // group. On Unix `Command::spawn` already returns immediately; the child
    // becomes an orphan when the parent exits.
    Command::new(&bin)
        .arg("--detached")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

fn resolve_daemon_bin(explicit: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        if p.exists() {
            return Some(p);
        }
    }
    if let Ok(p) = std::env::var("TOFFEE_DAEMON_BIN") {
        let path = PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
    }
    if let Ok(self_exe) = std::env::current_exe() {
        if let Some(dir) = self_exe.parent() {
            let candidate = dir.join("toffeed");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    // PATH lookup.
    if let Ok(path_env) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_env) {
            let candidate = dir.join("toffeed");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}
