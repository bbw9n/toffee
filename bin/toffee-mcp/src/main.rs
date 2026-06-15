//! MCP server that wraps `toffeed` so any MCP-aware agent (Claude Desktop,
//! Cursor, Zed, …) gets shared per-machine memory by editing one config
//! file.
//!
//! Wire: JSON-RPC 2.0 over stdio, framed by the `rmcp` SDK. Stdout is
//! reserved for the protocol; **all** human-facing diagnostics must go to
//! stderr (the `tracing` subscriber below is configured accordingly).
//!
//! Architecture: this binary owns no storage. It connects to the local
//! `toffeed` daemon over its Unix socket via `toffee-client`, forwards each
//! tool call, and renders the result as MCP `Content::text`. The daemon
//! does the work; this is a translator.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use tracing_subscriber::EnvFilter;

mod server;
mod tools;

use server::ToffeeMcp;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "toffee-mcp",
    version,
    about = "Toffee memory exposed as an MCP server (stdio transport)"
)]
struct Args {
    /// Default scopes used when a tool call omits `scope`. Repeat the flag
    /// or pass a comma list. When unset, calls without `scope` are rejected
    /// with a clear error — explicit is better than ambient.
    #[arg(
        long = "default-scope",
        env = "TOFFEE_DEFAULT_SCOPE",
        value_delimiter = ',',
        num_args = 0..,
    )]
    default_scope: Vec<String>,

    /// Override the toffeed socket path. Defaults to the standard location
    /// (`$XDG_RUNTIME_DIR/toffee/toffeed.sock` or the per-user `/tmp` fallback).
    #[arg(long, env = "TOFFEE_SOCKET")]
    socket: Option<PathBuf>,

    /// Disable auto-spawning `toffeed` if it isn't already running.
    /// Useful in tests where the daemon is managed externally.
    #[arg(long, env = "TOFFEE_NO_AUTOSPAWN")]
    no_autospawn: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // CRITICAL: stderr only. Stdout is the MCP wire.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("TOFFEE_LOG").unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(false)
        .try_init()
        .ok();

    let args = Args::parse();
    tracing::info!(
        socket = ?args.socket,
        default_scope = ?args.default_scope,
        "toffee-mcp starting"
    );

    let server = ToffeeMcp::new(Arc::new(args));
    let service = server
        .serve(stdio())
        .await
        .context("serve MCP server over stdio")?;
    service.waiting().await.context("MCP service exited")?;
    Ok(())
}
