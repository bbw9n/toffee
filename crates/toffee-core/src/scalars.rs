//! Pure scalar computations: the confidence formula and feedback deltas.
//!
//! These are kept here so they're exercised by tests without dragging in any
//! I/O. Keep the math conservative; the worker's discard threshold relies on
//! the numbers being reasonable.

use crate::memory::{ExtractionProvenance, FeedbackKind, MemoryCandidate};

/// Default confidence threshold for promotion to durable memory.
/// Below this, the promoter returns `Discard` (or `Episode` for narrative
/// candidates).
pub const PROMOTE_THRESHOLD: f64 = 0.7;

/// Floor at which a candidate is dropped entirely (not even saved as an
/// episode).
pub const DISCARD_THRESHOLD: f64 = 0.4;

/// Compute the initial confidence for a memory candidate.
///
/// If the candidate carries an explicit confidence (e.g. user provided one
/// via `toffee memory add --confidence`), that wins. Otherwise we derive
/// from the extraction provenance.
pub fn compute_confidence(candidate: &MemoryCandidate) -> f64 {
    if let Some(c) = candidate.confidence {
        return clamp_unit(c);
    }
    clamp_unit(match candidate.provenance {
        ExtractionProvenance::ExplicitUser => 0.92,
        ExtractionProvenance::AgentConfirmed => 0.85,
        ExtractionProvenance::RepeatedPattern => 0.75,
        ExtractionProvenance::Manual => 0.95,
        ExtractionProvenance::AgentInferred => 0.55,
        ExtractionProvenance::ToolOutput => 0.5,
    })
}

/// Update an existing confidence in response to feedback.
pub fn apply_feedback(current: f64, feedback: FeedbackKind) -> f64 {
    clamp_unit(match feedback {
        FeedbackKind::Helpful => current + 0.05,
        FeedbackKind::Correct => current + 0.10,
        FeedbackKind::Stale => current - 0.15,
        FeedbackKind::Wrong => current - 0.40,
    })
}

fn clamp_unit(x: f64) -> f64 {
    if x.is_nan() {
        return 0.0;
    }
    x.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventId;
    use crate::memory::MemoryKind;
    use crate::scope::Scope;

    fn cand(prov: ExtractionProvenance, explicit: Option<f64>) -> MemoryCandidate {
        MemoryCandidate {
            kind: MemoryKind::Claim,
            scope: Scope::new(["project:test"]),
            text: "x".into(),
            subject: Some("a".into()),
            predicate: Some("b".into()),
            object: Some("c".into()),
            entities: vec![],
            confidence: explicit,
            source_event_ids: vec![EventId::generate()],
            provenance: prov,
        }
    }

    #[test]
    fn explicit_user_above_threshold() {
        let c = compute_confidence(&cand(ExtractionProvenance::ExplicitUser, None));
        assert!(c >= PROMOTE_THRESHOLD);
    }

    #[test]
    fn tool_output_below_threshold() {
        let c = compute_confidence(&cand(ExtractionProvenance::ToolOutput, None));
        assert!(c < PROMOTE_THRESHOLD);
    }

    #[test]
    fn explicit_confidence_overrides_provenance() {
        let c = compute_confidence(&cand(ExtractionProvenance::ToolOutput, Some(0.99)));
        assert!((c - 0.99).abs() < 1e-9);
    }

    #[test]
    fn feedback_clamps_to_unit_range() {
        assert!((apply_feedback(0.0, FeedbackKind::Wrong) - 0.0).abs() < 1e-9);
        assert!((apply_feedback(1.0, FeedbackKind::Helpful) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn wrong_drops_more_than_stale() {
        let after_wrong = apply_feedback(0.9, FeedbackKind::Wrong);
        let after_stale = apply_feedback(0.9, FeedbackKind::Stale);
        assert!(after_wrong < after_stale);
    }
}
