//! Tap layer for ingesting AI agent session transcripts into toffee.
//!
//! The architecture is borrowed from filebeat / fluentd / vector:
//!
//! ```text
//!   ~/.claude/.../session.jsonl ─┐
//!   ~/.codex/.../rollout-*.jsonl ┼─► Source ─► RawTurn ─► Mapper ─► EventInput ─► toffee.append_event
//!   stdin (tmux pipe-pane) ──────┘                  ▲
//!                                                   │
//!                                              Registry (per-file checkpoint)
//! ```
//!
//! `Source` impls are format-specific (one per agent vendor). The `Mapper`
//! is format-agnostic — it turns the raw turn into an `EventInput` for
//! `toffee-client`. The `Registry` keeps per-file checkpoints so restart
//! resumes from the right offset (at-least-once delivery).

pub mod config;
pub mod mapper;
pub mod multiline;
pub mod registry;
pub mod runner;
pub mod sink;
pub mod source;
pub mod sources;

pub use config::{Config, FileSourceConfig, SourceConfig, StdinSourceConfig};
pub use mapper::{Mapper, MapperConfig};
pub use registry::{Registry, RegistryEntry};
pub use runner::{Runner, RunnerOptions};
pub use sink::{ClientSink, Sink};
pub use source::{RawTurn, Source, SourceError, Speaker};
