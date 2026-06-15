//! Introspection for a single input — the "why did it do that" view that
//! turns a failing corpus number into an actionable diff.
//!
//! `extract_report` dumps what the extractor produces for an arbitrary line;
//! `retrieve_report` replays a retrieval case and prints the per-memory score
//! breakdown (vector / entity / recency / confidence → final) straight from the
//! provenance the read path already records.

use std::fmt::Write as _;

use anyhow::{Context, Result};
use chrono::Utc;
use toffee_core::{Event, EventId, Lens, Scope, ScoringConfig};
use toffee_runtime::read_path::ReadContextRequest;

use crate::corpus::RetrievalCase;
use crate::retrieval;

/// Run the extractor over `text` as if it were a user message, and show every
/// candidate it produced (or that it stayed silent).
pub fn extract_report(text: &str) -> String {
    let event = Event {
        id: EventId::generate(),
        scope: Scope::new(["project:debug"]),
        actor: toffee_core::Actor::User,
        event_type: "user_message".into(),
        payload: serde_json::json!({ "text": text }),
        session_id: None,
        run_id: None,
        created_at: Utc::now(),
    };
    let candidates = toffee_runtime::extractor::extract(&event);
    let mut out = String::new();
    let _ = writeln!(out, "input: {text:?}");
    if candidates.is_empty() {
        out.push_str("  → no candidates (extractor stayed silent)\n");
        return out;
    }
    for (i, c) in candidates.iter().enumerate() {
        let _ = writeln!(out, "  [{i}] {} ({:?})", c.kind.as_str(), c.provenance);
        let _ = writeln!(
            out,
            "      spo: {} / {} / {}",
            c.subject.as_deref().unwrap_or("-"),
            c.predicate.as_deref().unwrap_or("-"),
            c.object.as_deref().unwrap_or("-"),
        );
        let _ = writeln!(out, "      text: {:?}", c.text);
    }
    out
}

/// Replay a retrieval case under `scoring` and print the scored ranking with
/// each signal's contribution, flagging which memories are gold-relevant.
pub fn retrieve_report(case: &RetrievalCase, scoring: &ScoringConfig) -> Result<String> {
    let seeded = retrieval::seed_case(case, scoring)?;
    let pkg = seeded.rt.read_context(ReadContextRequest {
        scope: case.scope.clone(),
        query: case.query.clone(),
        lens: Lens::default_lens(),
        token_budget: case.budget,
    })?;
    let report = seeded
        .rt
        .inspect_provenance(&pkg.id)
        .context("provenance should still be cached immediately after read_context")?;

    let mut entries = report.entries;
    entries.sort_by(|a, b| {
        b.final_score
            .partial_cmp(&a.final_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut out = String::new();
    let _ = writeln!(out, "case:  {}", case.name);
    let _ = writeln!(out, "query: {:?}", case.query);
    let _ = writeln!(out, "scope: {:?}", case.scope);
    let _ = writeln!(
        out,
        "weights: vec={:.3} ent={:.3} rec={:.3} conf={:.3} tau={:.0}d",
        scoring.vector_weight,
        scoring.entity_weight,
        scoring.recency_weight,
        scoring.confidence_weight,
        scoring.recency_half_life_days,
    );
    let _ = writeln!(
        out,
        "  {:<6} {:<8} {:>6} {:>6} {:>6} {:>6}  id",
        "rel", "final", "vector", "entity", "recncy", "conf"
    );
    for e in &entries {
        let corpus_id = seeded
            .real_to_corpus
            .get(&e.memory_id.0)
            .cloned()
            .unwrap_or_else(|| e.memory_id.0.clone());
        let rel = if seeded.relevant.contains(&corpus_id) {
            "★"
        } else {
            " "
        };
        let _ = writeln!(
            out,
            "  {:<6} {:<8.4} {:>6.3} {:>6} {:>6.3} {:>6.3}  {}",
            rel,
            e.final_score,
            e.vector_similarity.unwrap_or(0.0),
            if e.entity_match { "yes" } else { "no" },
            e.recency_score,
            e.confidence_at_selection,
            corpus_id,
        );
    }
    Ok(out)
}
