//! Conventional filesystem locations for toffee.
//!
//! Resolves XDG paths with a macOS fallback. Pure: depends only on env vars
//! and the compile-time target OS.

use std::path::PathBuf;

/// `$XDG_DATA_HOME/toffee/` or platform default.
pub fn data_dir() -> PathBuf {
    if let Ok(p) = std::env::var("XDG_DATA_HOME") {
        if !p.is_empty() {
            return PathBuf::from(p).join("toffee");
        }
    }
    let home = home_dir();
    #[cfg(target_os = "macos")]
    {
        home.join("Library/Application Support/toffee")
    }
    #[cfg(not(target_os = "macos"))]
    {
        home.join(".local/share/toffee")
    }
}

/// `$XDG_RUNTIME_DIR/toffee/` or platform default. This holds the socket and
/// PID file; on macOS `XDG_RUNTIME_DIR` is rarely set so we fall back to a
/// per-user tmp subdir.
pub fn runtime_dir() -> PathBuf {
    if let Ok(p) = std::env::var("XDG_RUNTIME_DIR") {
        if !p.is_empty() {
            return PathBuf::from(p).join("toffee");
        }
    }
    let user = std::env::var("USER")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "toffee".to_string());
    PathBuf::from(format!("/tmp/toffee-{}", user))
}

/// `$XDG_STATE_HOME/toffee/` or platform default.
pub fn state_dir() -> PathBuf {
    if let Ok(p) = std::env::var("XDG_STATE_HOME") {
        if !p.is_empty() {
            return PathBuf::from(p).join("toffee");
        }
    }
    let home = home_dir();
    #[cfg(target_os = "macos")]
    {
        home.join("Library/Application Support/toffee/state")
    }
    #[cfg(not(target_os = "macos"))]
    {
        home.join(".local/state/toffee")
    }
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("toffeed.sock")
}

pub fn pid_path() -> PathBuf {
    runtime_dir().join("toffeed.pid")
}

pub fn db_path() -> PathBuf {
    data_dir().join("toffee.db")
}

pub fn log_dir() -> PathBuf {
    state_dir().join("logs")
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}
