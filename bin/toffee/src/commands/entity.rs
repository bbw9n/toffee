use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use toffee_client::Client;
use toffee_core::EntityType;
use toffee_rpc::ListEntitiesRequest;

use crate::{format_for, stdout_is_tty, Effective, OutputFormat};

#[derive(Args, Debug)]
pub struct EntityCmd {
    #[command(subcommand)]
    action: EntityAction,
}

#[derive(Subcommand, Debug)]
enum EntityAction {
    /// List active entities.
    List {
        /// Filter by entity type (person|project|library|tool|concept|other).
        #[arg(long)]
        kind: Option<String>,
        /// Match entities whose name starts with this prefix.
        #[arg(long = "prefix")]
        prefix: Option<String>,
        /// Max results.
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Show an entity page: the entity plus the memories linked to it and
    /// other entities that co-occur. The identifier is either an `ent_…`
    /// id, a name, or an alias.
    Show {
        identifier: String,
        /// Filter linked memories to these scopes (scope inheritance is
        /// applied: `user:me` and `global` are always considered too).
        #[arg(long = "scope")]
        scope: Vec<String>,
    },
}

pub async fn run(cmd: EntityCmd, fmt: OutputFormat) -> Result<()> {
    let effective = format_for(fmt, stdout_is_tty());
    match cmd.action {
        EntityAction::List {
            kind,
            prefix,
            limit,
        } => list(kind, prefix, limit, effective).await,
        EntityAction::Show { identifier, scope } => show(identifier, scope, effective).await,
    }
}

async fn list(
    kind: Option<String>,
    prefix: Option<String>,
    limit: usize,
    fmt: Effective,
) -> Result<()> {
    let client = Client::connect().await.context("connect to daemon")?;
    let entity_type = kind.as_deref().and_then(EntityType::parse);
    let req = ListEntitiesRequest {
        entity_type,
        name_prefix: prefix,
        limit: Some(limit),
    };
    let entities = client.list_entities(req).await?;

    match fmt {
        Effective::Human => {
            if entities.is_empty() {
                println!("(no entities)");
                return Ok(());
            }
            for e in &entities {
                let aliases = if e.aliases.is_empty() {
                    String::new()
                } else {
                    format!("  aliases={}", e.aliases.join(","))
                };
                println!(
                    "{}  [{}] {}{}",
                    e.id,
                    e.entity_type.as_str(),
                    e.name,
                    aliases
                );
            }
        }
        Effective::Json => {
            println!("{}", serde_json::to_string(&entities)?);
        }
    }
    Ok(())
}

async fn show(identifier: String, scope: Vec<String>, fmt: Effective) -> Result<()> {
    let client = Client::connect().await.context("connect to daemon")?;
    let scope = if scope.is_empty() { None } else { Some(scope) };
    let page = client.get_entity_page(identifier, scope).await?;

    match fmt {
        Effective::Human => {
            let e = &page.entity;
            println!("entity:     {}", e.id);
            println!("type:       {}", e.entity_type.as_str());
            println!("name:       {}", e.name);
            if !e.aliases.is_empty() {
                println!("aliases:    {}", e.aliases.join(", "));
            }
            if let Some(summary) = &e.summary {
                println!("summary:    {summary}");
            }
            println!();
            println!("memories ({}):", page.memories.len());
            if page.memories.is_empty() {
                println!("  (none)");
            }
            for m in &page.memories {
                println!(
                    "  {}  [{}] conf={:.2}  {}",
                    m.id,
                    m.kind.as_str(),
                    m.confidence,
                    truncate(&m.text, 80)
                );
            }
            println!();
            println!("co-occurring entities ({}):", page.co_occurring.len());
            if page.co_occurring.is_empty() {
                println!("  (none)");
            }
            for (ent, count) in &page.co_occurring {
                println!("  {}  {}  ({}x)", ent.id, ent.name, count);
            }
        }
        Effective::Json => {
            println!("{}", serde_json::to_string(&page)?);
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
