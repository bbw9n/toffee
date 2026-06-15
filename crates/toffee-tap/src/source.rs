use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Speaker {
    User,
    Assistant,
    Tool,
    System,
}

/// One turn extracted from an agent session file. Format-agnostic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawTurn {
    /// Stable per-session id. For Claude Code this is `sessionId`; for
    /// Codex it's the session UUID from `session_meta.payload.id`.
    pub session_id: String,
    /// Monotonically increasing index within the session. For JSONL
    /// sources we use the line index of the qualifying record.
    pub turn_idx: u64,
    pub speaker: Speaker,
    /// The plain-text content of the turn. Tool calls and reasoning are
    /// flattened to text where it makes sense; richer payloads land in
    /// `extra`.
    pub content: String,
    /// Format-specific event kind, e.g. `"user_message"`,
    /// `"assistant_text"`, `"tool_call"`. The mapper prefixes this with
    /// the source kind to produce the toffee event_type, e.g.
    /// `"claude_code.user_message"`.
    pub event_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    /// The absolute path the source was reading. Mapper can use this for
    /// scope inference / provenance.
    pub source_path: String,
    /// Byte offset just past the line that produced this turn. The
    /// registry uses this for restart-resume.
    pub source_offset: u64,
    /// Working directory recorded with the turn, if the format carries
    /// one. Used by the mapper for scope inference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd_hint: Option<String>,
    /// Whatever else the source thought might be useful downstream. Lands
    /// in the event payload alongside `text`.
    #[serde(default)]
    pub extra: serde_json::Value,
}

#[derive(Debug, Error)]
pub enum SourceError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("other: {0}")]
    Other(String),
}

impl From<serde_json::Error> for SourceError {
    fn from(e: serde_json::Error) -> Self {
        SourceError::Decode(e.to_string())
    }
}

#[async_trait]
pub trait Source: Send {
    /// Stable identifier — `"claude_code"`, `"codex"`, `"stdin"`, etc.
    /// Used to prefix `event_type` and as the source-kind in the registry.
    fn kind(&self) -> &'static str;

    /// Read the next batch of turns. Returns empty `Vec` when there's
    /// nothing new (the runner will sleep and retry with backoff).
    async fn read_next(&mut self) -> Result<Vec<RawTurn>, SourceError>;
}
