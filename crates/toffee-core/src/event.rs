use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ulid::Ulid;

use crate::scope::Scope;

/// Opaque event identifier of the form `evt_<ulid>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventId(pub String);

impl EventId {
    pub fn generate() -> Self {
        EventId(format!("evt_{}", Ulid::new()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for EventId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Who produced an event. Custom values fall through `Other` so that the
/// daemon does not reject events from forward-rolling clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    User,
    Agent,
    Tool,
    System,
    #[serde(untagged)]
    Other(String),
}

impl Actor {
    pub fn as_str(&self) -> &str {
        match self {
            Actor::User => "user",
            Actor::Agent => "agent",
            Actor::Tool => "tool",
            Actor::System => "system",
            Actor::Other(s) => s,
        }
    }
}

impl std::fmt::Display for Actor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a client sends to `toffee.append_event`. The daemon assigns `id` and
/// `created_at`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventInput {
    pub scope: Scope,
    pub actor: Actor,
    pub event_type: String,
    #[serde(default)]
    pub payload: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

/// A durably stored event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: EventId,
    pub scope: Scope,
    pub actor: Actor,
    pub event_type: String,
    pub payload: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl Event {
    pub fn from_input(input: EventInput, id: EventId, created_at: DateTime<Utc>) -> Self {
        Event {
            id,
            scope: input.scope,
            actor: input.actor,
            event_type: input.event_type,
            payload: input.payload,
            session_id: input.session_id,
            run_id: input.run_id,
            created_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_id_generates_unique_ids() {
        let a = EventId::generate();
        let b = EventId::generate();
        assert_ne!(a, b);
        assert!(a.as_str().starts_with("evt_"));
    }

    #[test]
    fn actor_round_trips_known_variants() {
        for a in [Actor::User, Actor::Agent, Actor::Tool, Actor::System] {
            let s = serde_json::to_string(&a).unwrap();
            let back: Actor = serde_json::from_str(&s).unwrap();
            assert_eq!(a, back);
        }
    }

    #[test]
    fn actor_round_trips_unknown_variant() {
        let custom = Actor::Other("plugin".to_string());
        let s = serde_json::to_string(&custom).unwrap();
        let back: Actor = serde_json::from_str(&s).unwrap();
        assert_eq!(custom, back);
    }
}
