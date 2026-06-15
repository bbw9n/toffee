use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde_json::json;
use toffee_client::Client;
use toffee_core::{ConflictId, MemoryId};
use toffee_rpc::ResolveConflictAction;

use crate::{format_for, stdout_is_tty, Effective, OutputFormat};

#[derive(Args, Debug)]
pub struct ConflictCmd {
    #[command(subcommand)]
    action: ConflictAction,
}

#[derive(Subcommand, Debug)]
enum ConflictAction {
    /// List conflicts. By default, only unresolved.
    List {
        /// Include already-resolved conflicts too.
        #[arg(long = "all")]
        all: bool,
    },
    /// Show one conflict.
    Show { id: String },
    /// Resolve a conflict. Exactly one of `--pick`, `--merge`, or
    /// `--reject-all` must be supplied.
    Resolve {
        id: String,
        /// Pick this competing memory as the winner; the others are
        /// superseded.
        #[arg(long, group = "resolution")]
        pick: Option<String>,
        /// Author a merged memory; every competing memory is superseded by
        /// the new one.
        #[arg(long, group = "resolution")]
        merge: Option<String>,
        /// SPO and confidence overrides for `--merge`.
        #[arg(long, requires = "merge")]
        subject: Option<String>,
        #[arg(long, requires = "merge")]
        predicate: Option<String>,
        #[arg(long, requires = "merge")]
        object: Option<String>,
        #[arg(long, requires = "merge")]
        confidence: Option<f64>,
        /// Soft-delete every competing memory.
        #[arg(long = "reject-all", group = "resolution")]
        reject_all: bool,
    },
}

pub async fn run(cmd: ConflictCmd, fmt: OutputFormat) -> Result<()> {
    let effective = format_for(fmt, stdout_is_tty());
    match cmd.action {
        ConflictAction::List { all } => list(all, effective).await,
        ConflictAction::Show { id } => show(id, effective).await,
        ConflictAction::Resolve {
            id,
            pick,
            merge,
            subject,
            predicate,
            object,
            confidence,
            reject_all,
        } => {
            resolve(
                id, pick, merge, subject, predicate, object, confidence, reject_all, effective,
            )
            .await
        }
    }
}

async fn list(all: bool, fmt: Effective) -> Result<()> {
    let client = Client::connect().await.context("connect to daemon")?;
    let conflicts = client.list_conflicts(all).await?;

    match fmt {
        Effective::Human => {
            if conflicts.is_empty() {
                println!("(no conflicts)");
                return Ok(());
            }
            for c in &conflicts {
                println!(
                    "{}  [{}]  {}/{}  competing={}  scope={}",
                    c.id,
                    c.resolution.as_str(),
                    c.subject.as_deref().unwrap_or("?"),
                    c.predicate.as_deref().unwrap_or("?"),
                    c.competing_memory_ids.len(),
                    c.scope.as_slice().join(",")
                );
            }
        }
        Effective::Json => {
            println!("{}", serde_json::to_string(&conflicts)?);
        }
    }
    Ok(())
}

async fn show(id: String, fmt: Effective) -> Result<()> {
    let client = Client::connect().await.context("connect to daemon")?;
    let conflict = client.get_conflict(ConflictId(id)).await?;

    match fmt {
        Effective::Human => {
            println!("conflict:   {}", conflict.id);
            println!("resolution: {}", conflict.resolution.as_str());
            println!("scope:      {}", conflict.scope.as_slice().join(", "));
            println!("subject:    {}", conflict.subject.as_deref().unwrap_or("(none)"));
            println!("predicate:  {}", conflict.predicate.as_deref().unwrap_or("(none)"));
            println!("created:    {}", conflict.created_at);
            if let Some(t) = conflict.resolved_at {
                println!("resolved:   {t}");
            }
            println!("competing memories:");
            for m in &conflict.competing_memory_ids {
                // Fetch each to show object + confidence; ignore failures.
                match client.get_memory(m.clone()).await {
                    Ok(memory) => {
                        println!(
                            "  {}  conf={:.2}  obj={}  text={}",
                            memory.id,
                            memory.confidence,
                            memory.object.as_deref().unwrap_or("?"),
                            truncate(&memory.text, 60)
                        );
                    }
                    Err(_) => println!("  {} (could not fetch)", m),
                }
            }
        }
        Effective::Json => {
            println!("{}", serde_json::to_string(&conflict)?);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn resolve(
    id: String,
    pick: Option<String>,
    merge: Option<String>,
    subject: Option<String>,
    predicate: Option<String>,
    object: Option<String>,
    confidence: Option<f64>,
    reject_all: bool,
    fmt: Effective,
) -> Result<()> {
    let action = match (pick, merge, reject_all) {
        (Some(winner), None, false) => ResolveConflictAction::Pick {
            winner: MemoryId(winner),
        },
        (None, Some(text), false) => ResolveConflictAction::Merge {
            text,
            subject,
            predicate,
            object,
            confidence,
        },
        (None, None, true) => ResolveConflictAction::RejectAll,
        (None, None, false) => {
            anyhow::bail!("specify exactly one of --pick, --merge, or --reject-all")
        }
        _ => anyhow::bail!("specify exactly one of --pick, --merge, or --reject-all"),
    };
    let client = Client::connect().await.context("connect to daemon")?;
    let conflict = client.resolve_conflict(ConflictId(id), action).await?;

    match fmt {
        Effective::Human => {
            println!(
                "{}  -> {}",
                conflict.id,
                conflict.resolution.as_str()
            );
        }
        Effective::Json => {
            println!(
                "{}",
                json!({
                    "conflict_id": conflict.id.as_str(),
                    "resolution": conflict.resolution.as_str(),
                    "resolved_at": conflict.resolved_at,
                })
            );
        }
    }
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut buf: String = s.chars().take(max).collect();
        buf.push('…');
        buf
    }
}
