//! End-to-end integration: spawns a real `toffeed` and drives it from
//! `toffee-client` exactly the way the RFC §4 Rust example does.
//!
//! This is the Phase 4 integrator-milestone proof.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::json;
use tempfile::TempDir;
use toffee_client::{Client, ConnectOptions};
use toffee_core::{Actor, EventInput, Scope};

struct Daemon {
    child: Child,
    socket: PathBuf,
    _tmp: TempDir,
}

impl Daemon {
    fn spawn() -> Self {
        // Real Unix socket path; keep it short (macOS sun_path is 104 chars).
        let tmp = tempfile::Builder::new()
            .prefix("toffeed-it-")
            .tempdir_in("/tmp")
            .expect("tmpdir");
        let data = tmp.path().join("data");
        let socket = tmp.path().join("d.sock");
        std::fs::create_dir_all(&data).unwrap();

        let bin = env!("CARGO_BIN_EXE_toffeed");
        let mut child = Command::new(bin)
            .arg("--foreground")
            .arg("--socket")
            .arg(&socket)
            .arg("--db")
            .arg(data.join("toffee.db"))
            // Tests use the offline hash embedder so they don't pull BGE
            // weights over the network on every run.
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

        // Wait up to ~3s for the socket to appear.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !socket.exists() {
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                panic!("toffeed did not create socket in time");
            }
            std::thread::sleep(Duration::from_millis(25));
        }

        Daemon {
            child,
            socket,
            _tmp: tmp,
        }
    }

    fn connect_opts(&self) -> ConnectOptions {
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
async fn integrator_round_trip_read_context_after_append_event() {
    let d = Daemon::spawn();
    let client = Client::connect_with(d.connect_opts()).await.unwrap();

    // Sanity: hello.
    let info = client.hello("integration-test", "1.0").await.unwrap();
    assert_eq!(info.server_name, "toffeed");
    assert!(info.supported_methods.contains(&"toffee.read_context".into()));
    assert!(info.supported_methods.contains(&"toffee.append_event".into()));

    // Seed three turns.
    for text in [
        "The parser uses Pest.",
        "We decided to go with Rust over Go.",
        "I prefer concise responses.",
    ] {
        client
            .append_event(EventInput {
                scope: Scope::new(["project:magi"]),
                actor: Actor::User,
                event_type: "user_message".into(),
                payload: json!({"text": text}),
                session_id: None,
                run_id: None,
            })
            .await
            .unwrap();
    }

    // Give the worker time to drain. The worker's idle poll is 500ms;
    // sleep generously so this isn't flaky on slow CI.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // Now exercise the read path the way an agent would.
    let pkg = client
        .read_context(
            vec!["project:magi".into()],
            "which parser library are we using".into(),
            Some(3000),
        )
        .await
        .unwrap();

    assert_eq!(pkg.lens, "default");
    assert!(
        pkg.claims.iter().any(|m| m.text.contains("Pest")),
        "expected the Pest claim, got {:#?}",
        pkg.claims
    );
    assert!(!pkg.decisions.is_empty(), "expected the Rust decision");
    assert!(
        !pkg.preferences.is_empty(),
        "expected the concise-responses preference"
    );

    let md = pkg.render_markdown();
    assert!(md.contains("Pest"));
    assert!(md.contains("### Decisions"));

    // Provenance round-trip.
    let report = client.inspect_provenance(pkg.id.clone()).await.unwrap();
    assert_eq!(report.context_package_id, pkg.id);
    assert!(!report.entries.is_empty());
    for e in &report.entries {
        assert!(e.final_score > 0.0);
    }
}
