# toffee

A local-first memory layer for AI agents.

Toffee runs as a single per-user daemon. Any number of agents connect to it, ask what's relevant before they act, and record what happened after. Memory is shared: if one agent learns the parser uses Pest, every other agent on your machine knows that too.

```text
   agent A ──┐
             │
   agent B ──┼──► toffeed ◄── toffee (CLI)
             │
   agent C ──┘
```

## Install

From source:

```bash
git clone <repo>
cd toffee
cargo build --release

# Put the binaries on your PATH (or copy them somewhere stable):
export PATH="$PWD/target/release:$PATH"

toffee daemon start
```

That's it — `toffeed` runs in the background, the socket lives at `$XDG_RUNTIME_DIR/toffee/toffeed.sock` (or `/tmp/toffee-<user>/...` if `XDG_RUNTIME_DIR` isn't set), and the database lives at `$XDG_DATA_HOME/toffee/toffee.db`.

On first start the daemon lazy-downloads the BGE-small-en-v1.5 sentence-transformer (~130 MB) into `$XDG_DATA_HOME/toffee/models/`. Subsequent starts load from disk. If you have no network access — or want a fully offline / deterministic setup — pass `--embedder hash`:

```bash
toffee daemon start -- --embedder hash
```

On Apple Silicon, build with `--features metal` for the Metal compute backend:

```bash
cargo build --release --features metal
```

## Try it from the CLI

```bash
# Tell toffee a few things.
toffee event append --type user_message --scope project:demo \
  --payload '{"text":"The parser uses Pest."}'
toffee event append --type user_message --scope project:demo \
  --payload '{"text":"We decided to go with Rust over Go."}'
toffee event append --type user_message --scope project:demo \
  --payload '{"text":"I prefer concise responses."}'

# Ask what it remembers.
toffee memory list --scope project:demo
# [decision]   conf=0.92  Decided: Rust over Go
# [claim]      conf=0.92  The parser uses Pest
# [preference] conf=0.92  User prefers concise responses

# Ask toffee what's relevant for a query (this is the agent's read path).
toffee context --scope project:demo --query "which parser library do we use" --markdown
```

That last command prints a markdown block your LLM agent can prepend to its prompt:

```markdown
## Memory

### Decisions
- Decided: Rust over Go

### Preferences
- User prefers concise responses

### Claims
- The parser uses Pest
```

## Use it from Rust

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
    // Auto-spawns toffeed if it isn't already running.
    let toffee = Client::connect().await?;

    let user_message = "fix the parser bug";

    // Before generating, ask toffee what's relevant.
    let ctx = toffee
        .read_context(
            vec!["project:magi".into()],
            user_message.into(),
            Some(3000), // token budget
        )
        .await?;

    let prompt = format!("{}\n\n{}", ctx.render_markdown(), user_message);
    let response = /* your LLM call */ String::from("ack");

    // Record both sides of the turn.
    toffee.append_event(EventInput {
        scope: Scope::new(["project:magi"]),
        actor: Actor::User,
        event_type: "user_message".into(),
        payload: serde_json::json!({ "text": user_message }),
        ..Default::default()
    }).await?;

    toffee.append_event(EventInput {
        scope: Scope::new(["project:magi"]),
        actor: Actor::Agent,
        event_type: "agent_message".into(),
        payload: serde_json::json!({ "text": response }),
        ..Default::default()
    }).await?;

    Ok(())
}
```

`append_event` returns in microseconds. Extraction (turning user messages into typed memories) happens in the daemon's background worker.

## How toffee thinks about memory

**Events** are the raw record: a user message, an agent reply, a tool call. Events are append-only and live forever.

**Memories** are typed, durable facts derived from events. There are four kinds:

| Kind | Example |
|---|---|
| **Claim** | "The parser uses Pest." |
| **Decision** | "We decided to go with Rust over Go." |
| **Preference** | "User prefers concise responses." |
| **Episode** | A narrative summary of something that happened. |

The daemon's worker turns events into memories asynchronously using heuristic patterns (RFC §6.3). It's conservative — false negatives are recoverable via `toffee memory add`; false positives are noisy and hard to undo.

**Scopes** are how toffee groups memory. A memory belongs to one or more scopes (`project:foo`, `user:me`, `repo:github.com/foo/bar`, `global`). When you ask for `project:foo`, toffee also considers `user:me` and `global` automatically.

**Conflicts** are explicit. If the worker learns "the parser uses Nom" when "the parser uses Pest" is already known, both stay active and a conflict row appears for you to resolve:

```bash
toffee conflict list
# conf_…  [unresolved]  parser/uses  competing=2

toffee conflict resolve conf_… --pick mem_…       # one wins
toffee conflict resolve conf_… --merge "…"        # author a merged claim
toffee conflict resolve conf_… --reject-all       # delete both
```

Toffee never silently overwrites a fact.

## CLI surface

```text
toffee daemon start | stop | status | logs [-f] | rebuild-indexes | prefetch-models
toffee event append --type T --scope S --payload @-|<json>
toffee memory list | show | search | feedback | add | forget
toffee entity list | show
toffee conflict list | show | resolve --pick|--merge|--reject-all
toffee context --scope S --query Q [--budget N] [--markdown]
toffee provenance ctxpkg_<id>
toffee worker status | failures
toffee why mem_<id>
```

Everything supports `--format json` for scripting.

## Status

v0.1 — the integrator surface is stable. Hot-path methods (`read_context`, `append_event`, `record_feedback`, `search_memory`, `hello`) won't change names; new optional fields may appear additively.

Two embedder backends ship: **BGE-small-en-v1.5** via candle (384-dim, the daemon default, lazy-downloaded on first start, optional Metal acceleration on macOS with `--features metal`) and a deterministic feature-hashing fallback (256-dim, fully offline, used in the test suite and selectable with `--embedder hash`). Embeddings are tagged with the active model name; after switching backends, run `toffee daemon rebuild-indexes` to re-embed memories under the new model.

Single-machine only in v1. Cross-device sync, procedure memory, compaction, and richer lens variants are v2 work.

For contributors, see [DEVELOPMENT.md](DEVELOPMENT.md).

## License

MIT. See [LICENSE-MIT](LICENSE-MIT).
