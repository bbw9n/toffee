//! Integration test for the worker pipeline: events flow in via the runtime
//! and durable memories come out, with the correct kind and SPO.

use std::sync::Arc;

use serde_json::json;
use toffee_core::{Actor, EventInput, FeedbackKind, MemoryKind, Scope};
use toffee_runtime::{Runtime, SearchParams};
use toffee_store::{MemoryListFilter, Store};

fn rt() -> Runtime {
    let store = Arc::new(Store::open_in_memory().unwrap());
    Runtime::new(store)
}

fn user_event(text: &str) -> EventInput {
    EventInput {
        scope: Scope::new(["project:test"]),
        actor: Actor::User,
        event_type: "user_message".into(),
        payload: json!({"text": text}),
        session_id: None,
        run_id: None,
    }
}

#[tokio::test]
async fn worker_promotes_preference_from_user_message() {
    let runtime = rt();
    runtime
        .append_event(user_event("I prefer concise responses."))
        .unwrap();

    let processed = runtime.drain_for_test().await.unwrap();
    assert_eq!(processed, 1);

    let memories = runtime
        .store()
        .list_memories(&MemoryListFilter::default())
        .unwrap();
    assert_eq!(memories.len(), 1, "{:#?}", memories);
    let m = &memories[0];
    assert_eq!(m.kind, MemoryKind::Preference);
    assert_eq!(m.predicate.as_deref(), Some("prefers"));
    assert!(m.confidence >= 0.7, "confidence={}", m.confidence);
}

#[tokio::test]
async fn worker_extracts_claim_from_uses_pattern() {
    let runtime = rt();
    runtime
        .append_event(user_event("The parser uses Pest."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();
    let memories = runtime
        .store()
        .list_memories(&MemoryListFilter::default())
        .unwrap();
    assert_eq!(memories.len(), 1);
    let m = &memories[0];
    assert_eq!(m.kind, MemoryKind::Claim);
    assert_eq!(m.subject.as_deref(), Some("parser"));
    assert_eq!(m.object.as_deref(), Some("Pest"));
}

#[tokio::test]
async fn feedback_lowers_confidence_on_wrong() {
    let runtime = rt();
    runtime
        .append_event(user_event("The parser uses Pest."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();
    let memories = runtime
        .store()
        .list_memories(&MemoryListFilter::default())
        .unwrap();
    let mem = &memories[0];
    let before = mem.confidence;
    let after = runtime
        .record_feedback(&mem.id, FeedbackKind::Wrong)
        .unwrap();
    assert!(after < before, "before={before} after={after}");
}

#[tokio::test]
async fn forget_removes_memory_from_default_list() {
    let runtime = rt();
    runtime
        .append_event(user_event("I prefer concise responses."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();
    let id = runtime
        .store()
        .list_memories(&MemoryListFilter::default())
        .unwrap()[0]
        .id
        .clone();

    runtime.forget_memory(&id).unwrap();

    let after = runtime
        .store()
        .list_memories(&MemoryListFilter::default())
        .unwrap();
    assert!(after.is_empty());
}

#[tokio::test]
async fn worker_links_extracted_memories_to_entities() {
    let runtime = rt();
    runtime
        .append_event(user_event("The parser uses Pest."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();

    let memories = runtime
        .store()
        .list_memories(&MemoryListFilter::default())
        .unwrap();
    let memory = &memories[0];
    // Memory.entities should be populated with the auto-created entities.
    assert_eq!(
        memory.entities.len(),
        2,
        "expected parser + Pest, got {:?}",
        memory.entities
    );

    let parser = runtime
        .store()
        .find_entity_by_name("parser")
        .unwrap()
        .expect("parser entity");
    let pest = runtime
        .store()
        .find_entity_by_name("Pest")
        .unwrap()
        .expect("Pest entity");
    assert!(memory.entities.contains(&parser.id.0));
    assert!(memory.entities.contains(&pest.id.0));
}

#[tokio::test]
async fn entity_page_renders_linked_memories_and_co_occurrence() {
    let runtime = rt();
    runtime
        .append_event(user_event("The parser uses Pest."))
        .unwrap();
    runtime
        .append_event(user_event("Let's use Pest for the new feature."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();

    let page = runtime
        .get_entity_page("Pest", Some(vec!["project:test".into()]))
        .unwrap();
    assert_eq!(page.entity.name, "Pest");
    assert!(!page.memories.is_empty(), "page should include memories");
    assert!(page.memories.iter().any(|m| m.text.contains("Pest")));
    // The first event also created a "parser" entity that co-occurs with Pest.
    assert!(
        page.co_occurring.iter().any(|(e, _)| e.name == "parser"),
        "expected 'parser' in co-occurring entities, got {:?}",
        page.co_occurring
            .iter()
            .map(|(e, n)| (e.name.as_str(), n))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn worker_embeds_promoted_memories() {
    let runtime = rt();
    runtime
        .append_event(user_event("The parser uses Pest."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();
    let n_embeddings = runtime.store().embedding_count().unwrap();
    assert_eq!(n_embeddings, 1, "expected one embedding for the claim");
    assert_eq!(runtime.vector_index().len(), 1);
}

#[tokio::test]
async fn search_memory_returns_lexically_similar_memory() {
    let runtime = rt();
    runtime
        .append_event(user_event("The parser uses Pest."))
        .unwrap();
    runtime
        .append_event(user_event("I prefer concise responses."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();

    let hits = runtime
        .search_memory(SearchParams {
            query: "which parser library".into(),
            scope_any_of: Some(vec!["project:test".into()]),
            limit: 5,
            ..Default::default()
        })
        .unwrap();
    assert!(!hits.is_empty(), "expected at least one hit");
    // The Pest claim should win over the preference for a parser query.
    assert!(
        hits[0].memory.text.contains("Pest"),
        "first hit was {:?}",
        hits[0].memory.text
    );
}

#[tokio::test]
async fn search_respects_scope_filter() {
    let runtime = rt();
    runtime
        .append_event(EventInput {
            scope: Scope::new(["project:foo"]),
            actor: Actor::User,
            event_type: "user_message".into(),
            payload: json!({"text": "The parser uses Pest."}),
            session_id: None,
            run_id: None,
        })
        .unwrap();
    runtime
        .append_event(EventInput {
            scope: Scope::new(["project:bar"]),
            actor: Actor::User,
            event_type: "user_message".into(),
            payload: json!({"text": "The parser uses Pest."}),
            session_id: None,
            run_id: None,
        })
        .unwrap();
    runtime.drain_for_test().await.unwrap();

    let hits = runtime
        .search_memory(SearchParams {
            query: "parser".into(),
            scope_any_of: Some(vec!["project:foo".into()]),
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    assert!(!hits.is_empty());
    for h in &hits {
        assert!(
            h.memory.scope.as_slice().contains(&"project:foo".to_string()),
            "leaked scope: {:?}",
            h.memory.scope
        );
    }
}

#[tokio::test]
async fn rebuild_indexes_repopulates_from_store() {
    let runtime = rt();
    runtime
        .append_event(user_event("The parser uses Pest."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();
    assert_eq!(runtime.vector_index().len(), 1);
    // Wipe + rebuild.
    let n = runtime.rebuild_indexes().unwrap();
    assert_eq!(n, 1);
    assert_eq!(runtime.vector_index().len(), 1);
    // Search still works after rebuild.
    let hits = runtime
        .search_memory(SearchParams {
            query: "parser".into(),
            limit: 5,
            ..Default::default()
        })
        .unwrap();
    assert!(!hits.is_empty());
}

#[tokio::test]
async fn forget_drops_from_vector_index_too() {
    let runtime = rt();
    runtime
        .append_event(user_event("The parser uses Pest."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();
    let id = runtime
        .store()
        .list_memories(&MemoryListFilter::default())
        .unwrap()[0]
        .id
        .clone();
    runtime.forget_memory(&id).unwrap();
    let hits = runtime
        .search_memory(SearchParams {
            query: "parser".into(),
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    assert!(
        hits.iter().all(|h| h.memory.id != id),
        "forgotten memory came back"
    );
}

#[tokio::test]
async fn conflicting_claims_become_a_conflict_row() {
    let runtime = rt();
    runtime
        .append_event(user_event("The parser uses Pest."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();
    runtime
        .append_event(user_event("The parser uses Nom."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();

    let conflicts = runtime.store().list_unresolved_conflicts().unwrap();
    assert_eq!(conflicts.len(), 1, "expected exactly one conflict");
    let c = &conflicts[0];
    assert_eq!(c.subject.as_deref(), Some("parser"));
    assert_eq!(c.predicate.as_deref(), Some("uses"));
    assert!(c.competing_memory_ids.len() >= 2);
}
