//! Post-filter hits returned by [`crate::VectorIndex::search`].
//!
//! Phase 3 plumbs the simplest possible policy: over-fetch from HNSW, then
//! keep only hits whose scope intersects the caller's scope set (after
//! inheritance expansion) and whose kind matches if a kind filter was given.
//! Hits are returned in the index's similarity order.

use toffee_core::MemoryKind;

use crate::IndexHit;

#[derive(Debug, Clone, Default)]
pub struct FilterParams {
    /// Any-of match: a hit is kept if any of its scopes appears in this set.
    /// `None` means "don't filter by scope".
    pub scope_any_of: Option<Vec<String>>,
    pub kind: Option<MemoryKind>,
    /// Minimum cosine similarity to keep. `0.0` keeps everything HNSW gave
    /// us; setting >0 drops near-orthogonal matches that the index returned
    /// just to fill the over-fetch budget.
    pub min_similarity: f32,
    /// Number of results to keep after filtering. Caller is expected to
    /// have over-fetched by some factor.
    pub limit: usize,
}

pub fn apply(hits: Vec<IndexHit>, params: &FilterParams) -> Vec<IndexHit> {
    let mut out: Vec<IndexHit> = hits
        .into_iter()
        .filter(|h| h.similarity >= params.min_similarity)
        .filter(|h| {
            params
                .kind
                .map_or(true, |k| h.kind == k)
        })
        .filter(|h| match &params.scope_any_of {
            None => true,
            Some(scopes) => {
                let hit_scopes = h.scope.as_slice();
                scopes
                    .iter()
                    .any(|s| hit_scopes.iter().any(|hs| hs == s))
            }
        })
        .collect();
    if out.len() > params.limit {
        out.truncate(params.limit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use toffee_core::{MemoryId, Scope};

    fn h(id: &str, scopes: &[&str], kind: MemoryKind, sim: f32) -> IndexHit {
        IndexHit {
            memory_id: MemoryId(id.into()),
            scope: Scope::new(scopes.iter().copied()),
            kind,
            similarity: sim,
        }
    }

    #[test]
    fn scope_any_of_keeps_intersecting_hits() {
        let hits = vec![
            h("a", &["project:foo"], MemoryKind::Claim, 0.9),
            h("b", &["project:bar"], MemoryKind::Claim, 0.8),
            h("c", &["user:me"], MemoryKind::Preference, 0.7),
        ];
        let kept = apply(
            hits,
            &FilterParams {
                scope_any_of: Some(vec!["project:foo".into(), "user:me".into()]),
                limit: 10,
                ..Default::default()
            },
        );
        let ids: Vec<_> = kept.iter().map(|h| h.memory_id.0.as_str()).collect();
        assert_eq!(ids, vec!["a", "c"]);
    }

    #[test]
    fn kind_filter_works() {
        let hits = vec![
            h("a", &["g"], MemoryKind::Claim, 0.9),
            h("b", &["g"], MemoryKind::Decision, 0.8),
        ];
        let kept = apply(
            hits,
            &FilterParams {
                kind: Some(MemoryKind::Decision),
                limit: 10,
                ..Default::default()
            },
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].memory_id.0, "b");
    }

    #[test]
    fn min_similarity_drops_orthogonal_hits() {
        let hits = vec![
            h("a", &["g"], MemoryKind::Claim, 0.5),
            h("b", &["g"], MemoryKind::Claim, 0.05),
        ];
        let kept = apply(
            hits,
            &FilterParams {
                min_similarity: 0.3,
                limit: 10,
                ..Default::default()
            },
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].memory_id.0, "a");
    }

    #[test]
    fn limit_truncates() {
        let hits = (0..5)
            .map(|i| h(&format!("m{i}"), &["g"], MemoryKind::Claim, 0.9 - i as f32 * 0.01))
            .collect();
        let kept = apply(
            hits,
            &FilterParams {
                limit: 3,
                ..Default::default()
            },
        );
        assert_eq!(kept.len(), 3);
        assert_eq!(kept[0].memory_id.0, "m0");
    }
}
