//! Heuristic extractor: turn an `Event` into zero or more `MemoryCandidate`s.
//!
//! v1 is intentionally conservative — false positives degrade retrieval and
//! create user-visible memory churn, while false negatives are recoverable
//! via `toffee memory add`.

use std::sync::LazyLock;

use regex::Regex;
use toffee_core::{
    Actor, Event, ExtractionProvenance, MemoryCandidate, MemoryKind,
};

static REMEMBER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bremember\s+(?:that\s+)?(.{3,})").unwrap());

static PREFER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bI\s+prefer\s+(.{2,})").unwrap());

static MY_X_IS_Y_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bmy\s+([A-Za-z][A-Za-z0-9 _-]{0,40}?)\s+is\s+(.{2,})").unwrap());

static DECISION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bwe(?:'ve| have)?\s+decided\s+(?:to\s+(?:go\s+with\s+|use\s+))?(.{2,})")
        .unwrap()
});

static USES_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bthe\s+([A-Za-z][A-Za-z0-9 _-]{0,30}?)\s+uses\s+(.{2,})").unwrap());

static LETS_USE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\blet'?s\s+use\s+(.{2,})").unwrap());

static ALWAYS_NEVER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bI\s+(always|never)\s+(.{2,})").unwrap()
});

pub fn extract(event: &Event) -> Vec<MemoryCandidate> {
    if !matches!(event.actor, Actor::User) {
        return Vec::new();
    }
    let text = match event.payload.get("text").and_then(|v| v.as_str()) {
        Some(t) => t.trim(),
        None => return Vec::new(),
    };
    if text.is_empty() {
        return Vec::new();
    }

    let mut out: Vec<MemoryCandidate> = Vec::new();
    let scope = event.scope.clone();
    let source = vec![event.id.clone()];

    if let Some(cap) = PREFER_RE.captures(text) {
        let object = trim_punct(&cap[1]);
        out.push(MemoryCandidate {
            kind: MemoryKind::Preference,
            scope: scope.clone(),
            text: format!("User prefers {object}"),
            subject: Some("user".into()),
            predicate: Some("prefers".into()),
            object: Some(object),
            entities: vec![],
            confidence: None,
            source_event_ids: source.clone(),
            provenance: ExtractionProvenance::ExplicitUser,
        });
    }

    if let Some(cap) = ALWAYS_NEVER_RE.captures(text) {
        let verb = cap[1].to_lowercase();
        let object = trim_punct(&cap[2]);
        out.push(MemoryCandidate {
            kind: MemoryKind::Preference,
            scope: scope.clone(),
            text: format!("User {verb}s {object}").replace("alwayss", "always").replace("nevers", "never"),
            subject: Some("user".into()),
            predicate: Some(verb),
            object: Some(object),
            entities: vec![],
            confidence: None,
            source_event_ids: source.clone(),
            provenance: ExtractionProvenance::ExplicitUser,
        });
    }

    if let Some(cap) = DECISION_RE.captures(text) {
        let object = trim_punct(&cap[1]);
        out.push(MemoryCandidate {
            kind: MemoryKind::Decision,
            scope: scope.clone(),
            text: format!("Decided: {object}"),
            subject: Some("we".into()),
            predicate: Some("decided".into()),
            object: Some(object),
            entities: vec![],
            confidence: None,
            source_event_ids: source.clone(),
            provenance: ExtractionProvenance::ExplicitUser,
        });
    } else if let Some(cap) = LETS_USE_RE.captures(text) {
        let object = trim_punct(&cap[1]);
        out.push(MemoryCandidate {
            kind: MemoryKind::Decision,
            scope: scope.clone(),
            text: format!("Use {object}"),
            subject: Some("we".into()),
            predicate: Some("decided".into()),
            object: Some(object),
            entities: vec![],
            confidence: None,
            source_event_ids: source.clone(),
            provenance: ExtractionProvenance::ExplicitUser,
        });
    }

    if let Some(cap) = USES_RE.captures(text) {
        let subject = cap[1].trim().to_string();
        let object = trim_punct(&cap[2]);
        out.push(MemoryCandidate {
            kind: MemoryKind::Claim,
            scope: scope.clone(),
            text: format!("The {subject} uses {object}"),
            subject: Some(subject),
            predicate: Some("uses".into()),
            object: Some(object),
            entities: vec![],
            confidence: None,
            source_event_ids: source.clone(),
            provenance: ExtractionProvenance::ExplicitUser,
        });
    }

    if let Some(cap) = MY_X_IS_Y_RE.captures(text) {
        let attribute = cap[1].trim().to_string();
        let value = trim_punct(&cap[2]);
        out.push(MemoryCandidate {
            kind: MemoryKind::Claim,
            scope: scope.clone(),
            text: format!("User's {attribute} is {value}"),
            subject: Some("user".into()),
            predicate: Some(format!("has-{}", normalize_word(&attribute))),
            object: Some(value),
            entities: vec![],
            confidence: None,
            source_event_ids: source.clone(),
            provenance: ExtractionProvenance::ExplicitUser,
        });
    }

    if let Some(cap) = REMEMBER_RE.captures(text) {
        // Conservative SPO: treat the remainder as the object of a generic
        // user assertion. The text is what gets surfaced to a future agent.
        let body = trim_punct(&cap[1]);
        // Skip if we already emitted a higher-specificity candidate for the
        // same source event (heuristic: a more specific one ran above).
        if out.is_empty() {
            out.push(MemoryCandidate {
                kind: MemoryKind::Claim,
                scope: scope.clone(),
                text: body.clone(),
                subject: Some("user".into()),
                predicate: Some("asserts".into()),
                object: Some(body),
                entities: vec![],
                confidence: None,
                source_event_ids: source.clone(),
                provenance: ExtractionProvenance::ExplicitUser,
            });
        }
    }

    out
}

fn trim_punct(s: &str) -> String {
    s.trim()
        .trim_end_matches(|c: char| ".!?,;:".contains(c))
        .trim()
        .to_string()
}

fn normalize_word(s: &str) -> String {
    s.trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use toffee_core::{Actor, EventId, Scope};

    fn user_event(text: &str) -> Event {
        Event {
            id: EventId::generate(),
            scope: Scope::new(["project:test"]),
            actor: Actor::User,
            event_type: "user_message".into(),
            payload: json!({"text": text}),
            session_id: None,
            run_id: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn prefers_pattern() {
        let cands = extract(&user_event("I prefer concise responses."));
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].kind, MemoryKind::Preference);
        assert_eq!(cands[0].subject.as_deref(), Some("user"));
        assert_eq!(cands[0].predicate.as_deref(), Some("prefers"));
        assert_eq!(cands[0].object.as_deref(), Some("concise responses"));
    }

    #[test]
    fn decision_pattern() {
        let cands = extract(&user_event("We decided to go with Rust over Go."));
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].kind, MemoryKind::Decision);
        assert_eq!(cands[0].object.as_deref(), Some("Rust over Go"));
    }

    #[test]
    fn parser_uses_pattern() {
        let cands = extract(&user_event("The parser uses Pest."));
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].kind, MemoryKind::Claim);
        assert_eq!(cands[0].subject.as_deref(), Some("parser"));
        assert_eq!(cands[0].predicate.as_deref(), Some("uses"));
        assert_eq!(cands[0].object.as_deref(), Some("Pest"));
    }

    #[test]
    fn agent_messages_are_ignored() {
        let mut ev = user_event("I prefer concise responses.");
        ev.actor = Actor::Agent;
        assert!(extract(&ev).is_empty());
    }

    #[test]
    fn always_pattern() {
        let cands = extract(&user_event("I always write tests first."));
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].kind, MemoryKind::Preference);
        assert_eq!(cands[0].predicate.as_deref(), Some("always"));
    }

    #[test]
    fn empty_payload_no_candidates() {
        let mut ev = user_event("");
        ev.payload = json!({});
        assert!(extract(&ev).is_empty());
    }

    #[test]
    fn unmatched_message_yields_no_candidates() {
        // "hello world" is just noise — extractor should refuse to invent
        // memory.
        assert!(extract(&user_event("hello world")).is_empty());
    }
}
