//! Extraction eval: run the heuristic extractor over the corpus and score
//! predicted memories against the gold labels.
//!
//! This isolates surface #1 (event → memory) — it calls the pure `extract`
//! function directly, no runtime, no embedder. Swapping in a model-assisted
//! extractor later means re-pointing `run` at the new function and comparing
//! the numbers against today's baseline.

use chrono::Utc;
use toffee_core::{Actor, Event, EventId, MemoryCandidate, Scope};
use toffee_runtime::extractor;

use crate::corpus::{ExpectedMemory, ExtractionCase};
use crate::metrics::ExtractionScore;

/// A case whose predictions didn't match its labels — surfaced in the report
/// so a regression points at the offending input, not just a lower number.
#[derive(Debug, Clone)]
pub struct ExtractionMiss {
    pub name: String,
    pub note: String,
}

pub struct ExtractionReport {
    pub score: ExtractionScore,
    pub misses: Vec<ExtractionMiss>,
}

pub fn run(cases: &[ExtractionCase]) -> ExtractionReport {
    let mut score = ExtractionScore::default();
    let mut misses = Vec::new();

    for case in cases {
        score.cases += 1;
        if case.expect.is_empty() {
            score.negative_cases += 1;
        }
        let event = to_event(case);
        let predicted = extractor::extract(&event);
        score_case(case, &predicted, &mut score, &mut misses);
    }

    ExtractionReport { score, misses }
}

fn score_case(
    case: &ExtractionCase,
    predicted: &[MemoryCandidate],
    score: &mut ExtractionScore,
    misses: &mut Vec<ExtractionMiss>,
) {
    let mut used = vec![false; predicted.len()];

    for exp in &case.expect {
        match predicted
            .iter()
            .enumerate()
            .find(|(i, p)| !used[*i] && matches(p, exp))
        {
            Some((i, pred)) => {
                used[i] = true;
                score.true_positives += 1;
                grade_fields(pred, exp, score);
            }
            None => {
                score.false_negatives += 1;
                misses.push(ExtractionMiss {
                    name: case.name.clone(),
                    note: format!("missed expected {} {:?}", exp.kind, label(exp)),
                });
            }
        }
    }

    for (i, pred) in predicted.iter().enumerate() {
        if !used[i] {
            score.false_positives += 1;
            misses.push(ExtractionMiss {
                name: case.name.clone(),
                note: format!("spurious {} memory {:?}", pred.kind.as_str(), pred.text),
            });
        }
    }
}

/// Match predicted to expected loosely so paraphrase doesn't fail the match:
/// by normalized `object`, by `text_contains`, or — when the label pins
/// neither — by `kind` alone. Kind/SPO exactness is then graded separately on
/// the matched pair so a right-text/wrong-kind prediction still shows up.
fn matches(pred: &MemoryCandidate, exp: &ExpectedMemory) -> bool {
    if let Some(obj) = &exp.object {
        if norm(pred.object.as_deref().unwrap_or_default()) == norm(obj) {
            return true;
        }
    }
    if let Some(tc) = &exp.text_contains {
        if pred.text.to_lowercase().contains(&tc.to_lowercase()) {
            return true;
        }
    }
    exp.object.is_none() && exp.text_contains.is_none() && pred.kind.as_str() == exp.kind
}

fn grade_fields(pred: &MemoryCandidate, exp: &ExpectedMemory, score: &mut ExtractionScore) {
    score.kind.observe(pred.kind.as_str() == exp.kind);
    if let Some(s) = &exp.subject {
        score
            .subject
            .observe(norm(pred.subject.as_deref().unwrap_or_default()) == norm(s));
    }
    if let Some(p) = &exp.predicate {
        score
            .predicate
            .observe(norm(pred.predicate.as_deref().unwrap_or_default()) == norm(p));
    }
    if let Some(o) = &exp.object {
        score
            .object
            .observe(norm(pred.object.as_deref().unwrap_or_default()) == norm(o));
    }
}

fn label(exp: &ExpectedMemory) -> String {
    exp.object
        .clone()
        .or_else(|| exp.text_contains.clone())
        .unwrap_or_else(|| "<kind-only>".into())
}

fn norm(s: &str) -> String {
    s.trim().to_lowercase()
}

fn to_event(case: &ExtractionCase) -> Event {
    Event {
        id: EventId::generate(),
        scope: Scope::new(case.scope.iter().cloned()),
        actor: parse_actor(&case.actor),
        event_type: "user_message".into(),
        payload: serde_json::json!({ "text": case.input }),
        session_id: None,
        run_id: None,
        created_at: Utc::now(),
    }
}

fn parse_actor(s: &str) -> Actor {
    match s.to_lowercase().as_str() {
        "agent" => Actor::Agent,
        "tool" => Actor::Tool,
        "system" => Actor::System,
        _ => Actor::User,
    }
}
