use anyhow::{Context, Result};
use clap::Args;
use toffee_client::Client;

use crate::{format_for, stdout_is_tty, Effective, OutputFormat};

#[derive(Args, Debug)]
pub struct ContextCmd {
    /// Scope filter; repeat for OR. Scope inheritance is applied by the
    /// daemon, so a project scope automatically considers user:me and global.
    #[arg(long = "scope", required = true)]
    scope: Vec<String>,

    /// The query (often the user's incoming message).
    #[arg(long)]
    query: String,

    /// Lens to apply. Today only `default` is recognised.
    #[arg(long, default_value = "default")]
    lens: String,

    /// Total token budget across all kinds.
    #[arg(long, default_value_t = 3000)]
    budget: usize,

    /// Print the agent-ready markdown rather than the structured shape.
    #[arg(long)]
    markdown: bool,
}

pub async fn run(cmd: ContextCmd, fmt: OutputFormat) -> Result<()> {
    let effective = format_for(fmt, stdout_is_tty());
    let client = Client::connect().await.context("connect to daemon")?;
    let package = client
        .read_context_with(toffee_rpc::ReadContextRequest {
            scope: cmd.scope,
            query: cmd.query,
            lens: cmd.lens,
            custom_lens: None,
            token_budget: Some(cmd.budget),
        })
        .await?;

    if cmd.markdown {
        println!("{}", package.render_markdown());
        eprintln!("(ctx package id: {})", package.id);
        return Ok(());
    }

    match effective {
        Effective::Human => {
            println!("package:    {}", package.id);
            println!("lens:       {}", package.lens);
            println!("scope:      {}", package.scope.join(", "));
            println!("query:      {}", package.query);
            println!("tokens:     ~{}", package.token_estimate);
            println!();
            print_kind("Decisions", &package.decisions);
            print_kind("Claims", &package.claims);
            print_kind("Preferences", &package.preferences);
            print_kind("Episodes", &package.episodes);
            if !package.conflicts.is_empty() {
                println!();
                println!("conflicts ({}):", package.conflicts.len());
                for c in &package.conflicts {
                    println!(
                        "  {}: {} competing  {}/{}",
                        c.id,
                        c.competing_memory_ids.len(),
                        c.subject.as_deref().unwrap_or("?"),
                        c.predicate.as_deref().unwrap_or("?")
                    );
                }
            }
        }
        Effective::Json => {
            println!("{}", serde_json::to_string(&package)?);
        }
    }
    Ok(())
}

fn print_kind(heading: &str, memories: &[toffee_core::Memory]) {
    println!("{heading} ({n}):", n = memories.len());
    if memories.is_empty() {
        println!("  (none)");
        return;
    }
    for m in memories {
        println!(
            "  {}  conf={:.2}  {}",
            m.id,
            m.confidence,
            truncate(&m.text, 80)
        );
    }
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
