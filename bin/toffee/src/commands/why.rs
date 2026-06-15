use anyhow::{Context, Result};
use clap::Args;
use toffee_client::Client;
use toffee_core::MemoryId;

use crate::{format_for, stdout_is_tty, Effective, OutputFormat};

#[derive(Args, Debug)]
pub struct WhyCmd {
    /// Memory id (`mem_…`).
    memory_id: String,
}

pub async fn run(cmd: WhyCmd, fmt: OutputFormat) -> Result<()> {
    let effective = format_for(fmt, stdout_is_tty());
    let client = Client::connect().await.context("connect to daemon")?;
    let report = client.why_memory(MemoryId(cmd.memory_id)).await?;

    match effective {
        Effective::Human => {
            let m = &report.memory;
            println!("memory:     {}", m.id);
            println!("kind:       {}", m.kind.as_str());
            println!("confidence: {:.2}", m.confidence);
            println!("scope:      {}", m.scope.as_slice().join(", "));
            if let (Some(s), Some(p), Some(o)) = (&m.subject, &m.predicate, &m.object) {
                println!("spo:        {s} -- {p} --> {o}");
            }
            println!("text:       {}", m.text);
            println!("created:    {}", m.created_at);
            println!("updated:    {}", m.updated_at);
            if let Some(by) = &m.superseded_by {
                println!("superseded_by: {by}");
            }
            println!();
            println!("source events ({}):", report.source_events.len());
            for e in &report.source_events {
                let text = e
                    .payload
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(no text)");
                println!(
                    "  {}  {}  [{}/{}]  {}",
                    e.id,
                    e.created_at,
                    e.actor.as_str(),
                    e.event_type,
                    truncate(text, 80)
                );
            }
            println!();
            println!("linked entities ({}):", report.linked_entities.len());
            for e in &report.linked_entities {
                println!("  {}  [{}] {}", e.id, e.entity_type.as_str(), e.name);
            }
            println!();
            println!("conflicts ({}):", report.conflicts.len());
            for c in &report.conflicts {
                println!(
                    "  {}  [{}]  {}/{}  competing={}",
                    c.id,
                    c.resolution.as_str(),
                    c.subject.as_deref().unwrap_or("?"),
                    c.predicate.as_deref().unwrap_or("?"),
                    c.competing_memory_ids.len()
                );
            }
        }
        Effective::Json => {
            println!("{}", serde_json::to_string(&report)?);
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
