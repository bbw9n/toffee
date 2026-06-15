//! SQLite-backed persistence for toffee.
//!
//! Phase 0 surface: open/initialize the database and append/load events. The
//! crate keeps a single `rusqlite::Connection` behind a `Mutex` — adequate for
//! Phase 0 latency targets and concurrent reads on a Unix domain socket where
//! traffic is naturally serialized per-connection. A pool comes later if
//! contention shows up.

mod conflicts;
mod embeddings;
mod entities;
mod events;
mod memories;
mod schema;
mod worker_failures;
mod worker_state;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::Connection;
use thiserror::Error;

pub use embeddings::{EmbeddingRow, NewEmbedding};
pub use entities::EntityListFilter;
pub use events::EventRecord;
pub use memories::MemoryListFilter;
pub use worker_state::{WorkerCheckpoint, DEFAULT_WORKER_ID};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("parse error: {0}")]
    Parse(String),

    #[error("not found: {0}")]
    NotFound(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

pub struct Store {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl Store {
    /// Open or create a database at the given path. Parent directory must
    /// already exist.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&path)?;
        // WAL + NORMAL: fast durable writes, concurrent readers.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        schema::migrate(&conn)?;
        Ok(Store {
            conn: Mutex::new(conn),
            path,
        })
    }

    /// In-memory store for tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        schema::migrate(&conn)?;
        Ok(Store {
            conn: Mutex::new(conn),
            path: PathBuf::from(":memory:"),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn with_conn<R>(&self, f: impl FnOnce(&Connection) -> Result<R>) -> Result<R> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        f(&guard)
    }
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").field("path", &self.path).finish()
    }
}
