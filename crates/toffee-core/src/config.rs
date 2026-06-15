//! Runtime-tunable scoring configuration for the read path.
//!
//! These weights were hand-picked; `toffee-eval tune` fits them against a
//! retrieval corpus and writes them to `config.toml`, and the daemon hot-swaps
//! them live. The formula lives here in core (pure, no I/O) so the runtime,
//! daemon, and eval all rank with exactly the same arithmetic.

use serde::{Deserialize, Serialize};

/// Fusion weights for `read_context` ranking plus the recency decay constant.
/// Absolute weight scale is irrelevant — only the ratios affect ranking order —
/// but they're kept as plain coefficients so the config reads naturally.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScoringConfig {
    pub vector_weight: f64,
    pub entity_weight: f64,
    pub recency_weight: f64,
    pub confidence_weight: f64,
    pub recency_half_life_days: f64,
}

impl Default for ScoringConfig {
    /// The hand-picked baseline that shipped before tuning existed.
    fn default() -> Self {
        ScoringConfig {
            vector_weight: 0.60,
            entity_weight: 0.25,
            recency_weight: 0.10,
            confidence_weight: 0.05,
            recency_half_life_days: 14.0,
        }
    }
}

impl ScoringConfig {
    /// Exponential recency decay, `exp(-age / tau)`: 1.0 at age 0, ~0.368
    /// (1/e) at `recency_half_life_days`, asymptote 0. The field is a decay
    /// time-constant rather than a true half-life — the name is kept for the
    /// config key, and the math is byte-for-byte the pre-config behavior.
    /// Negative ages (clock skew) clamp to 1.0.
    pub fn recency_decay(&self, age_days: f64) -> f32 {
        (-age_days.max(0.0) / self.recency_half_life_days).exp() as f32
    }

    /// Combined ranking score. `entity_match` contributes a fixed 0.5 anchor
    /// (presence, not degree); the other signals are continuous in [0, 1].
    pub fn combined(
        &self,
        vector_similarity: f64,
        entity_match: bool,
        recency: f64,
        confidence: f64,
    ) -> f64 {
        let entity_anchor = if entity_match { 0.5 } else { 0.0 };
        self.vector_weight * vector_similarity
            + self.entity_weight * entity_anchor
            + self.recency_weight * recency
            + self.confidence_weight * confidence
    }
}

/// The on-disk config file. One `[scoring]` table today; `[lens]` /
/// `[extraction]` sections can be added later without breaking older files
/// (unknown sections are ignored, missing fields fall back to defaults).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub scoring: ScoringConfig,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_combined_matches_legacy_constants() {
        let c = ScoringConfig::default();
        // 0.6*v + 0.25*entity_anchor + 0.10*recency + 0.05*conf
        let got = c.combined(0.8, true, 0.5, 0.9);
        let want = 0.6 * 0.8 + 0.25 * 0.5 + 0.10 * 0.5 + 0.05 * 0.9;
        assert!((got - want).abs() < 1e-12);
    }

    #[test]
    fn recency_decay_is_one_over_e_at_tau() {
        let c = ScoringConfig::default();
        assert!((c.recency_decay(0.0) - 1.0).abs() < 1e-6);
        assert!((c.recency_decay(14.0) - std::f32::consts::E.recip()).abs() < 1e-6);
        assert_eq!(c.recency_decay(-5.0), 1.0);
    }

    #[test]
    fn partial_toml_falls_back_to_defaults() {
        // Only one field set; the rest should default, not error.
        let cfg: Config = toml::from_str("[scoring]\nvector_weight = 0.9\n").unwrap();
        assert_eq!(cfg.scoring.vector_weight, 0.9);
        assert_eq!(
            cfg.scoring.entity_weight,
            ScoringConfig::default().entity_weight
        );
    }
}
