use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use toffee_core::{Actor, EventInput, Notification, Scope};
use toffee_runtime::Runtime;
use toffee_store::Store;

fn rt() -> Runtime {
    Runtime::new(Arc::new(Store::open_in_memory().unwrap()))
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
async fn worker_status_reflects_queue_and_processed_state() {
    let runtime = rt();
    // Before any events:
    let s = runtime.worker_status().unwrap();
    assert_eq!(s.events_total, 0);
    assert_eq!(s.queue_depth, 0);
    assert!(s.last_processed_event_id.is_none());

    // Append three events; queue depth jumps before the worker drains.
    for text in [
        "The parser uses Pest.",
        "We decided to go with Rust.",
        "I prefer concise responses.",
    ] {
        runtime.append_event(user_event(text)).unwrap();
    }
    let s = runtime.worker_status().unwrap();
    assert_eq!(s.events_total, 3);
    assert_eq!(s.queue_depth, 3);

    // Drain the worker — queue should fall to 0 and totals catch up.
    runtime.drain_for_test().await.unwrap();
    let s = runtime.worker_status().unwrap();
    assert_eq!(s.events_total, 3);
    assert_eq!(s.queue_depth, 0);
    assert!(s.last_processed_event_id.is_some());
    assert!(s.lag_seconds.unwrap() < 5);
    assert!(s.memories_active >= 1);
}

#[tokio::test]
async fn promoted_memory_emits_notification() {
    let runtime = rt();
    let mut rx = runtime.subscribe_notifications();

    runtime
        .append_event(user_event("The parser uses Pest."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();

    let n = tokio::time::timeout(Duration::from_millis(500), rx.recv())
        .await
        .expect("notification should arrive")
        .expect("channel still open");
    match n {
        Notification::MemoryPromoted { kind, .. } => {
            assert_eq!(kind, toffee_core::MemoryKind::Claim);
        }
        other => panic!("expected MemoryPromoted, got {:?}", other),
    }
}

#[tokio::test]
async fn why_memory_returns_source_events_and_linked_entities() {
    let runtime = rt();
    runtime
        .append_event(user_event("The parser uses Pest."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();

    let mem = runtime
        .store()
        .list_memories(&toffee_store::MemoryListFilter::default())
        .unwrap()
        .pop()
        .unwrap();
    let report = runtime.why_memory(&mem.id).unwrap();
    assert_eq!(report.memory.id, mem.id);
    assert_eq!(report.source_events.len(), 1);
    assert_eq!(report.source_events[0].event_type, "user_message");
    assert!(
        !report.linked_entities.is_empty(),
        "should have linked the auto-created entities"
    );
    let names: Vec<_> = report.linked_entities.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"Pest") || names.contains(&"parser"));
}

#[tokio::test]
async fn why_memory_surfaces_conflicts_referencing_it() {
    let runtime = rt();
    runtime
        .append_event(user_event("The parser uses Pest."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();
    runtime
        .append_event(user_event("The parser uses Nom."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();

    let mem = runtime
        .store()
        .list_memories(&toffee_store::MemoryListFilter::default())
        .unwrap()
        .into_iter()
        .find(|m| m.text.contains("Pest"))
        .unwrap();
    let report = runtime.why_memory(&mem.id).unwrap();
    assert_eq!(report.conflicts.len(), 1);
    assert!(report.conflicts[0].competing_memory_ids.contains(&mem.id));
}

#[tokio::test]
async fn worker_failures_are_recorded_for_visibility() {
    // We can't easily induce a synthetic worker failure without breaking
    // the store, so directly call record_worker_failure to validate the
    // CRUD surface and that worker_status surfaces it.
    let runtime = rt();
    let evt = runtime.append_event(user_event("...")).unwrap();
    runtime
        .store()
        .record_worker_failure("default", &evt.id, "synthetic failure", chrono::Utc::now())
        .unwrap();

    let s = runtime.worker_status().unwrap();
    assert_eq!(s.failures_recent.len(), 1);
    assert_eq!(s.failures_recent[0].error, "synthetic failure");
}
