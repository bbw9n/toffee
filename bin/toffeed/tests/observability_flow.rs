//! Observability end-to-end: subscribe to notifications via the real
//! socket, verify worker_status + why_memory survive the round trip.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::json;
use tempfile::TempDir;
use toffee_client::{Client, ConnectOptions};
use toffee_core::{Actor, EventInput, Notification, Scope};

struct Daemon {
    child: Child,
    socket: PathBuf,
    _tmp: TempDir,
}

impl Daemon {
    fn spawn() -> Self {
        let tmp = tempfile::Builder::new()
            .prefix("toffeed-obs-")
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
        Daemon { child, socket, _tmp: tmp }
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

#[tokio::test]
async fn worker_status_and_why_memory_via_rpc() {
    let d = Daemon::spawn();
    let client = Client::connect_with(d.opts()).await.unwrap();

    let s = client.worker_status().await.unwrap();
    assert_eq!(s.queue_depth, 0);
    assert_eq!(s.events_total, 0);

    client
        .append_event(EventInput {
            scope: Scope::new(["project:obs"]),
            actor: Actor::User,
            event_type: "user_message".into(),
            payload: json!({"text": "The parser uses Pest."}),
            session_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    // Worker drains in <1s typically; give it 2 to be safe under CI load.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let s = client.worker_status().await.unwrap();
        if s.queue_depth == 0 && s.memories_active >= 1 {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("worker did not drain in time: status={:?}", s);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Pick the memory and ask the daemon to explain it.
    let memories = client
        .list_memories(toffee_rpc::ListMemoriesRequest {
            scope_any_of: Some(vec!["project:obs".into()]),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(!memories.is_empty());
    let report = client
        .why_memory(memories[0].id.clone())
        .await
        .unwrap();
    assert_eq!(report.memory.id, memories[0].id);
    assert!(!report.source_events.is_empty(), "expected at least one source event");
}

#[tokio::test]
async fn client_receives_memory_promoted_notification() {
    let d = Daemon::spawn();
    let client = Client::connect_with(d.opts()).await.unwrap();
    let mut rx = client.subscribe_notifications();

    client
        .append_event(EventInput {
            scope: Scope::new(["project:obs"]),
            actor: Actor::User,
            event_type: "user_message".into(),
            payload: json!({"text": "We decided to use Rust."}),
            session_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    // The notification should reach us within a couple of worker ticks.
    let n = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("notification should arrive")
        .expect("channel still open");
    match n {
        Notification::MemoryPromoted { kind, .. } => {
            assert_eq!(kind, toffee_core::MemoryKind::Decision);
        }
        other => panic!("expected MemoryPromoted, got {:?}", other),
    }
}
