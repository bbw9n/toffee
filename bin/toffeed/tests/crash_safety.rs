//! Crash-safety: SIGKILL the daemon without giving it a chance to clean up,
//! then restart against the same data directory and confirm the durable
//! state (events, memories, worker checkpoint) survived.

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
}

impl Daemon {
    fn spawn(tmp: &TempDir) -> Self {
        let socket = tmp.path().join("d.sock");
        if socket.exists() {
            std::fs::remove_file(&socket).ok();
        }
        let bin = env!("CARGO_BIN_EXE_toffeed");
        let child = Command::new(bin)
            .arg("--foreground")
            .arg("--socket")
            .arg(&socket)
            .arg("--db")
            .arg(tmp.path().join("data").join("toffee.db"))
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
        Daemon { child, socket }
    }

    fn opts(&self) -> ConnectOptions {
        ConnectOptions {
            socket_path: Some(self.socket.clone()),
            auto_spawn: false,
            daemon_bin: None,
            connect_timeout: Duration::from_secs(2),
        }
    }

    fn kill_hard(&mut self) {
        // SIGKILL — no graceful shutdown path, no flush, no pid file
        // cleanup. The next start has to recover everything from the
        // on-disk store + lock-file behaviour alone.
        let pid = self.child.id() as i32;
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

fn build_tmp() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("toffeed-crash-")
        .tempdir_in("/tmp")
        .expect("tmpdir");
    std::fs::create_dir_all(tmp.path().join("data")).unwrap();
    tmp
}

#[tokio::test]
async fn events_survive_sigkill() {
    let tmp = build_tmp();
    let mut daemon = Daemon::spawn(&tmp);
    {
        let client = Client::connect_with(daemon.opts()).await.unwrap();
        for text in ["one", "two", "three"] {
            client
                .append_event(EventInput {
                    scope: Scope::new(["project:crash"]),
                    actor: Actor::User,
                    event_type: "user_message".into(),
                    payload: json!({"text": text}),
                    session_id: None,
                    run_id: None,
                })
                .await
                .unwrap();
        }
    }

    daemon.kill_hard();

    // Wait briefly to make sure the OS released the socket inode.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Restart and verify.
    let daemon2 = Daemon::spawn(&tmp);
    let client = Client::connect_with(daemon2.opts()).await.unwrap();
    let info = client.hello("crash-test", "1.0").await.unwrap();
    assert_eq!(info.event_count, 3, "expected three events to survive");
}

#[tokio::test]
async fn worker_resumes_from_checkpoint_after_sigkill() {
    let tmp = build_tmp();
    let mut daemon = Daemon::spawn(&tmp);
    {
        let client = Client::connect_with(daemon.opts()).await.unwrap();
        // Seed enough extractable events that at least one promotes.
        for text in [
            "The parser uses Pest.",
            "We decided to use Rust.",
            "I prefer concise responses.",
        ] {
            client
                .append_event(EventInput {
                    scope: Scope::new(["project:crash"]),
                    actor: Actor::User,
                    event_type: "user_message".into(),
                    payload: json!({"text": text}),
                    session_id: None,
                    run_id: None,
                })
                .await
                .unwrap();
        }
        // Wait for the worker to drain so we have memories on disk.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let s = client.worker_status().await.unwrap();
            if s.queue_depth == 0 && s.memories_active >= 1 {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("worker did not drain pre-kill: {:?}", s);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    daemon.kill_hard();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let daemon2 = Daemon::spawn(&tmp);
    let client = Client::connect_with(daemon2.opts()).await.unwrap();

    // Worker_status should report 0 queue (we drained pre-kill), memories
    // should still be there, and the vector index should be rehydrated.
    let s = client.worker_status().await.unwrap();
    assert_eq!(s.queue_depth, 0, "queue should be drained at restart");
    assert!(s.memories_active >= 1);
    assert!(s.last_processed_event_id.is_some(), "checkpoint should be intact");

    // Append a new event post-restart; it must flow through and produce a
    // memory.
    client
        .append_event(EventInput {
            scope: Scope::new(["project:crash"]),
            actor: Actor::User,
            event_type: "user_message".into(),
            payload: json!({"text": "My editor is Helix."}),
            session_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let before = s.memories_active;
    loop {
        let s2 = client.worker_status().await.unwrap();
        if s2.memories_active > before && s2.queue_depth == 0 {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("worker did not produce a new memory post-restart: {:?}", s2);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn sigkill_releases_pid_lock_for_next_start() {
    let tmp = build_tmp();
    let mut daemon = Daemon::spawn(&tmp);
    daemon.kill_hard();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // If the pid lock weren't released, the second `Daemon::spawn` would
    // panic on the socket-wait timeout because the second daemon would
    // exit before creating the socket.
    let _daemon2 = Daemon::spawn(&tmp);
}
