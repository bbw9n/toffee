//! Heuristic entity resolver.
//!
//! For each candidate's SPO subject and object (excluding generic pronouns
//! and articles), we look up an existing entity by case-insensitive name or
//! alias. If none exists, we auto-create one with type `concept`. Phase 2
//! intentionally avoids type-inference heuristics — humans relabel via the
//! CLI when it matters.
//!
//! Additionally, we scan the candidate's `text` for any *existing* entity
//! whose name or alias appears as a substring. Longest-match wins to prefer
//! "Rust crate" over "Rust" when both exist.

use std::collections::HashSet;

use chrono::Utc;
use toffee_core::{Entity, EntityId, EntityType, MemoryCandidate};
use toffee_store::{EntityListFilter, Store};

use crate::Result;

const STOPLIST: &[&str] = &[
    "user", "we", "i", "you", "they", "the", "a", "an", "this", "that", "it",
    "thing", "things", "us", "them", "everyone", "anyone", "someone",
];

pub fn resolve(candidate: &MemoryCandidate, store: &Store) -> Result<Vec<EntityId>> {
    let mut ids: Vec<EntityId> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // SPO subject + object.
    for raw in [candidate.subject.as_deref(), candidate.object.as_deref()]
        .into_iter()
        .flatten()
    {
        if let Some(ent) = resolve_or_create_entity(raw, store)? {
            if seen.insert(ent.id.0.clone()) {
                ids.push(ent.id);
            }
        }
    }

    // Text scan: link to any existing entity whose name appears.
    let text_lower = candidate.text.to_lowercase();
    let mut known = store.list_entities(&EntityListFilter::default())?;
    // Longest-name first so "Rust crate" matches before "Rust".
    known.sort_by_key(|e| std::cmp::Reverse(e.name.len()));
    for ent in known {
        if seen.contains(&ent.id.0) {
            continue;
        }
        let needles: Vec<String> = std::iter::once(ent.name.clone())
            .chain(ent.aliases.iter().cloned())
            .map(|s| s.to_lowercase())
            .collect();
        if needles
            .iter()
            .any(|n| !n.is_empty() && contains_whole_word(&text_lower, n))
        {
            if seen.insert(ent.id.0.clone()) {
                ids.push(ent.id);
            }
        }
    }

    Ok(ids)
}

fn resolve_or_create_entity(raw: &str, store: &Store) -> Result<Option<Entity>> {
    let cleaned = clean_name(raw);
    if cleaned.is_empty() {
        return Ok(None);
    }
    let lowered = cleaned.to_lowercase();
    if STOPLIST.contains(&lowered.as_str()) {
        return Ok(None);
    }
    if cleaned.len() < 2 {
        return Ok(None);
    }

    if let Some(existing) = store.find_entity_by_name(&cleaned)? {
        return Ok(Some(existing));
    }

    let now = Utc::now();
    let entity = Entity {
        id: EntityId::generate(),
        entity_type: EntityType::Concept,
        name: cleaned,
        aliases: vec![],
        summary: None,
        created_at: now,
        updated_at: now,
    };
    store.insert_entity(&entity)?;
    Ok(Some(entity))
}

fn clean_name(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    // Strip leading articles. We treat the whole thing as the entity's
    // canonical name once the article is gone.
    for article in ["the ", "a ", "an "] {
        if let Some(rest) = strip_prefix_ci(&s, article) {
            s = rest.trim().to_string();
            break;
        }
    }
    // Drop trailing punctuation.
    s = s
        .trim_end_matches(|c: char| ".!?,;:".contains(c))
        .trim()
        .to_string();
    s
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() < prefix.len() {
        return None;
    }
    if s[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

fn contains_whole_word(haystack: &str, needle: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs = start + pos;
        let before_ok = abs == 0
            || !haystack[..abs]
                .chars()
                .last()
                .map(is_word_char)
                .unwrap_or(false);
        let after_idx = abs + needle.len();
        let after_ok = after_idx >= haystack.len()
            || !haystack[after_idx..]
                .chars()
                .next()
                .map(is_word_char)
                .unwrap_or(false);
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use toffee_core::{EventId, ExtractionProvenance, MemoryKind, Scope};

    fn cand_with_spo(subject: &str, predicate: &str, object: &str, text: &str) -> MemoryCandidate {
        MemoryCandidate {
            kind: MemoryKind::Claim,
            scope: Scope::new(["project:test"]),
            text: text.to_string(),
            subject: Some(subject.into()),
            predicate: Some(predicate.into()),
            object: Some(object.into()),
            entities: vec![],
            confidence: None,
            source_event_ids: vec![EventId::generate()],
            provenance: ExtractionProvenance::ExplicitUser,
        }
    }

    #[test]
    fn auto_creates_entities_for_subject_and_object() {
        let store = Store::open_in_memory().unwrap();
        let cand = cand_with_spo("parser", "uses", "Pest", "the parser uses Pest");
        let ids = resolve(&cand, &store).unwrap();
        assert_eq!(ids.len(), 2);
        assert!(store.find_entity_by_name("parser").unwrap().is_some());
        assert!(store.find_entity_by_name("Pest").unwrap().is_some());
    }

    #[test]
    fn reuses_existing_entity_on_second_resolve() {
        let store = Store::open_in_memory().unwrap();
        let cand1 = cand_with_spo("parser", "uses", "Pest", "parser uses Pest");
        let ids1 = resolve(&cand1, &store).unwrap();
        let cand2 = cand_with_spo("parser", "supports", "Nom", "parser supports Nom");
        let ids2 = resolve(&cand2, &store).unwrap();
        // "parser" should be reused; the entity count is 3 (parser, Pest, Nom).
        let parser_id = store.find_entity_by_name("parser").unwrap().unwrap().id;
        assert!(ids1.contains(&parser_id));
        assert!(ids2.contains(&parser_id));
        assert_eq!(store.entity_count_active().unwrap(), 3);
    }

    #[test]
    fn stoplist_terms_are_not_auto_created() {
        let store = Store::open_in_memory().unwrap();
        let cand = cand_with_spo("user", "prefers", "concise responses", "user prefers concise responses");
        let ids = resolve(&cand, &store).unwrap();
        // "user" is in stoplist; "concise responses" is fine.
        assert!(!ids.iter().any(|id| {
            store.get_entity(id).unwrap().unwrap().name.to_lowercase() == "user"
        }));
        assert!(store.find_entity_by_name("user").unwrap().is_none());
        assert!(store.find_entity_by_name("concise responses").unwrap().is_some());
    }

    #[test]
    fn text_scan_links_to_existing_entity() {
        let store = Store::open_in_memory().unwrap();
        // Seed an entity.
        let _ = resolve(
            &cand_with_spo("parser", "uses", "Pest", "uses Pest"),
            &store,
        )
        .unwrap();
        // Now a candidate that doesn't mention Pest in its SPO but does in text.
        let cand = MemoryCandidate {
            kind: MemoryKind::Episode,
            scope: Scope::new(["project:test"]),
            text: "Refactored the Pest grammar today".to_string(),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
            confidence: None,
            source_event_ids: vec![EventId::generate()],
            provenance: ExtractionProvenance::ExplicitUser,
        };
        let ids = resolve(&cand, &store).unwrap();
        let pest_id = store.find_entity_by_name("Pest").unwrap().unwrap().id;
        assert!(ids.contains(&pest_id));
    }

    #[test]
    fn strip_leading_article_during_resolve() {
        let store = Store::open_in_memory().unwrap();
        let cand = cand_with_spo("The Parser", "uses", "Pest", "x");
        let _ = resolve(&cand, &store).unwrap();
        // Should have created an entity named "Parser", not "The Parser".
        let parser = store.find_entity_by_name("Parser").unwrap();
        assert!(parser.is_some(), "expected entity 'Parser' to exist");
        assert_eq!(parser.unwrap().name, "Parser");
    }
}
