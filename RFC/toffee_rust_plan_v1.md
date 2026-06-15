# Toffee: Rust Implementation Plan

**Status:** Draft 1.0
**Companion to:** Toffee RFC v1.0
**Scope:** How toffee gets built in Rust.

---

## 1. Shape

Two binaries, one library, one workspace.

```text
toffeed         # daemon
toffee          # CLI
toffee-client   # library agents embed
```

`toffeed` is long-lived and owns the SQLite database, vector index, and embedding model. `toffee` is the CLI humans use. `toffee-client` is what Rust agents add to their `Cargo.toml`. All three speak the same JSON-RPC wire to the daemon.

---

## 2. Workspace Layout

```text
toffee/
  Cargo.toml                    # workspace root
  crates/
    toffee-core/                # pure types, no IO, no async
      src/
        event.rs
        memory.rs
        scope.rs
        conflict.rs
        context_package.rs
    toffee-store/               # SQLite-backed persistence
      src/
        lib.rs
        schema.rs
        migrations/
        events.rs
        memories.rs
        entities.rs
        conflicts.rs
    toffee-vector/              # embedding + HNSW
      src/
        lib.rs
        hnsw.rs
        embedder.rs
        post_filter.rs
    toffee-runtime/             # orchestration: read path, write path, worker
      src/
        lib.rs
        read_path.rs
        write_path.rs
        worker.rs
        extractor.rs
        promoter.rs
        entity_resolver.rs
        conflict_detector.rs
    toffee-rpc/                 # JSON-RPC wire types + server
      src/
        lib.rs
        framing.rs
        methods.rs
        server.rs
    toffee-client/              # async client for agents
      src/
        lib.rs
        connect.rs
        methods.rs
  bin/
    toffeed/
      src/main.rs
    toffee/
      src/
        main.rs
        commands/
```

The crate split is the design. A few rules:

- **`toffee-core` has no I/O and no async.** Pure types, pure functions, plus the formulas (confidence, scope expansion). This is what makes the rest testable.
- **`toffee-store` is the only crate that imports `rusqlite`.**
- **`toffee-vector` is the only crate that imports `hnsw_rs` and the embedding runtime.**
- **`toffee-runtime` orchestrates.** It depends on `toffee-core`, `toffee-store`, `toffee-vector` and exposes a single `Runtime` facade.
- **`toffee-rpc` defines wire types** (`AppendEventRequest`, `ContextPackageResponse`, etc.) and the server-side dispatcher. Reused by both `toffeed` and `toffee-client`.
- **`toffee-client`** is a thin async client that calls `toffee-rpc` types over the socket. The public crate agents depend on.
- **`bin/` crates are wiring.** No business logic.

---

## 3. Dependencies

```toml
[workspace.dependencies]
# Async
tokio = { version = "1", features = ["full"] }
tokio-util = { version = "0.7", features = ["codec"] }
futures = "0.3"

# Storage
rusqlite = { version = "0.32", features = ["bundled", "json"] }
r2d2 = "0.8"
r2d2_sqlite = "0.25"

# Vector / embedding
hnsw_rs = "0.3"
candle-core = "0.7"
candle-transformers = "0.7"
tokenizers = "0.20"

# Serialization
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# CLI
clap = { version = "4", features = ["derive", "env"] }

# Observability
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }

# IDs
ulid = "1"

# Misc
chrono = { version = "0.4", features = ["serde"] }
anyhow = "1"
thiserror = "1"
parking_lot = "0.12"
```

Notable choices:

- **No JSON-RPC framework.** The wire surface is five agent methods plus a handful of admin ones. A hand-rolled dispatcher is ~300 lines, has no dependencies, and lets us evolve the framing without library constraints. Revisit if we add HTTP or WebSocket transports.
- **`candle` for embeddings.** Pure Rust, runs on CPU and Metal, no Python/libtorch. Slower than ONNX Runtime but the dependency story is much cleaner.
- **`hnsw_rs` for vector search.** Mature pure-Rust HNSW. Worth benchmarking `instant-distance` once we have realistic data.
- **`rusqlite` with `bundled`.** Ships SQLite source; one less system dependency.

---

## 4. Daemon Internals

### 4.1 Concurrency model

```text
   ┌────────────────────────────────────────────────────────┐
   │                       tokio runtime                     │
   │                                                          │
   │   Socket listener ──► one task per connection           │
   │                              │                           │
   │                              ▼                           │
   │                  RPC dispatcher (toffee-rpc)            │
   │                              │                           │
   │              ┌───────────────┼───────────────┐          │
   │              │               │               │           │
   │              ▼               ▼               ▼           │
   │       write_path        read_path       admin methods  │
   │              │               │                          │
   │              ▼               ▼                          │
   │   ┌──────────────────────────────────┐                  │
   │   │ Shared state (all Send + Sync)   │                  │
   │   │  • r2d2 SQLite pool              │                  │
   │   │  • Arc<HnswIndex>                │                  │
   │   │  • Arc<Embedder>                 │                  │
   │   │  • mpsc::Sender<Event>           │                  │
   │   └──────────────────────────────────┘                  │
   │                              │                           │
   │                              ▼                           │
   │                    Background worker task              │
   │                    (mpsc::Receiver<Event>)             │
   └────────────────────────────────────────────────────────┘
```

All shared types are `Send + Sync`. Ordinary `tokio::spawn` works everywhere. No `LocalSet`, no thread pinning.

### 4.2 Write path: latency budget

`append_event` p99 target is 5ms on local SSD. Where the time goes:

| Step | Time |
|---|---|
| JSON parse | ~10µs |
| ULID generate | ~1µs |
| SQLite INSERT (WAL, NORMAL) | 1-3ms |
| Working memory upsert | ~0.5ms |
| `mpsc::Sender::send` to worker | <10µs |
| JSON response | ~10µs |

Typical: 2-4ms. Margin is small. If we miss the budget, the only real lever is batching: group multiple appends into one transaction at the cost of latency variance. Avoid until measurements demand it.

### 4.3 Worker backpressure

The worker channel is bounded (default 1000). If the worker falls behind, `append_event` does *not* block — the event is durably written, and the worker drains at its own pace, reading from the DB if the in-memory channel is full.

This is correct because:
- The event is already durable. Worker work is derived.
- On worker restart, it replays from `worker_state.last_processed_event_id`.
- A bursty agent doesn't slow other agents' reads (which don't go through the worker).

### 4.4 Read path

`read_context` queries three indexes concurrently:

```text
   ┌─ entity index ──────┐
   │                     │
   ├─ vector index ──────┼──► rerank ──► token-budget compile ──► response
   │                     │
   └─ recent event index ┘
```

Each is independent. Total latency is bounded by the slowest, not the sum. Target p99 100ms for a 3000-token package.

The vector index dominates: ~5ms for a 5×over-fetched HNSW query at top-k=20, plus ~10ms to embed the query string. Entity and recent-event are SQL — sub-millisecond.

### 4.5 Daemon lifecycle

`toffeed` auto-spawns when a client connects and no daemon is running. The client double-checks the socket exists, dials it, retries with exponential backoff for ~2 seconds while the daemon comes up.

PID file at `$XDG_RUNTIME_DIR/toffee/toffeed.pid`. Socket at `$XDG_RUNTIME_DIR/toffee/toffeed.sock`, mode `0600`.

Shutdown flushes the worker queue, fsyncs SQLite, releases the socket, exits. `toffee daemon stop` is the friendly path; SIGTERM does the same.

If two `toffeed` instances try to start, the second sees the lock file held and exits silently. SQLite is opened in WAL mode so concurrent readers are fine, but two writers would corrupt; the lock prevents it.

---

## 5. Storage Schema

SQLite, WAL mode, `synchronous=NORMAL`. Migrations are forward-only, versioned by integer.

```sql
CREATE TABLE events (
  id TEXT PRIMARY KEY,
  scope_json TEXT NOT NULL,
  session_id TEXT,
  run_id TEXT,
  actor TEXT NOT NULL,
  event_type TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  created_at TEXT NOT NULL
);
CREATE INDEX idx_events_run ON events(run_id);
CREATE INDEX idx_events_created ON events(created_at);

CREATE TABLE memories (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  scope_json TEXT NOT NULL,
  subject TEXT,
  predicate TEXT,
  object TEXT,
  text TEXT NOT NULL,
  confidence REAL NOT NULL,
  source_event_ids_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  superseded_by TEXT,
  deleted_at TEXT,
  CHECK (
    kind NOT IN ('claim','decision','preference')
    OR (subject IS NOT NULL AND predicate IS NOT NULL AND object IS NOT NULL)
  )
);
CREATE INDEX idx_memories_kind ON memories(kind);
CREATE INDEX idx_memories_subject ON memories(subject, predicate);
CREATE INDEX idx_memories_active ON memories(deleted_at, superseded_by);

CREATE TABLE entities (
  id TEXT PRIMARY KEY,
  entity_type TEXT NOT NULL,
  name TEXT NOT NULL,
  aliases_json TEXT,
  summary TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  deleted_at TEXT
);
CREATE INDEX idx_entities_type ON entities(entity_type);
CREATE INDEX idx_entities_name ON entities(name);

CREATE TABLE memory_entities (
  memory_id TEXT NOT NULL,
  entity_id TEXT NOT NULL,
  PRIMARY KEY (memory_id, entity_id)
);
CREATE INDEX idx_memory_entities_entity ON memory_entities(entity_id);

CREATE TABLE embeddings (
  id TEXT PRIMARY KEY,
  memory_id TEXT NOT NULL,
  scope_json TEXT NOT NULL,
  kind TEXT NOT NULL,
  model TEXT NOT NULL,
  vector_ref TEXT NOT NULL,        -- offset into HNSW file
  created_at TEXT NOT NULL
);
CREATE INDEX idx_embeddings_memory ON embeddings(memory_id);

CREATE TABLE conflicts (
  id TEXT PRIMARY KEY,
  scope_json TEXT NOT NULL,
  subject TEXT,
  predicate TEXT,
  competing_memory_ids_json TEXT NOT NULL,
  resolution TEXT NOT NULL DEFAULT 'unresolved',
  created_at TEXT NOT NULL,
  resolved_at TEXT
);
CREATE INDEX idx_conflicts_unresolved ON conflicts(resolution) WHERE resolution = 'unresolved';

CREATE TABLE worker_state (
  worker_id TEXT PRIMARY KEY,
  last_processed_event_id TEXT,
  last_processed_at TEXT
);
```

Notable absences from earlier RFC drafts:

- No HLC columns. v1 is single-machine; standard timestamps and ULIDs are enough.
- No `procedure_memories` table. v1 doesn't ship procedures.
- No `context_packages` cache table. v1 doesn't cache packages; per-index result caches are in-memory.
- No `sync_state` table. v1 is single-machine.

These can be added later without breaking the v1 schema.

---

## 6. The RPC Surface

Five agent-facing methods, plus admin:

```text
toffee.hello              → ServerInfo
toffee.append_event       → EventId
toffee.read_context       → ContextPackage
toffee.record_feedback    → ()
toffee.search_memory      → [Memory]

# Admin (mostly CLI):
toffee.get_entity_page          → EntityPage
toffee.forget_memory            → ()
toffee.inspect_provenance       → ProvenanceReport
toffee.worker_status            → WorkerStatus
toffee.daemon.shutdown          → ()
toffee.daemon.rebuild_indexes   → JobId
```

Wire types live in `toffee-rpc`. They're flat structs derived with `serde`, no clever generics. Each method has a `Request` and `Response` type with the same name, plus an enum `Method` for dispatch.

### 6.1 Server

In `toffee-rpc::server`:

```rust
pub trait Handler: Send + Sync + 'static {
    async fn hello(&self, req: HelloRequest) -> Result<ServerInfo, Error>;
    async fn append_event(&self, req: AppendEventRequest) -> Result<EventId, Error>;
    async fn read_context(&self, req: ReadContextRequest) -> Result<ContextPackage, Error>;
    async fn record_feedback(&self, req: RecordFeedbackRequest) -> Result<(), Error>;
    async fn search_memory(&self, req: SearchMemoryRequest) -> Result<Vec<Memory>, Error>;
    // ...
}

pub async fn serve<H: Handler>(
    listener: UnixListener,
    handler: Arc<H>,
) -> Result<()> { ... }
```

`toffeed` provides a `Handler` impl backed by `toffee-runtime::Runtime`. Tests provide mock handlers.

### 6.2 Client

In `toffee-client`:

```rust
pub struct Client { /* ... */ }

impl Client {
    pub async fn connect() -> Result<Self> { /* auto-spawn daemon if needed */ }
    pub async fn connect_to(socket_path: &Path) -> Result<Self> { /* explicit */ }

    pub async fn hello(&self, name: &str, version: &str) -> Result<ServerInfo>;
    pub async fn append_event(&self, req: EventInput) -> Result<EventId>;
    pub async fn read_context(&self, req: ReadContext) -> Result<ContextPackage>;
    pub async fn record_feedback(&self, memory_id: &str, kind: FeedbackKind) -> Result<()>;
    pub async fn search_memory(&self, req: SearchMemory) -> Result<Vec<Memory>>;

    pub fn subscribe_notifications(&self) -> impl Stream<Item = Notification>;
}
```

API matches the RPC methods 1:1 in v1. Ergonomic builders can come later if friction shows up.

Reconnection is transparent: if the daemon restarts mid-session, the client redials. The session_id is agent-supplied and stable, so the daemon picks up where it left off from the agent's perspective.

---

## 7. Background Worker

In `toffee-runtime::worker`:

```rust
pub async fn run(
    mut rx: mpsc::Receiver<Event>,
    store: Arc<Store>,
    vector: Arc<VectorIndex>,
) -> Result<()> {
    while let Some(event) = rx.recv().await {
        if let Err(e) = process_event(&event, &store, &vector).await {
            tracing::warn!(event_id = %event.id, error = ?e, "worker failure");
            store.record_worker_failure(&event.id, &e).await.ok();
            // continue; failure of one event doesn't block the queue
        }
        store.checkpoint_worker(&event.id).await?;
    }
    Ok(())
}

async fn process_event(
    event: &Event,
    store: &Store,
    vector: &VectorIndex,
) -> Result<()> {
    let candidates = extractor::extract(event, store).await?;
    for cand in candidates {
        let entities = entity_resolver::resolve(&cand, store).await?;
        let mut cand = cand.with_entities(entities);
        cand.confidence = scalars::compute_confidence(&cand);

        if let Some(conflict) = conflict_detector::find(&cand, store).await? {
            store.record_conflict(&cand, &conflict).await?;
            continue;
        }

        match promoter::decide(&cand) {
            Decision::Discard => continue,
            Decision::Episode => {
                store.save_episode(&cand).await?;
            }
            Decision::Promote => {
                let memory = store.upsert_memory(&cand).await?;
                vector.index(&memory).await?;
            }
        }
    }
    Ok(())
}
```

Semantics:

- **At-least-once.** All store and index operations are idempotent (upsert by id).
- **Crash recovery.** On restart, the worker replays from `worker_state.last_processed_event_id`. Events beyond the checkpoint may be re-processed; idempotency makes this safe.
- **Per-event failure isolation.** A failed extraction logs to `worker_failures` and doesn't block subsequent events.

The extractor is heuristic in v1. Patterns we recognize (from RFC §6.3):

- `"remember (that)? ..."`, `"my X is ..."`, `"I prefer ..."`, `"always|never"`, `"we decided"`, `"the project uses"`, `"let's use"`, `"don't"`
- Agent statements immediately followed by user confirmation (`yes`, `correct`, `right`, `that works`)
- Repeated SPO triples across ≥3 events in a scope

False negatives are recoverable via `toffee memory add`. False positives degrade retrieval; the conservative threshold matters.

---

## 8. CLI

`toffee` is a thin clap-based binary that calls `toffee-client`. Commands map roughly 1:1 onto RPC methods plus daemon control.

Layout:

```rust
// bin/toffee/src/main.rs

#[derive(Parser)]
enum Cli {
    Daemon(DaemonCmd),
    Context(ContextCmd),
    Memory(MemoryCmd),
    Event(EventCmd),
    Entity(EntityCmd),
    Conflict(ConflictCmd),
    Provenance(ProvenanceCmd),
    Worker(WorkerCmd),
    Why { memory_id: String },
}
```

Each subcommand is one file under `bin/toffee/src/commands/`. Output formatting is a shared utility — TTY-aware by default, `--format json` flag for scripts.

Notable: `toffee daemon start` is the only command that *doesn't* go through the RPC. It directly spawns `toffeed` and waits for the socket to appear. Everything else dials the existing daemon.

---

## 9. Rollout

Eight phases. Each one is independently shippable and produces something verifiable end-to-end, even if it's not yet useful to integrators.

### Phase 0 — Event log

Build:
- `toffee-core::Event` + `toffee-core::Scope`
- `toffee-store` with `events` table, append + replay
- `toffee-rpc` skeleton with `toffee.hello` and `toffee.append_event`
- `toffeed` daemon that listens on the socket
- `toffee daemon` commands and `toffee event append`

Done when: A client can append events and they survive a restart. `toffee daemon status` reports uptime.

### Phase 1 — Memory items and the worker

Build:
- `toffee-core::Memory` with kind/scope/SPO
- `memories` and `conflicts` tables
- Heuristic extractor and promoter in `toffee-runtime::worker`
- Confidence formula in `toffee-core::scalars`
- `toffee.record_feedback` RPC
- `toffee memory list/show/feedback/add/forget` CLI

Done when: Events get extracted into memories asynchronously. Feedback adjusts confidence.

### Phase 2 — Entity index

Build:
- `entities` and `memory_entities` tables
- Entity resolver (heuristic: longest-match against known names)
- Scope inheritance
- `toffee.get_entity_page` RPC
- `toffee entity` CLI commands

Done when: Memories link to canonical entities. Entity pages render.

### Phase 3 — Vector index

Build:
- `toffee-vector` crate
- HNSW index on disk
- Embedder (candle + BGE-small or MiniLM)
- Post-filter-and-over-fetch retrieval
- `toffee.search_memory` RPC
- `toffee memory search` CLI

Done when: Vector search returns scope-filtered results. p99 latency under 50ms for top-20.

### Phase 4 — `read_context` and the client library

Build:
- `toffee-runtime::read_path` (parallel index queries, rerank, compile)
- `default` lens
- `toffee.read_context` RPC
- `toffee.inspect_provenance` RPC
- `toffee context` and `toffee provenance` CLI
- **`toffee-client` v0.1 published**

Done when: An agent author can `cargo add toffee-client`, call `read_context` and `append_event`, and get memory-augmented prompts. **This is the first integrator-visible milestone.** Everything before is plumbing.

### Phase 5 — Conflict UX

Build:
- Conflict detection in the worker
- `toffee conflict list/show/resolve` CLI
- Conflicts surfaced in `ContextPackage`

Done when: Contradicting facts don't silently overwrite. Users can resolve.

### Phase 6 — Observability

Build:
- `toffee.worker_status` RPC with queue depth and lag
- Notifications (`toffee.memory.promoted`, `toffee.worker.lag_changed`)
- `toffee worker status/failures` CLI
- `toffee why <mem_id>` showing confidence breakdown

Done when: An operator can debug a misbehaving daemon without reading source.

### Phase 7 — Hardening

Build:
- Latency benchmarks in CI
- Property tests on `toffee-core`
- Integration tests against a real daemon
- macOS Metal acceleration for embeddings (opt-in)
- Embedding model lazy-download with `toffee daemon prefetch-models`
- Crash-safety tests (kill during write, kill during worker, ensure recovery)

Done when: We trust toffee enough to put real memory in it.

---

## 10. Testing Strategy

**Unit.** `toffee-core` is pure functions: confidence math, scope expansion, lens application. Property tests via `proptest` for the formulas.

**Integration.** `tests/` at workspace level spins up `Runtime` against an in-memory SQLite (`:memory:`) and exercises end-to-end flows without IPC. Validates extract → promote → retrieve cycles, conflict detection, forgetting.

**IPC.** `toffeed` + `toffee` CLI tested via `assert_cmd`. Start daemon with a temp `XDG_*` root; run CLI commands; assert behavior. Serial.

**Client.** `toffee-client` against a real daemon, including reconnection and notification delivery.

**Latency.** `cargo bench` measures p50/p99 of `append_event` and `read_context` against a populated store. CI fails on >20% regression from baseline.

**Retrieval quality.** Deferred to Phase 4. Build a small labeled corpus of (query, scope, expected memory IDs) for regression testing once we have real-ish data.

---

## 11. Open Questions

1. **Hand-rolled JSON-RPC vs `jsonrpsee`.** Hand-rolled in v1. Revisit if the surface grows or we add HTTP/WebSocket transports.
2. **Embedding model packaging.** Ship with binary (large, offline) or lazy-download on first run (small, online dependency)? Leaning lazy-download with a `prefetch-models` command.
3. **Scope inheritance defaults.** Always include `user:me` and `global` when a project scope is requested? Probably yes with a salience penalty, but the penalty value needs real data.
4. **Reconnection ergonomics.** Should `toffee-client` retry transparently on daemon restart, or surface the disconnection? Transparent retry is easier on agents but hides bugs. Leaning transparent for v1, with a flag to opt out.
5. **`toffee-client` API shape.** Match RPC method names 1:1, or build ergonomic wrappers? v1 is 1:1.
6. **Windows support.** Defer until someone asks. Unix socket portability is the friction point.

---

## 12. Summary

Toffee in Rust:

- Three artifacts: `toffeed` (daemon), `toffee` (CLI), `toffee-client` (library).
- Seven crates in a workspace, layered so the pure types are at the bottom and only the daemon binary touches everything.
- JSON-RPC over a Unix socket. Hand-rolled wire, no framework.
- SQLite + HNSW + candle for the on-disk and vector stack.
- Eight rollout phases. The integrator-visible milestone is Phase 4: `cargo add toffee-client` and have memory work.

What's *not* in v1: sync, procedure memory, compaction, lens taxonomy, protocol adapters (ACP/MCP), multi-user. Each is a clean follow-up.
