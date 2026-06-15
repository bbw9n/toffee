# toffee-eval

Offline, deterministic **memory-quality** eval for toffee. It answers one
question with numbers instead of vibes: *did a change make memory better or
worse?* — which is the prerequisite for swapping the heuristic extractor for a
model-assisted one without regressing.

It scores two surfaces independently, because they fail independently:

| Surface | What it measures | Metrics |
|---|---|---|
| **Extraction** (`event → memory`) | Did the worker turn raw events into the right memories? | precision, recall, **F0.5**, per-field accuracy (kind / subject / predicate / object) |
| **Retrieval** (`query → context`) | Given the memories that exist, did the read path surface the right ones? | `search_memory`: MRR, Recall@10, nDCG@10 · `read_context`: package recall, bucket accuracy |

Extraction is **precision-weighted** (F-beta, β=0.5): a wrong memory poisons
retrieval and erodes trust, while a missed one is recoverable via
`toffee memory add`. That asymmetry is taken straight from the extractor's own
design note.

Everything runs in-process against an in-memory store and the **hash embedder**,
so the eval is fully offline and reproducible — the same property the daemon
end-to-end tests get from `--embedder hash`. Extraction calls the pure
`extract` function directly; retrieval seeds memories *verbatim* (bypassing
extraction) so the two numbers never contaminate each other.

It's not just a meter — it's a small control surface: **measure → tune → write
config → the daemon reflects it live**, with a `debug` view to see *why* any
single case scores how it does.

## Subcommands

### `run` — score the corpus (default)

```bash
cargo run -p toffee-eval                    # scorecard; non-zero exit on a missed gate
cargo run -p toffee-eval -- run --verbose   # list every failing/spurious case
cargo run -p toffee-eval -- run --json      # machine-readable, for trend tracking
cargo run -p toffee-eval -- run --config ~/.config/toffee/config.toml   # score under tuned weights
cargo run -p toffee-eval -- run --extraction my.jsonl --retrieval mine.jsonl
```

The `harness_smoke` test runs the shipped corpus through the default gate, so
`cargo test --workspace` (and therefore CI) fails on a memory-quality
regression.

### `tune` — fit the read-path weights to the retrieval corpus

Coordinate-descent over the four fusion weights (`vector` / `entity` /
`recency` / `confidence`) + the recency time-constant, using the **real read
path** as the objective (`0.5·MRR + 0.5·nDCG`), so a tuned config means exactly
what the daemon executes. The four weights are normalized to sum to 1 (ranking
is scale-invariant).

```bash
cargo run -p toffee-eval -- tune                     # report best-vs-baseline
cargo run -p toffee-eval -- tune --write             # persist to $XDG_CONFIG_HOME/toffee/config.toml
cargo run -p toffee-eval -- tune --write --out x.toml
```

`tune` refuses to fool itself: on a small corpus it flags that a gain is likely
overfit, and it warns when a weight is driven to 0 (e.g. the hash embedder's
weak vector signal) — that would hurt the real BGE read path. With the current
shipped corpus it honestly reports *no improvement*, which is the signal to add
adversarial cases before trusting a tune.

### `debug` — introspect one input

```bash
# What does the extractor produce for this line?
cargo run -p toffee-eval -- debug extract "We decided to use Tauri."

# Replay a retrieval case and show the per-signal score breakdown.
cargo run -p toffee-eval -- debug retrieve auth-multi
#   rel    final    vector entity recncy   conf  id
#   ★      0.3376    0.113    yes  1.000  0.900  a1
#   ★      0.2858    0.026    yes  1.000  0.900  a2
#          0.1608    0.026     no  1.000  0.900  a3
```

The retrieval breakdown comes straight from the `ProvenanceReport` the read
path already records — the same data behind `toffee why`.

## The scoring config

The read-path ranking weights live in `config.toml`, loaded by `toffeed` at
startup and **hot-reloaded** while it runs (it polls the file's mtime). So
`tune --write` followed by nothing — no restart — changes how a live daemon
ranks. The type is `toffee_core::ScoringConfig`; defaults match the weights that
shipped before tuning existed.

```toml
[scoring]
vector_weight = 0.6
entity_weight = 0.25
recency_weight = 0.10
confidence_weight = 0.05
recency_half_life_days = 14.0
```

A malformed edit is logged and ignored — the daemon keeps the last good
weights rather than falling over.

## The corpus

Two JSONL files under `corpus/`, one case per line (blank and `//` lines are
skipped). Append to grow them — the realistic distribution comes from TAP
captures of Claude Code / Codex sessions.

**`corpus/extraction.jsonl`** — an event and the memories it should produce.
An empty `expect` is a negative case (the extractor must stay silent). Fields
on an expected memory are all optional except `kind`; `text_contains` is the
loose match for when the worker paraphrases.

```json
{"name":"prefer-concise","input":"I prefer concise responses.","expect":[{"kind":"preference","subject":"user","predicate":"prefers","object":"concise responses"}]}
{"name":"neg-greeting","input":"hello world","expect":[]}
```

**`corpus/retrieval.jsonl`** — a seeded memory set, a query, and the
corpus-local ids that should surface. Seeded `text` must be unique within a
case (the harness maps a returned memory back to its id by text). A memory with
its own `scope` tests inheritance; multiple `relevant` ids exercise Recall@k /
nDCG.

```json
{"name":"parser-lib","scope":["project:demo"],"query":"which parser library do we use","memories":[{"id":"m1","kind":"claim","text":"The parser uses Pest"},{"id":"m2","kind":"claim","text":"The CI runs on GitHub Actions"}],"relevant":["m1"]}
```

## Thresholds

`Thresholds::default()` is the floor a healthy v1 should clear, not an
aspiration — raise it as the corpus grows and hardens. The current shipped
corpus is curated to today's behavior and clears the gate with headroom; the
next move is to add **adversarial** cases (near-duplicate distractors,
paraphrase, contradicting decisions) so the metrics stop saturating at 1.0 and
start discriminating between extractor versions.
