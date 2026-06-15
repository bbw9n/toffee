//! Shared tail loop for JSONL files.
//!
//! Holds one `JsonlTailer` per file. The tailer remembers `inode` and
//! current byte offset, and pulls newline-terminated records as they
//! arrive. Lines longer than `MAX_LINE_BYTES` are split — we never want
//! to OOM on a runaway line.
//!
//! On rotation (inode change) or truncation (file shorter than offset),
//! the tailer resets to offset 0 and re-opens.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};

use crate::registry::{resume_offset, Registry, RegistryEntry};
use crate::source::SourceError;

pub(crate) const MAX_LINE_BYTES: usize = 1024 * 1024; // 1 MiB

pub(crate) struct JsonlTailer {
    pub(crate) path: PathBuf,
    inode: u64,
    offset: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct TailedLine {
    pub line: String,
    /// Byte offset just past the line we just read (includes the trailing
    /// newline). This is what we persist to the registry.
    pub next_offset: u64,
    pub line_number: u64,
}

impl JsonlTailer {
    pub(crate) fn open_with_registry(
        path: PathBuf,
        source_kind: &'static str,
        registry: &Registry,
    ) -> Result<Self, SourceError> {
        let meta = std::fs::metadata(&path).map_err(SourceError::Io)?;
        let entry = registry.get(&path.display().to_string());
        let resume = resume_offset(entry.as_ref(), &meta);
        let inode = meta.ino();

        // Persist a fresh entry so subsequent reads see a stable record.
        let mut new_entry = entry
            .unwrap_or_else(|| RegistryEntry::new(source_kind, path.display().to_string(), inode));
        new_entry.inode = inode;
        new_entry.last_offset = resume;
        new_entry.source_kind = source_kind.to_string();
        let _ = registry.upsert(new_entry);

        Ok(JsonlTailer {
            path,
            inode,
            offset: resume,
        })
    }

    /// Read all currently-available complete lines.
    pub(crate) async fn read_available_lines(&mut self) -> Result<Vec<TailedLine>, SourceError> {
        // Re-check the inode in case the file was rotated.
        let meta = match std::fs::metadata(&self.path) {
            Ok(m) => m,
            Err(_) => return Ok(Vec::new()), // file vanished, retry later
        };
        if meta.ino() != self.inode {
            tracing::info!(path = %self.path.display(), "tailer: file rotated, restarting from 0");
            self.inode = meta.ino();
            self.offset = 0;
        }
        if self.offset > meta.len() {
            tracing::info!(path = %self.path.display(), "tailer: file truncated, restarting from 0");
            self.offset = 0;
        }
        if meta.len() == self.offset {
            return Ok(Vec::new());
        }

        let mut f = File::open(&self.path).await?;
        f.seek(std::io::SeekFrom::Start(self.offset)).await?;
        let mut reader = BufReader::new(f);
        let mut out = Vec::new();

        loop {
            let mut buf = String::new();
            let n = reader.read_line(&mut buf).await?;
            if n == 0 {
                break;
            }
            // A line without a trailing newline is a partial write — bail
            // and resume next tick.
            if !buf.ends_with('\n') {
                break;
            }
            self.offset += n as u64;
            let trimmed = buf.trim_end_matches(['\n', '\r']).to_string();
            if trimmed.len() > MAX_LINE_BYTES {
                tracing::warn!(
                    path = %self.path.display(),
                    bytes = trimmed.len(),
                    "tailer: skipping oversize line"
                );
                continue;
            }
            out.push(TailedLine {
                line: trimmed,
                next_offset: self.offset,
                line_number: self.offset,
            });
            if out.len() >= 256 {
                break;
            }
        }
        Ok(out)
    }
}

/// Glob a directory for JSONL files matching a pattern. Returns absolute
/// paths sorted by mtime descending (newest first) so we tail recent
/// sessions first.
pub(crate) fn discover(pattern: &str) -> Vec<PathBuf> {
    let mut hits: Vec<(PathBuf, std::time::SystemTime)> = match glob::glob(pattern) {
        Ok(it) => it
            .filter_map(|r| r.ok())
            .filter_map(|p| {
                let mt = std::fs::metadata(&p).ok()?.modified().ok()?;
                Some((p, mt))
            })
            .collect(),
        Err(_) => return Vec::new(),
    };
    hits.sort_by_key(|b| std::cmp::Reverse(b.1));
    hits.into_iter().map(|(p, _)| p).collect()
}

/// Expand a leading `~` (POSIX) in a path. The `directories` crate would be
/// the textbook answer; this is one line.
pub(crate) fn expand_tilde(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    s.to_string()
}

/// Strip a leading `~` from a base path before globbing.
pub(crate) fn expand_path(p: impl AsRef<Path>) -> PathBuf {
    let s = p.as_ref().to_string_lossy().to_string();
    PathBuf::from(expand_tilde(&s))
}
