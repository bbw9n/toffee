//! Retrieval eval: seed a fresh in-process runtime with a known memory set,
//! then score what the two read surfaces return for each query.
//!
//! This isolates surface #2 (query → context). Memories are seeded verbatim
//! via `insert_memory_and_link_entities`, bypassing extraction, so a change in
//! extractor quality can't move these numbers. Uses the default hash embedder,
//! so the whole eval is offline and deterministic.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use toffee_core::{Lens, Memory, MemoryId, MemoryKind, Scope, ScoringConfig};
use toffee_runtime::{read_path::ReadContextRequest, Runtime, SearchParams};
use toffee_store::Store;

use crate::corpus::{RetrievalCase, SeedMemory};
use crate::metrics::RetrievalScore;

/// Top-k cutoff for the ranked `search_memory` metrics.
pub const K: usize = 10;

pub struct RetrievalReport {
    pub score: RetrievalScore,
    pub misses: Vec<RetrievalMiss>,
}

#[derive(Debug, Clone)]
pub struct RetrievalMiss {
    pub name: String,
    pub note: String,
}

/// A seeded runtime for one case, plus the maps needed to translate returned
/// memories back to corpus-local ids. Shared by scoring and `debug`.
pub struct SeededCase {
    pub rt: Runtime,
    pub text_to_id: HashMap<String, String>,
    pub id_to_kind: HashMap<String, MemoryKind>,
    /// Real (runtime-assigned) `MemoryId` string → corpus-local id. Needed to
    /// label provenance entries, which key by the real id.
    pub real_to_corpus: HashMap<String, String>,
    pub relevant: HashSet<String>,
}

/// Build a fresh in-memory runtime, seed it with the case's memories verbatim,
/// and apply `scoring` so the read path ranks under the config being evaluated.
pub fn seed_case(case: &RetrievalCase, scoring: &ScoringConfig) -> Result<SeededCase> {
    let store = Arc::new(Store::open_in_memory()?);
    let rt = Runtime::new(store);
    rt.set_scoring_config(*scoring);

    let mut text_to_id: HashMap<String, String> = HashMap::new();
    let mut id_to_kind: HashMap<String, MemoryKind> = HashMap::new();
    let mut real_to_corpus: HashMap<String, String> = HashMap::new();
    // Capture one timestamp and stagger memories by index. Distinct
    // `updated_at` values make the read path's score tie-break total and
    // deterministic (otherwise equal-score memories order by HashMap
    // iteration, and the eval jitters run to run); the 1s offsets are far
    // below the recency time-constant, so they don't distort ranking.
    let base = Utc::now();
    for (i, seed) in case.memories.iter().enumerate() {
        let at = base - chrono::Duration::seconds(i as i64);
        let mem = build_memory(case, seed, at)?;
        text_to_id.insert(mem.text.clone(), seed.id.clone());
        id_to_kind.insert(seed.id.clone(), mem.kind);
        real_to_corpus.insert(mem.id.0.clone(), seed.id.clone());
        rt.insert_memory_and_link_entities(mem)?;
    }
    Ok(SeededCase {
        rt,
        text_to_id,
        id_to_kind,
        real_to_corpus,
        relevant: case.relevant.iter().cloned().collect(),
    })
}

/// Score the corpus under the default scoring config.
pub fn run(cases: &[RetrievalCase]) -> Result<RetrievalReport> {
    run_with(cases, &ScoringConfig::default())
}

/// Score the corpus under an arbitrary scoring config. This is the objective
/// `tune` optimizes — same seeded runtimes, different ranking weights.
pub fn run_with(cases: &[RetrievalCase], scoring: &ScoringConfig) -> Result<RetrievalReport> {
    let mut score = RetrievalScore::new(K);
    let mut misses = Vec::new();

    for case in cases {
        score_case(case, scoring, &mut score, &mut misses)
            .with_context(|| format!("retrieval case {}", case.name))?;
    }
    score.finalize();
    Ok(RetrievalReport { score, misses })
}

fn score_case(
    case: &RetrievalCase,
    scoring: &ScoringConfig,
    score: &mut RetrievalScore,
    misses: &mut Vec<RetrievalMiss>,
) -> Result<()> {
    let SeededCase {
        rt,
        text_to_id,
        id_to_kind,
        relevant,
        ..
    } = seed_case(case, scoring)?;

    // --- Ranked surface: search_memory -> MRR / Recall@k / nDCG@k.
    let hits = rt.search_memory(SearchParams {
        query: case.query.clone(),
        scope_any_of: Some(case.scope.clone()),
        kind: None,
        limit: K,
        min_similarity: 0.0,
    })?;
    let ranked: Vec<String> = hits
        .iter()
        .filter_map(|h| text_to_id.get(&h.memory.text).cloned())
        .collect();
    let mrr = crate::metrics::reciprocal_rank(&ranked, &relevant);
    let recall_k = crate::metrics::recall_at_k(&ranked, &relevant, K);
    let ndcg_k = crate::metrics::ndcg_at_k(&ranked, &relevant, K);

    // --- Prompt surface: read_context -> did relevant survive into the
    //     package, and in the right bucket.
    let pkg = rt.read_context(ReadContextRequest {
        scope: case.scope.clone(),
        query: case.query.clone(),
        lens: Lens::default_lens(),
        token_budget: case.budget,
    })?;

    let mut present: HashSet<String> = HashSet::new();
    for (bucket_kind, memories) in [
        (MemoryKind::Decision, &pkg.decisions),
        (MemoryKind::Claim, &pkg.claims),
        (MemoryKind::Preference, &pkg.preferences),
        (MemoryKind::Episode, &pkg.episodes),
    ] {
        for m in memories {
            if let Some(id) = text_to_id.get(&m.text) {
                present.insert(id.clone());
                if relevant.contains(id) {
                    let want = id_to_kind.get(id).copied();
                    score.bucket.observe(want == Some(bucket_kind));
                }
            }
        }
    }

    let package_recall = if relevant.is_empty() {
        1.0
    } else {
        let hit = relevant.iter().filter(|id| present.contains(*id)).count();
        hit as f64 / relevant.len() as f64
    };

    for id in &relevant {
        if !present.contains(id) {
            misses.push(RetrievalMiss {
                name: case.name.clone(),
                note: format!("relevant memory '{id}' missing from package"),
            });
        }
    }

    score.observe_case(package_recall, mrr, recall_k, ndcg_k);
    Ok(())
}

fn build_memory(
    case: &RetrievalCase,
    seed: &SeedMemory,
    at: chrono::DateTime<Utc>,
) -> Result<Memory> {
    let kind = MemoryKind::parse(&seed.kind)
        .with_context(|| format!("unknown memory kind '{}'", seed.kind))?;
    let scope = seed.scope.clone().unwrap_or_else(|| case.scope.clone());
    // The store's CHECK requires a full SPO triple for every non-episode kind.
    // Retrieval scoring keys on text/embedding, not SPO, so synthesize a
    // placeholder when the corpus leaves it off (distractors usually do).
    let (subject, predicate, object) = if kind.requires_spo() {
        (
            seed.subject.clone().or_else(|| Some("note".into())),
            seed.predicate.clone().or_else(|| Some("states".into())),
            seed.object.clone().or_else(|| Some(seed.text.clone())),
        )
    } else {
        (
            seed.subject.clone(),
            seed.predicate.clone(),
            seed.object.clone(),
        )
    };
    Ok(Memory {
        id: MemoryId::generate(),
        kind,
        scope: Scope::new(scope),
        text: seed.text.clone(),
        subject,
        predicate,
        object,
        entities: vec![],
        confidence: seed.confidence,
        source_event_ids: vec![],
        created_at: at,
        updated_at: at,
        superseded_by: None,
    })
}
