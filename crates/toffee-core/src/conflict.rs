use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::memory::MemoryId;
use crate::scope::Scope;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConflictId(pub String);

impl ConflictId {
    pub fn generate() -> Self {
        ConflictId(format!("conf_{}", Ulid::new()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ConflictId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictResolution {
    Unresolved,
    Picked,
    Merged,
    RejectedAll,
}

impl ConflictResolution {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConflictResolution::Unresolved => "unresolved",
            ConflictResolution::Picked => "picked",
            ConflictResolution::Merged => "merged",
            ConflictResolution::RejectedAll => "rejected_all",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "unresolved" => ConflictResolution::Unresolved,
            "picked" => ConflictResolution::Picked,
            "merged" => ConflictResolution::Merged,
            "rejected_all" => ConflictResolution::RejectedAll,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConflict {
    pub id: ConflictId,
    pub scope: Scope,
    pub subject: Option<String>,
    pub predicate: Option<String>,
    pub competing_memory_ids: Vec<MemoryId>,
    pub resolution: ConflictResolution,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<DateTime<Utc>>,
}
