mod commands;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "toffee", version, about = "Toffee CLI", long_about = None)]
struct Cli {
    /// Output format. `auto` picks human-readable on a TTY, JSON otherwise.
    #[arg(long, value_enum, default_value_t = OutputFormat::Auto, global = true)]
    format: OutputFormat,

    #[command(subcommand)]
    command: Command,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum OutputFormat {
    Auto,
    Human,
    Json,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Manage the toffee daemon.
    Daemon(commands::daemon::DaemonCmd),
    /// Inspect and append raw events.
    Event(commands::event::EventCmd),
    /// Inspect, add, and curate memories.
    Memory(commands::memory::MemoryCmd),
    /// Inspect entities and their pages.
    Entity(commands::entity::EntityCmd),
    /// Assemble a memory-augmented context package for an agent prompt.
    Context(commands::context::ContextCmd),
    /// Explain why each memory was chosen for a given context package.
    Provenance(commands::provenance::ProvenanceCmd),
    /// List, show, and resolve contradicting facts.
    Conflict(commands::conflict::ConflictCmd),
    /// Background worker status and recent failures.
    Worker(commands::worker::WorkerCmd),
    /// Explain how a memory came to be.
    Why(commands::why::WhyCmd),
    /// Stream agent session transcripts (Claude Code, Codex, stdin) into toffeed.
    Tap(commands::tap::TapCmd),
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("TOFFEE_LOG")
                .unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init()
        .ok();

    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        match cli.command {
            Command::Daemon(cmd) => commands::daemon::run(cmd, cli.format).await,
            Command::Event(cmd) => commands::event::run(cmd, cli.format).await,
            Command::Memory(cmd) => commands::memory::run(cmd, cli.format).await,
            Command::Entity(cmd) => commands::entity::run(cmd, cli.format).await,
            Command::Context(cmd) => commands::context::run(cmd, cli.format).await,
            Command::Provenance(cmd) => commands::provenance::run(cmd, cli.format).await,
            Command::Conflict(cmd) => commands::conflict::run(cmd, cli.format).await,
            Command::Worker(cmd) => commands::worker::run(cmd, cli.format).await,
            Command::Why(cmd) => commands::why::run(cmd, cli.format).await,
            Command::Tap(cmd) => commands::tap::run(cmd, cli.format).await,
        }
    })
}

pub fn format_for(fmt: OutputFormat, is_stdout_tty: bool) -> Effective {
    match fmt {
        OutputFormat::Human => Effective::Human,
        OutputFormat::Json => Effective::Json,
        OutputFormat::Auto => {
            if is_stdout_tty {
                Effective::Human
            } else {
                Effective::Json
            }
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum Effective {
    Human,
    Json,
}

pub fn stdout_is_tty() -> bool {
    // Safe: isatty just consults the fd table.
    unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 }
}
