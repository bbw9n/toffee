use std::io::Read;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde_json::{json, Value};
use toffee_client::Client;
use toffee_core::{Actor, EventInput, Scope};

use crate::{format_for, stdout_is_tty, Effective, OutputFormat};

#[derive(Args, Debug)]
pub struct EventCmd {
    #[command(subcommand)]
    action: EventAction,
}

#[derive(Subcommand, Debug)]
enum EventAction {
    /// Append an event to the daemon.
    Append {
        /// Event type, e.g. `user_message`.
        #[arg(long = "type", name = "event_type")]
        event_type: String,

        /// Scope strings; repeat for multiple.
        #[arg(long = "scope", value_name = "SCOPE", required = true)]
        scope: Vec<String>,

        /// Actor (user|agent|tool|system|<custom>).
        #[arg(long, default_value = "user")]
        actor: String,

        /// JSON payload. Pass `@-` to read from stdin, `@path` to read from a
        /// file, or a literal JSON value.
        #[arg(long, default_value = "{}")]
        payload: String,

        /// Optional session identifier.
        #[arg(long)]
        session_id: Option<String>,

        /// Optional run identifier.
        #[arg(long)]
        run_id: Option<String>,
    },
}

pub async fn run(cmd: EventCmd, fmt: OutputFormat) -> Result<()> {
    let effective = format_for(fmt, stdout_is_tty());
    match cmd.action {
        EventAction::Append {
            event_type,
            scope,
            actor,
            payload,
            session_id,
            run_id,
        } => {
            append(
                event_type, scope, actor, payload, session_id, run_id, effective,
            )
            .await
        }
    }
}

async fn append(
    event_type: String,
    scope: Vec<String>,
    actor: String,
    payload_spec: String,
    session_id: Option<String>,
    run_id: Option<String>,
    fmt: Effective,
) -> Result<()> {
    let payload = parse_payload(&payload_spec).context("parse --payload")?;
    let actor = parse_actor(&actor);
    let input = EventInput {
        scope: Scope::new(scope),
        actor,
        event_type,
        payload,
        session_id,
        run_id,
    };

    let client = Client::connect()
        .await
        .context("connect to daemon (try `toffee daemon start` first)")?;
    let id = client.append_event(input).await?;

    match fmt {
        Effective::Human => println!("{}", id),
        Effective::Json => println!("{}", json!({"event_id": id.as_str()})),
    }
    Ok(())
}

fn parse_payload(spec: &str) -> Result<Value> {
    if spec == "@-" {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        if buf.trim().is_empty() {
            return Ok(Value::Object(Default::default()));
        }
        return Ok(serde_json::from_str(&buf)?);
    }
    if let Some(rest) = spec.strip_prefix('@') {
        let s = std::fs::read_to_string(rest)?;
        return Ok(serde_json::from_str(&s)?);
    }
    Ok(serde_json::from_str(spec)?)
}

fn parse_actor(s: &str) -> Actor {
    match s {
        "user" => Actor::User,
        "agent" => Actor::Agent,
        "tool" => Actor::Tool,
        "system" => Actor::System,
        other => Actor::Other(other.to_string()),
    }
}
