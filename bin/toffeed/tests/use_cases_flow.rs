//! Situation/behavior end-to-end tests — one per agent-facing use case
//! described in the README's "How toffee thinks about memory" framing.
//!
//! Each test spins up a real `toffeed` and drives it through `toffee-client`
//! the way an MCP-wrapped agent would. They're deliberately scenario-shaped
//! (not unit-shaped): the assertions are on user-visible outcomes
//! (memory survives, conflict surfaces, preference inherits) rather than
//! internal state.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::json;
use tempfile::TempDir;
use toffee_client::{Client, ConnectOptions};
use toffee_core::{Actor, EventInput, MemoryKind, Scope};
use toffee_rpc::{ListMemoriesRequest, ResolveConflictAction};

struct Daemon {
    child: Child,
    socket: PathBuf,
    _tmp: TempDir,
}

impl Daemon {
    fn spawn() -> Self {
        let tmp = tempfile::Builder::new()
            .prefix("toffeed-uc-")
            .tempdir_in("/tmp")
            .expect("tmpdir");
        let data = tmp.path().join("data");
        let socket = tmp.path().join("d.sock");
        std::fs::create_dir_all(&data).unwrap();
        let bin = env!("CARGO_BIN_EXE_toffeed");
        let child = Command::new(bin)
            .arg("--foreground")
            .arg("--socket")
            .arg(&socket)
            .arg("--db")
            .arg(data.join("toffee.db"))
            .arg("--embedder")
            .arg("hash")
            .env("XDG_DATA_HOME", tmp.path().join("xdg-data"))
            .env("XDG_RUNTIME_DIR", tmp.path().join("xdg-run"))
            .env("XDG_STATE_HOME", tmp.path().join("xdg-state"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn toffeed");
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !socket.exists() {
            if std::time::Instant::now() > deadline {
                panic!("toffeed did not create socket");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        Daemon {
            child,
            socket,
            _tmp: tmp,
        }
    }

    fn opts(&self) -> ConnectOptions {
        ConnectOptions {
            socket_path: Some(self.socket.clone()),
            auto_spawn: false,
            daemon_bin: None,
            connect_timeout: Duration::from_secs(2),
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn wait_for_drain(client: &Client, min_memories: i64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let s = client.worker_status().await.unwrap();
        if s.queue_depth == 0 && s.memories_active >= min_memories {
            return;
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "worker did not drain to >= {} memories in time: {:?}",
                min_memories, s
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn append_user_message(client: &Client, scope: Scope, text: &str) {
    client
        .append_event(EventInput {
            scope,
            actor: Actor::User,
            event_type: "user_message".into(),
            payload: json!({"text": text}),
            session_id: None,
            run_id: None,
        })
        .await
        .unwrap();
}

/// Use case 1 — Project conventions an agent would otherwise re-discover
/// every session. The agent learns "the parser uses Pest" in one session;
/// a brand-new client (mimicking a fresh session / different MCP-wrapped
/// model / IDE switch) sees the claim surface in `read_context`.
#[tokio::test]
async fn project_convention_survives_across_sessions() {
    let d = Daemon::spawn();

    // Session 1: drop a project convention into the daemon and disconnect.
    {
        let client = Client::connect_with(d.opts()).await.unwrap();
        append_user_message(
            &client,
            Scope::new(["project:my-api"]),
            "The parser uses Pest.",
        )
        .await;
        wait_for_drain(&client, 1).await;
    }

    // Session 2: a fresh connection (new session) asks a query whose answer
    // depends on the learned convention.
    let client = Client::connect_with(d.opts()).await.unwrap();
    let pkg = client
        .read_context(
            vec!["project:my-api".into()],
            "which parser library do we use".into(),
            Some(3000),
        )
        .await
        .unwrap();

    assert!(
        pkg.claims.iter().any(|m| m.text.contains("Pest")),
        "fresh-session read_context should surface the Pest claim; got {:#?}",
        pkg.claims
    );
    assert!(pkg.render_markdown().contains("Pest"));
}

/// Use case 2a — Architectural decisions are surfaced via the `Decisions`
/// bucket and weighted highest by the default lens. Uses `add_memory`,
/// which is the explicit "don't make me wait for the heuristic" path the
/// README documents.
#[tokio::test]
async fn explicit_decision_via_add_memory_surfaces_in_decisions_bucket() {
    let d = Daemon::spawn();
    let client = Client::connect_with(d.opts()).await.unwrap();

    let mem = client
        .add_memory(
            MemoryKind::Decision,
            Scope::new(["project:desktop"]),
            "Use Tauri over Electron. Rationale: smaller binary; users on metered connections."
                .into(),
            Some("desktop-framework".into()),
            Some("chosen".into()),
            Some("tauri".into()),
            Some(0.95),
        )
        .await
        .unwrap();
    assert_eq!(mem.kind, MemoryKind::Decision);

    let pkg = client
        .read_context(
            vec!["project:desktop".into()],
            "wrap this CLI in a desktop app".into(),
            Some(3000),
        )
        .await
        .unwrap();

    assert!(
        pkg.decisions.iter().any(|m| m.text.contains("Tauri")),
        "Tauri decision should land in decisions bucket; got {:#?}",
        pkg.decisions
    );
    let md = pkg.render_markdown();
    assert!(md.contains("### Decisions"));
    assert!(md.contains("Tauri"));
}

/// Use case 2b — A contradicting decision later doesn't silently overwrite
/// the prior one. The worker detects the SPO collision (same subject +
/// predicate, different object) and opens a conflict. `read_context` then
/// surfaces a `Conflicts` entry so the agent sees contested ground rather
/// than silently picking a side.
#[tokio::test]
async fn competing_decisions_open_conflict_then_resolve_via_pick() {
    let d = Daemon::spawn();
    let client = Client::connect_with(d.opts()).await.unwrap();
    let scope = Scope::new(["project:desktop"]);

    // First decision lands cleanly.
    append_user_message(&client, scope.clone(), "We decided to use Tauri.").await;
    wait_for_drain(&client, 1).await;

    // Contradicting decision — same SPO subject/predicate (we / decided),
    // different object — triggers the conflict detector.
    append_user_message(&client, scope.clone(), "We decided to use Electron.").await;
    wait_for_drain(&client, 2).await;

    // Conflict should surface in any read_context that includes this scope.
    let pkg = client
        .read_context(
            vec!["project:desktop".into()],
            "what desktop framework should I scaffold".into(),
            Some(3000),
        )
        .await
        .unwrap();
    assert_eq!(
        pkg.conflicts.len(),
        1,
        "expected exactly one open conflict, got {:#?}",
        pkg.conflicts
    );
    let conflict = &pkg.conflicts[0];
    assert_eq!(conflict.predicate.as_deref(), Some("decided"));
    assert!(conflict.competing_memory_ids.len() >= 2);
    assert!(pkg.render_markdown().contains("Open conflicts"));

    // Resolve by picking the Tauri side. Find the Tauri memory id.
    let memories = client
        .list_memories(ListMemoriesRequest {
            scope_any_of: Some(vec!["project:desktop".into()]),
            kind: Some(MemoryKind::Decision),
            ..Default::default()
        })
        .await
        .unwrap();
    let tauri = memories
        .iter()
        .find(|m| m.text.to_lowercase().contains("tauri"))
        .expect("tauri decision should exist as a memory");
    client
        .resolve_conflict(
            conflict.id.clone(),
            ResolveConflictAction::Pick {
                winner: tauri.id.clone(),
            },
        )
        .await
        .unwrap();

    let pkg2 = client
        .read_context(
            vec!["project:desktop".into()],
            "what desktop framework should I scaffold".into(),
            Some(3000),
        )
        .await
        .unwrap();
    assert!(
        pkg2.conflicts.is_empty(),
        "conflict should be resolved; got {:#?}",
        pkg2.conflicts
    );
}

/// Use case 3 — Personal preferences declared in one project follow you
/// to every project on the same machine via scope inheritance: a memory
/// in `user:me` automatically surfaces when an agent queries a different
/// project scope.
#[tokio::test]
async fn user_preference_inherits_into_other_project_queries() {
    let d = Daemon::spawn();
    let client = Client::connect_with(d.opts()).await.unwrap();

    // Stated once, in the user scope (mirrors --default-scope project:foo,user:me
    // where the extractor lands personal preferences in the broader scope).
    append_user_message(
        &client,
        Scope::new(["user:me"]),
        "I prefer concise responses.",
    )
    .await;
    wait_for_drain(&client, 1).await;

    // Different project, different day. expand_inherited adds user:me to the
    // requested scope set automatically.
    let pkg = client
        .read_context(
            vec!["project:totally-other-thing".into()],
            "refactor this helper".into(),
            Some(3000),
        )
        .await
        .unwrap();

    assert!(
        pkg.preferences
            .iter()
            .any(|m| m.text.to_lowercase().contains("concise")),
        "user:me preference should inherit into a project-scoped read_context; got {:#?}",
        pkg.preferences
    );
}

/// Use case 4 — Bug lore: "this isn't a bug, it's intentional." Stored as
/// a high-confidence claim. An agent later working on the same area sees
/// the claim surface in `read_context` because the vector path links the
/// memory text to the query.
#[tokio::test]
async fn bug_lore_claim_surfaces_for_related_query() {
    let d = Daemon::spawn();
    let client = Client::connect_with(d.opts()).await.unwrap();

    client
        .add_memory(
            MemoryKind::Claim,
            Scope::new(["project:my-api"]),
            "The upstream API returns 200 with empty body when rate-limited. \
             We swallow that and retry — do not turn it into an error."
                .into(),
            Some("rate-limit-handling".into()),
            Some("behavior".into()),
            Some("200-empty-body-is-intentional".into()),
            Some(0.95),
        )
        .await
        .unwrap();

    let pkg = client
        .read_context(
            vec!["project:my-api".into()],
            "refactor the rate-limited response handling".into(),
            Some(3000),
        )
        .await
        .unwrap();
    assert!(
        pkg.claims.iter().any(|m| m.text.contains("rate-limited")),
        "bug-lore claim should surface for a related refactor query; got {:#?}",
        pkg.claims
    );
}

/// Use case 5 — Cross-tool continuity within a single workday. Two
/// concurrently-open clients on the same daemon: client A writes, client B
/// reads after the worker drains. Mirrors morning-Claude-Code /
/// afternoon-Cursor sharing the local daemon via MCP.
#[tokio::test]
async fn two_clients_see_each_others_writes() {
    let d = Daemon::spawn();
    let client_a = Client::connect_with(d.opts()).await.unwrap();
    let client_b = Client::connect_with(d.opts()).await.unwrap();
    let scope = Scope::new(["project:shared"]);

    // Client A appends — the morning session, say.
    append_user_message(
        &client_a,
        scope.clone(),
        "We decided to split this into three PRs: schema, handler, UI.",
    )
    .await;
    // Drain via whichever client; the daemon is the same.
    wait_for_drain(&client_b, 1).await;

    // Client B — afternoon, different tool — asks about the plan and sees
    // the morning's decision.
    let pkg = client_b
        .read_context(
            vec!["project:shared".into()],
            "what's the PR plan".into(),
            Some(3000),
        )
        .await
        .unwrap();
    assert!(
        pkg.decisions.iter().any(|m| m.text.contains("three PRs")),
        "second client should see the first client's decision; got {:#?}",
        pkg.decisions
    );
}
