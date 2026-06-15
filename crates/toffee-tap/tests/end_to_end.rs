//! End-to-end ingest: write synthetic Claude Code and Codex session
//! files, point the runner at them, and verify the expected `EventInput`s
//! land in a mock sink.
//!
//! This is the cheapest way to exercise discovery → tail → parse →
//! mapper → sink without spinning up a real `toffeed`.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tempfile::TempDir;
use toffee_core::{Actor, EventId, EventInput};
use toffee_tap::{Config, FileSourceConfig, Runner, RunnerOptions, Sink, SourceConfig};

/// Inline MockSink — the lib's cfg-gated one isn't visible to integration
/// tests, and inlining avoids feature-flag gymnastics.
#[derive(Default)]
struct MockSink {
    events: Mutex<Vec<EventInput>>,
}

impl MockSink {
    fn captured(&self) -> Vec<EventInput> {
        self.events.lock().unwrap().clone()
    }
}

#[async_trait]
impl Sink for MockSink {
    async fn append_event(
        &self,
        input: EventInput,
    ) -> Result<EventId, toffee_tap::sink::SinkError> {
        let id = EventId::generate();
        self.events.lock().unwrap().push(input);
        Ok(id)
    }
}

fn write_claude_session(
    root: &TempDir,
    project: &str,
    session_id: &str,
    lines: &[serde_json::Value],
) -> std::path::PathBuf {
    let dir = root.path().join("projects").join(project);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{session_id}.jsonl"));
    let mut f = std::fs::File::create(&path).unwrap();
    for l in lines {
        writeln!(f, "{}", serde_json::to_string(l).unwrap()).unwrap();
    }
    f.sync_all().unwrap();
    path
}

fn write_codex_rollout(
    root: &TempDir,
    day: &str,
    session: &str,
    lines: &[serde_json::Value],
) -> std::path::PathBuf {
    let dir = root.path().join("2026").join("05").join(day);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("rollout-2026-05-{day}-{session}.jsonl"));
    let mut f = std::fs::File::create(&path).unwrap();
    for l in lines {
        writeln!(f, "{}", serde_json::to_string(l).unwrap()).unwrap();
    }
    f.sync_all().unwrap();
    path
}

#[tokio::test]
async fn ingests_synthetic_claude_session() {
    let tmp = TempDir::new().unwrap();
    let registry = tmp.path().join("registry.json");

    let _ = write_claude_session(
        &tmp,
        "-Users-me-code-widget",
        "session-1",
        &[
            json!({"type": "permission-mode", "sessionId": "session-1"}),
            json!({
                "type": "user",
                "sessionId": "session-1",
                "uuid": "u1",
                "cwd": "/Users/me/code/widget",
                "gitBranch": "main",
                "timestamp": "2026-05-20T01:02:03.000Z",
                "message": {"role": "user", "content": "fix the parser bug"},
            }),
            json!({
                "type": "assistant",
                "sessionId": "session-1",
                "uuid": "a1",
                "cwd": "/Users/me/code/widget",
                "timestamp": "2026-05-20T01:02:05.000Z",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "text": "internal"},
                        {"type": "text", "text": "hello"},
                        {"type": "text", "text": "world"},
                        {"type": "tool_use", "name": "bash", "input": {"cmd": "ls"}},
                    ],
                },
            }),
        ],
    );

    let config = Config {
        registry: Some(registry),
        sources: vec![SourceConfig::ClaudeCode(FileSourceConfig {
            path: Some(tmp.path().join("projects")),
            scope: None,
            scope_auto: true,
            scope_fallback: vec!["global".into()],
        })],
    };
    let sink = Arc::new(MockSink::default());
    let runner = Runner::build(
        RunnerOptions {
            config,
            one_shot: true,
        },
        sink.clone(),
    )
    .unwrap();

    // Drive the runner for a fixed window then cancel.
    let (tx, rx) = tokio::sync::broadcast::channel(1);
    let handle = tokio::spawn(async move { runner.run(rx).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = tx.send(());
    let _ = handle.await.unwrap();

    let events = sink.captured();
    assert_eq!(
        events.len(),
        2,
        "expected user + assistant, got {events:#?}"
    );
    assert_eq!(events[0].actor, Actor::User);
    assert_eq!(events[0].event_type, "claude_code.user_message");
    assert_eq!(events[0].scope.as_slice(), &["project:widget".to_string()]);
    assert_eq!(events[0].payload["text"], "fix the parser bug");
    assert_eq!(events[1].actor, Actor::Agent);
    assert_eq!(events[1].payload["text"], "hello\nworld");
    // The non-text content blocks landed in the tap.extra blob.
    let extra = &events[1].payload["tap"]["extra"];
    assert!(extra.get("content_blocks").is_some());
}

#[tokio::test]
async fn ingests_synthetic_codex_rollout() {
    let tmp = TempDir::new().unwrap();
    let registry = tmp.path().join("registry.json");

    let _ = write_codex_rollout(
        &tmp,
        "20",
        "abc",
        &[
            json!({
                "type": "session_meta",
                "timestamp": "2026-05-20T01:02:03.000Z",
                "payload": {
                    "id": "sess-uuid",
                    "cwd": "/Users/me/code/widget",
                },
            }),
            json!({
                "type": "turn_context",
                "timestamp": "2026-05-20T01:02:03.500Z",
                "payload": {"turn_id": "t1", "cwd": "/Users/me/code/widget"},
            }),
            json!({
                "type": "response_item",
                "timestamp": "2026-05-20T01:02:04.000Z",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "build the cli"}],
                },
            }),
            json!({
                "type": "response_item",
                "timestamp": "2026-05-20T01:02:06.000Z",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "sure"}],
                },
            }),
            // Should be filtered out:
            json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "developer",
                    "content": [{"type": "input_text", "text": "sandbox config"}],
                },
            }),
            json!({
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "t1"},
            }),
        ],
    );

    let config = Config {
        registry: Some(registry),
        sources: vec![SourceConfig::Codex(FileSourceConfig {
            path: Some(tmp.path().to_path_buf()),
            scope: None,
            scope_auto: true,
            scope_fallback: vec!["global".into()],
        })],
    };
    let sink = Arc::new(MockSink::default());
    let runner = Runner::build(
        RunnerOptions {
            config,
            one_shot: true,
        },
        sink.clone(),
    )
    .unwrap();

    let (tx, rx) = tokio::sync::broadcast::channel(1);
    let handle = tokio::spawn(async move { runner.run(rx).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = tx.send(());
    let _ = handle.await.unwrap();

    let events = sink.captured();
    assert_eq!(events.len(), 2, "expected just the user + assistant turns");
    assert_eq!(events[0].actor, Actor::User);
    assert_eq!(events[0].event_type, "codex.user_message");
    assert_eq!(events[0].scope.as_slice(), &["project:widget".to_string()]);
    assert_eq!(events[0].payload["text"], "build the cli");
    assert_eq!(events[1].payload["text"], "sure");
}

#[tokio::test]
async fn registry_resumes_from_offset_on_restart() {
    let tmp = TempDir::new().unwrap();
    let registry = tmp.path().join("registry.json");

    // Pass 1: write one user turn, run, expect one event.
    let _ = write_claude_session(
        &tmp,
        "-Users-me-code-widget",
        "session-2",
        &[json!({
            "type": "user",
            "sessionId": "session-2",
            "uuid": "u1",
            "cwd": "/Users/me/code/widget",
            "timestamp": "2026-05-20T01:02:03.000Z",
            "message": {"role": "user", "content": "first"},
        })],
    );
    let pass1_sink = Arc::new(MockSink::default());
    {
        let config = Config {
            registry: Some(registry.clone()),
            sources: vec![SourceConfig::ClaudeCode(FileSourceConfig {
                path: Some(tmp.path().join("projects")),
                scope: None,
                scope_auto: true,
                scope_fallback: vec!["global".into()],
            })],
        };
        let runner = Runner::build(
            RunnerOptions {
                config,
                one_shot: true,
            },
            pass1_sink.clone(),
        )
        .unwrap();
        let (tx, rx) = tokio::sync::broadcast::channel(1);
        let h = tokio::spawn(async move { runner.run(rx).await });
        tokio::time::sleep(Duration::from_millis(400)).await;
        let _ = tx.send(());
        let _ = h.await.unwrap();
    }
    assert_eq!(pass1_sink.captured().len(), 1);

    // Pass 2: append a second turn to the same file, run a fresh runner
    // pointed at the same registry. Should pick up only the new turn.
    let path = tmp
        .path()
        .join("projects")
        .join("-Users-me-code-widget")
        .join("session-2.jsonl");
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(
        f,
        "{}",
        serde_json::to_string(&json!({
            "type": "user",
            "sessionId": "session-2",
            "uuid": "u2",
            "cwd": "/Users/me/code/widget",
            "timestamp": "2026-05-20T01:02:10.000Z",
            "message": {"role": "user", "content": "second"},
        }))
        .unwrap()
    )
    .unwrap();
    f.sync_all().unwrap();
    drop(f);

    let pass2_sink = Arc::new(MockSink::default());
    {
        let config = Config {
            registry: Some(registry),
            sources: vec![SourceConfig::ClaudeCode(FileSourceConfig {
                path: Some(tmp.path().join("projects")),
                scope: None,
                scope_auto: true,
                scope_fallback: vec!["global".into()],
            })],
        };
        let runner = Runner::build(
            RunnerOptions {
                config,
                one_shot: true,
            },
            pass2_sink.clone(),
        )
        .unwrap();
        let (tx, rx) = tokio::sync::broadcast::channel(1);
        let h = tokio::spawn(async move { runner.run(rx).await });
        tokio::time::sleep(Duration::from_millis(400)).await;
        let _ = tx.send(());
        let _ = h.await.unwrap();
    }
    let pass2 = pass2_sink.captured();
    assert_eq!(pass2.len(), 1, "registry should have skipped the old turn");
    assert_eq!(pass2[0].payload["text"], "second");
}
