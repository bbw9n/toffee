//! Context packages, lenses, and provenance.
//!
//! A [`ContextPackage`] is what `toffee.read_context` returns: memories
//! grouped by kind, fitted to a token budget, plus any unresolved conflicts
//! in scope. A [`Lens`] is a named retrieval policy — Phase 4 ships exactly
//! one, [`Lens::default_lens`], matching RFC §8.2.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::conflict::MemoryConflict;
use crate::event::EventId;
use crate::memory::{Memory, MemoryId, MemoryKind};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContextPackageId(pub String);

impl ContextPackageId {
    pub fn generate() -> Self {
        ContextPackageId(format!("ctxpkg_{}", Ulid::new()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ContextPackageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What `read_context` returns. Memories are bucketed by kind; the agent
/// renders them as it pleases (a markdown helper is on [`ContextPackage`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextPackage {
    pub id: ContextPackageId,
    pub scope: Vec<String>,
    pub lens: String,
    pub query: String,
    pub claims: Vec<Memory>,
    pub decisions: Vec<Memory>,
    pub preferences: Vec<Memory>,
    pub episodes: Vec<Memory>,
    /// Unresolved conflicts visible from this scope set, surfaced so the
    /// agent knows it's looking at contested ground.
    pub conflicts: Vec<MemoryConflict>,
    pub token_estimate: usize,
    pub created_at: DateTime<Utc>,
}

impl ContextPackage {
    /// Approximate token budget consumed by the rendered markdown. Uses the
    /// "4 chars per token" rule; close enough for budget control without
    /// pulling in a real tokenizer.
    pub fn estimate_tokens_for(text: &str) -> usize {
        (text.chars().count() + 3) / 4
    }

    /// Render to a markdown block agents can prepend to their prompts. Order
    /// reflects RFC §8.1's guidance: decisions and preferences at the top
    /// (load-bearing), claims in the middle (factual grounding), episodes
    /// at the bottom (background).
    pub fn render_markdown(&self) -> String {
        let mut out = String::with_capacity(512);
        out.push_str("## Memory\n");
        section(&mut out, "Decisions", &self.decisions);
        section(&mut out, "Preferences", &self.preferences);
        section(&mut out, "Claims", &self.claims);
        section(&mut out, "Episodes", &self.episodes);
        if !self.conflicts.is_empty() {
            out.push_str("\n### Open conflicts\n");
            for c in &self.conflicts {
                let pair = match (&c.subject, &c.predicate) {
                    (Some(s), Some(p)) => format!("{s} — {p}"),
                    _ => "(no subject/predicate)".to_string(),
                };
                out.push_str(&format!(
                    "- {pair}: {n} competing memories ({id})\n",
                    n = c.competing_memory_ids.len(),
                    id = c.id
                ));
            }
        }
        out
    }
}

fn section(out: &mut String, heading: &str, memories: &[Memory]) {
    if memories.is_empty() {
        return;
    }
    out.push_str(&format!("\n### {heading}\n"));
    for m in memories {
        out.push_str(&format!("- {}\n", m.text.trim()));
    }
}

/// Named retrieval policy. v1 ships [`Lens::default_lens`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lens {
    pub name: String,
    pub min_confidence: f64,
    /// Token-budget fraction per kind. Values are renormalised to sum to 1.0
    /// so callers can ignore exact weights.
    pub claim_weight: f64,
    pub decision_weight: f64,
    pub preference_weight: f64,
    pub episode_weight: f64,
    /// Maximum memories to include per kind even if the budget allows more.
    /// Keeps the package readable rather than dense.
    pub max_per_kind: usize,
}

impl Lens {
    /// RFC §8.2 defaults: all four kinds, min_confidence=0.6, 40/30/20/10
    /// across decisions/claims/preferences/episodes.
    pub fn default_lens() -> Self {
        Lens {
            name: "default".to_string(),
            min_confidence: 0.6,
            claim_weight: 0.30,
            decision_weight: 0.40,
            preference_weight: 0.20,
            episode_weight: 0.10,
            max_per_kind: 12,
        }
    }

    /// Token budget for one kind given a total and the renormalised weight.
    pub fn budget_for(&self, kind: MemoryKind, total: usize) -> usize {
        let w = self.weight_for(kind);
        let sum = self.claim_weight + self.decision_weight + self.preference_weight + self.episode_weight;
        if sum <= 0.0 {
            return 0;
        }
        ((total as f64) * (w / sum)).round() as usize
    }

    fn weight_for(&self, kind: MemoryKind) -> f64 {
        match kind {
            MemoryKind::Claim => self.claim_weight,
            MemoryKind::Decision => self.decision_weight,
            MemoryKind::Preference => self.preference_weight,
            MemoryKind::Episode => self.episode_weight,
        }
    }
}

/// Where a memory came from in a `read_context` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalSource {
    Vector,
    Entity,
    Recent,
}

impl RetrievalSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            RetrievalSource::Vector => "vector",
            RetrievalSource::Entity => "entity",
            RetrievalSource::Recent => "recent",
        }
    }
}

/// Per-memory breakdown explaining why it made it into a context package.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvenanceEntry {
    pub memory_id: MemoryId,
    pub final_score: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector_similarity: Option<f32>,
    pub entity_match: bool,
    pub recency_score: f32,
    pub confidence_at_selection: f64,
    pub sources: Vec<RetrievalSource>,
    pub source_event_ids: Vec<EventId>,
    pub kept_in_kind: MemoryKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvenanceReport {
    pub context_package_id: ContextPackageId,
    pub query: String,
    pub scope: Vec<String>,
    pub lens: String,
    pub entries: Vec<ProvenanceEntry>,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lens_budget_for_default_lens_sums_close_to_total() {
        let lens = Lens::default_lens();
        let total = 3000;
        let s: usize = [
            MemoryKind::Decision,
            MemoryKind::Claim,
            MemoryKind::Preference,
            MemoryKind::Episode,
        ]
        .iter()
        .map(|k| lens.budget_for(*k, total))
        .sum();
        // Rounding may push us +/- a handful of tokens.
        assert!((s as i32 - total as i32).abs() <= 4, "got {s}");
    }

    #[test]
    fn token_estimate_is_roughly_chars_over_four() {
        let t = ContextPackage::estimate_tokens_for("abcd"); // 4 chars
        assert_eq!(t, 1);
        let t = ContextPackage::estimate_tokens_for("abcdefgh"); // 8 chars
        assert_eq!(t, 2);
    }
}
