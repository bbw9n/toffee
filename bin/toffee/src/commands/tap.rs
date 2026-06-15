use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Args;
use toffee_client::Client;
use toffee_tap::sink::ClientSink;
use toffee_tap::{
    Config, FileSourceConfig, Runner, RunnerOptions, SourceConfig, StdinSourceConfig,
};

use crate::OutputFormat;

#[derive(Args, Debug)]
pub struct TapCmd {
    /// Path to a TOML config file. If absent, the default pair
    /// (Claude Code + Codex with auto-scope) is used.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Override individual sources. Repeatable. Format: `claude_code`,
    /// `codex`, `stdin`, or `<kind>:<path>` for the file kinds, or
    /// `stdin:<prefix>` for the user-prefix variant. When `--from` is
    /// supplied, the `--config` file is ignored entirely.
    #[arg(long = "from")]
    from: Vec<String>,

    /// Override the scope on every source. Useful when piping a single
    /// pane: `toffee tap --from stdin:'> ' --scope project:magi`.
    #[arg(long = "scope")]
    scope: Vec<String>,
}

pub async fn run(cmd: TapCmd, _fmt: OutputFormat) -> Result<()> {
    let config = if !cmd.from.is_empty() {
        build_config_from_flags(&cmd.from, &cmd.scope)?
    } else if let Some(path) = &cmd.config {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read config {path:?}"))?;
        toffee_tap::Config::from_toml(&text).context("parse config TOML")?
    } else {
        let mut c = Config::default_pair();
        if !cmd.scope.is_empty() {
            apply_scope_override(&mut c, &cmd.scope);
        }
        c
    };

    let client = Client::connect()
        .await
        .context("connect to toffeed (run `toffee daemon start` first)")?;
    let sink = Arc::new(ClientSink::new(client));

    let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel::<()>(4);
    let signal_tx = shutdown_tx.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = signal_tx.send(());
    });

    let runner = Runner::build(
        RunnerOptions {
            config,
            one_shot: false,
        },
        sink,
    )
    .context("build runner")?;

    eprintln!("toffee tap: streaming agent transcripts → toffeed. Ctrl-C to stop.");
    runner
        .run(shutdown_rx)
        .await
        .context("runner exited with error")?;
    Ok(())
}

fn build_config_from_flags(from: &[String], scope: &[String]) -> Result<Config> {
    let mut sources = Vec::new();
    for spec in from {
        let (kind, arg) = split_kind(spec);
        let s = match kind {
            "claude_code" => SourceConfig::ClaudeCode(file_cfg(arg, scope)),
            "codex" => SourceConfig::Codex(file_cfg(arg, scope)),
            "stdin" => SourceConfig::Stdin(stdin_cfg(arg, scope)),
            other => anyhow::bail!("unknown --from kind: {other}"),
        };
        sources.push(s);
    }
    Ok(Config {
        registry: None,
        sources,
    })
}

fn split_kind(spec: &str) -> (&str, Option<&str>) {
    match spec.split_once(':') {
        Some((k, a)) if !a.is_empty() => (k, Some(a)),
        _ => (spec, None),
    }
}

fn file_cfg(arg: Option<&str>, scope: &[String]) -> FileSourceConfig {
    FileSourceConfig {
        path: arg.map(PathBuf::from),
        scope: if scope.is_empty() {
            None
        } else {
            Some(scope.to_vec())
        },
        scope_auto: scope.is_empty(),
        scope_fallback: vec!["global".into()],
    }
}

fn stdin_cfg(prefix: Option<&str>, scope: &[String]) -> StdinSourceConfig {
    StdinSourceConfig {
        user_prefix: prefix.map(|s| s.to_string()),
        scope: if scope.is_empty() {
            None
        } else {
            Some(scope.to_vec())
        },
        scope_fallback: vec!["global".into()],
    }
}

fn apply_scope_override(cfg: &mut Config, scope: &[String]) {
    for s in &mut cfg.sources {
        match s {
            SourceConfig::ClaudeCode(c) | SourceConfig::Codex(c) => {
                c.scope = Some(scope.to_vec());
                c.scope_auto = false;
            }
            SourceConfig::Stdin(c) => {
                c.scope = Some(scope.to_vec());
            }
        }
    }
}
