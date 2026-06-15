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
