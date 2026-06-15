use std::sync::Arc;

use serde_json::json;
use toffee_core::{Actor, ConflictResolution, EventInput, Scope};
use toffee_runtime::{ResolutionAction, Runtime};
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
async fn repeated_contradiction_extends_single_conflict() {
    let runtime = rt();
    runtime
        .append_event(user_event("The parser uses Pest."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();
    runtime
        .append_event(user_event("The parser uses Nom."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();
    // Third memory pointing at a third option — should attach to the
    // existing conflict, not open a new one.
    runtime
        .append_event(user_event("The parser uses Chumsky."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();

    let conflicts = runtime.list_conflicts(false).unwrap();
    assert_eq!(conflicts.len(), 1, "expected one deduped conflict");
    assert_eq!(conflicts[0].competing_memory_ids.len(), 3);
}

#[tokio::test]
async fn pick_supersedes_losers_and_drops_from_search() {
    let runtime = rt();
    runtime
        .append_event(user_event("The parser uses Pest."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();
    runtime
        .append_event(user_event("The parser uses Nom."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();

    let conflict = runtime.list_conflicts(false).unwrap().pop().unwrap();
    let winner = conflict.competing_memory_ids[0].clone();
    let loser = conflict.competing_memory_ids[1].clone();

    let resolved = runtime
        .resolve_conflict(
            &conflict.id,
            ResolutionAction::Pick {
                winner: winner.clone(),
            },
        )
        .unwrap();
    assert_eq!(resolved.resolution, ConflictResolution::Picked);

    // Loser is superseded.
    let loser_row = runtime.store().get_memory(&loser).unwrap().unwrap();
    assert_eq!(loser_row.superseded_by.as_ref(), Some(&winner));

    // Search no longer surfaces the loser.
    let hits = runtime
        .search_memory(toffee_runtime::SearchParams {
            query: "parser".into(),
            scope_any_of: Some(vec!["project:test".into()]),
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    assert!(
        hits.iter().all(|h| h.memory.id != loser),
        "loser memory came back: {:?}",
        hits
    );
}

#[tokio::test]
async fn merge_creates_new_memory_and_supersedes_all() {
    let runtime = rt();
    runtime
        .append_event(user_event("The parser uses Pest."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();
    runtime
        .append_event(user_event("The parser uses Nom."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();

    let conflict = runtime.list_conflicts(false).unwrap().pop().unwrap();
    let competing = conflict.competing_memory_ids.clone();

    let resolved = runtime
        .resolve_conflict(
            &conflict.id,
            ResolutionAction::Merge {
                text: "The parser uses Pest for grammar and Nom for binary inputs".into(),
                subject: None,
                predicate: None,
                object: None,
                confidence: Some(0.97),
            },
        )
        .unwrap();
    assert_eq!(resolved.resolution, ConflictResolution::Merged);

    // Every original competing memory is now superseded.
    let mut merged_target: Option<toffee_core::MemoryId> = None;
    for id in &competing {
        let m = runtime.store().get_memory(id).unwrap().unwrap();
        let sb = m.superseded_by.expect("loser should be superseded");
        match &merged_target {
            None => merged_target = Some(sb),
            Some(prev) => assert_eq!(
                prev, &sb,
                "all losers should point at the same merged memory"
            ),
        }
    }
    let merged_id = merged_target.unwrap();
    let merged = runtime.store().get_memory(&merged_id).unwrap().unwrap();
    assert!(merged.text.contains("Pest"));
    assert!(merged.text.contains("Nom"));
    assert!((merged.confidence - 0.97).abs() < 1e-9);
}

#[tokio::test]
async fn reject_all_soft_deletes_everything() {
    let runtime = rt();
    runtime
        .append_event(user_event("The parser uses Pest."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();
    runtime
        .append_event(user_event("The parser uses Nom."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();

    let conflict = runtime.list_conflicts(false).unwrap().pop().unwrap();
    let resolved = runtime
        .resolve_conflict(&conflict.id, ResolutionAction::RejectAll)
        .unwrap();
    assert_eq!(resolved.resolution, ConflictResolution::RejectedAll);

    let listed = runtime
        .store()
        .list_memories(&toffee_store::MemoryListFilter::default())
        .unwrap();
    assert!(
        listed.is_empty(),
        "expected no active memories after reject-all"
    );
}

#[tokio::test]
async fn resolving_twice_is_rejected() {
    let runtime = rt();
    runtime
        .append_event(user_event("The parser uses Pest."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();
    runtime
        .append_event(user_event("The parser uses Nom."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();

    let conflict = runtime.list_conflicts(false).unwrap().pop().unwrap();
    runtime
        .resolve_conflict(&conflict.id, ResolutionAction::RejectAll)
        .unwrap();
    let again = runtime.resolve_conflict(&conflict.id, ResolutionAction::RejectAll);
    assert!(matches!(
        again,
        Err(toffee_runtime::RuntimeError::AlreadyResolved(_))
    ));
}

#[tokio::test]
async fn pick_with_invalid_winner_is_rejected() {
    let runtime = rt();
    runtime
        .append_event(user_event("The parser uses Pest."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();
    runtime
        .append_event(user_event("The parser uses Nom."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();

    let conflict = runtime.list_conflicts(false).unwrap().pop().unwrap();
    let bogus = toffee_core::MemoryId("mem_unrelated".into());
    let res = runtime.resolve_conflict(&conflict.id, ResolutionAction::Pick { winner: bogus });
    assert!(matches!(
        res,
        Err(toffee_runtime::RuntimeError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn resolved_conflict_does_not_block_new_contradictions() {
    let runtime = rt();
    runtime
        .append_event(user_event("The parser uses Pest."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();
    runtime
        .append_event(user_event("The parser uses Nom."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();

    // Resolve by picking Pest as winner.
    let first = runtime.list_conflicts(false).unwrap().pop().unwrap();
    let winner = first.competing_memory_ids[0].clone();
    runtime
        .resolve_conflict(&first.id, ResolutionAction::Pick { winner })
        .unwrap();

    // A new contradicting claim should open a *new* conflict, not extend
    // the closed one.
    runtime
        .append_event(user_event("The parser uses Chumsky."))
        .unwrap();
    runtime.drain_for_test().await.unwrap();

    let unresolved = runtime.list_conflicts(false).unwrap();
    assert_eq!(unresolved.len(), 1, "expected exactly one unresolved");
    let all = runtime.list_conflicts(true).unwrap();
    assert_eq!(all.len(), 2, "expected one resolved + one unresolved");
}
