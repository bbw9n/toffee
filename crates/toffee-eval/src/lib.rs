//! Offline, deterministic memory-quality eval for toffee.
//!
//! Two surfaces are scored independently, because they fail independently:
//!
//! - **Extraction** (`event → memory`): run the heuristic extractor over a
//!   labeled corpus, score precision/recall/F-beta and per-field accuracy.
//!   Precision-weighted (beta = 0.5) because a wrong memory poisons retrieval
//!   while a missed one is recoverable via `toffee memory add`.
//! - **Retrieval** (`query → context`): seed a fresh runtime with a known
//!   memory set, then score `search_memory` (MRR / Recall@k / nDCG@k) and
//!   `read_context` (did the relevant memory survive into the package, in the
//!   right bucket).
//!
//! Both run in-process against an in-memory store and the hash embedder, so
//! the whole thing is offline and reproducible — the same property the daemon
//! end-to-end tests get from `--embedder hash`.
//!
//! The point is the *baseline*: run it on today's extractor/read path, record
//! the numbers, and gate the heuristics → model-assisted swap on beating them.

pub mod corpus;
pub mod debug;
pub mod extraction;
pub mod metrics;
pub mod retrieval;
pub mod tune;

/// Minimum acceptable scores. The binary exits non-zero if any are missed, so
/// the eval doubles as a CI regression gate. Defaults are the floor a healthy
/// v1 should clear, not aspirational targets — raise them as the corpus grows.
#[derive(Debug, Clone)]
pub struct Thresholds {
    pub extraction_precision: f64,
    pub extraction_recall: f64,
    pub retrieval_mrr: f64,
    pub retrieval_package_recall: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            extraction_precision: 0.90,
            extraction_recall: 0.70,
            retrieval_mrr: 0.70,
            retrieval_package_recall: 0.90,
        }
    }
}

/// Which gates failed, if any. Empty means the run passed.
pub fn check_thresholds(
    extraction: &metrics::ExtractionScore,
    retrieval: &metrics::RetrievalScore,
    t: &Thresholds,
) -> Vec<String> {
    let mut failures = Vec::new();
    let mut gate = |name: &str, got: f64, min: f64| {
        if got + 1e-9 < min {
            failures.push(format!("{name}: {got:.3} < {min:.3}"));
        }
    };
    gate(
        "extraction precision",
        extraction.precision(),
        t.extraction_precision,
    );
    gate(
        "extraction recall",
        extraction.recall(),
        t.extraction_recall,
    );
    gate("retrieval MRR", retrieval.mrr, t.retrieval_mrr);
    gate(
        "retrieval package recall",
        retrieval.package_recall,
        t.retrieval_package_recall,
    );
    failures
}
