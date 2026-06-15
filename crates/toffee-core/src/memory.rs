use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::event::EventId;
use crate::scope::Scope;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MemoryId(pub String);

impl MemoryId {
    pub fn generate() -> Self {
        MemoryId(format!("mem_{}", Ulid::new()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MemoryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Claim,
    Decision,
    Preference,
    Episode,
}

impl MemoryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryKind::Claim => "claim",
            MemoryKind::Decision => "decision",
            MemoryKind::Preference => "preference",
            MemoryKind::Episode => "episode",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "claim" => MemoryKind::Claim,
            "decision" => MemoryKind::Decision,
            "preference" => MemoryKind::Preference,
            "episode" => MemoryKind::Episode,
            _ => return None,
        })
    }

    /// Episodes are summaries — they live without subject/predicate/object.
    pub fn requires_spo(&self) -> bool {
        !matches!(self, MemoryKind::Episode)
    }
}

/// A typed, durable memory derived from one or more events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: MemoryId,
    pub kind: MemoryKind,
    pub scope: Scope,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predicate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    #[serde(default)]
    pub entities: Vec<String>,
    pub confidence: f64,
    pub source_event_ids: Vec<EventId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<MemoryId>,
}

/// A pre-promotion memory the worker produces and the promoter decides on.
#[derive(Debug, Clone)]
pub struct MemoryCandidate {
    pub kind: MemoryKind,
    pub scope: Scope,
    pub text: String,
    pub subject: Option<String>,
    pub predicate: Option<String>,
    pub object: Option<String>,
    pub entities: Vec<String>,
    /// Optional pre-computed confidence; `scalars::compute_confidence` may
    /// also derive it from `provenance` and `kind`.
    pub confidence: Option<f64>,
    pub source_event_ids: Vec<EventId>,
    pub provenance: ExtractionProvenance,
}

impl MemoryCandidate {
    pub fn with_entities(mut self, entities: Vec<String>) -> Self {
        self.entities = entities;
        self
    }
}

/// Where a candidate came from. Drives the default confidence and the
/// promoter's discard/episode/promote decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractionProvenance {
    /// User said it directly ("remember that...", "I prefer...").
    ExplicitUser,
    /// Agent said it and the user confirmed in a follow-up turn.
    AgentConfirmed,
    /// Same SPO seen in ≥3 events within a scope.
    RepeatedPattern,
    /// Agent statement, unconfirmed.
    AgentInferred,
    /// Tool output.
    ToolOutput,
    /// CLI / manual add. Trust whoever typed it.
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackKind {
    Helpful,
    Wrong,
    Stale,
    Correct,
}

impl FeedbackKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            FeedbackKind::Helpful => "helpful",
            FeedbackKind::Wrong => "wrong",
            FeedbackKind::Stale => "stale",
            FeedbackKind::Correct => "correct",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "helpful" => FeedbackKind::Helpful,
            "wrong" => FeedbackKind::Wrong,
            "stale" => FeedbackKind::Stale,
            "correct" => FeedbackKind::Correct,
            _ => return None,
        })
    }
}
