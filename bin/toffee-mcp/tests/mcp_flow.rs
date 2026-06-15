//! End-to-end: spawn `toffeed`, point `toffee-mcp` at it via stdio, then
//! drive an rmcp client through the wire to exercise the tool surface.
//!
//! This is the test the next agent integration depends on — if it breaks,
//! Claude Desktop / Cursor / Zed configs that point at `toffee-mcp` will
//! break too.

use std::path::PathBuf;
use std::process::{Command as StdCommand, Stdio};
use std::time::{Duration, Instant};

use rmcp::model::CallToolRequestParams;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::{object, ServiceExt};
use tempfile::TempDir;
use tokio::process::Command as TokioCommand;

/// Path to the binary cargo built for *this* test crate.
fn toffee_mcp_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_toffee-mcp"))
}

/// Path to `toffeed`. Cargo only sets `CARGO_BIN_EXE_*` for binaries in the
/// same package, so we synthesize the sibling path in the workspace target
/// directory and lazily build it on first run.
fn toffeed_bin() -> PathBuf {
    let target_dir = toffee_mcp_bin()
        .parent()
        .expect("toffee-mcp lives in target dir")
        .to_path_buf();
    let path = target_dir.join("toffeed");
    if !path.exists() {
        let status = StdCommand::new(env!("CARGO"))
            .args(["build", "-p", "toffeed", "--bin", "toffeed"])
            .status()
            .expect("invoke cargo build for toffeed");
        assert!(status.success(), "cargo build -p toffeed failed");
    }
    path
}

struct Daemon {
    child: std::process::Child,
    socket: PathBuf,
    _tmp: TempDir,
}

impl Daemon {
    fn start() -> Self {
        let tmp = TempDir::new().expect("tmpdir");
        let socket = tmp.path().join("run").join("toffeed.sock");
        let data = tmp.path().join("data");
        std::fs::create_dir_all(data.parent().unwrap()).ok();
        std::fs::create_dir_all(socket.parent().unwrap()).ok();
        std::fs::create_dir_all(&data).ok();

        let child = StdCommand::new(toffeed_bin())
            .arg("--socket")
            .arg(&socket)
            .arg("--db")
            .arg(data.join("toffee.db"))
            .arg("--embedder")
            .arg("hash")
            .env("XDG_DATA_HOME", tmp.path().join("xdg-data"))
            .env("XDG_RUNTIME_DIR", tmp.path().join("xdg-run"))
            .env("XDG_STATE_HOME", tmp.path().join("xdg-state"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn toffeed");

        let deadline = Instant::now() + Duration::from_secs(10);
        while !socket.exists() {
            if Instant::now() > deadline {
                panic!("toffeed socket never appeared at {socket:?}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        Daemon {
            child,
            socket,
            _tmp: tmp,
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
async fn mcp_tools_list_and_round_trip() -> anyhow::Result<()> {
    let daemon = Daemon::start();

    // Spawn toffee-mcp as a child process; rmcp's TokioChildProcess wires
    // stdin/stdout into the MCP transport for us.
    let socket = daemon.socket.clone();
    let mcp_bin = toffee_mcp_bin();
    let client = ()
        .serve(TokioChildProcess::new(
            TokioCommand::new(mcp_bin).configure(|cmd| {
                cmd.arg("--socket")
                    .arg(&socket)
                    .arg("--default-scope")
                    .arg("project:mcp_test")
                    .arg("--no-autospawn")
                    .stderr(Stdio::null());
            }),
        )?)
        .await?;

    // tools/list
    let tools = client.list_all_tools().await?;
    let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    for expected in [
        "read_context",
        "search_memory",
        "append_event",
        "add_memory",
        "record_feedback",
        "list_memories",
        "get_memory",
        "forget_memory",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "tool {expected} missing from list_tools: {names:?}"
        );
    }

    // Write: append an event the worker will turn into a memory.
    let append = client
        .call_tool(
            CallToolRequestParams::new("append_event").with_arguments(object!({
                "event_type": "user_message",
                "payload": { "text": "Decision: the parser uses Pest." },
                "actor": "user",
            })),
        )
        .await?;
    assert!(
        !append.is_error.unwrap_or(false),
        "append_event reported error: {append:#?}"
    );

    // Wait for the background worker to extract a memory. The hash embedder
    // path is in-process and fast, but extraction is async.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut memory_found = false;
    while Instant::now() < deadline {
        let listed = client
            .call_tool(CallToolRequestParams::new("list_memories").with_arguments(object!({})))
            .await?;
        let text = listed
            .content
            .iter()
            .filter_map(|c| c.as_text())
            .map(|t| t.text.as_str())
            .collect::<String>();
        if text.contains("Pest") {
            memory_found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        memory_found,
        "extractor never promoted a memory mentioning Pest"
    );

    // Read: assemble a context package and assert the memory shows up.
    let ctx = client
        .call_tool(
            CallToolRequestParams::new("read_context").with_arguments(object!({
                "query": "what parser library do we use?",
            })),
        )
        .await?;
    let body = ctx
        .content
        .iter()
        .filter_map(|c| c.as_text())
        .map(|t| t.text.as_str())
        .collect::<String>();
    assert!(
        body.contains("## Memory"),
        "context package should include the markdown header; got:\n{body}"
    );

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn mcp_rejects_call_without_scope() -> anyhow::Result<()> {
    let daemon = Daemon::start();
    let socket = daemon.socket.clone();
    let mcp_bin = toffee_mcp_bin();
    // No --default-scope this time; tool calls that omit `scope` must fail.
    let client = ()
        .serve(TokioChildProcess::new(
            TokioCommand::new(mcp_bin).configure(|cmd| {
                cmd.arg("--socket")
                    .arg(&socket)
                    .arg("--no-autospawn")
                    .stderr(Stdio::null());
            }),
        )?)
        .await?;

    let result = client
        .call_tool(
            CallToolRequestParams::new("read_context").with_arguments(object!({
                "query": "anything",
            })),
        )
        .await;
    match result {
        Ok(r) => assert!(
            r.is_error.unwrap_or(false),
            "expected tool to report is_error=true when scope is missing; got: {r:#?}"
        ),
        Err(_) => { /* protocol-level error is also acceptable */ }
    }

    client.cancel().await?;
    Ok(())
}
