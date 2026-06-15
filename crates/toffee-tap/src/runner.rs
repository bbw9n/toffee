//! Drive one or more sources, apply mappers, ship to the sink.
//!
//! Single-threaded round-robin over sources with exponential backoff when
//! everyone returns empty. We don't bother with multi-task fan-in because
//! at single-user scale the cost of `read_next` is dominated by the
//! `read_available_lines` syscall and the cost of `append_event` is
//! microseconds.

use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use toffee_core::paths;

use crate::config::{Config, FileSourceConfig, SourceConfig, StdinSourceConfig};
use crate::mapper::{Mapper, MapperConfig};
use crate::registry::Registry;
use crate::sink::{Sink, SinkError};
use crate::source::{Source, SourceError};
use crate::sources::{ClaudeCodeSource, CodexSource, StdinSource};

const POLL_BACKOFF_MIN: Duration = Duration::from_millis(100);
const POLL_BACKOFF_MAX: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("registry error: {0}")]
    Registry(#[from] crate::registry::RegistryError),
    #[error("source error: {0}")]
    Source(#[from] SourceError),
    #[error("sink error: {0}")]
    Sink(#[from] SinkError),
    #[error("config error: {0}")]
    Config(String),
}

pub struct RunnerOptions {
    pub config: Config,
    /// If true, exit when all sources are file-based and have reached
    /// EOF (i.e. one drain pass + no new data after backoff). Default
    /// false — the runner tails forever.
    pub one_shot: bool,
}

pub struct Runner {
    sources: Vec<SourceCell>,
    sink: Arc<dyn Sink>,
}

struct SourceCell {
    source: Box<dyn Source>,
    mapper: Mapper,
}

impl Runner {
    pub fn build(opts: RunnerOptions, sink: Arc<dyn Sink>) -> Result<Self, RunnerError> {
        let Config { registry, sources } = opts.config;
        let registry_path =
            registry.unwrap_or_else(|| paths::state_dir().join("tap.registry.json"));
        let registry = Arc::new(Registry::load(&registry_path)?);

        let mut cells = Vec::with_capacity(sources.len());
        for s in sources {
            let cell = build_source(s, registry.clone())?;
            cells.push(cell);
        }
        if cells.is_empty() {
            return Err(RunnerError::Config("no sources configured".to_string()));
        }
        Ok(Runner {
            sources: cells,
            sink,
        })
    }

    pub async fn run(
        mut self,
        shutdown: tokio::sync::broadcast::Receiver<()>,
    ) -> Result<(), RunnerError> {
        let mut shutdown = shutdown;
        let mut backoff = POLL_BACKOFF_MIN;
        loop {
            tokio::select! {
                _ = shutdown.recv() => {
                    tracing::info!("tap runner: shutdown");
                    return Ok(());
                }
                drained = self.drain_round() => {
                    let drained = drained?;
                    if drained > 0 {
                        backoff = POLL_BACKOFF_MIN;
                    } else {
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(POLL_BACKOFF_MAX);
                    }
                }
            }
        }
    }

    async fn drain_round(&mut self) -> Result<usize, RunnerError> {
        let mut total = 0usize;
        for cell in &mut self.sources {
            let turns = cell.source.read_next().await?;
            for t in turns {
                let kind = cell.source.kind();
                let event = cell.mapper.map(kind, &t);
                self.sink.append_event(event).await?;
                total += 1;
            }
        }
        Ok(total)
    }
}

fn build_source(s: SourceConfig, registry: Arc<Registry>) -> Result<SourceCell, RunnerError> {
    match s {
        SourceConfig::ClaudeCode(cfg) => {
            let path = cfg
                .path
                .clone()
                .unwrap_or_else(ClaudeCodeSource::default_path);
            let source = ClaudeCodeSource::new(path, registry);
            let mapper = Mapper::new(file_mapper_config(cfg));
            Ok(SourceCell {
                source: Box::new(source),
                mapper,
            })
        }
        SourceConfig::Codex(cfg) => {
            let path = cfg.path.clone().unwrap_or_else(CodexSource::default_path);
            let source = CodexSource::new(path, registry);
            let mapper = Mapper::new(file_mapper_config(cfg));
            Ok(SourceCell {
                source: Box::new(source),
                mapper,
            })
        }
        SourceConfig::Stdin(cfg) => {
            let StdinSourceConfig {
                user_prefix,
                scope,
                scope_fallback,
            } = cfg;
            let source = StdinSource::new(user_prefix);
            let mapper = Mapper::new(MapperConfig {
                scope_override: scope,
                scope_auto: false,
                scope_fallback,
            });
            Ok(SourceCell {
                source: Box::new(source),
                mapper,
            })
        }
    }
}

fn file_mapper_config(cfg: FileSourceConfig) -> MapperConfig {
    MapperConfig {
        scope_override: cfg.scope,
        scope_auto: cfg.scope_auto,
        scope_fallback: cfg.scope_fallback,
    }
}
