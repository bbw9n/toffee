//! Latency benchmarks for the hot-path Runtime methods.
//!
//! These run against an in-memory store + the default HashEmbedder, so the
//! numbers are pure compute + SQLite (no IPC). They're representative of
//! the time the daemon spends per call, before the network/socket cost
//! that the integrator pays.

use std::sync::Arc;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use serde_json::json;
use toffee_core::{Actor, EventInput, Lens, Scope};
use toffee_runtime::read_path::ReadContextRequest;
use toffee_runtime::{Runtime, SearchParams};
use toffee_store::Store;

fn user_event(text: &str) -> EventInput {
    EventInput {
        scope: Scope::new(["project:bench"]),
        actor: Actor::User,
        event_type: "user_message".into(),
        payload: json!({"text": text}),
        session_id: None,
        run_id: None,
    }
}

const SAMPLE_TEXTS: &[&str] = &[
    "The parser uses Pest.",
    "We decided to go with Rust.",
    "I prefer concise responses.",
    "My editor is Helix.",
    "The web server runs on port 8080.",
    "We use Postgres for the primary store.",
    "I always write tests first.",
    "The team agreed to ship on Friday.",
    "The release branch tracks v0.2.",
    "Performance is a v2 concern.",
];

fn fresh_runtime() -> Runtime {
    Runtime::new(Arc::new(Store::open_in_memory().unwrap()))
}

/// Build a runtime pre-populated with `n` extracted memories.
fn populate(n: usize) -> Runtime {
    let runtime = fresh_runtime();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for i in 0..n {
        let text = SAMPLE_TEXTS[i % SAMPLE_TEXTS.len()];
        runtime.append_event(user_event(text)).unwrap();
    }
    rt.block_on(async {
        loop {
            let drained = runtime.drain_for_test().await.unwrap();
            if drained == 0 {
                break;
            }
        }
    });
    runtime
}

fn bench_append_event(c: &mut Criterion) {
    let runtime = fresh_runtime();
    c.bench_function("append_event", |b| {
        b.iter(|| {
            runtime.append_event(user_event("bench")).unwrap();
        });
    });
}

fn bench_search_memory(c: &mut Criterion) {
    let mut group = c.benchmark_group("search_memory");
    for n in [10usize, 100, 500] {
        let runtime = populate(n);
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let _ = runtime
                    .search_memory(SearchParams {
                        query: "parser library".into(),
                        scope_any_of: Some(vec!["project:bench".into()]),
                        limit: 20,
                        ..Default::default()
                    })
                    .unwrap();
            });
        });
    }
    group.finish();
}

fn bench_read_context(c: &mut Criterion) {
    let mut group = c.benchmark_group("read_context");
    for n in [10usize, 100, 500] {
        let runtime = populate(n);
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let _ = runtime
                    .read_context(ReadContextRequest {
                        scope: vec!["project:bench".into()],
                        query: "which parser library do we use".into(),
                        lens: Lens::default_lens(),
                        token_budget: 3000,
                    })
                    .unwrap();
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_append_event, bench_search_memory, bench_read_context);
criterion_main!(benches);
