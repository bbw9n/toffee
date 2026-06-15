//! Property tests on the pure-function surface of `toffee-core`.
//!
//! These guarantee the formulas behave under any combination of inputs the
//! runtime might throw at them — not just the canonical cases the unit
//! tests cover.

use proptest::prelude::*;
use toffee_core::scalars::{
    apply_feedback, compute_confidence, DISCARD_THRESHOLD, PROMOTE_THRESHOLD,
};
use toffee_core::{
    expand_inherited, ContextPackage, EventId, ExtractionProvenance, FeedbackKind, MemoryCandidate,
    MemoryKind, Scope,
};

fn any_provenance() -> impl Strategy<Value = ExtractionProvenance> {
    prop_oneof![
        Just(ExtractionProvenance::ExplicitUser),
        Just(ExtractionProvenance::AgentConfirmed),
        Just(ExtractionProvenance::RepeatedPattern),
        Just(ExtractionProvenance::AgentInferred),
        Just(ExtractionProvenance::ToolOutput),
        Just(ExtractionProvenance::Manual),
    ]
}

fn any_kind() -> impl Strategy<Value = MemoryKind> {
    prop_oneof![
        Just(MemoryKind::Claim),
        Just(MemoryKind::Decision),
        Just(MemoryKind::Preference),
        Just(MemoryKind::Episode),
    ]
}

fn any_feedback() -> impl Strategy<Value = FeedbackKind> {
    prop_oneof![
        Just(FeedbackKind::Helpful),
        Just(FeedbackKind::Wrong),
        Just(FeedbackKind::Stale),
        Just(FeedbackKind::Correct),
    ]
}

fn candidate_strategy() -> impl Strategy<Value = MemoryCandidate> {
    (
        any_kind(),
        any_provenance(),
        // Explicit confidence: arbitrary f64 (including NaN). Confidence
        // clamping should make sure NaN doesn't escape.
        prop_oneof![Just(None), any::<f64>().prop_map(Some),],
    )
        .prop_map(|(kind, provenance, explicit)| MemoryCandidate {
            kind,
            scope: Scope::new(["g"]),
            text: "x".into(),
            subject: Some("a".into()),
            predicate: Some("b".into()),
            object: Some("c".into()),
            entities: vec![],
            confidence: explicit,
            source_event_ids: vec![EventId::generate()],
            provenance,
        })
}

proptest! {
    #[test]
    fn confidence_always_in_unit_interval(cand in candidate_strategy()) {
        let c = compute_confidence(&cand);
        prop_assert!(!c.is_nan(), "confidence is NaN for {:?}", cand);
        prop_assert!((0.0..=1.0).contains(&c), "confidence out of range: {c}");
    }

    #[test]
    fn feedback_keeps_confidence_in_unit_interval(
        start in any::<f64>(),
        feedback in any_feedback(),
    ) {
        // The input may itself be out of range or NaN — apply_feedback should
        // still produce a clean number in [0,1].
        let out = apply_feedback(start, feedback);
        prop_assert!(!out.is_nan());
        prop_assert!((0.0..=1.0).contains(&out));
    }

    #[test]
    fn wrong_drops_at_least_as_much_as_stale(start in 0.0f64..1.0) {
        let after_wrong = apply_feedback(start, FeedbackKind::Wrong);
        let after_stale = apply_feedback(start, FeedbackKind::Stale);
        prop_assert!(after_wrong <= after_stale + 1e-9);
    }

    #[test]
    fn helpful_and_correct_never_decrease(start in 0.0f64..=1.0) {
        let after_helpful = apply_feedback(start, FeedbackKind::Helpful);
        let after_correct = apply_feedback(start, FeedbackKind::Correct);
        // Both bounded above by 1.0 (clamping). Either equal to start
        // (when already at 1.0) or strictly greater.
        prop_assert!(after_helpful >= start - 1e-9);
        prop_assert!(after_correct >= start - 1e-9);
    }

    #[test]
    fn promote_threshold_is_above_discard(_unused in any::<u8>()) {
        prop_assert!(PROMOTE_THRESHOLD > DISCARD_THRESHOLD);
    }

    #[test]
    fn expand_inherited_is_idempotent(scopes in proptest::collection::vec(
        "[a-z]+:[a-z]+", 0..6,
    )) {
        let once = expand_inherited(&scopes);
        let twice = expand_inherited(&once);
        prop_assert_eq!(once, twice);
    }

    #[test]
    fn expand_inherited_preserves_explicit_inputs(scopes in proptest::collection::vec(
        "[a-z]+:[a-z]+", 1..6,
    )) {
        let out = expand_inherited(&scopes);
        for s in &scopes {
            prop_assert!(out.iter().any(|x| x == s), "missing input {s} in {:?}", out);
        }
    }

    #[test]
    fn token_estimate_monotone_in_length(s1 in ".*", extra in "[a-z]{0,40}") {
        let s2 = format!("{s1}{extra}");
        let t1 = ContextPackage::estimate_tokens_for(&s1);
        let t2 = ContextPackage::estimate_tokens_for(&s2);
        prop_assert!(t2 >= t1);
    }
}
