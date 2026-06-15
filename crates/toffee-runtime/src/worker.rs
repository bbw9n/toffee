//! The background worker: replays the event log, extracts memory candidates,
//! and either promotes them, saves them as episodes, or records conflicts.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::broadcast;
use toffee_core::{
    scalars, ConflictId, ConflictResolution, Event, EventId, Memory, MemoryCandidate,
    MemoryConflict, MemoryId,
};
use toffee_store::DEFAULT_WORKER_ID;
use toffee_vector::{Embedder, IndexedPoint, VectorIndex};

use crate::{
    compose_text_for_embedding, conflict_detector, entity_resolver, extractor, promoter, Result,
    RuntimeError, RuntimeInner,
};

const BATCH_SIZE: usize = 64;
const IDLE_POLL: Duration = Duration::from_millis(500);

pub(crate) async fn run(
    runtime: Arc<RuntimeInner>,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<()> {
    loop {
        // Drain everything currently behind the checkpoint.
        loop {
            let processed = drain_once(&runtime).await?;
            if processed == 0 {
                break;
            }
        }

        // Wait for either a new-event notification or the idle poll timeout,
        // or shutdown. The idle poll guards against missed notifications
        // (e.g. an event inserted directly into the DB by a future tool).
        tokio::select! {
            _ = runtime.notify().notified() => {},
            _ = tokio::time::sleep(IDLE_POLL) => {},
            _ = shutdown.recv() => {
                tracing::info!("worker: shutdown");
                return Ok(());
            }
        }
    }
}

/// Process up to `BATCH_SIZE` events past the current checkpoint. Returns
/// the number of events processed (0 means no work waiting).
pub(crate) async fn drain_once(runtime: &Arc<RuntimeInner>) -> Result<usize> {
    let runtime_for_blocking = runtime.clone();
    tokio::task::spawn_blocking(move || drain_once_sync(&runtime_for_blocking))
        .await
        .map_err(|e| RuntimeError::Store(toffee_store::StoreError::Parse(e.to_string())))?
}

fn drain_once_sync(runtime: &Arc<RuntimeInner>) -> Result<usize> {
    let store = runtime.store();
    let embedder = runtime.embedder().clone();
    let vector = runtime.vector().clone();

    let checkpoint = store.worker_checkpoint(DEFAULT_WORKER_ID)?;
    let after = checkpoint
        .as_ref()
        .and_then(|c| {
            c.last_processed_at
                .zip(c.last_processed_event_id.clone())
        });
    let events = store.events_after(after.as_ref(), BATCH_SIZE)?;
    let count = events.len();

    for event in events {
        match process_event(runtime, store, embedder.as_ref(), vector.as_ref(), &event) {
            Ok(()) => {}
            Err(e) => {
                let msg = format!("{e}");
                tracing::warn!(event_id = %event.id, error = %msg, "worker: process_event failed");
                if let Err(persist) = store.record_worker_failure(
                    DEFAULT_WORKER_ID,
                    &event.id,
                    &msg,
                    chrono::Utc::now(),
                ) {
                    tracing::warn!(error = ?persist, "worker: also failed to persist failure row");
                }
            }
        }
        store.set_worker_checkpoint(DEFAULT_WORKER_ID, &event.id, event.created_at)?;
    }

    // After draining a batch, recompute lag and notify if it crossed.
    if count > 0 {
        runtime.check_lag_and_maybe_notify();
    }
    Ok(count)
}

fn process_event(
    runtime: &Arc<RuntimeInner>,
    store: &toffee_store::Store,
    embedder: &dyn Embedder,
    vector: &VectorIndex,
    event: &Event,
) -> Result<()> {
    let candidates = extractor::extract(event);
    for candidate in candidates {
        let conf = scalars::compute_confidence(&candidate);
        let decision = promoter::decide(&candidate, conf);
        match decision {
            promoter::Decision::Discard => {
                tracing::debug!(
                    event_id = %event.id,
                    kind = candidate.kind.as_str(),
                    text = %candidate.text,
                    confidence = conf,
                    "worker: discarded candidate"
                );
                continue;
            }
            promoter::Decision::Episode => {
                let memory = candidate_to_memory(&candidate, conf, true);
                store.insert_memory(&memory)?;
                link_entities(&candidate, store, &memory.id)?;
                embed_and_index(store, embedder, vector, &memory)?;
                tracing::debug!(memory_id = %memory.id, "worker: saved episode");
            }
            promoter::Decision::Promote => {
                if let Some(finding) = conflict_detector::find(&candidate, store)? {
                    record_conflict(store, embedder, vector, &candidate, &finding)?;
                    continue;
                }
                let memory = candidate_to_memory(&candidate, conf, false);
                store.insert_memory(&memory)?;
                link_entities(&candidate, store, &memory.id)?;
                embed_and_index(store, embedder, vector, &memory)?;
                tracing::debug!(memory_id = %memory.id, kind = memory.kind.as_str(), "worker: promoted memory");
                runtime.emit(toffee_core::Notification::MemoryPromoted {
                    memory_id: memory.id.clone(),
                    kind: memory.kind,
                    scope: memory.scope.as_slice().to_vec(),
                    confidence: memory.confidence,
                });
            }
        }
    }
    Ok(())
}

fn embed_and_index(
    store: &toffee_store::Store,
    embedder: &dyn Embedder,
    vector: &VectorIndex,
    memory: &Memory,
) -> Result<()> {
    let model = embedder.model().to_string();
    if store.embedding_exists(&memory.id, &model)? {
        return Ok(());
    }
    let text = compose_text_for_embedding(memory);
    let v = embedder.embed(&text)?;
    let seq = store.insert_embedding(&toffee_store::NewEmbedding {
        memory_id: memory.id.clone(),
        scope: memory.scope.clone(),
        kind: memory.kind,
        model,
        vector: v.clone(),
    })?;
    vector.insert_with_seq(
        seq as usize,
        &v,
        IndexedPoint {
            memory_id: memory.id.clone(),
            scope: memory.scope.clone(),
            kind: memory.kind,
        },
    )?;
    Ok(())
}

fn link_entities(
    candidate: &MemoryCandidate,
    store: &toffee_store::Store,
    memory_id: &MemoryId,
) -> Result<()> {
    let entity_ids = entity_resolver::resolve(candidate, store)?;
    for ent_id in entity_ids {
        store.link_memory_entity(memory_id, &ent_id)?;
    }
    Ok(())
}

fn candidate_to_memory(candidate: &MemoryCandidate, confidence: f64, force_episode: bool) -> Memory {
    let kind = if force_episode {
        toffee_core::MemoryKind::Episode
    } else {
        candidate.kind
    };
    let now = Utc::now();
    // Episodes are allowed to be SPO-less, so blank them out when we demote.
    let (subject, predicate, object) = if matches!(kind, toffee_core::MemoryKind::Episode)
        && !candidate.kind.requires_spo()
    {
        (None, None, None)
    } else if force_episode {
        // Demoted: drop SPO so we don't trip the schema CHECK by accident.
        (None, None, None)
    } else {
        (
            candidate.subject.clone(),
            candidate.predicate.clone(),
            candidate.object.clone(),
        )
    };
    Memory {
        id: MemoryId::generate(),
        kind,
        scope: candidate.scope.clone(),
        text: candidate.text.clone(),
        subject,
        predicate,
        object,
        entities: candidate.entities.clone(),
        confidence,
        source_event_ids: candidate.source_event_ids.clone(),
        created_at: now,
        updated_at: now,
        superseded_by: None,
    }
}

fn record_conflict(
    store: &toffee_store::Store,
    embedder: &dyn Embedder,
    vector: &VectorIndex,
    candidate: &MemoryCandidate,
    finding: &conflict_detector::ConflictFinding,
) -> Result<()> {
    // Persist the candidate itself so both sides have an id we can resolve.
    let conf = scalars::compute_confidence(candidate);
    let candidate_memory = candidate_to_memory(candidate, conf, false);
    store.insert_memory(&candidate_memory)?;
    link_entities(candidate, store, &candidate_memory.id)?;
    embed_and_index(store, embedder, vector, &candidate_memory)?;

    // Dedup: if an unresolved conflict already exists for this (subject,
    // predicate) in an intersecting scope, extend it rather than opening a
    // new one. Otherwise open a new conflict with both sides.
    let (Some(subject), Some(predicate)) = (
        candidate.subject.as_deref(),
        candidate.predicate.as_deref(),
    ) else {
        // Shouldn't happen — conflict_detector only fires when SPO is
        // present — but be defensive.
        return Ok(());
    };
    let scope_strings: Vec<String> = candidate.scope.as_slice().to_vec();
    if let Some(existing) =
        store.find_unresolved_conflict_for_spo(&scope_strings, subject, predicate)?
    {
        // Append every competing memory we just discovered, plus the
        // candidate. `extend_conflict` is idempotent so duplicate adds are
        // safe.
        for m in &finding.competing {
            store.extend_conflict(&existing.id, &m.id)?;
        }
        store.extend_conflict(&existing.id, &candidate_memory.id)?;
        tracing::info!(
            conflict_id = %existing.id,
            subject,
            predicate,
            new_competing = %candidate_memory.id,
            "worker: extended existing conflict"
        );
        return Ok(());
    }

    let mut competing_ids: Vec<MemoryId> =
        finding.competing.iter().map(|m| m.id.clone()).collect();
    competing_ids.push(candidate_memory.id.clone());

    let conflict = MemoryConflict {
        id: ConflictId::generate(),
        scope: candidate.scope.clone(),
        subject: candidate.subject.clone(),
        predicate: candidate.predicate.clone(),
        competing_memory_ids: competing_ids,
        resolution: ConflictResolution::Unresolved,
        created_at: Utc::now(),
        resolved_at: None,
    };
    store.insert_conflict(&conflict)?;
    tracing::info!(
        conflict_id = %conflict.id,
        subject = ?conflict.subject,
        predicate = ?conflict.predicate,
        "worker: recorded conflict"
    );
    Ok(())
}

// Silence unused-imports warning until later phases use them.
#[allow(dead_code)]
fn _ensure_event_id(_: EventId) {}
