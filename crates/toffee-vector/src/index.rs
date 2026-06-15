//! In-memory HNSW index keyed by an internal `seq_id`. SQLite is the source
//! of truth for embedding rows; the index is rebuilt from there on daemon
//! startup. The runtime maps `seq_id` to `MemoryId` via a side table held
//! inside this index so search results don't need a SQLite round-trip for
//! every hit.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use hnsw_rs::prelude::*;
use parking_lot::RwLock;
use toffee_core::{MemoryId, MemoryKind, Scope};

use crate::{Result, VectorError};

/// Default upper bound for the HNSW graph. Above this, recall degrades and
/// memory grows; rebuilding with a larger cap is fine. RFC §8 puts the
/// scale revisit at ~100k embeddings, so 100k headroom matches that.
pub const DEFAULT_MAX_ELEMENTS: usize = 100_000;

/// Side-table entry attached to each indexed embedding.
#[derive(Debug, Clone)]
pub struct IndexedPoint {
    pub memory_id: MemoryId,
    pub scope: Scope,
    pub kind: MemoryKind,
}

#[derive(Debug, Clone)]
pub struct IndexHit {
    pub memory_id: MemoryId,
    pub scope: Scope,
    pub kind: MemoryKind,
    pub similarity: f32,
}

pub struct VectorIndex {
    dim: usize,
    inner: RwLock<Inner>,
    next_seq: AtomicUsize,
}

struct Inner {
    /// Owned to keep lifetimes simple. hnsw_rs's `Hnsw<'a, ...>` borrows the
    /// distance metric; for `DistCosine` (zero-sized) that's harmless and we
    /// use `'static`.
    hnsw: Hnsw<'static, f32, DistCosine>,
    side: HashMap<usize, IndexedPoint>,
}

impl VectorIndex {
    pub fn new(dim: usize) -> Self {
        Self::with_capacity(dim, DEFAULT_MAX_ELEMENTS)
    }

    pub fn with_capacity(dim: usize, max_elements: usize) -> Self {
        // hnsw_rs tuning: small, fast, recall-friendly enough for Phase 3.
        let max_nb_connection = 16;
        let max_layer = 16;
        let ef_construction = 200;
        let hnsw = Hnsw::<f32, DistCosine>::new(
            max_nb_connection,
            max_elements,
            max_layer,
            ef_construction,
            DistCosine {},
        );
        VectorIndex {
            dim,
            inner: RwLock::new(Inner {
                hnsw,
                side: HashMap::new(),
            }),
            next_seq: AtomicUsize::new(0),
        }
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn len(&self) -> usize {
        self.inner.read().side.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Insert one vector and remember its mapping. Returns the internal
    /// `seq_id` the HNSW graph uses to refer to this point.
    pub fn insert(&self, vector: &[f32], point: IndexedPoint) -> Result<usize> {
        if vector.len() != self.dim {
            return Err(VectorError::DimensionMismatch {
                expected: self.dim,
                actual: vector.len(),
            });
        }
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.inner.write();
        guard.hnsw.insert((vector, seq));
        guard.side.insert(seq, point);
        Ok(seq)
    }

    /// Insert with an explicit seq id — used during rebuild from SQLite,
    /// where the stored `seq_id` should be preserved so the index lines up
    /// with the persisted `embeddings` table.
    pub fn insert_with_seq(&self, seq: usize, vector: &[f32], point: IndexedPoint) -> Result<()> {
        if vector.len() != self.dim {
            return Err(VectorError::DimensionMismatch {
                expected: self.dim,
                actual: vector.len(),
            });
        }
        let mut guard = self.inner.write();
        guard.hnsw.insert((vector, seq));
        guard.side.insert(seq, point);
        // Move next_seq forward so subsequent live inserts don't collide.
        let mut current = self.next_seq.load(Ordering::Relaxed);
        while seq >= current {
            match self.next_seq.compare_exchange(
                current,
                seq + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(v) => current = v,
            }
        }
        Ok(())
    }

    /// Approximate-nearest-neighbour search. Returns up to `k` hits in
    /// descending similarity order. Caller does scope/kind post-filtering
    /// via [`crate::post_filter`].
    pub fn search(&self, query: &[f32], k: usize, ef_search: usize) -> Result<Vec<IndexHit>> {
        if query.len() != self.dim {
            return Err(VectorError::DimensionMismatch {
                expected: self.dim,
                actual: query.len(),
            });
        }
        let guard = self.inner.read();
        let neighbours = guard.hnsw.search(query, k, ef_search.max(k));
        let mut hits: Vec<IndexHit> = Vec::with_capacity(neighbours.len());
        for n in neighbours {
            let Some(point) = guard.side.get(&n.d_id) else {
                continue; // stale or rebuilt away
            };
            // DistCosine returns 1 - cosine_similarity. Convert back.
            let similarity = 1.0 - n.distance;
            hits.push(IndexHit {
                memory_id: point.memory_id.clone(),
                scope: point.scope.clone(),
                kind: point.kind,
                similarity,
            });
        }
        // hnsw_rs gives us ascending distance; we already converted, so
        // higher similarity should now be first.
        hits.sort_by(|a, b| {
            b.similarity
                .partial_cmp(&a.similarity)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(hits)
    }

    /// Drop every point. Used by `daemon.rebuild_indexes` before re-streaming
    /// from SQLite.
    pub fn clear(&self) {
        let mut guard = self.inner.write();
        // hnsw_rs doesn't expose a clear() method; rebuild a fresh graph.
        guard.hnsw = Hnsw::<f32, DistCosine>::new(16, DEFAULT_MAX_ELEMENTS, 16, 200, DistCosine {});
        guard.side.clear();
        self.next_seq.store(0, Ordering::Relaxed);
    }

    /// Drop a single point by memory id.
    pub fn forget(&self, memory_id: &MemoryId) {
        let mut guard = self.inner.write();
        // hnsw_rs has no delete; remove from side table and let stale
        // entries fall out of search via the lookup miss above. For Phase 3
        // this is acceptable — the next rebuild will compact.
        guard.side.retain(|_, p| &p.memory_id != memory_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(name: &str) -> IndexedPoint {
        IndexedPoint {
            memory_id: MemoryId(format!("mem_{name}")),
            scope: Scope::new(["project:test"]),
            kind: MemoryKind::Claim,
        }
    }

    #[test]
    fn insert_and_search_returns_nearest() {
        let idx = VectorIndex::new(4);
        idx.insert(&[1.0, 0.0, 0.0, 0.0], point("a")).unwrap();
        idx.insert(&[0.0, 1.0, 0.0, 0.0], point("b")).unwrap();
        idx.insert(&[0.0, 0.0, 1.0, 0.0], point("c")).unwrap();

        let hits = idx.search(&[0.9, 0.1, 0.0, 0.0], 2, 32).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].memory_id.0, "mem_a");
        assert!(hits[0].similarity > hits[1].similarity);
    }

    #[test]
    fn forget_drops_point_from_results() {
        let idx = VectorIndex::new(2);
        idx.insert(&[1.0, 0.0], point("a")).unwrap();
        idx.insert(&[0.0, 1.0], point("b")).unwrap();
        idx.forget(&MemoryId("mem_a".into()));
        let hits = idx.search(&[1.0, 0.0], 5, 32).unwrap();
        assert!(!hits.iter().any(|h| h.memory_id.0 == "mem_a"));
    }

    #[test]
    fn dimension_mismatch_errors() {
        let idx = VectorIndex::new(4);
        assert!(idx.insert(&[1.0, 0.0, 0.0], point("a")).is_err());
        assert!(idx.search(&[1.0], 1, 16).is_err());
    }

    #[test]
    fn clear_resets_index() {
        let idx = VectorIndex::new(2);
        idx.insert(&[1.0, 0.0], point("a")).unwrap();
        assert_eq!(idx.len(), 1);
        idx.clear();
        assert!(idx.is_empty());
    }
}
