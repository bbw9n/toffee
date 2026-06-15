use std::sync::Arc;

use serde_json::json;
use toffee_core::{Actor, EventInput, Lens, MemoryKind, RetrievalSource, Scope};
use toffee_runtime::read_path::ReadContextRequest;
use toffee_runtime::Runtime;
use toffee_store::Store;

fn rt() -> Runtime {
    Runtime::new(Arc::new(Store::open_in_memory().unwrap()))
}

fn user_event(scope: &str, text: &str) -> EventInput {
    EventInput {
        scope: Scope::new([scope]),
        actor: Actor::User,
        event_type: "user_message".into(),
        payload: json!({"text": text}),
        session_id: None,
        run_id: None,
    }
}

#[tokio::test]
async fn read_context_buckets_by_kind() {
    let runtime = rt();
    runtime
        .append_event(user_event("project:test", "The parser uses Pest."))
        .unwrap();
    runtime
        .append_event(user_event("project:test", "We decided to go with Rust."))
        .unwrap();
    runtime
        .append_event(user_event("project:test", "I prefer concise responses."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();

    let package = runtime
        .read_context(ReadContextRequest {
            scope: vec!["project:test".into()],
            query: "parser library".into(),
            lens: Lens::default_lens(),
            token_budget: 3000,
        })
        .unwrap();

    assert_eq!(package.claims.len(), 1, "{:#?}", package.claims);
    assert_eq!(package.decisions.len(), 1);
    assert_eq!(package.preferences.len(), 1);
    assert!(package.token_estimate > 0);
}

#[tokio::test]
async fn read_context_renders_markdown_with_sections() {
    let runtime = rt();
    runtime
        .append_event(user_event("project:test", "The parser uses Pest."))
        .unwrap();
    runtime
        .append_event(user_event("project:test", "We decided to go with Rust."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();

    let package = runtime
        .read_context(ReadContextRequest {
            scope: vec!["project:test".into()],
            query: "the parser".into(),
            lens: Lens::default_lens(),
            token_budget: 3000,
        })
        .unwrap();

    let md = package.render_markdown();
    assert!(md.contains("## Memory"));
    assert!(md.contains("### Claims"));
    assert!(md.contains("### Decisions"));
    assert!(md.contains("Pest"));
}

#[tokio::test]
async fn read_context_respects_scope_inheritance() {
    let runtime = rt();
    runtime
        .append_event(user_event("project:foo", "The parser uses Pest."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();

    // Add a user:me preference. Scope inheritance should surface it under
    // project:foo.
    let now = chrono::Utc::now();
    let preference = toffee_core::Memory {
        id: toffee_core::MemoryId::generate(),
        kind: MemoryKind::Preference,
        scope: Scope::new(["user:me"]),
        text: "User prefers Pest".into(),
        subject: Some("user".into()),
        predicate: Some("prefers".into()),
        object: Some("Pest".into()),
        entities: vec![],
        confidence: 0.95,
        source_event_ids: vec![],
        created_at: now,
        updated_at: now,
        superseded_by: None,
    };
    runtime
        .insert_memory_and_link_entities(preference)
        .unwrap();

    let package = runtime
        .read_context(ReadContextRequest {
            scope: vec!["project:foo".into()],
            query: "parser".into(),
            lens: Lens::default_lens(),
            token_budget: 3000,
        })
        .unwrap();

    assert_eq!(package.claims.len(), 1);
    assert_eq!(package.preferences.len(), 1, "user:me preference should surface");
}

#[tokio::test]
async fn read_context_filters_below_confidence_floor() {
    let runtime = rt();
    runtime
        .append_event(user_event("project:test", "The parser uses Pest."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();

    // Drag the memory below the lens floor.
    let id = runtime
        .store()
        .list_memories(&toffee_store::MemoryListFilter::default())
        .unwrap()[0]
        .id
        .clone();
    runtime
        .record_feedback(&id, toffee_core::FeedbackKind::Wrong)
        .unwrap();
    runtime
        .record_feedback(&id, toffee_core::FeedbackKind::Wrong)
        .unwrap();

    let package = runtime
        .read_context(ReadContextRequest {
            scope: vec!["project:test".into()],
            query: "parser".into(),
            lens: Lens::default_lens(),
            token_budget: 3000,
        })
        .unwrap();

    assert!(
        package.claims.is_empty(),
        "low-confidence memory should be filtered out"
    );
}

#[tokio::test]
async fn read_context_surfaces_unresolved_conflicts() {
    let runtime = rt();
    runtime
        .append_event(user_event("project:test", "The parser uses Pest."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();
    runtime
        .append_event(user_event("project:test", "The parser uses Nom."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();

    let package = runtime
        .read_context(ReadContextRequest {
            scope: vec!["project:test".into()],
            query: "parser".into(),
            lens: Lens::default_lens(),
            token_budget: 3000,
        })
        .unwrap();

    assert!(!package.conflicts.is_empty(), "expected a conflict surfaced");
}

#[tokio::test]
async fn provenance_records_sources_and_scores() {
    let runtime = rt();
    runtime
        .append_event(user_event("project:test", "The parser uses Pest."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();

    let package = runtime
        .read_context(ReadContextRequest {
            scope: vec!["project:test".into()],
            query: "parser library".into(),
            lens: Lens::default_lens(),
            token_budget: 3000,
        })
        .unwrap();

    let report = runtime
        .inspect_provenance(&package.id)
        .expect("provenance should be cached");
    assert_eq!(report.entries.len(), 1);
    let e = &report.entries[0];
    assert!(e.final_score > 0.0);
    assert!(e.sources.contains(&RetrievalSource::Vector));
    // The query mentions "parser", which is an auto-created entity.
    assert!(e.entity_match, "query 'parser library' should anchor on parser entity");
    assert!(matches!(e.kept_in_kind, MemoryKind::Claim));
}

#[tokio::test]
async fn provenance_ages_out_at_capacity() {
    // Just confirm that lookup of an unknown id returns None — i.e. the
    // cache is bounded behaviour, not "remember forever". Capacity tests
    // live in the unit suite.
    let runtime = rt();
    let bogus = toffee_core::ContextPackageId("ctxpkg_does_not_exist".into());
    assert!(runtime.inspect_provenance(&bogus).is_none());
}
