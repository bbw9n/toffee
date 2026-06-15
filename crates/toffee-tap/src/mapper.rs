//! Turn a `RawTurn` into an `EventInput` that toffee can ingest.
//!
//! Format-agnostic. The job is:
//!
//! - Decide the scope (caller config wins; else derive from `cwd_hint`;
//!   else fall back to a configured default).
//! - Map `Speaker` to `Actor`.
//! - Tag `event_type` as `"{source_kind}.{event_kind}"`.
//! - Stash the original cwd, timestamp, session, turn_idx in the payload
//!   so the worker / `toffee why` has provenance.

use std::path::Path;

use toffee_core::{Actor, EventInput, Scope};

use crate::source::{RawTurn, Speaker};

/// Override behaviour per source declaration.
#[derive(Debug, Clone, Default)]
pub struct MapperConfig {
    /// Explicit scope to attach to every event. If supplied, `auto`
    /// inference is ignored.
    pub scope_override: Option<Vec<String>>,
    /// When `scope_override` is `None`, derive scope from the turn's
    /// `cwd_hint`. The basename of the cwd becomes `project:<basename>`.
    /// If the cwd has no basename (root path), this is empty.
    pub scope_auto: bool,
    /// Default scope to use when neither override nor auto-inference
    /// produced anything. Empty by default — leaving the event with no
    /// scope, which the daemon will reject. Set to e.g. `["global"]` for
    /// noisy / unscoped sources.
    pub scope_fallback: Vec<String>,
}

pub struct Mapper {
    config: MapperConfig,
}

impl Mapper {
    pub fn new(config: MapperConfig) -> Self {
        Mapper { config }
    }

    pub fn map(&self, source_kind: &str, turn: &RawTurn) -> EventInput {
        let scope_strings = self.scope_for(turn);
        EventInput {
            scope: Scope::new(scope_strings),
            actor: speaker_to_actor(turn.speaker),
            event_type: format!("{source_kind}.{kind}", kind = turn.event_kind),
            payload: serde_json::json!({
                "text": turn.content,
                "tap": {
                    "source_kind": source_kind,
                    "source_path": turn.source_path,
                    "source_offset": turn.source_offset,
                    "session_id": turn.session_id,
                    "turn_idx": turn.turn_idx,
                    "cwd": turn.cwd_hint,
                    "timestamp": turn.timestamp,
                    "extra": turn.extra,
                },
            }),
            session_id: Some(turn.session_id.clone()),
            run_id: None,
        }
    }

    fn scope_for(&self, turn: &RawTurn) -> Vec<String> {
        if let Some(scope) = &self.config.scope_override {
            return scope.clone();
        }
        if self.config.scope_auto {
            if let Some(s) = derive_scope_from_cwd(turn.cwd_hint.as_deref()) {
                return vec![s];
            }
        }
        self.config.scope_fallback.clone()
    }
}

fn speaker_to_actor(s: Speaker) -> Actor {
    match s {
        Speaker::User => Actor::User,
        Speaker::Assistant => Actor::Agent,
        Speaker::Tool => Actor::Tool,
        Speaker::System => Actor::System,
    }
}

fn derive_scope_from_cwd(cwd: Option<&str>) -> Option<String> {
    let cwd = cwd?;
    let basename = Path::new(cwd).file_name().and_then(|s| s.to_str())?;
    if basename.is_empty() {
        return None;
    }
    Some(format!("project:{basename}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_turn() -> RawTurn {
        RawTurn {
            session_id: "s1".into(),
            turn_idx: 1,
            speaker: Speaker::User,
            content: "hello".into(),
            event_kind: "user_message".into(),
            timestamp: None,
            source_path: "/x/y.jsonl".into(),
            source_offset: 100,
            cwd_hint: Some("/Users/me/code/widget".into()),
            extra: json!({}),
        }
    }

    #[test]
    fn auto_scope_is_basename_of_cwd() {
        let m = Mapper::new(MapperConfig {
            scope_auto: true,
            ..Default::default()
        });
        let e = m.map("claude_code", &sample_turn());
        assert_eq!(e.scope.as_slice(), &["project:widget".to_string()]);
        assert_eq!(e.event_type, "claude_code.user_message");
    }

    #[test]
    fn override_wins() {
        let m = Mapper::new(MapperConfig {
            scope_override: Some(vec!["project:explicit".into()]),
            scope_auto: true,
            ..Default::default()
        });
        let e = m.map("codex", &sample_turn());
        assert_eq!(e.scope.as_slice(), &["project:explicit".to_string()]);
    }

    #[test]
    fn fallback_used_when_nothing_else_matches() {
        let m = Mapper::new(MapperConfig {
            scope_auto: false,
            scope_fallback: vec!["global".into()],
            ..Default::default()
        });
        let mut t = sample_turn();
        t.cwd_hint = None;
        let e = m.map("stdin", &t);
        assert_eq!(e.scope.as_slice(), &["global".to_string()]);
    }

    #[test]
    fn tap_metadata_lands_in_payload() {
        let m = Mapper::new(MapperConfig {
            scope_auto: true,
            ..Default::default()
        });
        let e = m.map("claude_code", &sample_turn());
        assert_eq!(e.payload["text"], "hello");
        assert_eq!(e.payload["tap"]["source_kind"], "claude_code");
        assert_eq!(e.payload["tap"]["session_id"], "s1");
    }
}
