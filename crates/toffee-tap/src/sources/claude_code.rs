//! Claude Code session JSONL.
//!
//! Path: `~/.claude/projects/<encoded-cwd>/<session-uuid>.jsonl`.
//! Each line is `{type, sessionId, uuid, parentUuid, cwd, gitBranch,
//!   timestamp, message:{role, content}, ...}`.
//!
//! Relevant types:
//!   - `"user"`: `message.role="user"`, `message.content` is a string.
//!   - `"assistant"`: `message.role="assistant"`, `message.content` is a
//!     list of `{type:text|thinking|tool_use, text?}`. We flatten the
//!     concatenated `text` parts; tool calls and thinking lands in
//!     `extra` for downstream inspection.
//!
//! Everything else (`permission-mode`, `file-history-snapshot`, etc.) is
//! ignored — it's metadata not memory.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::registry::Registry;
use crate::source::{RawTurn, Source, SourceError, Speaker};
use crate::sources::jsonl_tail::{discover, expand_path, JsonlTailer, TailedLine};

const SOURCE_KIND: &str = "claude_code";

pub struct ClaudeCodeSource {
    registry: Arc<Registry>,
    discovery_root: PathBuf,
    tailers: Vec<JsonlTailer>,
    /// How often (in `read_next` calls) to re-scan the discovery root for
    /// new session files.
    rediscover_every: u32,
    ticks: u32,
}

impl ClaudeCodeSource {
    /// `path` should point at `~/.claude/projects`. Default is fine for
    /// most installs.
    pub fn new(path: impl AsRef<Path>, registry: Arc<Registry>) -> Self {
        ClaudeCodeSource {
            registry,
            discovery_root: expand_path(path),
            tailers: Vec::new(),
            rediscover_every: 8,
            ticks: 0,
        }
    }

    pub fn default_path() -> PathBuf {
        expand_path("~/.claude/projects")
    }

    fn rediscover(&mut self) {
        let pattern = format!("{}/**/*.jsonl", self.discovery_root.display());
        let found = discover(&pattern);
        for p in found {
            let key = p.display().to_string();
            if self.tailers.iter().any(|t| t.path.display().to_string() == key) {
                continue;
            }
            match JsonlTailer::open_with_registry(p.clone(), SOURCE_KIND, &self.registry) {
                Ok(t) => {
                    tracing::info!(path = %p.display(), "claude_code: now tailing");
                    self.tailers.push(t);
                }
                Err(e) => {
                    tracing::debug!(path = %p.display(), error = ?e, "claude_code: skipping");
                }
            }
        }
    }
}

#[async_trait]
impl Source for ClaudeCodeSource {
    fn kind(&self) -> &'static str {
        SOURCE_KIND
    }

    async fn read_next(&mut self) -> Result<Vec<RawTurn>, SourceError> {
        if self.tailers.is_empty() || self.ticks % self.rediscover_every == 0 {
            self.rediscover();
        }
        self.ticks = self.ticks.wrapping_add(1);

        let mut out: Vec<RawTurn> = Vec::new();
        for tailer in &mut self.tailers {
            let lines = tailer.read_available_lines().await?;
            if lines.is_empty() {
                continue;
            }
            for TailedLine { line, next_offset, line_number } in lines {
                match parse_line(&line, &tailer.path, line_number, next_offset) {
                    Ok(Some(t)) => {
                        // Advance registry to next_offset on every successful turn.
                        let key = tailer.path.display().to_string();
                        let session_id = t.session_id.clone();
                        let turn_idx = t.turn_idx;
                        let _ = self.registry.update(&key, |entry| {
                            entry.last_offset = next_offset;
                            entry.last_session_id = Some(session_id);
                            entry.last_turn_idx = turn_idx;
                        });
                        out.push(t);
                    }
                    Ok(None) => {
                        // Skipped record. Still advance the offset.
                        let key = tailer.path.display().to_string();
                        let _ = self.registry.update(&key, |entry| {
                            entry.last_offset = next_offset;
                        });
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %tailer.path.display(),
                            offset = next_offset,
                            error = ?e,
                            "claude_code: parse failed; skipping"
                        );
                        let key = tailer.path.display().to_string();
                        let _ = self.registry.update(&key, |entry| {
                            entry.last_offset = next_offset;
                        });
                    }
                }
            }
        }
        Ok(out)
    }
}

#[derive(Debug, Deserialize)]
struct ClaudeRecord {
    #[serde(rename = "type")]
    record_type: String,
    #[serde(default, rename = "sessionId")]
    session_id: Option<String>,
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default, rename = "gitBranch")]
    git_branch: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    message: Option<Value>,
}

fn parse_line(
    line: &str,
    path: &Path,
    line_number: u64,
    next_offset: u64,
) -> Result<Option<RawTurn>, SourceError> {
    let r: ClaudeRecord = serde_json::from_str(line)?;

    let (speaker, event_kind) = match r.record_type.as_str() {
        "user" => (Speaker::User, "user_message"),
        "assistant" => (Speaker::Assistant, "assistant_message"),
        "system" => (Speaker::System, "system_message"),
        _ => return Ok(None),
    };

    let session_id = match r.session_id.clone() {
        Some(s) => s,
        None => return Ok(None),
    };

    let (content, extra) = extract_content(&r.message);
    if content.trim().is_empty() {
        return Ok(None);
    }

    let timestamp = r
        .timestamp
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));

    let mut extra_with_branch = extra;
    if let Some(b) = r.git_branch {
        extra_with_branch["git_branch"] = serde_json::Value::String(b);
    }
    if let Some(uuid) = r.uuid {
        extra_with_branch["uuid"] = serde_json::Value::String(uuid);
    }

    Ok(Some(RawTurn {
        session_id,
        turn_idx: line_number,
        speaker,
        content,
        event_kind: event_kind.to_string(),
        timestamp,
        source_path: path.display().to_string(),
        source_offset: next_offset,
        cwd_hint: r.cwd,
        extra: extra_with_branch,
    }))
}

/// Flatten Claude's `message.content` into plain text plus a structured
/// `extra` blob. Strings come through verbatim; lists get their `text`
/// parts joined and any non-text parts (tool_use, thinking) surfaced in
/// `extra.content_blocks`.
fn extract_content(message: &Option<Value>) -> (String, serde_json::Value) {
    let mut extra = serde_json::Map::new();
    let Some(message) = message else {
        return (String::new(), serde_json::Value::Object(extra));
    };
    let content = match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => {
            let mut text_parts = Vec::new();
            let mut blocks = Vec::new();
            for p in parts {
                if let Some(t) = p.get("type").and_then(|v| v.as_str()) {
                    if t == "text" {
                        if let Some(s) = p.get("text").and_then(|v| v.as_str()) {
                            text_parts.push(s.to_string());
                        }
                    } else {
                        blocks.push(p.clone());
                    }
                }
            }
            if !blocks.is_empty() {
                extra.insert("content_blocks".into(), Value::Array(blocks));
            }
            text_parts.join("\n")
        }
        _ => String::new(),
    };
    (content, serde_json::Value::Object(extra))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_user_message() {
        let line = r#"{"type":"user","sessionId":"s1","uuid":"u1","cwd":"/Users/me/code/widget","gitBranch":"main","timestamp":"2026-05-20T01:02:03.000Z","message":{"role":"user","content":"hello there"}}"#;
        let t = parse_line(line, Path::new("/x/y.jsonl"), 1, 200).unwrap().unwrap();
        assert_eq!(t.speaker, Speaker::User);
        assert_eq!(t.content, "hello there");
        assert_eq!(t.session_id, "s1");
        assert_eq!(t.cwd_hint.as_deref(), Some("/Users/me/code/widget"));
        assert!(t.extra.get("git_branch").is_some());
    }

    #[test]
    fn parses_assistant_text_blocks() {
        let line = r#"{"type":"assistant","sessionId":"s1","uuid":"u2","cwd":"/x","timestamp":"2026-05-20T01:02:03.000Z","message":{"role":"assistant","content":[{"type":"thinking","text":"hmm"},{"type":"text","text":"hi"},{"type":"text","text":"there"},{"type":"tool_use","name":"bash","input":{"cmd":"ls"}}]}}"#;
        let t = parse_line(line, Path::new("/x/y.jsonl"), 2, 300).unwrap().unwrap();
        assert_eq!(t.speaker, Speaker::Assistant);
        assert_eq!(t.content, "hi\nthere");
        // The thinking and tool_use blocks land in extra.content_blocks.
        let blocks = t.extra.get("content_blocks").unwrap().as_array().unwrap();
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn skips_metadata_types() {
        let line = r#"{"type":"file-history-snapshot","sessionId":"s1"}"#;
        assert!(parse_line(line, Path::new("/x"), 1, 50).unwrap().is_none());
    }

    #[test]
    fn skips_blank_content() {
        let line = r#"{"type":"user","sessionId":"s1","message":{"role":"user","content":"   "}}"#;
        assert!(parse_line(line, Path::new("/x"), 1, 50).unwrap().is_none());
    }
}
