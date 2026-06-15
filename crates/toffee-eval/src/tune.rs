//! Fit the read-path scoring weights to the retrieval corpus.
//!
//! Coordinate descent over the four fusion weights + the recency time-constant,
//! using the real read path as the objective (via [`crate::retrieval::run_with`])
//! so the tuned config means exactly what the daemon will execute. The four
//! weights are ranking-invariant under a common scale, so the result is
//! normalized to sum to 1 for readability before it's reported or written.

use anyhow::Result;
use toffee_core::ScoringConfig;

use crate::corpus::RetrievalCase;
use crate::metrics::RetrievalScore;
use crate::retrieval;

/// Objective: reward both first-hit rank (MRR) and overall ordering (nDCG).
pub fn fitness(score: &RetrievalScore) -> f64 {
    0.5 * score.mrr + 0.5 * score.ndcg_at_k
}

pub struct TuneResult {
    pub start: ScoringConfig,
    pub best: ScoringConfig,
    pub start_fitness: f64,
    pub best_fitness: f64,
    pub start_score: RetrievalScore,
    pub best_score: RetrievalScore,
}

impl TuneResult {
    pub fn improved(&self) -> bool {
        self.best_fitness > self.start_fitness + 1e-9
    }
}

const WEIGHT_STEPS: usize = 20; // 0.00, 0.05, … 1.00
const HALF_LIFE_GRID: &[f64] = &[1.0, 3.0, 7.0, 14.0, 30.0, 60.0, 90.0, 180.0];

pub fn tune(cases: &[RetrievalCase], start: ScoringConfig, passes: usize) -> Result<TuneResult> {
    let eval =
        |c: &ScoringConfig| -> Result<f64> { Ok(fitness(&retrieval::run_with(cases, c)?.score)) };

    let start_fitness = eval(&start)?;
    let mut best = start;
    let mut best_fit = start_fitness;

    // The four weights, addressed as a slice of field accessors so the descent
    // loop stays declarative.
    type Field = fn(&mut ScoringConfig) -> &mut f64;
    let weights: [Field; 4] = [
        |c| &mut c.vector_weight,
        |c| &mut c.entity_weight,
        |c| &mut c.recency_weight,
        |c| &mut c.confidence_weight,
    ];

    for _ in 0..passes {
        let mut improved = false;

        for field in weights {
            for step in 0..=WEIGHT_STEPS {
                let mut cand = best;
                *field(&mut cand) = step as f64 / WEIGHT_STEPS as f64;
                let f = eval(&cand)?;
                if f > best_fit + 1e-9 {
                    best = cand;
                    best_fit = f;
                    improved = true;
                }
            }
        }

        for &h in HALF_LIFE_GRID {
            let mut cand = best;
            cand.recency_half_life_days = h;
            let f = eval(&cand)?;
            if f > best_fit + 1e-9 {
                best = cand;
                best_fit = f;
                improved = true;
            }
        }

        if !improved {
            break;
        }
    }

    normalize_weights(&mut best);

    Ok(TuneResult {
        start_score: retrieval::run_with(cases, &start)?.score,
        best_score: retrieval::run_with(cases, &best)?.score,
        start,
        best,
        start_fitness,
        best_fitness: best_fit,
    })
}

/// Scale the four weights to sum to 1. Ranking-invariant (the combined score is
/// linear in the weights), so this only changes presentation, not behavior.
fn normalize_weights(c: &mut ScoringConfig) {
    let sum = c.vector_weight + c.entity_weight + c.recency_weight + c.confidence_weight;
    if sum > 0.0 {
        c.vector_weight /= sum;
        c.entity_weight /= sum;
        c.recency_weight /= sum;
        c.confidence_weight /= sum;
    }
}
