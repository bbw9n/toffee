//! Observability types: worker status, recorded failures, and the
//! notification envelope the daemon pushes to subscribed clients.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::conflict::MemoryConflict;
use crate::entity::Entity;
use crate::event::{Event, EventId};
use crate::memory::{Memory, MemoryId, MemoryKind};

/// A row from the `worker_failures` table. Records one event the worker
/// couldn't process and the error it produced. Phase 6 doesn't retry these
/// automatically — that's a v2 concern; for now they're just visible to the
/// operator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerFailure {
    pub id: i64,
    pub event_id: EventId,
    pub worker_id: String,
    pub error: String,
    pub occurred_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retried_at: Option<DateTime<Utc>>,
}

/// What `toffee.worker_status` returns. Everything is read at query time —
/// the daemon doesn't push status; clients that want change events
/// subscribe via [`Notification`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerStatus {
    pub worker_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_processed_event_id: Option<EventId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_processed_at: Option<DateTime<Utc>>,
    /// Events strictly after the checkpoint, still waiting.
    pub queue_depth: i64,
    /// Seconds since the last processed event. `None` if the worker hasn't
    /// processed anything yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lag_seconds: Option<i64>,
    /// Total events recorded.
    pub events_total: i64,
    pub memories_active: i64,
    pub conflicts_unresolved: i64,
    pub failures_recent: Vec<WorkerFailure>,
}

/// JSON-RPC notification envelope. The daemon emits these as plain
/// JSON-RPC notifications (no `id`, no response). Each variant maps to one
/// `method` name on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Notification {
    /// `toffee.memory.promoted` — a new durable memory landed.
    #[serde(rename = "toffee.memory.promoted")]
    MemoryPromoted {
        memory_id: MemoryId,
        kind: MemoryKind,
        scope: Vec<String>,
        confidence: f64,
    },
    /// `toffee.worker.lag_changed` — fired when the worker's lag crosses
    /// the operator-visible threshold (currently hard-coded at 30s in the
    /// runtime).
    #[serde(rename = "toffee.worker.lag_changed")]
    WorkerLagChanged {
        lagging: bool,
        lag_seconds: i64,
        queue_depth: i64,
    },
}

impl Notification {
    pub fn method_name(&self) -> &'static str {
        match self {
            Notification::MemoryPromoted { .. } => "toffee.memory.promoted",
            Notification::WorkerLagChanged { .. } => "toffee.worker.lag_changed",
        }
    }
}

/// Everything the daemon knows about how one memory came to be.
/// Returned by `toffee.why_memory` and rendered by `toffee why <mem_id>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhyMemoryReport {
    pub memory: Memory,
    /// Source events that fed this memory (resolved from
    /// `memory.source_event_ids`). Missing events (e.g. tombstoned) are
    /// silently skipped.
    pub source_events: Vec<Event>,
    /// Full entity rows for `memory.entities`.
    pub linked_entities: Vec<Entity>,
    /// Conflicts (resolved or not) that this memory participates in.
    pub conflicts: Vec<MemoryConflict>,
}
