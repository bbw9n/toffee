//! OpenAI Codex CLI session rollouts.
//!
//! Path: `~/.codex/sessions/<yyyy>/<mm>/<dd>/rollout-<ts>-<session-uuid>.jsonl`.
//! Each line is `{type, timestamp, payload}`.
//!
//! Records we care about:
//!   - `session_meta` (one per file): `payload.id` is the session UUID,
//!     `payload.cwd` is the working directory. We don't emit a memory
//!     turn for this; we cache it so subsequent records inherit the
//!     session id and cwd.
//!   - `response_item` with `payload.type=="message"` and `payload.role`
//!     ∈ {user, assistant}. The `payload.content` is a list of
//!     `{type:input_text|output_text, text}`.
//!
//! Everything else (`event_msg`, `turn_context`, `reasoning`,
//! `function_call(_output)`, `developer` / `system` roles) is ignored —
//! it's either metadata or content we don't yet promote to memory. Tool
//! calls are a v2 addition.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::registry::Registry;
use crate::source::{RawTurn, Source, SourceError, Speaker};
use crate::sources::jsonl_tail::{discover, expand_path, JsonlTailer, TailedLine};

const SOURCE_KIND: &str = "codex";

pub struct CodexSource {
    registry: Arc<Registry>,
    discovery_root: PathBuf,
    tailers: Vec<JsonlTailer>,
    /// Per-file remembered session header (id, cwd). Populated from the
    /// first `session_meta` record we see for a given path.
    session_meta: HashMap<String, SessionMeta>,
    rediscover_every: u32,
    ticks: u32,
}

#[derive(Debug, Clone)]
struct SessionMeta {
    id: String,
    cwd: Option<String>,
}

impl CodexSource {
    pub fn new(path: impl AsRef<Path>, registry: Arc<Registry>) -> Self {
        CodexSource {
            registry,
            discovery_root: expand_path(path),
            tailers: Vec::new(),
            session_meta: HashMap::new(),
            rediscover_every: 8,
            ticks: 0,
        }
    }

    pub fn default_path() -> PathBuf {
        expand_path("~/.codex/sessions")
    }

    fn rediscover(&mut self) {
        let pattern = format!(
            "{}/**/rollout-*.jsonl",
            self.discovery_root.display()
        );
        let found = discover(&pattern);
        for p in found {
            let key = p.display().to_string();
            if self.tailers.iter().any(|t| t.path.display().to_string() == key) {
                continue;
            }
            match JsonlTailer::open_with_registry(p.clone(), SOURCE_KIND, &self.registry) {
                Ok(t) => {
                    tracing::info!(path = %p.display(), "codex: now tailing");
                    self.tailers.push(t);
                }
                Err(e) => {
                    tracing::debug!(path = %p.display(), error = ?e, "codex: skipping");
                }
            }
        }
    }
}

#[async_trait]
impl Source for CodexSource {
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
            let path_key = tailer.path.display().to_string();
            for TailedLine { line, next_offset, line_number } in lines {
                let raw: Result<CodexRecord, _> = serde_json::from_str(&line);
                let parsed = match raw {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(
                            path = %tailer.path.display(),
                            offset = next_offset,
                            error = %e,
                            "codex: parse failed; skipping"
                        );
                        let _ = self.registry.update(&path_key, |entry| {
                            entry.last_offset = next_offset;
                        });
                        continue;
                    }
                };

                // session_meta caches per-file context. Don't emit a turn.
                if parsed.record_type == "session_meta" {
                    if let Some(p) = parsed.payload.as_ref() {
                        if let (Some(id), cwd) = (
                            p.get("id").and_then(|v| v.as_str()),
                            p.get("cwd").and_then(|v| v.as_str()).map(|s| s.to_string()),
                        ) {
                            self.session_meta.insert(
                                path_key.clone(),
                                SessionMeta { id: id.to_string(), cwd },
                            );
                        }
                    }
                    let _ = self.registry.update(&path_key, |entry| {
                        entry.last_offset = next_offset;
                    });
                    continue;
                }

                // turn_context can refresh the cwd mid-session.
                if parsed.record_type == "turn_context" {
                    if let Some(p) = parsed.payload.as_ref() {
                        if let Some(cwd) = p.get("cwd").and_then(|v| v.as_str()) {
                            self.session_meta
                                .entry(path_key.clone())
                                .and_modify(|m| m.cwd = Some(cwd.to_string()));
                        }
                    }
                    let _ = self.registry.update(&path_key, |entry| {
                        entry.last_offset = next_offset;
                    });
                    continue;
                }

                if parsed.record_type != "response_item" {
                    let _ = self.registry.update(&path_key, |entry| {
                        entry.last_offset = next_offset;
                    });
                    continue;
                }

                let Some(payload) = parsed.payload else {
                    let _ = self.registry.update(&path_key, |entry| {
                        entry.last_offset = next_offset;
                    });
                    continue;
                };

                let meta = self.session_meta.get(&path_key);
                let turn = parse_response_item(
                    &payload,
                    parsed.timestamp.as_deref(),
                    meta,
                    &tailer.path,
                    line_number,
                    next_offset,
                );

                match turn {
                    Some(t) => {
                        let session_id = t.session_id.clone();
                        let turn_idx = t.turn_idx;
                        let _ = self.registry.update(&path_key, |entry| {
                            entry.last_offset = next_offset;
                            entry.last_session_id = Some(session_id);
                            entry.last_turn_idx = turn_idx;
                        });
                        out.push(t);
                    }
                    None => {
                        let _ = self.registry.update(&path_key, |entry| {
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
struct CodexRecord {
    #[serde(rename = "type")]
    record_type: String,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    payload: Option<Value>,
}

fn parse_response_item(
    payload: &Value,
    timestamp: Option<&str>,
    meta: Option<&SessionMeta>,
    path: &Path,
    line_number: u64,
    next_offset: u64,
) -> Option<RawTurn> {
    if payload.get("type")?.as_str()? != "message" {
        return None;
    }
    let role = payload.get("role")?.as_str()?;
    let (speaker, event_kind) = match role {
        "user" => (Speaker::User, "user_message"),
        "assistant" => (Speaker::Assistant, "assistant_message"),
        _ => return None, // skip developer / system / unknown
    };
    let content_array = payload.get("content")?.as_array()?;
    let mut texts = Vec::new();
    for part in content_array {
        let t = part.get("type")?.as_str()?;
        if matches!(t, "input_text" | "output_text") {
            if let Some(s) = part.get("text").and_then(|v| v.as_str()) {
                texts.push(s.to_string());
            }
        }
    }
    let content = texts.join("\n");
    if content.trim().is_empty() {
        return None;
    }

    let meta = meta?;
    let timestamp = timestamp
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));

    Some(RawTurn {
        session_id: meta.id.clone(),
        turn_idx: line_number,
        speaker,
        content,
        event_kind: event_kind.to_string(),
        timestamp,
        source_path: path.display().to_string(),
        source_offset: next_offset,
        cwd_hint: meta.cwd.clone(),
        extra: serde_json::Value::Object(Default::default()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> SessionMeta {
        SessionMeta {
            id: "sess-1".into(),
            cwd: Some("/Users/me/code/widget".into()),
        }
    }

    #[test]
    fn parses_user_message_payload() {
        let p: Value = serde_json::from_str(
            r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"build me a thing"}]}"#,
        )
        .unwrap();
        let t = parse_response_item(
            &p,
            Some("2026-05-20T00:00:00.000Z"),
            Some(&meta()),
            Path::new("/x/y.jsonl"),
            5,
            500,
        )
        .unwrap();
        assert_eq!(t.speaker, Speaker::User);
        assert_eq!(t.content, "build me a thing");
        assert_eq!(t.session_id, "sess-1");
        assert_eq!(t.cwd_hint.as_deref(), Some("/Users/me/code/widget"));
    }

    #[test]
    fn parses_assistant_output_text() {
        let p: Value = serde_json::from_str(
            r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"sure thing"},{"type":"output_text","text":"more"}]}"#,
        )
        .unwrap();
        let t = parse_response_item(&p, None, Some(&meta()), Path::new("/x"), 6, 600).unwrap();
        assert_eq!(t.speaker, Speaker::Assistant);
        assert_eq!(t.content, "sure thing\nmore");
    }

    #[test]
    fn skips_developer_role() {
        let p: Value = serde_json::from_str(
            r#"{"type":"message","role":"developer","content":[{"type":"input_text","text":"x"}]}"#,
        )
        .unwrap();
        assert!(parse_response_item(&p, None, Some(&meta()), Path::new("/x"), 1, 1).is_none());
    }

    #[test]
    fn skips_reasoning_and_function_calls() {
        let p: Value =
            serde_json::from_str(r#"{"type":"reasoning","summary":[{"type":"summary_text","text":"…"}]}"#).unwrap();
        assert!(parse_response_item(&p, None, Some(&meta()), Path::new("/x"), 1, 1).is_none());
    }
}
