//! Decide what to do with a `MemoryCandidate` once confidence is known.

use toffee_core::{scalars, MemoryCandidate, MemoryKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Drop the candidate entirely.
    Discard,
    /// Persist as a low-stakes episode (narrative memory).
    Episode,
    /// Promote to durable memory.
    Promote,
}

pub fn decide(candidate: &MemoryCandidate, computed_confidence: f64) -> Decision {
    // Episodes are special: they always persist (they're the "background"
    // narrative) but at whatever confidence we computed.
    if matches!(candidate.kind, MemoryKind::Episode) {
        if computed_confidence < scalars::DISCARD_THRESHOLD {
            return Decision::Discard;
        }
        return Decision::Episode;
    }

    // Claims / decisions / preferences require SPO to be persisted. If the
    // extractor failed to produce SPO, demote to episode if possible.
    if candidate.subject.is_none() || candidate.predicate.is_none() || candidate.object.is_none() {
        if computed_confidence < scalars::DISCARD_THRESHOLD {
            return Decision::Discard;
        }
        return Decision::Episode;
    }

    if computed_confidence >= scalars::PROMOTE_THRESHOLD {
        Decision::Promote
    } else if computed_confidence >= scalars::DISCARD_THRESHOLD {
        Decision::Episode
    } else {
        Decision::Discard
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use toffee_core::{EventId, ExtractionProvenance, Scope};

    fn base(kind: MemoryKind, has_spo: bool) -> MemoryCandidate {
        MemoryCandidate {
            kind,
            scope: Scope::new(["project:t"]),
            text: "x".into(),
            subject: if has_spo { Some("a".into()) } else { None },
            predicate: if has_spo { Some("b".into()) } else { None },
            object: if has_spo { Some("c".into()) } else { None },
            entities: vec![],
            confidence: None,
            source_event_ids: vec![EventId::generate()],
            provenance: ExtractionProvenance::ExplicitUser,
        }
    }

    #[test]
    fn high_confidence_claim_promotes() {
        let c = base(MemoryKind::Claim, true);
        assert_eq!(decide(&c, 0.9), Decision::Promote);
    }

    #[test]
    fn low_confidence_claim_becomes_episode() {
        let c = base(MemoryKind::Claim, true);
        assert_eq!(decide(&c, 0.5), Decision::Episode);
    }

    #[test]
    fn very_low_confidence_discards() {
        let c = base(MemoryKind::Claim, true);
        assert_eq!(decide(&c, 0.1), Decision::Discard);
    }

    #[test]
    fn missing_spo_demotes_to_episode() {
        let c = base(MemoryKind::Claim, false);
        assert_eq!(decide(&c, 0.9), Decision::Episode);
    }
}
