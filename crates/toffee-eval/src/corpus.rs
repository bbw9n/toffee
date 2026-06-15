//! Golden-corpus types and the JSONL loader.
//!
//! The corpus is two JSONL files (one case per line) so it diffs cleanly and
//! grows by appending. Blank lines and `//`-prefixed lines are ignored, which
//! lets the files carry section headers and per-case notes.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// One expected memory in an extraction case. Every field except `kind` is
/// optional: omit a field to skip asserting on it. `text_contains` is the
/// loose match used when the worker paraphrases (so the corpus doesn't pin
/// exact surface text).
#[derive(Debug, Clone, Deserialize)]
pub struct ExpectedMemory {
    pub kind: String,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub predicate: Option<String>,
    #[serde(default)]
    pub object: Option<String>,
    #[serde(default)]
    pub text_contains: Option<String>,
}

/// One extraction case: an event the worker would see, and the memories it is
/// expected to (or expected *not* to) produce. An empty `expect` is a negative
/// case — the extractor must stay silent.
#[derive(Debug, Clone, Deserialize)]
pub struct ExtractionCase {
    pub name: String,
    pub input: String,
    #[serde(default = "default_actor")]
    pub actor: String,
    #[serde(default = "default_scope")]
    pub scope: Vec<String>,
    #[serde(default)]
    pub expect: Vec<ExpectedMemory>,
}

/// A memory seeded into the runtime before a retrieval query runs. `id` is
/// corpus-local — the harness maps it back from the returned memory text.
#[derive(Debug, Clone, Deserialize)]
pub struct SeedMemory {
    pub id: String,
    pub kind: String,
    pub text: String,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub predicate: Option<String>,
    #[serde(default)]
    pub object: Option<String>,
    #[serde(default = "default_confidence")]
    pub confidence: f64,
    /// Overrides the case scope for this memory (e.g. to seed a `user:` scope
    /// alongside `project:` memories and test inheritance).
    #[serde(default)]
    pub scope: Option<Vec<String>>,
}

/// One retrieval case: a set of seeded memories, a query, and the corpus-local
/// ids of the memories that *should* surface for that query.
#[derive(Debug, Clone, Deserialize)]
pub struct RetrievalCase {
    pub name: String,
    pub scope: Vec<String>,
    pub query: String,
    #[serde(default = "default_budget")]
    pub budget: usize,
    pub memories: Vec<SeedMemory>,
    pub relevant: Vec<String>,
}

fn default_actor() -> String {
    "user".into()
}
fn default_scope() -> Vec<String> {
    vec!["project:demo".into()]
}
fn default_confidence() -> f64 {
    0.9
}
fn default_budget() -> usize {
    3000
}

/// Load a JSONL corpus, skipping blanks and `//` comment lines. Each surviving
/// line must be one JSON object of type `T`; a parse error names the file and
/// 1-based line number so a bad entry is easy to find.
pub fn load_jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Vec<T>> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("reading corpus {}", path.display()))?;
    let mut out = Vec::new();
    for (i, raw) in body.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        let case: T = serde_json::from_str(line)
            .with_context(|| format!("{}:{}: malformed case", path.display(), i + 1))?;
        out.push(case);
    }
    Ok(out)
}
