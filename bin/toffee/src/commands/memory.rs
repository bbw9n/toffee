use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde_json::json;
use toffee_client::Client;
use toffee_core::{FeedbackKind, MemoryId, MemoryKind, Scope};
use toffee_rpc::{ListMemoriesRequest, SearchMemoryRequest};

use crate::{format_for, stdout_is_tty, Effective, OutputFormat};

#[derive(Args, Debug)]
pub struct MemoryCmd {
    #[command(subcommand)]
    action: MemoryAction,
}

#[derive(Subcommand, Debug)]
enum MemoryAction {
    /// List active memories.
    List {
        /// Filter by scope; repeat for OR semantics.
        #[arg(long = "scope")]
        scope: Vec<String>,
        /// Filter by memory kind.
        #[arg(long)]
        kind: Option<String>,
        /// Case-insensitive substring filter on the memory text.
        #[arg(long)]
        query: Option<String>,
        /// Max results.
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Show one memory by id.
    Show { id: String },
    /// Record feedback on a memory.
    Feedback {
        id: String,
        /// One of: helpful | wrong | stale | correct.
        #[arg(long = "type", name = "kind")]
        kind: String,
    },
    /// Manually add a memory.
    Add {
        #[arg(long)]
        kind: String,
        #[arg(long = "scope", required = true)]
        scope: Vec<String>,
        #[arg(long)]
        text: String,
        #[arg(long)]
        subject: Option<String>,
        #[arg(long)]
        predicate: Option<String>,
        #[arg(long)]
        object: Option<String>,
        #[arg(long)]
        confidence: Option<f64>,
    },
    /// Forget a memory (soft-delete).
    Forget { id: String },
    /// Vector-ranked search over memories.
    Search {
        /// The natural-language query.
        query: String,
        /// Filter by scope; repeat for OR. Scope inheritance (user:me + global)
        /// is applied by the daemon.
        #[arg(long = "scope")]
        scope: Vec<String>,
        /// Filter by memory kind.
        #[arg(long)]
        kind: Option<String>,
        /// Max results.
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Drop hits below this cosine similarity.
        #[arg(long = "min-similarity")]
        min_similarity: Option<f32>,
    },
}

pub async fn run(cmd: MemoryCmd, fmt: OutputFormat) -> Result<()> {
    let effective = format_for(fmt, stdout_is_tty());
    match cmd.action {
        MemoryAction::List {
            scope,
            kind,
            query,
            limit,
        } => list(scope, kind, query, limit, effective).await,
        MemoryAction::Show { id } => show(id, effective).await,
        MemoryAction::Feedback { id, kind } => feedback(id, kind, effective).await,
        MemoryAction::Add {
            kind,
            scope,
            text,
            subject,
            predicate,
            object,
            confidence,
        } => {
            add(
                kind, scope, text, subject, predicate, object, confidence, effective,
            )
            .await
        }
        MemoryAction::Forget { id } => forget(id, effective).await,
        MemoryAction::Search {
            query,
            scope,
            kind,
            limit,
            min_similarity,
        } => search(query, scope, kind, limit, min_similarity, effective).await,
    }
}

async fn search(
    query: String,
    scope: Vec<String>,
    kind: Option<String>,
    limit: usize,
    min_similarity: Option<f32>,
    fmt: Effective,
) -> Result<()> {
    let client = Client::connect().await.context("connect to daemon")?;
    let req = SearchMemoryRequest {
        query,
        scope_any_of: if scope.is_empty() { None } else { Some(scope) },
        kind: kind.as_deref().and_then(MemoryKind::parse),
        limit: Some(limit),
        min_similarity,
    };
    let hits = client.search_memory(req).await?;

    match fmt {
        Effective::Human => {
            if hits.is_empty() {
                println!("(no matches)");
                return Ok(());
            }
            for h in &hits {
                println!(
                    "{}  sim={:.3}  [{}] conf={:.2}  {}",
                    h.memory.id,
                    h.similarity,
                    h.memory.kind.as_str(),
                    h.memory.confidence,
                    truncate(&h.memory.text, 80),
                );
            }
        }
        Effective::Json => {
            println!("{}", serde_json::to_string(&hits)?);
        }
    }
    Ok(())
}

async fn list(
    scope: Vec<String>,
    kind: Option<String>,
    query: Option<String>,
    limit: usize,
    fmt: Effective,
) -> Result<()> {
    let client = Client::connect().await.context("connect to daemon")?;
    let req = ListMemoriesRequest {
        scope_any_of: if scope.is_empty() { None } else { Some(scope) },
        kind: kind.as_deref().and_then(MemoryKind::parse),
        query,
        limit: Some(limit),
    };
    let memories = client.list_memories(req).await?;

    match fmt {
        Effective::Human => {
            if memories.is_empty() {
                println!("(no memories)");
                return Ok(());
            }
            for m in &memories {
                println!(
                    "{}  [{}] conf={:.2}  {}",
                    m.id,
                    m.kind.as_str(),
                    m.confidence,
                    truncate(&m.text, 100)
                );
                let scopes = m.scope.as_slice().join(", ");
                println!("    scope: {scopes}");
            }
        }
        Effective::Json => {
            println!("{}", serde_json::to_string(&memories)?);
        }
    }
    Ok(())
}

async fn show(id: String, fmt: Effective) -> Result<()> {
    let client = Client::connect().await.context("connect to daemon")?;
    let memory = client.get_memory(MemoryId(id)).await?;

    match fmt {
        Effective::Human => {
            println!("id:         {}", memory.id);
            println!("kind:       {}", memory.kind.as_str());
            println!("confidence: {:.2}", memory.confidence);
            println!("scope:      {}", memory.scope.as_slice().join(", "));
            if let (Some(s), Some(p), Some(o)) =
                (&memory.subject, &memory.predicate, &memory.object)
            {
                println!("spo:        {s} -- {p} --> {o}");
            }
            println!("text:       {}", memory.text);
            println!("created:    {}", memory.created_at);
            println!("updated:    {}", memory.updated_at);
            if !memory.source_event_ids.is_empty() {
                println!(
                    "sources:    {}",
                    memory
                        .source_event_ids
                        .iter()
                        .map(|e| e.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        Effective::Json => {
            println!("{}", serde_json::to_string(&memory)?);
        }
    }
    Ok(())
}

async fn feedback(id: String, kind_str: String, fmt: Effective) -> Result<()> {
    let kind = FeedbackKind::parse(&kind_str).ok_or_else(|| {
        anyhow::anyhow!("unknown feedback kind '{kind_str}'; expected helpful|wrong|stale|correct")
    })?;
    let client = Client::connect().await.context("connect to daemon")?;
    let resp = client.record_feedback(MemoryId(id), kind).await?;

    match fmt {
        Effective::Human => println!(
            "{} -> confidence={:.2}",
            resp.memory_id, resp.new_confidence
        ),
        Effective::Json => println!(
            "{}",
            json!({
                "memory_id": resp.memory_id.as_str(),
                "new_confidence": resp.new_confidence,
            })
        ),
    }
    Ok(())
}

// Mirrors the SPO memory shape and the `add` CLI flags one-to-one.
#[allow(clippy::too_many_arguments)]
async fn add(
    kind_str: String,
    scope: Vec<String>,
    text: String,
    subject: Option<String>,
    predicate: Option<String>,
    object: Option<String>,
    confidence: Option<f64>,
    fmt: Effective,
) -> Result<()> {
    let kind = MemoryKind::parse(&kind_str).ok_or_else(|| {
        anyhow::anyhow!("unknown kind '{kind_str}'; expected claim|decision|preference|episode")
    })?;
    let client = Client::connect().await.context("connect to daemon")?;
    let memory = client
        .add_memory(
            kind,
            Scope::new(scope),
            text,
            subject,
            predicate,
            object,
            confidence,
        )
        .await?;

    match fmt {
        Effective::Human => println!("{}", memory.id),
        Effective::Json => println!("{}", serde_json::to_string(&memory)?),
    }
    Ok(())
}

async fn forget(id: String, fmt: Effective) -> Result<()> {
    let client = Client::connect().await.context("connect to daemon")?;
    client.forget_memory(MemoryId(id.clone())).await?;
    match fmt {
        Effective::Human => println!("forgot {id}"),
        Effective::Json => println!("{}", json!({"memory_id": id, "forgotten": true})),
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
