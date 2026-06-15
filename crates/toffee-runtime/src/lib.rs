//! Toffee runtime: write path, read path, and background worker glue.
//!
//! Phase 1 surface: a `Runtime` facade that owns the store and exposes the
//! write-path entry point plus the worker loop. The worker turns events into
//! memories asynchronously, off the request-handling path.
//!
//! Phase 3 adds the read path: an [`Embedder`] and [`VectorIndex`] live on
//! the runtime, the worker auto-embeds promoted memories, and [`search_memory`]
//! does HNSW over-fetch + scope/kind post-filter on top of inherited scopes.

pub mod conflict_detector;
pub mod entity_page;
pub mod entity_resolver;
pub mod extractor;
pub mod promoter;
pub mod read_path;
pub mod search;
pub mod worker;

use std::sync::Arc;

use chrono::Utc;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use toffee_core::{
    ConflictId, ConflictResolution, ContextPackage, ContextPackageId, Event, EventInput,
    FeedbackKind, Memory, MemoryConflict, MemoryId, MemoryKind, Notification, ProvenanceReport,
    Scope, ScoringConfig, WhyMemoryReport, WorkerStatus,
};
use toffee_store::{Store, StoreError};
use toffee_vector::{Embedder, HashEmbedder, VectorError, VectorIndex};
use tokio::sync::{broadcast, Notify};

use crate::read_path::{ProvenanceCache, ReadContextRequest};

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("vector error: {0}")]
    Vector(#[from] VectorError),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("already resolved: conflict {0}")]
    AlreadyResolved(String),
}

pub type Result<T> = std::result::Result<T, RuntimeError>;

/// One ranked search hit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryHit {
    pub memory: Memory,
    pub similarity: f32,
}

/// How a human resolved a [`MemoryConflict`].
#[derive(Debug, Clone)]
pub enum ResolutionAction {
    /// Pick one of the competing memories as the winner. The losers are
    /// superseded by the winner (their `superseded_by` points at it) and
    /// dropped from the vector index.
    Pick { winner: MemoryId },
    /// Author a new merged memory. Every competing memory is superseded by
    /// the new one. If `subject` / `predicate` / `object` are omitted, the
    /// conflict's SPO is reused.
    Merge {
        text: String,
        subject: Option<String>,
        predicate: Option<String>,
        object: Option<String>,
        confidence: Option<f64>,
    },
    /// Soft-delete every competing memory. Useful when the contradiction is
    /// noise on both sides.
    RejectAll,
}

/// Parameters for [`Runtime::search_memory`].
#[derive(Debug, Clone, Default)]
pub struct SearchParams {
    pub query: String,
    /// Caller's scope set. The runtime applies scope inheritance internally.
    pub scope_any_of: Option<Vec<String>>,
    pub kind: Option<MemoryKind>,
    pub limit: usize,
    /// Drop hits below this cosine similarity. Default 0.0.
    pub min_similarity: f32,
}

#[derive(Clone)]
pub struct Runtime {
    inner: Arc<Inner>,
}

pub(crate) struct Inner {
    store: Arc<Store>,
    vector: Arc<VectorIndex>,
    embedder: Arc<dyn Embedder>,
    notify: Notify,
    /// Read-path ranking weights. Swapped live by the daemon's config watcher;
    /// every `read_context` snapshots it under the lock.
    scoring: RwLock<ScoringConfig>,
    provenance_cache: Mutex<ProvenanceCache>,
    /// Broadcast channel for daemon → client notifications. Bounded; if
    /// no subscribers, sends silently drop.
    notifications: broadcast::Sender<Notification>,
    /// In-memory worker lag flag. Toggled by the worker after each batch;
    /// flips trigger a [`Notification::WorkerLagChanged`].
    was_lagging: parking_lot::Mutex<bool>,
}

const PROVENANCE_CACHE_CAPACITY: usize = 64;
const NOTIFICATIONS_CHANNEL_CAPACITY: usize = 256;
/// Lag threshold at which we say the worker is "behind".
pub const LAG_THRESHOLD_SECONDS: i64 = 30;

impl Runtime {
    /// Default constructor: hash embedder and a fresh in-memory HNSW.
    pub fn new(store: Arc<Store>) -> Self {
        let embedder: Arc<dyn Embedder> = Arc::new(HashEmbedder::default());
        let vector = Arc::new(VectorIndex::new(embedder.dim()));
        Self::with_components(store, embedder, vector)
    }

    pub fn with_components(
        store: Arc<Store>,
        embedder: Arc<dyn Embedder>,
        vector: Arc<VectorIndex>,
    ) -> Self {
        let (notifications, _) = broadcast::channel(NOTIFICATIONS_CHANNEL_CAPACITY);
        Runtime {
            inner: Arc::new(Inner {
                store,
                vector,
                embedder,
                notify: Notify::new(),
                scoring: RwLock::new(ScoringConfig::default()),
                provenance_cache: Mutex::new(ProvenanceCache::new(PROVENANCE_CACHE_CAPACITY)),
                notifications,
                was_lagging: parking_lot::Mutex::new(false),
            }),
        }
    }

    /// Subscribe to daemon → client notifications. Each call returns a
    /// fresh receiver; multiple subscribers each see the same messages.
    pub fn subscribe_notifications(&self) -> broadcast::Receiver<Notification> {
        self.inner.notifications.subscribe()
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.inner.store
    }

    pub fn vector_index(&self) -> &Arc<VectorIndex> {
        &self.inner.vector
    }

    pub fn embedder(&self) -> &Arc<dyn Embedder> {
        &self.inner.embedder
    }

    /// Append an event and nudge the worker.
    pub fn append_event(&self, input: EventInput) -> Result<Event> {
        let event = self.inner.store.append_event(input)?;
        self.inner.notify.notify_one();
        Ok(event)
    }

    /// Apply user feedback to an existing memory.
    pub fn record_feedback(&self, id: &MemoryId, kind: FeedbackKind) -> Result<f64> {
        let memory = self
            .inner
            .store
            .get_memory(id)?
            .ok_or_else(|| RuntimeError::NotFound(format!("memory {id}")))?;
        let new_conf = toffee_core::scalars::apply_feedback(memory.confidence, kind);
        self.inner
            .store
            .update_memory_confidence(id, new_conf, Utc::now())?;
        Ok(new_conf)
    }

    /// Soft-delete a memory. Drops its vector from the index and embedding
    /// rows from the store.
    pub fn forget_memory(&self, id: &MemoryId) -> Result<()> {
        if self.inner.store.get_memory(id)?.is_none() {
            return Err(RuntimeError::NotFound(format!("memory {id}")));
        }
        self.inner.store.soft_delete_memory(id, Utc::now())?;
        self.inner.store.delete_embeddings_for_memory(id)?;
        self.inner.vector.forget(id);
        Ok(())
    }

    /// Assemble an entity page for the given identifier (entity id or name).
    pub fn get_entity_page(
        &self,
        identifier: &str,
        requested_scopes: Option<Vec<String>>,
    ) -> Result<toffee_core::EntityPage> {
        entity_page::build(self.inner.store.as_ref(), identifier, requested_scopes)
    }

    /// Insert a memory verbatim (CLI / manual add path) and link its
    /// entities + index its embedding.
    pub fn insert_memory_and_link_entities(
        &self,
        memory: toffee_core::Memory,
    ) -> Result<toffee_core::Memory> {
        let store = self.inner.store.as_ref();
        store.insert_memory(&memory)?;
        // Build a faux candidate just to feed the resolver.
        let cand = toffee_core::MemoryCandidate {
            kind: memory.kind,
            scope: memory.scope.clone(),
            text: memory.text.clone(),
            subject: memory.subject.clone(),
            predicate: memory.predicate.clone(),
            object: memory.object.clone(),
            entities: vec![],
            confidence: Some(memory.confidence),
            source_event_ids: memory.source_event_ids.clone(),
            provenance: toffee_core::ExtractionProvenance::Manual,
        };
        let entity_ids = entity_resolver::resolve(&cand, store)?;
        for ent_id in entity_ids {
            store.link_memory_entity(&memory.id, &ent_id)?;
        }
        self.embed_and_index(&memory)?;
        Ok(store
            .get_memory(&memory.id)?
            .expect("just-inserted memory should be retrievable"))
    }

    /// Embed a memory and add it to both the durable embeddings table and
    /// the in-memory HNSW index. Idempotent per (memory_id, model).
    pub fn embed_and_index(&self, memory: &toffee_core::Memory) -> Result<()> {
        let store = self.inner.store.as_ref();
        let model = self.inner.embedder.model().to_string();
        if store.embedding_exists(&memory.id, &model)? {
            return Ok(());
        }
        let text = compose_text_for_embedding(memory);
        let vector = self.inner.embedder.embed(&text)?;
        let seq = store.insert_embedding(&toffee_store::NewEmbedding {
            memory_id: memory.id.clone(),
            scope: memory.scope.clone(),
            kind: memory.kind,
            model,
            vector: vector.clone(),
        })?;
        self.inner.vector.insert_with_seq(
            seq as usize,
            &vector,
            toffee_vector::IndexedPoint {
                memory_id: memory.id.clone(),
                scope: memory.scope.clone(),
                kind: memory.kind,
            },
        )?;
        Ok(())
    }

    /// Rehydrate the in-memory HNSW index from the persisted embeddings
    /// table. Called on daemon startup; also exposed via
    /// `toffee.daemon.rebuild_indexes`.
    pub fn load_index_from_store(&self) -> Result<usize> {
        let store = self.inner.store.as_ref();
        let model = self.inner.embedder.model().to_string();
        let rows = store.list_embeddings_for_model(&model)?;
        self.inner.vector.clear();
        let mut n = 0usize;
        for row in rows {
            self.inner.vector.insert_with_seq(
                row.seq_id as usize,
                &row.vector,
                toffee_vector::IndexedPoint {
                    memory_id: row.memory_id,
                    scope: row.scope,
                    kind: row.kind,
                },
            )?;
            n += 1;
        }
        Ok(n)
    }

    /// Drop the embeddings table and re-embed every active memory under the
    /// current model. Used by `toffee.daemon.rebuild_indexes`.
    pub fn rebuild_indexes(&self) -> Result<usize> {
        let store = self.inner.store.as_ref();
        store.clear_embeddings()?;
        self.inner.vector.clear();
        let memories = store.list_memories(&toffee_store::MemoryListFilter {
            limit: None,
            ..Default::default()
        })?;
        let mut n = 0usize;
        for m in memories {
            self.embed_and_index(&m)?;
            n += 1;
        }
        Ok(n)
    }

    /// Vector search over indexed memories. Applies scope inheritance and
    /// post-filter. Over-fetches by a factor of 5 so a tight scope/kind
    /// filter still has plenty of candidates.
    pub fn search_memory(&self, params: SearchParams) -> Result<Vec<MemoryHit>> {
        if params.limit == 0 {
            return Ok(vec![]);
        }
        let query_vec = self.inner.embedder.embed(&params.query)?;
        let oversample = (params.limit * 5).max(20);
        let raw = self.inner.vector.search(&query_vec, oversample, 64)?;

        let expanded = params
            .scope_any_of
            .as_ref()
            .map(|s| toffee_core::expand_inherited(s));
        let kept = toffee_vector::post_filter::apply(
            raw,
            &toffee_vector::post_filter::FilterParams {
                scope_any_of: expanded,
                kind: params.kind,
                min_similarity: params.min_similarity,
                limit: params.limit,
            },
        );

        let store = self.inner.store.as_ref();
        let mut out: Vec<MemoryHit> = Vec::with_capacity(kept.len());
        for hit in kept {
            if let Some(memory) = store.get_memory(&hit.memory_id)? {
                out.push(MemoryHit {
                    memory,
                    similarity: hit.similarity,
                });
            }
        }
        Ok(out)
    }

    /// Assemble a memory-augmented context package for an agent's prompt.
    /// This is the integrator entry point — `cargo add toffee-client` and
    /// call this before LLM completion.
    pub fn read_context(&self, req: ReadContextRequest) -> Result<ContextPackage> {
        let scoring = *self.inner.scoring.read();
        let (package, report) = read_path::read_context(
            self.inner.store.as_ref(),
            self.inner.embedder.as_ref(),
            self.inner.vector.as_ref(),
            &scoring,
            req,
        )?;
        self.inner.provenance_cache.lock().insert(report);
        Ok(package)
    }

    /// Current read-path ranking weights (a cheap copy of the live config).
    pub fn scoring_config(&self) -> ScoringConfig {
        *self.inner.scoring.read()
    }

    /// Swap the read-path ranking weights live. The next `read_context` picks
    /// them up; in-flight calls finish under the weights they snapshotted.
    pub fn set_scoring_config(&self, scoring: ScoringConfig) {
        *self.inner.scoring.write() = scoring;
    }

    /// Look up the per-memory breakdown for an earlier context package.
    /// Returns `None` if the package has aged out of the in-memory cache.
    pub fn inspect_provenance(&self, id: &ContextPackageId) -> Option<ProvenanceReport> {
        self.inner.provenance_cache.lock().get(id)
    }

    /// Build a worker status snapshot — checkpoint, queue depth, lag,
    /// totals, recent failures. Used by `toffee.worker_status` and
    /// `toffee worker status`.
    pub fn worker_status(&self) -> Result<WorkerStatus> {
        let store = self.inner.store.as_ref();
        let checkpoint = store.worker_checkpoint(toffee_store::DEFAULT_WORKER_ID)?;
        let after = checkpoint
            .as_ref()
            .and_then(|c| c.last_processed_at.zip(c.last_processed_event_id.clone()));
        let queue_depth = store.count_events_after(after.as_ref())?;
        let events_total = store.count_events_after(None)?;
        let memories_active = store.memory_count_active()?;
        let conflicts_unresolved = store.list_unresolved_conflicts()?.len() as i64;
        let failures_recent = store.list_worker_failures(20)?;

        // Lag = now - last processed timestamp, only if anything has been
        // processed.
        let lag_seconds = checkpoint
            .as_ref()
            .and_then(|c| c.last_processed_at)
            .map(|t| (Utc::now() - t).num_seconds().max(0));

        Ok(WorkerStatus {
            worker_id: toffee_store::DEFAULT_WORKER_ID.to_string(),
            last_processed_event_id: checkpoint
                .as_ref()
                .and_then(|c| c.last_processed_event_id.clone()),
            last_processed_at: checkpoint.as_ref().and_then(|c| c.last_processed_at),
            queue_depth,
            lag_seconds,
            events_total,
            memories_active,
            conflicts_unresolved,
            failures_recent,
        })
    }

    /// Assemble a "why does this memory exist" report — source events,
    /// linked entities, conflicts mentioning it.
    pub fn why_memory(&self, id: &MemoryId) -> Result<WhyMemoryReport> {
        let store = self.inner.store.as_ref();
        let memory = store
            .get_memory(id)?
            .ok_or_else(|| RuntimeError::NotFound(format!("memory {id}")))?;
        let mut source_events: Vec<Event> = Vec::new();
        for evt_id in &memory.source_event_ids {
            if let Some(ev) = store.get_event(evt_id)? {
                source_events.push(ev);
            }
        }
        let linked_entities = store.entities_for_memory(&memory.id)?;
        let conflicts = store
            .list_conflicts(true)?
            .into_iter()
            .filter(|c| c.competing_memory_ids.contains(&memory.id))
            .collect();
        Ok(WhyMemoryReport {
            memory,
            source_events,
            linked_entities,
            conflicts,
        })
    }

    pub fn list_conflicts(&self, include_resolved: bool) -> Result<Vec<MemoryConflict>> {
        Ok(self.inner.store.list_conflicts(include_resolved)?)
    }

    pub fn get_conflict(&self, id: &ConflictId) -> Result<Option<MemoryConflict>> {
        Ok(self.inner.store.get_conflict(id)?)
    }

    /// Resolve an unresolved conflict. See [`ResolutionAction`] for the
    /// three modes. Picks supersede the losers; merge creates a new memory
    /// the conflict points to; reject-all soft-deletes every candidate.
    pub fn resolve_conflict(
        &self,
        id: &ConflictId,
        action: ResolutionAction,
    ) -> Result<MemoryConflict> {
        let store = self.inner.store.as_ref();
        let conflict = store
            .get_conflict(id)?
            .ok_or_else(|| RuntimeError::NotFound(format!("conflict {id}")))?;
        if conflict.resolution != ConflictResolution::Unresolved {
            return Err(RuntimeError::AlreadyResolved(id.0.clone()));
        }
        let now = Utc::now();
        match action {
            ResolutionAction::Pick { winner } => {
                if !conflict.competing_memory_ids.contains(&winner) {
                    return Err(RuntimeError::InvalidInput(format!(
                        "winner {winner} is not among the competing memories"
                    )));
                }
                for loser in &conflict.competing_memory_ids {
                    if loser == &winner {
                        continue;
                    }
                    store.supersede_memory(loser, &winner, now)?;
                    store.delete_embeddings_for_memory(loser)?;
                    self.inner.vector.forget(loser);
                }
                store.resolve_conflict(id, ConflictResolution::Picked, now)?;
            }
            ResolutionAction::Merge {
                text,
                subject,
                predicate,
                object,
                confidence,
            } => {
                // The merged memory inherits scope from the conflict and
                // SPO from the conflict's subject/predicate, with the
                // caller's object. If the caller doesn't supply SPO, we
                // fall back to the conflict's SPO (subject/predicate) and
                // the merged text becomes the object.
                let subject = subject.or_else(|| conflict.subject.clone());
                let predicate = predicate.or_else(|| conflict.predicate.clone());
                let object = object.or_else(|| Some(text.clone()));
                let merged = Memory {
                    id: MemoryId::generate(),
                    kind: MemoryKind::Claim,
                    scope: conflict.scope.clone(),
                    text,
                    subject,
                    predicate,
                    object,
                    entities: vec![],
                    confidence: confidence.unwrap_or(0.95),
                    source_event_ids: collect_source_events(store, &conflict.competing_memory_ids)?,
                    created_at: now,
                    updated_at: now,
                    superseded_by: None,
                };
                self.insert_memory_and_link_entities(merged.clone())?;
                for loser in &conflict.competing_memory_ids {
                    store.supersede_memory(loser, &merged.id, now)?;
                    store.delete_embeddings_for_memory(loser)?;
                    self.inner.vector.forget(loser);
                }
                store.resolve_conflict(id, ConflictResolution::Merged, now)?;
            }
            ResolutionAction::RejectAll => {
                for loser in &conflict.competing_memory_ids {
                    store.soft_delete_memory(loser, now)?;
                    store.delete_embeddings_for_memory(loser)?;
                    self.inner.vector.forget(loser);
                }
                store.resolve_conflict(id, ConflictResolution::RejectedAll, now)?;
            }
        }
        // Re-fetch so the caller sees the post-resolution row.
        store
            .get_conflict(id)?
            .ok_or_else(|| RuntimeError::NotFound(format!("conflict {id}")))
    }

    /// Run the worker loop until `shutdown` fires.
    pub async fn run_worker(self, shutdown: broadcast::Receiver<()>) -> Result<()> {
        worker::run(self.inner.clone(), shutdown).await
    }

    /// Test helper: drain everything currently pending without waiting on
    /// the notify signal. Returns the number of events processed.
    pub async fn drain_for_test(&self) -> Result<usize> {
        worker::drain_once(&self.inner).await
    }
}

impl Inner {
    pub(crate) fn store(&self) -> &Arc<Store> {
        &self.store
    }
    pub(crate) fn notify(&self) -> &Notify {
        &self.notify
    }
    pub(crate) fn embedder(&self) -> &Arc<dyn Embedder> {
        &self.embedder
    }
    pub(crate) fn vector(&self) -> &Arc<VectorIndex> {
        &self.vector
    }

    /// Fire a notification. Sends silently drop if no subscribers.
    pub(crate) fn emit(&self, n: Notification) {
        let _ = self.notifications.send(n);
    }

    /// After draining a batch, recompute lag and emit
    /// `WorkerLagChanged` if the lagging state flipped.
    pub(crate) fn check_lag_and_maybe_notify(&self) {
        let store = self.store.as_ref();
        let checkpoint = match store.worker_checkpoint(toffee_store::DEFAULT_WORKER_ID) {
            Ok(c) => c,
            Err(_) => return,
        };
        let after = checkpoint
            .as_ref()
            .and_then(|c| c.last_processed_at.zip(c.last_processed_event_id.clone()));
        let queue_depth = store.count_events_after(after.as_ref()).unwrap_or(0);
        let lag_seconds = checkpoint
            .as_ref()
            .and_then(|c| c.last_processed_at)
            .map(|t| (Utc::now() - t).num_seconds().max(0))
            .unwrap_or(0);

        let now_lagging = queue_depth > 0 && lag_seconds >= LAG_THRESHOLD_SECONDS;
        let mut was = self.was_lagging.lock();
        if *was != now_lagging {
            *was = now_lagging;
            drop(was);
            self.emit(Notification::WorkerLagChanged {
                lagging: now_lagging,
                lag_seconds,
                queue_depth,
            });
        }
    }
}

// Re-export Inner for the worker module (same crate).
pub(crate) use Inner as RuntimeInner;

/// Union of source event ids across a set of memories. Used during merge
/// resolution so the merged memory remembers everyone's provenance.
fn collect_source_events(
    store: &toffee_store::Store,
    ids: &[MemoryId],
) -> Result<Vec<toffee_core::EventId>> {
    use std::collections::HashSet;
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<toffee_core::EventId> = Vec::new();
    for id in ids {
        if let Some(m) = store.get_memory(id)? {
            for e in m.source_event_ids {
                if seen.insert(e.0.clone()) {
                    out.push(e);
                }
            }
        }
    }
    Ok(out)
}

/// Build the text the embedder sees for a memory. Combining SPO and text
/// gives both lexical (subject/object names) and narrative recall.
pub fn compose_text_for_embedding(memory: &toffee_core::Memory) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(4);
    parts.push(memory.text.clone());
    if let (Some(s), Some(p), Some(o)) = (&memory.subject, &memory.predicate, &memory.object) {
        parts.push(format!("{s} {p} {o}"));
    }
    parts.join(" \n ")
}

// Quiet warnings on unused re-exports until they get wired in callers.
#[allow(dead_code)]
fn _quiet(_: Scope) {}
