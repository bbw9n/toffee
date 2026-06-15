use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::memory::Memory;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EntityId(pub String);

impl EntityId {
    pub fn generate() -> Self {
        EntityId(format!("ent_{}", Ulid::new()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for EntityId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Coarse taxonomy. v1 is heuristic and conservative — most auto-extracted
/// entities land in `Concept` and humans relabel via the CLI when it matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityType {
    Person,
    Project,
    Library,
    Tool,
    Concept,
    Other,
}

impl EntityType {
    pub fn as_str(&self) -> &'static str {
        match self {
            EntityType::Person => "person",
            EntityType::Project => "project",
            EntityType::Library => "library",
            EntityType::Tool => "tool",
            EntityType::Concept => "concept",
            EntityType::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "person" => EntityType::Person,
            "project" => EntityType::Project,
            "library" => EntityType::Library,
            "tool" => EntityType::Tool,
            "concept" => EntityType::Concept,
            "other" => EntityType::Other,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub id: EntityId,
    pub entity_type: EntityType,
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityPage {
    pub entity: Entity,
    /// Memories referencing this entity, across inherited scopes.
    pub memories: Vec<Memory>,
    /// Other entities that co-occur with this one. Sorted by descending
    /// co-occurrence count.
    pub co_occurring: Vec<(Entity, usize)>,
}
