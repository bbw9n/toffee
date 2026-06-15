//! Read path: assemble a [`ContextPackage`] from the indexes.
//!
//! Phase 4 pipeline:
//!
//! 1. Resolve the scope set (with [`expand_inherited`]).
//! 2. Pull memories from three sources: vector ANN, entity anchoring, and
//!    a recency window. These hit the same SQLite mutex today, so we run
//!    them sequentially inside a single blocking task — switching to a
//!    real connection pool is the cheapest future speedup.
//! 3. Merge by `MemoryId`, attributing each entry to the sources that
//!    surfaced it. Compute a combined score; the lens applies a minimum
//!    confidence floor.
//! 4. Allocate the token budget per kind (decisions / claims / preferences /
//!    episodes per the default lens) and greedily fill in descending score.
//! 5. Surface unresolved conflicts that intersect the requested scope.
//! 6. Persist a [`ProvenanceReport`] in a bounded ring keyed by the
//!    package id so `toffee.inspect_provenance` can answer later.

use std::collections::{HashMap, HashSet};

use chrono::Utc;
use toffee_core::{
    context::ProvenanceEntry, expand_inherited, ContextPackage, ContextPackageId, Lens, Memory,
    MemoryId, MemoryKind, ProvenanceReport, RetrievalSource,
};
use toffee_store::{EntityListFilter, MemoryListFilter, Store};
use toffee_vector::{Embedder, VectorIndex};

use crate::{compose_text_for_embedding, Result};

const VECTOR_OVERSAMPLE: usize = 64;
const RECENT_WINDOW: usize = 32;
const ENTITY_PER_HIT_LIMIT: usize = 16;
const RECENCY_HALF_LIFE_DAYS: f64 = 14.0;

/// Inputs to a `read_context` call.
#[derive(Debug, Clone)]
pub struct ReadContextRequest {
    pub scope: Vec<String>,
    pub query: String,
    pub lens: Lens,
    pub token_budget: usize,
}

#[derive(Debug, Clone)]
struct WorkItem {
    memory: Memory,
    sources: HashSet<RetrievalSource>,
    vector_similarity: Option<f32>,
    entity_match: bool,
}

/// Build a context package + provenance report. Synchronous-blocking; the
/// caller wraps this in `spawn_blocking`.
pub fn read_context(
    store: &Store,
    embedder: &dyn Embedder,
    vector: &VectorIndex,
    req: ReadContextRequest,
) -> Result<(ContextPackage, ProvenanceReport)> {
    let expanded_scopes = expand_inherited(&req.scope);
    let lens = req.lens.clone();

    // Source 1: vector ANN.
    let query_vec = embedder.embed(&req.query)?;
    let vector_hits = vector.search(&query_vec, VECTOR_OVERSAMPLE, 64)?;

    // Source 2: entity anchoring — entities whose name matches a token in
    // the query, then their linked memories.
    let entity_hits = entity_anchored(store, &req.query, &expanded_scopes)?;

    // Source 3: recency — most-recently-updated memories in scope.
    let recent_hits = recent_in_scope(store, &expanded_scopes, RECENT_WINDOW)?;

    // Merge.
    let mut items: HashMap<MemoryId, WorkItem> = HashMap::new();
    let now = Utc::now();

    for h in vector_hits {
        if !any_scope_match(&h.scope, &expanded_scopes) {
            continue;
        }
        let Some(memory) = store.get_memory(&h.memory_id)? else {
            continue;
        };
        let entry = items.entry(memory.id.clone()).or_insert_with(|| WorkItem {
            memory: memory.clone(),
            sources: HashSet::new(),
            vector_similarity: None,
            entity_match: false,
        });
        entry.sources.insert(RetrievalSource::Vector);
        // Keep the best vector similarity we've seen for this memory.
        entry.vector_similarity = Some(match entry.vector_similarity {
            Some(prev) => prev.max(h.similarity),
            None => h.similarity,
        });
    }

    for memory in entity_hits {
        if !any_scope_match(&memory.scope, &expanded_scopes) {
            continue;
        }
        let entry = items.entry(memory.id.clone()).or_insert_with(|| WorkItem {
            memory: memory.clone(),
            sources: HashSet::new(),
            vector_similarity: None,
            entity_match: false,
        });
        entry.sources.insert(RetrievalSource::Entity);
        entry.entity_match = true;
    }

    for memory in recent_hits {
        if !any_scope_match(&memory.scope, &expanded_scopes) {
            continue;
        }
        let entry = items.entry(memory.id.clone()).or_insert_with(|| WorkItem {
            memory: memory.clone(),
            sources: HashSet::new(),
            vector_similarity: None,
            entity_match: false,
        });
        entry.sources.insert(RetrievalSource::Recent);
    }

    // Filter by confidence floor.
    let mut scored: Vec<(WorkItem, f64, f32)> = items
        .into_values()
        .filter(|w| w.memory.confidence >= lens.min_confidence)
        .map(|w| {
            let recency = recency_score(&w.memory, now);
            let score = combined_score(&w, recency);
            (w, score, recency)
        })
        .collect();

    // Sort by score descending.
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.0.memory.updated_at.cmp(&a.0.memory.updated_at))
    });

    // Bucket by kind, then apply per-kind token budget.
    let mut bucketed: HashMap<MemoryKind, Vec<(WorkItem, f64, f32)>> = HashMap::new();
    for triple in scored {
        bucketed
            .entry(triple.0.memory.kind)
            .or_default()
            .push(triple);
    }

    let (claims, claims_prov) = allocate_bucket(
        bucketed.remove(&MemoryKind::Claim).unwrap_or_default(),
        &lens,
        MemoryKind::Claim,
        req.token_budget,
    );
    let (decisions, decisions_prov) = allocate_bucket(
        bucketed.remove(&MemoryKind::Decision).unwrap_or_default(),
        &lens,
        MemoryKind::Decision,
        req.token_budget,
    );
    let (preferences, preferences_prov) = allocate_bucket(
        bucketed.remove(&MemoryKind::Preference).unwrap_or_default(),
        &lens,
        MemoryKind::Preference,
        req.token_budget,
    );
    let (episodes, episodes_prov) = allocate_bucket(
        bucketed.remove(&MemoryKind::Episode).unwrap_or_default(),
        &lens,
        MemoryKind::Episode,
        req.token_budget,
    );

    // Conflicts in scope.
    let conflicts = store
        .list_unresolved_conflicts()?
        .into_iter()
        .filter(|c| any_scope_match(&c.scope, &expanded_scopes))
        .collect::<Vec<_>>();

    let pkg_id = ContextPackageId::generate();
    let token_estimate = sum_tokens(&claims)
        + sum_tokens(&decisions)
        + sum_tokens(&preferences)
        + sum_tokens(&episodes);

    let package = ContextPackage {
        id: pkg_id.clone(),
        scope: expanded_scopes.clone(),
        lens: lens.name.clone(),
        query: req.query.clone(),
        claims,
        decisions,
        preferences,
        episodes,
        conflicts,
        token_estimate,
        created_at: now,
    };

    let mut entries: Vec<ProvenanceEntry> = Vec::new();
    entries.extend(claims_prov);
    entries.extend(decisions_prov);
    entries.extend(preferences_prov);
    entries.extend(episodes_prov);

    let report = ProvenanceReport {
        context_package_id: pkg_id,
        query: req.query,
        scope: expanded_scopes,
        lens: lens.name,
        entries,
        created_at: now,
    };

    Ok((package, report))
}

fn allocate_bucket(
    mut sorted_items: Vec<(WorkItem, f64, f32)>,
    lens: &Lens,
    kind: MemoryKind,
    total_budget: usize,
) -> (Vec<Memory>, Vec<ProvenanceEntry>) {
    let budget = lens.budget_for(kind, total_budget);
    let mut taken: Vec<Memory> = Vec::new();
    let mut prov: Vec<ProvenanceEntry> = Vec::new();
    let mut consumed = 0usize;

    for (w, score, recency) in sorted_items.drain(..) {
        if taken.len() >= lens.max_per_kind {
            break;
        }
        let tokens = ContextPackage::estimate_tokens_for(&w.memory.text);
        if consumed + tokens > budget && !taken.is_empty() {
            // Stop once we'd overflow, unless we've taken nothing yet (we
            // always emit at least one if any are available).
            break;
        }
        consumed += tokens;
        let memory = w.memory.clone();
        prov.push(ProvenanceEntry {
            memory_id: memory.id.clone(),
            final_score: score,
            vector_similarity: w.vector_similarity,
            entity_match: w.entity_match,
            recency_score: recency,
            confidence_at_selection: memory.confidence,
            sources: w.sources.iter().copied().collect(),
            source_event_ids: memory.source_event_ids.clone(),
            kept_in_kind: kind,
        });
        taken.push(memory);
    }
    (taken, prov)
}

fn entity_anchored(
    store: &Store,
    query: &str,
    scopes: &[String],
) -> Result<Vec<Memory>> {
    let query_lower = query.to_lowercase();
    // Pull a bounded set of entities and match their names (or aliases)
    // against query tokens. The plan's "longest-match" promise is already
    // covered by the entity resolver during write; on read we just take
    // hits.
    let entities = store.list_entities(&EntityListFilter::default())?;
    let mut hits: Vec<Memory> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for ent in entities {
        let mut names: Vec<String> = std::iter::once(ent.name.clone())
            .chain(ent.aliases.iter().cloned())
            .map(|s| s.to_lowercase())
            .collect();
        names.sort_by_key(|s| std::cmp::Reverse(s.len()));
        if !names.iter().any(|n| !n.is_empty() && query_lower.contains(n)) {
            continue;
        }
        let memories = store.memories_for_entity(&ent.id, Some(scopes), ENTITY_PER_HIT_LIMIT)?;
        for m in memories {
            if seen.insert(m.id.0.clone()) {
                hits.push(m);
            }
        }
    }
    Ok(hits)
}

fn recent_in_scope(store: &Store, scopes: &[String], limit: usize) -> Result<Vec<Memory>> {
    let filter = MemoryListFilter {
        scope_any_of: Some(scopes.to_vec()),
        kind: None,
        include_deleted: false,
        include_superseded: false,
        limit: Some(limit),
    };
    Ok(store.list_memories(&filter)?)
}

fn any_scope_match(memory_scopes: &toffee_core::Scope, allowed: &[String]) -> bool {
    let mem = memory_scopes.as_slice();
    allowed.iter().any(|s| mem.iter().any(|m| m == s))
}

fn recency_score(memory: &Memory, now: chrono::DateTime<chrono::Utc>) -> f32 {
    let age_days = (now - memory.updated_at).num_seconds() as f64 / 86_400.0;
    // Exponential decay: 1.0 at age 0, 0.5 at half-life, asymptote 0.
    let val = (-age_days.max(0.0) / RECENCY_HALF_LIFE_DAYS).exp();
    val as f32
}

fn combined_score(w: &WorkItem, recency: f32) -> f64 {
    let v = w.vector_similarity.unwrap_or(0.0) as f64;
    let conf_bias = w.memory.confidence;
    let entity_anchor = if w.entity_match { 0.5 } else { 0.0 };
    // Weights: vector dominates when present, entity anchoring is a
    // meaningful second signal, recency / confidence are tiebreakers.
    0.6 * v + 0.25 * entity_anchor + 0.10 * (recency as f64) + 0.05 * conf_bias
}

fn sum_tokens(memories: &[Memory]) -> usize {
    memories
        .iter()
        .map(|m| ContextPackage::estimate_tokens_for(&m.text))
        .sum()
}

/// Bounded provenance cache. Newest-first ring buffer; cap is small because
/// `inspect_provenance` is a debug tool, not steady-state traffic.
pub struct ProvenanceCache {
    capacity: usize,
    entries: std::collections::VecDeque<(ContextPackageId, ProvenanceReport)>,
}

impl ProvenanceCache {
    pub fn new(capacity: usize) -> Self {
        ProvenanceCache {
            capacity,
            entries: std::collections::VecDeque::with_capacity(capacity),
        }
    }

    pub fn insert(&mut self, report: ProvenanceReport) {
        if self.entries.len() == self.capacity {
            self.entries.pop_back();
        }
        self.entries
            .push_front((report.context_package_id.clone(), report));
    }

    pub fn get(&self, id: &ContextPackageId) -> Option<ProvenanceReport> {
        self.entries
            .iter()
            .find(|(k, _)| k == id)
            .map(|(_, r)| r.clone())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// Quiet warnings for `compose_text_for_embedding` import — used elsewhere.
#[allow(dead_code)]
fn _ref(_: &dyn Fn(&Memory) -> String) {}
#[allow(dead_code)]
fn _force_use() {
    let _ = compose_text_for_embedding;
}
