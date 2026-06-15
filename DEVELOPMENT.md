# Development guide

This is the contributor's tour: how the workspace is laid out, where to put a change, and how to verify it.

The companion documents are [`toffee_rfc_v1.md`](toffee_rfc_v1.md) (the design rationale) and [`toffee_rust_plan_v1.md`](toffee_rust_plan_v1.md) (the implementation plan that drove phases 0–7). When in doubt about why something is shaped a certain way, read the RFC first.

---

## Workspace layout

```text
toffee/
├── crates/
│   ├── toffee-core         # pure types and pure functions, no I/O
│   ├── toffee-store        # SQLite persistence (only crate that imports rusqlite)
│   ├── toffee-vector       # embedder trait + HNSW (only crate that imports hnsw_rs)
│   ├── toffee-runtime      # write path, read path, background worker
│   ├── toffee-rpc          # JSON-RPC wire types + server
│   └── toffee-client       # async client that agents add to their Cargo.toml
└── bin/
    ├── toffeed             # daemon binary
    ├── toffee              # CLI for humans
    └── toffee-mcp          # MCP server that wraps toffeed over stdio
```

The split is the design — each layer can only depend on layers below it. A few load-bearing rules:

- **`toffee-core` has no I/O and no async.** Pure types, pure functions, formulas. This is what makes everything above it testable in isolation. Add types here; add `proptest` properties to `crates/toffee-core/tests/properties.rs`.
- **`toffee-store` is the only crate that imports `rusqlite`.** All SQL lives here. Migrations are forward-only, versioned by integer (see [Schema migrations](#schema-migrations)).
- **`toffee-vector` is the only crate that imports `hnsw_rs`.** All ANN and embedding code lives here. Use the `Embedder` trait to add backends; see [Embedder backends](#embedder-backends).
- **`toffee-runtime` orchestrates.** It depends on core + store + vector. The `Runtime` struct is the facade the daemon and tests both use.
- **`bin/` crates are wiring.** No business logic. The daemon's `Handler` impl just unwraps RPC requests and calls `Runtime` methods inside `spawn_blocking`. `toffee-mcp` is a thin translator from MCP tool calls to `toffee-client` calls — it owns no storage and forwards everything to a running `toffeed`.

---

## Build, test, bench

```bash
# Build everything (debug).
cargo build --workspace

# Build release binaries.
cargo build --workspace --release

# Build with Metal acceleration for the BGE embedder (macOS / Apple Silicon).
# The feature forwards through bin/toffeed → toffee-runtime → toffee-vector.
cargo build --workspace --release --features metal

# Run all tests.
cargo test --workspace

# Run latency benchmarks.
cargo bench --bench runtime -p toffee-runtime

# A faster bench cycle for iteration:
cargo bench --bench runtime -p toffee-runtime -- --measurement-time 2 --warm-up-time 1
```

### Pre-commit formatting hook

CI gates on `cargo fmt --all -- --check`. A repo-tracked pre-commit hook runs the
same check locally so drift never reaches CI. Enable it once per clone (the
`core.hooksPath` setting is local, not committed):

```bash
git config core.hooksPath .githooks
```

It blocks a commit with unformatted code and tells you to run `cargo fmt --all`;
bypass a single commit with `git commit --no-verify`.

There are 100+ tests across the workspace. The big buckets:

| Suite | Lives in | What it covers |
|---|---|---|
| Unit tests | per-crate `src/*/tests` mods | Per-module logic in isolation. |
| Property tests | `crates/toffee-core/tests/properties.rs` | Scalar formulas, scope expansion, token estimation under random inputs. |
| Worker pipeline | `crates/toffee-runtime/tests/worker_pipeline.rs` | Event → memory + entities + vector index. |
| Read path | `crates/toffee-runtime/tests/read_path.rs` | `read_context` bucketing, lens, provenance. |
| Conflicts | `crates/toffee-runtime/tests/conflicts.rs` | Dedup + each resolution mode. |
| Observability | `crates/toffee-runtime/tests/observability.rs` | Worker status, failure tracking, notifications. |
| Daemon round-trip | `bin/toffeed/tests/integrator_flow.rs` | A real `toffeed` spawned and driven via `toffee-client`. |
| Notifications over the wire | `bin/toffeed/tests/observability_flow.rs` | `toffee.memory.promoted` delivered through the real socket. |
| Crash safety | `bin/toffeed/tests/crash_safety.rs` | SIGKILL mid-write, mid-worker, pid-lock release. |
| MCP round-trip | `bin/toffee-mcp/tests/mcp_flow.rs` | Spawn `toffeed`, drive `toffee-mcp` over stdio via an `rmcp` client, exercise tools/list + tools/call. |

Run a single suite with `cargo test -p <crate> --test <name>`.

The end-to-end daemon tests under `bin/toffeed/tests/` spawn `toffeed` with `--embedder hash` so CI never pulls BGE weights over the network. If you add a new daemon-spawning test, copy that flag — letting it default to `bge` will hit hf-hub on every run.

---

## Latency targets

From RFC §6, currently met by 2–3 orders of magnitude:

| Operation | Target (p99) | Measured median (in-process, hash embedder) |
|---|---|---|
| `append_event` | 5 ms | ~7.6 µs |
| `search_memory` (top-20, 100 memories) | 50 ms | ~170 µs |
| `read_context` (3000-token budget, 500 memories) | 100 ms | ~1.14 ms |

CLI / IPC adds ~5–10 ms on top.

Reproduce with `cargo bench --bench runtime -p toffee-runtime`. If a regression > 20% appears, that's a real issue.

---

## Evaluating memory quality

Latency tells you the daemon is fast; it says nothing about whether the
memories are any *good*. That's what `crates/toffee-eval` measures, and it's
the gate you run before swapping the heuristic extractor for anything smarter.

```bash
cargo run -p toffee-eval                 # scorecard (non-zero exit on a missed gate)
cargo run -p toffee-eval -- run --verbose   # every failing / spurious case
cargo run -p toffee-eval -- tune            # fit the read-path weights to the corpus
cargo run -p toffee-eval -- debug retrieve <case>   # per-signal score breakdown
```

It scores two surfaces separately, because they fail separately:

- **Extraction** (`event → memory`) — runs the pure `extract` function over a
  labeled corpus. precision / recall / **F0.5** (precision-weighted, per the
  extractor's own asymmetry note) plus per-field accuracy.
- **Retrieval** (`query → context`) — seeds a fresh in-process runtime with a
  known memory set, then scores `search_memory` (MRR / Recall@k / nDCG@k) and
  `read_context` (package recall, bucket accuracy).

Both run offline against the hash embedder, deterministically (memories are
seeded with staggered timestamps so the read path's score tie-break is total —
otherwise `tune` would chase HashMap-order noise). The corpus is two JSONL
files under `crates/toffee-eval/corpus/`; append to grow them. The
`harness_smoke` test runs the shipped corpus through the default gate, so
`cargo test --workspace` fails on a memory-quality regression.

**`tune` and the scoring config.** The read-path ranking weights in
`combined_score` are no longer hardcoded — they live in `toffee_core::ScoringConfig`,
loaded by `toffeed` from `config.toml` and **hot-reloaded** while it runs (mtime
poll). `tune` coordinate-descents those weights against the retrieval corpus and
`--write` persists them, so `tune --write` retunes a live daemon with no
restart. See [`crates/toffee-eval/README.md`](crates/toffee-eval/README.md) for
the corpus format, subcommands, and config schema.

---

## How to make common changes

### Schema migrations

Migrations live in `crates/toffee-store/src/schema.rs` as a `&[(version: i64, sql: &str)]` array. They run in array order, each only applied if its version is above the current `schema_version` row.

Adding a migration is the only thing that needs the array — everything downstream works off `Store` methods.

```rust
// crates/toffee-store/src/schema.rs
const MIGRATIONS: &[(i64, &str)] = &[
    // ... existing ones ...
    (6, r#"
        CREATE TABLE my_new_table ( ... );
        CREATE INDEX ...;
    "#),
];
```

Then add the CRUD methods in a sibling module (e.g. `crates/toffee-store/src/my_new_table.rs`), register it in `lib.rs`, and write unit tests against `Store::open_in_memory()`.

### Adding an RPC method

The method lands in five places. There's no codegen — it's hand-rolled JSON-RPC because the surface is small.

1. **Wire type** in `crates/toffee-rpc/src/methods.rs`. Define `XxxRequest` / `XxxResponse`. Add a constant to `mod method_names` and include it in `method_names::all()` (this is what `toffee.hello` reports as supported).
2. **Export** the new types from `crates/toffee-rpc/src/lib.rs`.
3. **`Handler` trait** in `crates/toffee-rpc/src/server.rs`. Add the async method signature. Add the dispatch arm in `dispatch()`.
4. **Runtime method** in `crates/toffee-runtime/src/lib.rs`. The actual logic. Tests against an in-memory `Runtime`.
5. **Daemon handler** in `bin/toffeed/src/main.rs`. Wrap the runtime call in `spawn_blocking` and convert errors via `map_runtime_err`.
6. **Client wrapper** in `crates/toffee-client/src/lib.rs`. A thin async method that calls `self.call(method_names::XXX, req)`.
7. **CLI subcommand** in `bin/toffee/src/commands/` if the surface is user-facing.

For methods that mutate state, also consider whether the worker should emit a [`Notification`](#notifications) afterward.

### Adding a tool to `toffee-mcp`

The MCP server lives in `bin/toffee-mcp/` and is built with `rmcp`. Each tool is a method on `ToffeeMcp` annotated with `#[tool]`; the `#[tool_router]` macro on the `impl` block wires them into the JSON-RPC dispatch table, and the `#[tool_handler]` macro on the `ServerHandler` impl plumbs the router into the rmcp service.

To add a tool that forwards a new `toffee-client` method:

1. **Input type** in `bin/toffee-mcp/src/tools.rs`. Derive `Deserialize` and `rmcp::schemars::JsonSchema`. Use `String` for typed IDs (`MemoryId`, `EventId`, …) — those wrappers don't implement `JsonSchema` and the macro needs a schema.
2. **Tool method** in `bin/toffee-mcp/src/server.rs` inside the `#[tool_router] impl ToffeeMcp` block:
   ```rust
   #[tool(description = "...")]
   async fn my_tool(&self, Parameters(args): Parameters<MyArgs>) -> Result<CallToolResult, McpError> {
       let client = self.client().await?;
       let out = call_with_retry(self, |c| { let args = args.clone(); async move { c.my_method(args).await } }, client).await?;
       Ok(CallToolResult::success(vec![Content::text(/* render */ )]))
   }
   ```
   Wrap the client call in `call_with_retry` so a stale connection auto-reconnects once. If the tool needs a `scope`, use `self.resolve_scope(args.scope)?` to fall back to `--default-scope`.
3. **Test** by adding an arm to `bin/toffee-mcp/tests/mcp_flow.rs` — `client.call_tool(CallToolRequestParams::new("my_tool").with_arguments(object!({...})))`. The fixture spawns a real `toffeed` with `--embedder hash` so the test stays offline.

The MCP server intentionally exposes only the **agent-facing** subset of the RPC surface (read / search / append / add / forget / list / get / record_feedback). Human-in-the-loop operations — `resolve_conflict`, `worker_status`, `inspect_provenance` — are deliberately CLI-only; an autonomous agent should not be silently picking conflict winners.

**Output stream discipline.** Stdout is the MCP wire — anything else written there corrupts the protocol. The `tracing` subscriber in `main.rs` is pinned to stderr; do not change that, and avoid `println!` / `dbg!` in tool implementations.

### Adding a memory kind

If you want a fifth memory kind (e.g. `procedure`):

1. Extend `MemoryKind` in `crates/toffee-core/src/memory.rs`. Add the `as_str` / `parse` entries.
2. Update `Lens::default_lens()` (`crates/toffee-core/src/context.rs`) with a budget weight.
3. Update the `ContextPackage` markdown rendering if the new kind should appear in a specific section order.
4. Update the schema `CHECK` constraint in migration 2 only if the SPO requirement changes — but at this point you'd add a new migration that relaxes / tightens the CHECK, never edit a past migration.
5. Add an extractor pattern in `crates/toffee-runtime/src/extractor.rs` if it should land automatically.

### Notifications

Daemon → client notifications go through `Runtime::emit(Notification::...)`. Each connection task subscribes (`Handler::notification_subscriber`) and writes them out as JSON-RPC notification frames.

To add a new notification variant:

1. Extend `Notification` in `crates/toffee-core/src/observability.rs`. The `#[serde(tag = "method", content = "params", rename_all = "snake_case")]` envelope makes the wire shape derive automatically.
2. Emit it from the runtime via `inner.emit(Notification::YourNew { … })`.

Clients pick it up via `Client::subscribe_notifications()` → `broadcast::Receiver<Notification>`.

### Embedder backends

Two backends ship today. Both implement the `Embedder` trait in `crates/toffee-vector/src/embedder.rs` and produce L2-normalised vectors so the HNSW index can treat cosine similarity as dot product regardless of which one is active.

```rust
pub trait Embedder: Send + Sync + 'static {
    fn model(&self) -> &str;
    fn dim(&self) -> usize;
    fn embed(&self, text: &str) -> Result<Vec<f32>>;
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> { … }
}
```

| Backend | `model()` tag | `dim()` | Notes |
|---|---|---|---|
| `BgeEmbedder` (`crates/toffee-vector/src/bge.rs`) | `bge-small-en-v1.5` | 384 | candle + tokenizers, CLS-pooled. Lazy-downloads weights via `hf-hub` on first construction; `metal` cargo feature opts into the Metal backend on macOS. |
| `HashEmbedder` (`crates/toffee-vector/src/embedder.rs`) | `hash-feature-v1` | 256 | Feature-hashed bag-of-tokens. Offline, deterministic, ~zero runtime cost. |

The daemon picks one at startup from `--embedder bge|hash` (env: `TOFFEE_EMBEDDER`, default `bge`):

```rust
// bin/toffeed/src/main.rs
let embedder: Arc<dyn Embedder> = match args.embedder {
    EmbedderChoice::Bge => Arc::new(BgeEmbedder::new(&cache_dir)?),
    EmbedderChoice::Hash => Arc::new(HashEmbedder::default()),
};
let vector = Arc::new(VectorIndex::new(embedder.dim()));
let runtime = Runtime::with_components(store, embedder, vector);
```

Embeddings are tagged with the active `model()` string in SQLite. The runtime's `load_index_from_store()` filters by the current model name on startup, so vectors written under a different backend are skipped (not deleted). After switching backends, run `toffee daemon rebuild-indexes` to re-embed the active memory set under the new model.

Adding a third backend: implement `Embedder`, extend `EmbedderChoice` in `bin/toffeed/src/main.rs`, and pick a unique `model()` string so the cross-model filtering stays correct.

**Where weights live.** `BgeEmbedder::new(cache_dir)` uses `cache_dir` as the hf-hub cache root. The daemon defaults this to `$XDG_DATA_HOME/toffee/models/` (via `paths::models_dir()`); override with `--models-dir` or `TOFFEE_MODELS_DIR`. The `toffee daemon prefetch-models` CLI remains a stub — first-start lazy download is the supported path today.

---

## How the daemon recovers from a crash

Three things guarantee crash safety:

1. **The event log is the source of truth.** Events are written through `INSERT` in a single statement before `append_event` returns. SQLite WAL + `synchronous=NORMAL` makes that durable across hard kill.
2. **The worker keeps a checkpoint.** After processing each event the worker updates `worker_state.last_processed_event_id`. On startup the worker reads the checkpoint and picks up at the next event. At-most-once durability + idempotent inserts mean re-processing one or two events is harmless.
3. **The vector index rehydrates from SQLite.** On startup, `toffeed` reads every embedding row for the active model and re-inserts them into the in-memory HNSW. The HNSW file format is not used today; the rebuild is fast enough at the scale the plan targets.

The pid lock is acquired via `flock(LOCK_EX | LOCK_NB)` on `$XDG_RUNTIME_DIR/toffee/toffeed.pid`. SIGKILL releases the lock at the kernel level, so the next start can claim it cleanly.

`bin/toffeed/tests/crash_safety.rs` verifies all three.

---

## Phase history

Toffee was built in eight phases following `toffee_rust_plan_v1.md`:

| Phase | Milestone |
|---|---|
| 0 | Event log + daemon socket + minimal CLI. |
| 1 | Memory items + heuristic worker + record_feedback. |
| 2 | Entity index + scope inheritance. |
| 3 | Vector index (HNSW + hash embedder) + search_memory. |
| 4 | **`read_context` and `toffee-client` v0.1.** The first integrator-visible milestone. |
| 5 | Conflict UX (dedup + pick / merge / reject-all). |
| 6 | Observability (worker status, notifications, `toffee why`). |
| 7 | Hardening (property tests, criterion benches, crash-safety tests). |

Anything new should land in a similar incremental style: a small spike with tests at each layer, then the wire / CLI surface on top.

---

## Style

- **Idiomatic Rust, conservative on dependencies.** New crates need to justify themselves.
- **No comments that say what the code does.** Use comments only for the *why* — a non-obvious invariant, a workaround, a deliberate trade-off.
- **Tests live next to the thing they test.** Unit tests in `#[cfg(test)] mod tests` blocks; integration tests in `tests/` directories.
- **Errors propagate.** `Result` everywhere; `thiserror` for typed errors; `anyhow` only in binaries.
- **Async is `tokio`.** Blocking work goes through `spawn_blocking`.
