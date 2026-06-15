use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use toffee_client::Client;

use crate::{format_for, stdout_is_tty, Effective, OutputFormat};

#[derive(Args, Debug)]
pub struct WorkerCmd {
    #[command(subcommand)]
    action: WorkerAction,
}

#[derive(Subcommand, Debug)]
enum WorkerAction {
    /// Show queue depth, lag, totals.
    Status,
    /// List recent worker failures (events that errored during extraction).
    Failures {
        /// Max results.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

pub async fn run(cmd: WorkerCmd, fmt: OutputFormat) -> Result<()> {
    let effective = format_for(fmt, stdout_is_tty());
    match cmd.action {
        WorkerAction::Status => status(effective).await,
        WorkerAction::Failures { limit } => failures(limit, effective).await,
    }
}

async fn status(fmt: Effective) -> Result<()> {
    let client = Client::connect().await.context("connect to daemon")?;
    let s = client.worker_status().await?;

    match fmt {
        Effective::Human => {
            println!("worker:           {}", s.worker_id);
            println!("queue_depth:      {}", s.queue_depth);
            println!(
                "lag_seconds:      {}",
                s.lag_seconds
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "n/a".into())
            );
            println!("events_total:     {}", s.events_total);
            println!("memories_active:  {}", s.memories_active);
            println!("unresolved_conflicts: {}", s.conflicts_unresolved);
            if let Some(id) = &s.last_processed_event_id {
                println!("last_processed:   {id}");
            }
            if let Some(ts) = s.last_processed_at {
                println!("                  at {ts}");
            }
            println!("recent_failures:  {}", s.failures_recent.len());
        }
        Effective::Json => {
            println!("{}", serde_json::to_string(&s)?);
        }
    }
    Ok(())
}

async fn failures(limit: usize, fmt: Effective) -> Result<()> {
    let client = Client::connect().await.context("connect to daemon")?;
    let s = client.worker_status().await?;
    let recent: Vec<_> = s.failures_recent.into_iter().take(limit).collect();

    match fmt {
        Effective::Human => {
            if recent.is_empty() {
                println!("(no recent failures)");
                return Ok(());
            }
            for f in &recent {
                println!(
                    "#{}  {}  event={}  retried={}",
                    f.id,
                    f.occurred_at,
                    f.event_id,
                    f.retried_at
                        .map(|t| t.to_string())
                        .unwrap_or_else(|| "no".into())
                );
                println!("    error: {}", f.error);
            }
        }
        Effective::Json => {
            println!("{}", serde_json::to_string(&recent)?);
        }
    }
    Ok(())
}
