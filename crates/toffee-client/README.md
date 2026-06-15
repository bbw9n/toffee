# toffee-client

Async Rust client for [toffee](https://github.com/example/toffee), a local-first memory layer for AI agents. Any number of agents on the same machine talk to a single long-lived `toffeed` daemon over a Unix domain socket.

This is the crate you add to your agent: it speaks JSON-RPC to the daemon and gives you typed methods for the five hot-path calls.

## Quick start

```toml
[dependencies]
toffee-client = "0.1"
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

```rust,no_run
use toffee_client::Client;
use toffee_core::{Actor, EventInput, Scope};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Connects to `$XDG_RUNTIME_DIR/toffee/toffeed.sock`, auto-spawning
    // the daemon if it isn't already running.
    let toffee = Client::connect().await?;

    let user_message = "fix the parser bug";

    // 1. Before generating, ask toffee what's relevant.
    let ctx = toffee
        .read_context(
            vec!["project:magi".into()],
            user_message.into(),
            Some(3000), // token budget
        )
        .await?;

    let prompt = format!("{}\n\n{}", ctx.render_markdown(), user_message);
    // let response = llm.complete(&prompt).await?;
    let response = "ack".to_string();

    // 2. Record what happened — extraction and indexing run in the daemon's
    //    background worker, so this returns in milliseconds.
    toffee
        .append_event(EventInput {
            scope: Scope::new(["project:magi"]),
            actor: Actor::User,
            event_type: "user_message".into(),
            payload: serde_json::json!({ "text": user_message }),
            session_id: None,
            run_id: None,
        })
        .await?;

    toffee
        .append_event(EventInput {
            scope: Scope::new(["project:magi"]),
            actor: Actor::Agent,
            event_type: "agent_message".into(),
            payload: serde_json::json!({ "text": response }),
            session_id: None,
            run_id: None,
        })
        .await?;

    Ok(())
}
```

## The surface

Hot path (you'll call these every turn):

- `Client::connect()` / `Client::connect_to(path)` — open a socket, auto-spawn `toffeed` if needed.
- `Client::read_context(scope, query, token_budget)` → `ContextPackage` — memories grouped by kind (decisions, claims, preferences, episodes), fitted to the budget, with unresolved conflicts surfaced. Call `.render_markdown()` for a prompt-ready block.
- `Client::append_event(EventInput)` → `EventId` — append a user / agent / tool / system event. Returns immediately; extraction is asynchronous.

Less-frequent:

- `Client::record_feedback(memory_id, FeedbackKind)` — tell toffee a memory was helpful, wrong, stale, or correct. Adjusts confidence.
- `Client::search_memory(req)` → ranked hits — escape hatch for raw retrieval without lens shaping.
- `Client::add_memory(...)` / `Client::forget_memory(id)` — manual curation.
- `Client::get_entity_page(name, scope)` — the entity-centric view.
- `Client::inspect_provenance(ctxpkg_id)` — explain why each memory ended up in a context package.
- `Client::rebuild_indexes()` — admin path to wipe and recompute embeddings.

## Daemon lifecycle

`Client::connect()` finds the daemon at `$XDG_RUNTIME_DIR/toffee/toffeed.sock`. If the socket doesn't exist or refuses connections, the client looks for a `toffeed` binary (env `TOFFEE_DAEMON_BIN`, sibling of the current executable, then `PATH`) and spawns it detached. Backoff is bounded — three seconds default.

A `toffee daemon start` invocation from the CLI does the same thing.

## Status

v0.1.0. The wire is stable for hot-path methods (`read_context`, `append_event`, `record_feedback`, `search_memory`, `hello`). Admin methods may evolve.
