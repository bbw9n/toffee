//! The eval is itself under test: the shipped corpus must load, run, and clear
//! the default threshold gate. If a change to the extractor or read path
//! regresses memory quality below the floor, this test goes red — which is the
//! whole point of having the harness in the tree.

use std::path::Path;

use toffee_core::ScoringConfig;
use toffee_eval::corpus::{self, ExtractionCase, RetrievalCase};
use toffee_eval::{check_thresholds, extraction, retrieval, tune, Thresholds};

fn corpus_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("corpus")
        .join(name)
}

#[test]
fn shipped_corpus_loads_and_passes_gate() {
    let extraction_cases: Vec<ExtractionCase> =
        corpus::load_jsonl(&corpus_path("extraction.jsonl")).expect("load extraction corpus");
    let retrieval_cases: Vec<RetrievalCase> =
        corpus::load_jsonl(&corpus_path("retrieval.jsonl")).expect("load retrieval corpus");

    assert!(extraction_cases.len() >= 10, "corpus should be non-trivial");
    assert!(retrieval_cases.len() >= 5, "corpus should be non-trivial");

    let ext = extraction::run(&extraction_cases);
    let ret = retrieval::run(&retrieval_cases).expect("retrieval run");

    let failures = check_thresholds(&ext.score, &ret.score, &Thresholds::default());
    assert!(
        failures.is_empty(),
        "baseline corpus must clear the gate; failures: {failures:?}\n\
         extraction misses: {:?}\nretrieval misses: {:?}",
        ext.misses,
        ret.misses,
    );
}

/// The eval must be reproducible — otherwise `tune` chases tie-break noise.
/// Two independent runs over the same corpus must agree to the bit.
#[test]
fn retrieval_scoring_is_deterministic() {
    let cases: Vec<RetrievalCase> =
        corpus::load_jsonl(&corpus_path("retrieval.jsonl")).expect("load retrieval corpus");
    let a = retrieval::run(&cases).expect("run a").score;
    let b = retrieval::run(&cases).expect("run b").score;
    assert_eq!(a.mrr, b.mrr, "MRR not deterministic");
    assert_eq!(a.ndcg_at_k, b.ndcg_at_k, "nDCG not deterministic");
    assert_eq!(
        a.package_recall, b.package_recall,
        "package recall not deterministic"
    );
}

/// `tune` should never make the objective worse than the starting point, and
/// must run end to end over the shipped corpus.
#[test]
fn tune_never_regresses_objective() {
    let cases: Vec<RetrievalCase> =
        corpus::load_jsonl(&corpus_path("retrieval.jsonl")).expect("load retrieval corpus");
    let result = tune::tune(&cases, ScoringConfig::default(), 2).expect("tune");
    assert!(
        result.best_fitness + 1e-9 >= result.start_fitness,
        "tune regressed: {} < {}",
        result.best_fitness,
        result.start_fitness,
    );
}
