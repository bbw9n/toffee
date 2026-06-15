//! Stdin source — primarily for `tmux pipe-pane | toffee tap …`.
//!
//! The format is plain text. We accept an optional `starts_with` prefix
//! that marks user-initiated turns (e.g. `> `); everything between two
//! prefix lines becomes one assistant turn. If no prefix is configured,
//! every non-empty line becomes its own turn.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, BufReader, Stdin};
use tokio::sync::Mutex;

use crate::multiline::{MultilineAccumulator, MultilineConfig};
use crate::source::{RawTurn, Source, SourceError, Speaker};

const SOURCE_KIND: &str = "stdin";

pub struct StdinSource {
    reader: Mutex<BufReader<Stdin>>,
    accumulator: Arc<Mutex<MultilineAccumulator>>,
    user_prefix: Option<String>,
    session_id: String,
    next_turn_idx: u64,
    eof: bool,
}

impl StdinSource {
    pub fn new(user_prefix: Option<String>) -> Self {
        let starts_with = user_prefix.clone();
        StdinSource {
            reader: Mutex::new(BufReader::new(tokio::io::stdin())),
            accumulator: Arc::new(Mutex::new(MultilineAccumulator::new(
                MultilineConfig {
                    starts_with,
                    ..Default::default()
                },
            ))),
            user_prefix,
            session_id: format!("stdin-{}", chrono::Utc::now().timestamp()),
            next_turn_idx: 0,
            eof: false,
        }
    }
}

#[async_trait]
impl Source for StdinSource {
    fn kind(&self) -> &'static str {
        SOURCE_KIND
    }

    async fn read_next(&mut self) -> Result<Vec<RawTurn>, SourceError> {
        if self.eof {
            // Flush any remaining buffered content one last time, then
            // return empty forever.
            let mut acc = self.accumulator.lock().await;
            if let Some(record) = acc.flush() {
                return Ok(vec![turn_from(
                    record,
                    self.user_prefix.as_deref(),
                    &mut self.next_turn_idx,
                    &self.session_id,
                )]);
            }
            return Ok(Vec::new());
        }

        let mut reader = self.reader.lock().await;
        let mut out = Vec::new();
        // Read up to 64 lines per call so we don't starve the runner of
        // backoff opportunities.
        for _ in 0..64 {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                self.eof = true;
                drop(reader);
                let mut acc = self.accumulator.lock().await;
                if let Some(record) = acc.flush() {
                    out.push(turn_from(
                        record,
                        self.user_prefix.as_deref(),
                        &mut self.next_turn_idx,
                        &self.session_id,
                    ));
                }
                return Ok(out);
            }
            let trimmed = line.trim_end_matches(['\n', '\r']);
            if trimmed.is_empty() {
                continue;
            }
            let mut acc = self.accumulator.lock().await;
            if let Some(record) = acc.push_line(trimmed) {
                out.push(turn_from(
                    record,
                    self.user_prefix.as_deref(),
                    &mut self.next_turn_idx,
                    &self.session_id,
                ));
            }
        }
        Ok(out)
    }
}

fn turn_from(
    content: String,
    user_prefix: Option<&str>,
    next_idx: &mut u64,
    session_id: &str,
) -> RawTurn {
    let speaker = match user_prefix {
        Some(p) if !p.is_empty() && content.starts_with(p) => Speaker::User,
        Some(_) => Speaker::Assistant,
        None => Speaker::Assistant, // best guess
    };
    let event_kind = match speaker {
        Speaker::User => "user_message",
        Speaker::Assistant => "assistant_message",
        _ => "stream_chunk",
    };
    let idx = *next_idx;
    *next_idx += 1;
    let content = match (speaker, user_prefix) {
        (Speaker::User, Some(p)) => content.trim_start_matches(p).trim().to_string(),
        _ => content,
    };
    RawTurn {
        session_id: session_id.to_string(),
        turn_idx: idx,
        speaker,
        content,
        event_kind: event_kind.to_string(),
        timestamp: Some(chrono::Utc::now()),
        source_path: "stdin".to_string(),
        source_offset: 0,
        cwd_hint: None,
        extra: serde_json::Value::Object(Default::default()),
    }
}
