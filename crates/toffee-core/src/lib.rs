//! Pure types and pure functions for toffee.
//!
//! No I/O, no async. Everything in this crate is `Send + Sync` and trivial to
//! construct in tests.

pub mod config;
pub mod conflict;
pub mod context;
pub mod entity;
pub mod event;
pub mod memory;
pub mod observability;
pub mod paths;
pub mod scalars;
pub mod scope;

pub use config::{Config, ScoringConfig};
pub use conflict::{ConflictId, ConflictResolution, MemoryConflict};
pub use context::{
    ContextPackage, ContextPackageId, Lens, ProvenanceEntry, ProvenanceReport, RetrievalSource,
};
pub use entity::{Entity, EntityId, EntityPage, EntityType};
pub use event::{Actor, Event, EventId, EventInput};
pub use memory::{
    ExtractionProvenance, FeedbackKind, Memory, MemoryCandidate, MemoryId, MemoryKind,
};
pub use observability::{Notification, WhyMemoryReport, WorkerFailure, WorkerStatus};
pub use scope::{expand_inherited, Scope};
