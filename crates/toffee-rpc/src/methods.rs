use serde::{Deserialize, Serialize};
use toffee_core::{
    ConflictId, ContextPackage, ContextPackageId, Entity, EntityPage, EntityType, EventId,
    EventInput, FeedbackKind, Lens, Memory, MemoryConflict, MemoryId, MemoryKind, ProvenanceReport,
    Scope, WhyMemoryReport, WorkerStatus,
};

/// `toffee.hello` — optional handshake. Clients may identify themselves and
/// learn what the server supports.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloRequest {
    #[serde(default)]
    pub client_name: Option<String>,
    #[serde(default)]
    pub client_version: Option<String>,
}

pub type HelloResponse = ServerInfo;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub server_name: String,
    pub server_version: String,
    /// Seconds the daemon has been running.
    pub uptime_seconds: u64,
    /// Method names this server understands.
    pub supported_methods: Vec<String>,
    /// Path to the SQLite database (informational).
    pub db_path: String,
    /// Number of events currently stored.
    pub event_count: i64,
}

/// `toffee.append_event` — record one event.
pub type AppendEventRequest = EventInput;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppendEventResponse {
    pub event_id: EventId,
}

/// `toffee.record_feedback` — adjust a memory's confidence based on user signal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordFeedbackRequest {
    pub memory_id: MemoryId,
    pub kind: FeedbackKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordFeedbackResponse {
    pub memory_id: MemoryId,
    pub new_confidence: f64,
}

/// `toffee.add_memory` — manually add a memory (CLI / human-curated).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddMemoryRequest {
    pub kind: MemoryKind,
    pub scope: Scope,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predicate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddMemoryResponse {
    pub memory: Memory,
}

/// `toffee.forget_memory` — soft-delete a memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgetMemoryRequest {
    pub memory_id: MemoryId,
}

/// `toffee.list_memories` — enumerate stored memories with simple filters.
/// Full vector-ranked retrieval is `toffee.search_memory` in a later phase.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListMemoriesRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_any_of: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<MemoryKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// Optional case-insensitive substring filter on `text`. The "search"
    /// here is naive — vector ranking arrives in a later phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListMemoriesResponse {
    pub memories: Vec<Memory>,
}

/// `toffee.get_memory` — fetch a memory by id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetMemoryRequest {
    pub memory_id: MemoryId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetMemoryResponse {
    pub memory: Memory,
}

/// `toffee.list_entities` — enumerate stored entities.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListEntitiesRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_type: Option<EntityType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListEntitiesResponse {
    pub entities: Vec<Entity>,
}

/// `toffee.search_memory` — vector-ranked memory search.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchMemoryRequest {
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_any_of: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<MemoryKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_similarity: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchMemoryHit {
    pub memory: Memory,
    pub similarity: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchMemoryResponse {
    pub hits: Vec<SearchMemoryHit>,
}

/// `toffee.read_context` — assemble the context package an agent prepends
/// to its prompt. This is the Phase 4 integrator entry point.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadContextRequest {
    pub scope: Vec<String>,
    pub query: String,
    /// Lens name. Today only `"default"` is recognised; an unknown name
    /// falls back to default with a server-side warning. Future lenses
    /// can be plugged in without a wire change.
    #[serde(default)]
    pub lens: String,
    /// Optional custom lens overriding the named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_lens: Option<Lens>,
    /// Total token budget across all kinds. Default 3000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadContextResponse {
    pub package: ContextPackage,
}

/// `toffee.inspect_provenance` — explain why each memory landed in a
/// previously-issued context package.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectProvenanceRequest {
    pub context_package_id: ContextPackageId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectProvenanceResponse {
    pub report: ProvenanceReport,
}

/// `toffee.worker_status` — checkpoint, queue depth, lag, totals,
/// recent failures.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkerStatusRequest {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerStatusResponse {
    pub status: WorkerStatus,
}

/// `toffee.why_memory` — assemble the full breakdown for one memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhyMemoryRequest {
    pub memory_id: MemoryId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhyMemoryResponse {
    pub report: WhyMemoryReport,
}

/// `toffee.list_conflicts` — enumerate conflicts (defaults to unresolved).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListConflictsRequest {
    /// If true, include already-resolved conflicts as well.
    #[serde(default)]
    pub include_resolved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListConflictsResponse {
    pub conflicts: Vec<MemoryConflict>,
}

/// `toffee.get_conflict` — fetch one conflict by id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetConflictRequest {
    pub conflict_id: ConflictId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetConflictResponse {
    pub conflict: MemoryConflict,
}

/// `toffee.resolve_conflict` — pick / merge / reject-all. See
/// `toffee_runtime::ResolutionAction` for the semantics.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum ResolveConflictAction {
    Pick {
        winner: MemoryId,
    },
    Merge {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subject: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        predicate: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        object: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confidence: Option<f64>,
    },
    RejectAll,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveConflictRequest {
    pub conflict_id: ConflictId,
    #[serde(flatten)]
    pub action: ResolveConflictAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveConflictResponse {
    pub conflict: MemoryConflict,
}

/// `toffee.daemon.rebuild_indexes` — drop and recompute every embedding +
/// the in-memory HNSW. Returns the number of memories re-indexed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebuildIndexesResponse {
    pub reindexed: usize,
}

/// `toffee.get_entity_page` — assemble the rendered entity page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetEntityPageRequest {
    /// Either an entity id (`ent_…`) or a case-insensitive name / alias.
    pub identifier: String,
    /// Optional scope filter; the runtime applies inherited expansion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetEntityPageResponse {
    pub page: EntityPage,
}

/// Method names we dispatch on. Keeping these as `&'static str` lets the
/// server side use a plain `match`.
pub mod method_names {
    pub const HELLO: &str = "toffee.hello";
    pub const APPEND_EVENT: &str = "toffee.append_event";
    pub const RECORD_FEEDBACK: &str = "toffee.record_feedback";
    pub const ADD_MEMORY: &str = "toffee.add_memory";
    pub const FORGET_MEMORY: &str = "toffee.forget_memory";
    pub const LIST_MEMORIES: &str = "toffee.list_memories";
    pub const GET_MEMORY: &str = "toffee.get_memory";
    pub const LIST_ENTITIES: &str = "toffee.list_entities";
    pub const GET_ENTITY_PAGE: &str = "toffee.get_entity_page";
    pub const SEARCH_MEMORY: &str = "toffee.search_memory";
    pub const READ_CONTEXT: &str = "toffee.read_context";
    pub const INSPECT_PROVENANCE: &str = "toffee.inspect_provenance";
    pub const LIST_CONFLICTS: &str = "toffee.list_conflicts";
    pub const GET_CONFLICT: &str = "toffee.get_conflict";
    pub const RESOLVE_CONFLICT: &str = "toffee.resolve_conflict";
    pub const WORKER_STATUS: &str = "toffee.worker_status";
    pub const WHY_MEMORY: &str = "toffee.why_memory";
    pub const DAEMON_SHUTDOWN: &str = "toffee.daemon.shutdown";
    pub const DAEMON_REBUILD_INDEXES: &str = "toffee.daemon.rebuild_indexes";

    pub fn all() -> Vec<String> {
        vec![
            HELLO.to_string(),
            APPEND_EVENT.to_string(),
            RECORD_FEEDBACK.to_string(),
            ADD_MEMORY.to_string(),
            FORGET_MEMORY.to_string(),
            LIST_MEMORIES.to_string(),
            GET_MEMORY.to_string(),
            LIST_ENTITIES.to_string(),
            GET_ENTITY_PAGE.to_string(),
            SEARCH_MEMORY.to_string(),
            READ_CONTEXT.to_string(),
            INSPECT_PROVENANCE.to_string(),
            LIST_CONFLICTS.to_string(),
            GET_CONFLICT.to_string(),
            RESOLVE_CONFLICT.to_string(),
            WORKER_STATUS.to_string(),
            WHY_MEMORY.to_string(),
            DAEMON_SHUTDOWN.to_string(),
            DAEMON_REBUILD_INDEXES.to_string(),
        ]
    }
}
