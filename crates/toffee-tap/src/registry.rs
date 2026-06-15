//! At-least-once checkpoint registry, filebeat-style.
//!
//! Persistent map keyed by absolute path → `RegistryEntry { inode, offset,
//! last_session, last_turn, updated_at }`. Saved atomically (tmp + rename)
//! whenever a source advances. Sources read the entry on open and resume
//! from `last_offset` if the inode still matches.
//!
//! On rotation (different inode) or truncation (offset > current size), the
//! source starts from offset 0.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("decode error: {0}")]
    Decode(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntry {
    /// `"claude_code"`, `"codex"`, etc.
    pub source_kind: String,
    /// Absolute path on disk.
    pub path: String,
    /// Inode at the time we last read this file. If it changes we treat
    /// the file as rotated and start over at offset 0.
    pub inode: u64,
    /// Byte offset just past the last successfully processed record.
    pub last_offset: u64,
    /// Per-session checkpoint inside the file. Used for richer dedup when
    /// a future event-level idempotency key arrives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_session_id: Option<String>,
    #[serde(default)]
    pub last_turn_idx: u64,
    pub updated_at: DateTime<Utc>,
}

impl RegistryEntry {
    pub fn new(source_kind: impl Into<String>, path: impl Into<String>, inode: u64) -> Self {
        RegistryEntry {
            source_kind: source_kind.into(),
            path: path.into(),
            inode,
            last_offset: 0,
            last_session_id: None,
            last_turn_idx: 0,
            updated_at: Utc::now(),
        }
    }
}

/// Persistent dirty-write registry. Methods are cheap; saves are atomic.
pub struct Registry {
    path: PathBuf,
    inner: Mutex<Inner>,
}

#[derive(Default, Serialize, Deserialize)]
struct Inner {
    entries: HashMap<String, RegistryEntry>,
}

impl Registry {
    /// Load from `path`, or start empty if it doesn't exist.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, RegistryError> {
        let path = path.as_ref().to_path_buf();
        let inner = match std::fs::read_to_string(&path) {
            Ok(s) if !s.trim().is_empty() => serde_json::from_str(&s)?,
            _ => Inner::default(),
        };
        Ok(Registry {
            path,
            inner: Mutex::new(inner),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get(&self, key: &str) -> Option<RegistryEntry> {
        self.inner.lock().entries.get(key).cloned()
    }

    pub fn update<F>(&self, key: &str, mutate: F) -> Result<(), RegistryError>
    where
        F: FnOnce(&mut RegistryEntry),
    {
        {
            let mut guard = self.inner.lock();
            if let Some(e) = guard.entries.get_mut(key) {
                mutate(e);
                e.updated_at = Utc::now();
            }
        }
        self.save()
    }

    pub fn upsert(&self, entry: RegistryEntry) -> Result<(), RegistryError> {
        let key = entry.path.clone();
        {
            let mut guard = self.inner.lock();
            guard.entries.insert(key, entry);
        }
        self.save()
    }

    pub fn save(&self) -> Result<(), RegistryError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let serialised = {
            let guard = self.inner.lock();
            serde_json::to_string_pretty(&*guard)?
        };
        let tmp = self.path.with_extension("json.tmp");
        {
            let mut f = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp)?;
            f.write_all(serialised.as_bytes())?;
            f.sync_all().ok();
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.inner.lock().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().entries.is_empty()
    }
}

/// Decide where the source should resume reading.
///
/// - if no entry exists: start at offset 0
/// - if inode matches and `last_offset <= file_size`: resume at `last_offset`
/// - if inode changed: rotation, start at 0
/// - if `last_offset > file_size`: truncation, start at 0
pub fn resume_offset(entry: Option<&RegistryEntry>, current_meta: &std::fs::Metadata) -> u64 {
    let Some(e) = entry else {
        return 0;
    };
    if e.inode != current_meta.ino() {
        return 0;
    }
    if e.last_offset > current_meta.len() {
        return 0;
    }
    e.last_offset
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.json");
        let r = Registry::load(&path).unwrap();
        assert_eq!(r.len(), 0);

        r.upsert(RegistryEntry {
            source_kind: "claude_code".into(),
            path: "/x/y.jsonl".into(),
            inode: 42,
            last_offset: 100,
            last_session_id: Some("s1".into()),
            last_turn_idx: 5,
            updated_at: Utc::now(),
        })
        .unwrap();

        let r2 = Registry::load(&path).unwrap();
        let e = r2.get("/x/y.jsonl").unwrap();
        assert_eq!(e.inode, 42);
        assert_eq!(e.last_offset, 100);
        assert_eq!(e.last_turn_idx, 5);
    }

    #[test]
    fn update_mutates_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.json");
        let r = Registry::load(&path).unwrap();
        r.upsert(RegistryEntry {
            source_kind: "codex".into(),
            path: "/a/b".into(),
            inode: 1,
            last_offset: 0,
            last_session_id: None,
            last_turn_idx: 0,
            updated_at: Utc::now(),
        })
        .unwrap();
        r.update("/a/b", |e| {
            e.last_offset = 500;
            e.last_turn_idx = 10;
        })
        .unwrap();
        let e = r.get("/a/b").unwrap();
        assert_eq!(e.last_offset, 500);
        assert_eq!(e.last_turn_idx, 10);
    }
}
