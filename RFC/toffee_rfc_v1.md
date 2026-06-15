# Toffee: A Memory Layer for AI Agents

**Status:** Draft 1.0
**Goal:** A universal, local-first memory layer that multiple AI agents can share.

---

## 1. What Toffee Is

Toffee is a memory layer for AI agents. It runs as a single long-lived daemon on the user's machine. Any number of agents can connect to it, record what they're doing, and ask what's relevant before they act.

The daemon owns one persistent store. Agents are short-lived clients. A CLI lets humans inspect and edit memory directly.

```text
   agent A ──┐
             │
   agent B ──┼──► toffeed ◄── toffee (CLI)
             │
   agent C ──┘
```

The point of toffee is that memory is shared. If agent A learns that the user's parser is written in Rust, agent B knows that too. If the user corrects something via the CLI, every subsequent agent sees the correction.

---

## 2. Why a Daemon

The straightforward alternative is to ship toffee as a library each agent embeds. We're not doing that, for three reasons:

1. **Shared memory across agents.** Multiple agents running at the same time should see the same store. With embedded libraries, each has its own — or they fight over a SQLite file with no coordinator.
2. **Expensive shared state.** The embedding model is ~100MB resident; indexes grow over time. One daemon amortizes this across all agents.
3. **Lifecycle decoupling.** Memory outlives any single agent run. A daemon is the natural owner.

A library mode may ship later for tests and embedded use cases, but the daemon is the default.

---

## 3. What Memory Looks Like

Toffee distinguishes two things that most memory systems conflate:

**Events** are the raw record of what happened. A user message, an agent response, a tool call, a tool result, a feedback signal. Events are append-only and never modified.

**Memories** are typed, durable, retrievable facts derived from events. They come in a few kinds:

- **Claim**: a fact about the world. "The parser uses Pest."
- **Decision**: a deliberate choice. "We're going with Rust over Go."
- **Preference**: a stable user pattern. "User prefers concise responses."
- **Episode**: a summarized account of something that happened. "Discussed memory architecture, decided on a daemon model."

Other memory kinds (procedures, artifact summaries, entity pages) are mentioned in §11 as future work but aren't required for v1.

The separation matters because events and memories serve different purposes. Events are the audit trail and the source of truth. Memories are what gets injected into prompts. The daemon turns events into memories asynchronously, in a background worker that doesn't block agents.

### 3.1 Memory shape

```typescript
type Memory = {
  id: string;                      // "mem_<ulid>"
  kind: "claim" | "decision" | "preference" | "episode";
  scope: string[];                 // e.g. ["project:magi", "user:me"]
  text: string;                    // human-readable
  subject?: string;                // structured fields, present for claim/decision/preference
  predicate?: string;
  object?: string;
  entities: string[];              // canonical entity refs mentioned
  confidence: number;              // 0..1
  source_event_ids: string[];      // provenance
  created_at: string;
  updated_at: string;
  superseded_by?: string;          // id of the memory that replaced this one
};
```

`scope` is a list of strings. Common scopes: `project:<name>`, `user:<id>`, `repo:<remote>`, `global`. An agent reads from a set of scopes; a memory belongs to one or more scopes.

`confidence` is computed by the worker from how the memory was extracted (explicit user statement > agent inference > tool output) and updated when feedback arrives. v1 keeps the formula simple; the field exists so retrieval can filter on it.

### 3.2 Event shape

```typescript
type Event = {
  id: string;                      // daemon-assigned, "evt_<ulid>"
  scope: string[];
  actor: "user" | "agent" | "tool" | "system";
  event_type: string;              // e.g. "user_message", "tool_call"
  payload: Record<string, unknown>;
  session_id?: string;             // agent-supplied
  run_id?: string;                 // agent-supplied
  created_at: string;
};
```

Common event types in v1: `user_message`, `agent_message`, `tool_call`, `tool_result`, `user_feedback`, `manual_memory_assertion`, `forget_memory`. Custom event types are allowed; the worker just won't extract memory from types it doesn't recognize.

---

## 4. How Agents Use Toffee

The integration loop is two calls.

**Before generating a response, the agent asks toffee what's relevant:**

```text
read_context(scope, query, lens, token_budget) → ContextPackage
```

The returned package contains memories grouped by kind, fitted to the requested token budget. The agent renders it into the prompt however it wants — most will prepend a markdown block.

**After each turn (user message in, agent message out, tool call, tool result), the agent records what happened:**

```text
append_event(scope, event_type, payload, ...) → EventId
```

The event lands durably and the agent gets back its ID immediately. Extraction and promotion to durable memory happen in the background.

That's the hot path. Two methods. A complete integration in Rust:

```rust
let toffee = toffee_client::connect()?;

let ctx = toffee.read_context(ReadContext {
    scope: vec!["project:magi".into()],
    query: user_message.clone(),
    lens: "default".into(),
    token_budget: 3000,
}).await?;

let prompt = format!("{}\n\n{}", ctx.render_markdown(), user_message);
let response = llm.complete(&prompt).await?;

toffee.append_event(EventInput {
    scope: vec!["project:magi".into()],
    actor: "user".into(),
    event_type: "user_message".into(),
    payload: json!({ "text": user_message }),
    ..Default::default()
}).await?;

toffee.append_event(EventInput {
    scope: vec!["project:magi".into()],
    actor: "agent".into(),
    event_type: "agent_message".into(),
    payload: json!({ "text": response }),
    ..Default::default()
}).await?;
```

Three additional methods exist for completeness:

- `record_feedback(memory_id, type)` — tell toffee a memory was helpful, wrong, or stale. Updates confidence.
- `search_memory(query, scope, ...)` — escape hatch for raw memory retrieval without lens shaping. Useful for debugging and unusual access patterns.
- `hello(client_name, client_version)` — optional handshake. Returns server version and supported methods. Lets a client check whether a newer method is available before calling it.

Administrative methods (`get_entity_page`, `forget_memory`, `inspect_provenance`, `worker_status`, daemon control) exist but are mainly used by the CLI.

---

## 5. The Wire

JSON-RPC 2.0 over a Unix domain socket at `$XDG_RUNTIME_DIR/toffee/toffeed.sock`, mode `0600`. Newline-delimited framing: one JSON object per line.

The Rust `toffee-client` crate wraps this. Other languages can speak JSON-RPC directly — the surface is small enough that a Python or TypeScript client is a weekend's work — or shell out to the CLI for one-shot use.

A complete `append_event` exchange:

```json
→ {"jsonrpc":"2.0","id":1,"method":"toffee.append_event","params":{
    "scope":["project:magi"],
    "actor":"user",
    "event_type":"user_message",
    "payload":{"text":"fix the parser bug"}}}
← {"jsonrpc":"2.0","id":1,"result":"evt_01HXABC..."}
```

The daemon also pushes notifications (no `id`, no response expected):

```text
toffee.memory.promoted          # a new durable memory landed
toffee.worker.lag_changed       # background worker is falling behind
```

Clients that don't care can ignore them.

### 5.1 Versioning

Method names are stable. Once `toffee.read_context` ships, its name doesn't change. Schema evolves additively — new optional fields don't bump anything; clients ignore fields they don't recognize. Breaking changes get new method names (`read_context_v2`). Clients negotiate via `hello.supported_methods`.

No protocol version negotiation. Same model Stripe and Slack use; it ages well.

### 5.2 Errors

Standard JSON-RPC error codes. Toffee-specific codes start at `-32100`. The list grows with real failures; not enumerated up front.

### 5.3 Auth

Filesystem permissions on the socket. Single-user by default. Multi-user or remote-access scenarios are explicitly out of scope for v1.

---

## 6. The Daemon

`toffeed` runs once per user. It auto-spawns on first client connection if not already running (same pattern as `gpg-agent` or `tmux`).

Three components run inside the process:

```text
   ┌─────────────────────────────────────────────────────┐
   │                       toffeed                        │
   │                                                       │
   │   Socket listener ─────► Connection tasks            │
   │                              │                        │
   │                              ▼                        │
   │   ┌──────────────┐    ┌─────────────────┐            │
   │   │ Event log    │◄───┤ Append (sync)   │            │
   │   │ (SQLite WAL) │    │                  │            │
   │   └──────┬───────┘    │ Read (sync)     │            │
   │          │            └─────────────────┘            │
   │          │                                            │
   │          ▼                                            │
   │   ┌──────────────────────────────────┐                │
   │   │ Background worker (mpsc queue)    │                │
   │   │ • extract candidate memories      │                │
   │   │ • resolve entities                 │                │
   │   │ • detect contradictions            │                │
   │   │ • update indexes                   │                │
   │   └──────────────────────────────────┘                │
   │                                                        │
   │   ┌──────────────┐    ┌──────────────┐                │
   │   │ Entity index │    │ Vector index │                │
   │   │ (SQLite)     │    │ (HNSW)       │                │
   │   └──────────────┘    └──────────────┘                │
   └─────────────────────────────────────────────────────┘
```

### 6.1 Write path

`append_event` is synchronous up to three steps:

1. Append to event log (SQLite, durable).
2. Update working memory for the active run.
3. Enqueue to the worker channel.

Then it returns. Target p99 latency under 5ms on local SSD.

Everything else — extraction, entity resolution, contradiction detection, index updates — happens in the background worker. If the agent crashes after `append_event` returns, the event is safe. If the worker crashes mid-extraction, it resumes from a checkpoint on restart and reprocesses.

### 6.2 Read path

`read_context` is synchronous and parallel:

1. Resolve the requested scope set (expand inherited scopes if configured).
2. Query the entity index, vector index, and recent-event index concurrently.
3. Rerank, deduplicate, fit to token budget.
4. Return the assembled package.

Target p99 latency under 100ms for a 3000-token package against local indexes.

Most of the latency budget is in vector retrieval and reranking. The entity index is fast (it's just SQL). v1's reranker is simple: scope match, confidence, recency, with light learned weights once we have feedback data.

### 6.3 Extraction

The background worker turns events into candidate memories using heuristics in v1. Patterns we extract on:

- Explicit user assertions: "remember that...", "my X is...", "I prefer...", "always/never...", "we decided..."
- Agent statements confirmed by the user.
- Repeated patterns across multiple events in a scope.

Conservative by default. False negatives (missed memory) are recoverable via the CLI; false positives (noisy memory) degrade retrieval quality and are harder to clean up.

Promotion to durable memory requires confidence above a threshold (default 0.7) and no contradiction with existing memory. If contradicted, the candidate becomes a `memory_conflict` row that surfaces to humans for resolution — toffee doesn't auto-resolve conflicting facts.

Model-assisted extraction is a future improvement; the heuristic baseline is the v1 floor.

### 6.4 Storage layout

```text
$XDG_DATA_HOME/toffee/
    toffee.db          SQLite — events, memories, entities, conflicts
    vectors/           HNSW index files
    models/            cached embedding model weights
    config.toml        user config

$XDG_RUNTIME_DIR/toffee/
    toffeed.sock       client socket
    toffeed.pid        daemon PID

$XDG_STATE_HOME/toffee/
    logs/              tracing output
    worker.checkpoint  last processed event id
```

Cross-platform paths follow OS conventions. macOS falls back to `~/Library/Application Support/toffee/` when XDG vars are unset.

---

## 7. The CLI

`toffee` is the human surface. Commands group by noun.

**Daemon:**
```text
toffee daemon start | stop | status | logs [-f]
toffee daemon rebuild-indexes
```

**Reading:**
```text
toffee context --scope project:foo --query "..." --lens default --budget 3000
toffee memory list --scope project:foo --kind decision
toffee memory show mem_<id>
toffee memory search "parser" --scope project:foo
toffee entity show project:foo
toffee conflict list --unresolved
toffee conflict resolve conf_<id> --pick mem_<id>
```

**Writing:**
```text
toffee memory add --kind claim --scope project:foo --text "..." --confidence 0.95
toffee memory feedback mem_<id> --type wrong --correction "..."
toffee memory forget mem_<id> --reason "user request"
toffee event append --type user_message --scope project:foo --payload @-
```

**Debug:**
```text
toffee why mem_<id>              # source events, confidence breakdown
toffee provenance ctxpkg_<id>    # which memories went into a context package and why
toffee worker status
```

Output is human-readable on a TTY, JSON with `--format json`.

---

## 8. Retrieval

Retrieval is hybrid: scope filter first, then entity anchoring, then vector similarity. Returning to the simple top-k-by-cosine pattern would be a regression.

### 8.1 What `read_context` returns

```typescript
type ContextPackage = {
  id: string;
  scope: string[];
  lens: string;
  query: string;
  claims: Memory[];
  decisions: Memory[];
  preferences: Memory[];
  episodes: Memory[];
  conflicts: MemoryConflict[];     // unresolved contradictions surfaced for visibility
  token_estimate: number;
  created_at: string;
};
```

The package is grouped by kind so the agent can render it sensibly. A typical markdown rendering puts decisions and preferences high (they're load-bearing for the agent's behavior), claims in the middle (factual grounding), and episodes lowest (background context).

### 8.2 Lenses

A lens is a named retrieval policy. v1 ships one: `default`. It includes all four memory kinds, filters by minimum confidence (0.6), prefers stable memories over recent ones, and splits the token budget roughly 40/30/20/10 across decisions/claims/preferences/episodes.

Additional lenses (planner, executor, reflection, final-response) are a v1.5 concern once we know what variants real agents actually need. Custom lenses are accepted from clients via the `customLens` field.

### 8.3 Vector index

v1 uses HNSW with post-filter-and-over-fetch for scope and entity constraints. The alternative — pre-filter — isn't well-supported by mature pure-Rust ANN libraries and trades index complexity for filter selectivity that doesn't matter at our index sizes. Revisit when a single scope exceeds ~100k embeddings.

Default embedding model: a small local model (BGE-small or MiniLM class) loaded once into the daemon. Runs on CPU; macOS uses Metal when available. ~100MB resident, ~10ms per embedding.

---

## 9. Lifecycle: Conflicts and Forgetting

### 9.1 Conflicts

When the worker promotes a candidate that contradicts an existing memory with the same subject and predicate, it doesn't pick a winner. It records a `memory_conflict` row containing all competing claims with their confidences and sources.

Conflicts surface in two places:
- `read_context` includes unresolved conflicts in the returned package so the agent sees them.
- `toffee conflict list --unresolved` shows them to the human.

Resolution is explicit: `toffee conflict resolve conf_<id> --pick mem_<id>` or `--merge "..."` or `--reject-all`. The chosen claim becomes durable; the others are superseded.

This matters because last-write-wins on semantic memory is wrong. If an agent learned the parser uses Pest in week 1 and the user said it switched to nom in week 3, both facts have provenance and the resolution should be deliberate.

### 9.2 Forgetting

`forget_memory` is a tombstone event. The memory is marked deleted; derived indexes drop it on next compaction.

The CLI exposes this:
```text
toffee memory forget mem_<id> --cascade memory_only|with_events
```

`memory_only` keeps the source events but removes the memory and any related index entries. `with_events` also tombstones the source events — useful when the user wants something gone entirely (a deleted credential, a private note).

Forgetting is irreversible by design. There is no undelete.

---

## 10. What's Not in v1

The following are explicitly deferred. Each is interesting; none is in the critical path of getting toffee usable.

- **Sync.** v1 is single-machine. No cross-device sync, no remote backup. The data model supports adding sync later (events are the source of truth; indexes are derived), but the wire protocol, conflict semantics, and HLC clocks for cross-client ordering are all left for v2.
- **Procedure memory.** Capturing reusable action patterns ("when symptom X appears, try steps Y, Z") is a natural extension once we have signal on which workflows recur. Schema and worker support land in v1.5 once retrieval and extraction baselines exist.
- **Compaction.** v1 doesn't aggressively dedupe or roll up old memories. Storage growth is bounded by the heuristic extractor being conservative. Real compaction (session summaries, entity page generation, salience decay) is a v2 concern.
- **Lens taxonomy.** v1 ships one lens. Real lens variants are designed around real agent needs.
- **ACP/MCP adapters.** Toffee can be wrapped by an ACP proxy to give editor-based coding agents memory without code changes, or exposed as an MCP server for agents that prefer tool-style access. Both are clean follow-ups that depend on the core being stable first.
- **Model-assisted extraction.** Heuristics are the floor; NLI-style entailment for contradiction detection and richer extraction patterns come later.
- **Multi-user / multi-tenant deployment.** Single user per daemon. Sharing memory across users is a different product.

---

## 11. Open Questions

1. **Scope inheritance.** When the agent asks for `project:foo`, should toffee also search `user:me` and `global` with a salience penalty? Probably yes, but the penalty needs tuning against real retrieval data.
2. **Embedding model packaging.** Ship weights with the binary (large download, works offline immediately) or lazy-download on first use (small binary, first-run network dependency)?
3. **Event payload size limit.** Tool results can be huge. Truncate, store-elsewhere, or accept large rows? Probably 64KB inline limit with overflow to side files.
4. **Multi-process write safety on SQLite.** WAL mode handles concurrent reads fine; concurrent writes from multiple toffeed instances would be a problem. Lock file at startup.
5. **What happens when an agent connects with no `scope`.** Reject the request, or fall back to `global`? Leaning reject — explicit is better than ambient.

---

## 12. Summary

Toffee is a memory layer for AI agents:

- A single **daemon** holds the durable store.
- Agents talk to it via **five JSON-RPC methods** (`hello`, `append_event`, `read_context`, `record_feedback`, `search_memory`) over a Unix socket.
- A **CLI** lets humans inspect, edit, and resolve conflicts.
- Memory is **typed** — claims, decisions, preferences, episodes — not a flat vector blob.
- **Events are the source of truth**; memories are derived asynchronously by a background worker.
- **Retrieval is hybrid**: scope filter, then entity anchoring, then vector similarity.
- **Conflicts are explicit**, not silently overwritten.
- **v1 is single-machine.** Sync, procedures, compaction, and protocol adapters are future work.

The first integrator-visible milestone: an agent author adds `toffee-client` to their `Cargo.toml`, calls `read_context` and `append_event`, and gets memory-augmented prompts. Everything before that is plumbing.
