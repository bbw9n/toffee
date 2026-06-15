//! Pure scoring primitives and the aggregate report structs.
//!
//! Nothing here touches the runtime — it's just arithmetic over predicted vs
//! expected, kept separate so the metrics are unit-testable in isolation.

use std::collections::HashSet;

/// F-beta. `beta < 1` weights precision over recall; we default the extraction
/// gate to beta = 0.5 because a wrong memory poisons retrieval while a missed
/// one is recoverable via `toffee memory add` (see the extractor's header).
pub fn f_beta(precision: f64, recall: f64, beta: f64) -> f64 {
    let b2 = beta * beta;
    let denom = b2 * precision + recall;
    if denom == 0.0 {
        0.0
    } else {
        (1.0 + b2) * precision * recall / denom
    }
}

/// Reciprocal rank of the first relevant id in `ranked` (0 if none present).
pub fn reciprocal_rank(ranked: &[String], relevant: &HashSet<String>) -> f64 {
    ranked
        .iter()
        .position(|id| relevant.contains(id))
        .map(|p| 1.0 / (p as f64 + 1.0))
        .unwrap_or(0.0)
}

/// Fraction of the relevant set that appears in the top `k` of `ranked`.
pub fn recall_at_k(ranked: &[String], relevant: &HashSet<String>, k: usize) -> f64 {
    if relevant.is_empty() {
        return 1.0;
    }
    let hits = ranked
        .iter()
        .take(k)
        .filter(|id| relevant.contains(*id))
        .count();
    hits as f64 / relevant.len() as f64
}

/// Binary-relevance nDCG@k. Ideal DCG assumes all relevant docs ranked first.
pub fn ndcg_at_k(ranked: &[String], relevant: &HashSet<String>, k: usize) -> f64 {
    if relevant.is_empty() {
        return 1.0;
    }
    let dcg: f64 = ranked
        .iter()
        .take(k)
        .enumerate()
        .filter(|(_, id)| relevant.contains(*id))
        .map(|(i, _)| 1.0 / ((i as f64 + 2.0).log2()))
        .sum();
    let ideal: f64 = (0..relevant.len().min(k))
        .map(|i| 1.0 / ((i as f64 + 2.0).log2()))
        .sum();
    if ideal == 0.0 {
        0.0
    } else {
        dcg / ideal
    }
}

/// Running counter for a hit/total ratio. Keeps the report code declarative.
#[derive(Debug, Default, Clone, Copy)]
pub struct Ratio {
    pub hits: usize,
    pub total: usize,
}

impl Ratio {
    pub fn observe(&mut self, correct: bool) {
        self.total += 1;
        if correct {
            self.hits += 1;
        }
    }
    pub fn value(&self) -> f64 {
        if self.total == 0 {
            1.0
        } else {
            self.hits as f64 / self.total as f64
        }
    }
}

/// Aggregate extraction metrics, accumulated case by case.
#[derive(Debug, Default, Clone)]
pub struct ExtractionScore {
    pub cases: usize,
    pub negative_cases: usize,
    pub true_positives: usize,
    pub false_positives: usize,
    pub false_negatives: usize,
    /// Field accuracy over matched (predicted, expected) pairs. Each ratio
    /// only counts pairs where the expected side specified that field.
    pub kind: Ratio,
    pub subject: Ratio,
    pub predicate: Ratio,
    pub object: Ratio,
}

impl ExtractionScore {
    pub fn precision(&self) -> f64 {
        let denom = self.true_positives + self.false_positives;
        if denom == 0 {
            1.0
        } else {
            self.true_positives as f64 / denom as f64
        }
    }
    pub fn recall(&self) -> f64 {
        let denom = self.true_positives + self.false_negatives;
        if denom == 0 {
            1.0
        } else {
            self.true_positives as f64 / denom as f64
        }
    }
    pub fn f_beta(&self, beta: f64) -> f64 {
        f_beta(self.precision(), self.recall(), beta)
    }
}

/// Aggregate retrieval metrics. `read_context` gives the prompt-level numbers
/// (did the relevant memory survive into the package, in the right bucket);
/// `search_memory` gives the ranked IR numbers.
#[derive(Debug, Clone)]
pub struct RetrievalScore {
    pub cases: usize,
    pub k: usize,
    /// Avg over cases of (relevant ids present in the package / total relevant).
    pub package_recall: f64,
    /// Did each surfaced relevant memory land in the bucket its kind implies.
    pub bucket: Ratio,
    pub mrr: f64,
    pub recall_at_k: f64,
    pub ndcg_at_k: f64,
    // running sums, divided out into the fields above when finalized
    package_recall_sum: f64,
    mrr_sum: f64,
    recall_sum: f64,
    ndcg_sum: f64,
}

impl RetrievalScore {
    pub fn new(k: usize) -> Self {
        RetrievalScore {
            cases: 0,
            k,
            package_recall: 0.0,
            bucket: Ratio::default(),
            mrr: 0.0,
            recall_at_k: 0.0,
            ndcg_at_k: 0.0,
            package_recall_sum: 0.0,
            mrr_sum: 0.0,
            recall_sum: 0.0,
            ndcg_sum: 0.0,
        }
    }

    pub fn observe_case(&mut self, package_recall: f64, mrr: f64, recall_k: f64, ndcg_k: f64) {
        self.cases += 1;
        self.package_recall_sum += package_recall;
        self.mrr_sum += mrr;
        self.recall_sum += recall_k;
        self.ndcg_sum += ndcg_k;
    }

    pub fn finalize(&mut self) {
        let n = self.cases.max(1) as f64;
        self.package_recall = self.package_recall_sum / n;
        self.mrr = self.mrr_sum / n;
        self.recall_at_k = self.recall_sum / n;
        self.ndcg_at_k = self.ndcg_sum / n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }
    fn ranked(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn f_beta_half_weights_precision() {
        // precision 1.0, recall 0.5 should beat precision 0.5, recall 1.0
        // under beta = 0.5.
        assert!(f_beta(1.0, 0.5, 0.5) > f_beta(0.5, 1.0, 0.5));
    }

    #[test]
    fn reciprocal_rank_finds_first_relevant() {
        assert_eq!(
            reciprocal_rank(&ranked(&["a", "b", "c"]), &set(&["b"])),
            0.5
        );
        assert_eq!(reciprocal_rank(&ranked(&["a"]), &set(&["x"])), 0.0);
    }

    #[test]
    fn recall_at_k_caps_at_k() {
        let r = ranked(&["a", "b", "c", "d"]);
        assert_eq!(recall_at_k(&r, &set(&["a", "d"]), 2), 0.5);
        assert_eq!(recall_at_k(&r, &set(&["a", "d"]), 4), 1.0);
    }

    #[test]
    fn ndcg_perfect_ranking_is_one() {
        let r = ranked(&["a", "b", "c"]);
        assert!((ndcg_at_k(&r, &set(&["a", "b"]), 10) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn empty_relevant_is_vacuously_perfect() {
        let r = ranked(&["a"]);
        assert_eq!(recall_at_k(&r, &set(&[]), 5), 1.0);
        assert_eq!(ndcg_at_k(&r, &set(&[]), 5), 1.0);
    }
}
