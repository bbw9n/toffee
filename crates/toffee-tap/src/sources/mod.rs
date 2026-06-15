//! Format-specific source implementations.
//!
//! Each source is a JSONL or text tailer. All file-based sources share a
//! common tail loop (open, seek to checkpoint, read new lines, parse, emit
//! turns). The format-specific work lives in the parser closure.

pub mod claude_code;
pub mod codex;
pub mod jsonl_tail;
pub mod stdin;

pub use claude_code::ClaudeCodeSource;
pub use codex::CodexSource;
pub use stdin::StdinSource;
