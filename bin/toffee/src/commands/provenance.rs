use anyhow::{Context, Result};
use clap::Args;
use toffee_client::Client;
use toffee_core::ContextPackageId;

use crate::{format_for, stdout_is_tty, Effective, OutputFormat};

#[derive(Args, Debug)]
pub struct ProvenanceCmd {
    /// Context package id (`ctxpkg_…`).
    id: String,
}

pub async fn run(cmd: ProvenanceCmd, fmt: OutputFormat) -> Result<()> {
    let effective = format_for(fmt, stdout_is_tty());
    let client = Client::connect().await.context("connect to daemon")?;
    let report = client.inspect_provenance(ContextPackageId(cmd.id)).await?;

    match effective {
        Effective::Human => {
            println!("ctxpkg:     {}", report.context_package_id);
            println!("lens:       {}", report.lens);
            println!("query:      {}", report.query);
            println!("scope:      {}", report.scope.join(", "));
            println!("entries:    {}", report.entries.len());
            println!();
            for e in &report.entries {
                let sources: Vec<&str> = e.sources.iter().map(|s| s.as_str()).collect();
                let vec_sim = e
                    .vector_similarity
                    .map(|v| format!("{v:.3}"))
                    .unwrap_or_else(|| "n/a".to_string());
                println!(
                    "  {}  [{}] score={:.3}  vec={}  ent={}  rec={:.3}  conf={:.2}  src={}",
                    e.memory_id,
                    e.kept_in_kind.as_str(),
                    e.final_score,
                    vec_sim,
                    if e.entity_match { "yes" } else { "no" },
                    e.recency_score,
                    e.confidence_at_selection,
                    sources.join(","),
                );
                if !e.source_event_ids.is_empty() {
                    let events: Vec<&str> = e.source_event_ids.iter().map(|i| i.as_str()).collect();
                    println!("      events: {}", events.join(", "));
                }
            }
        }
        Effective::Json => {
            println!("{}", serde_json::to_string(&report)?);
        }
    }
    Ok(())
}
