//! Embeddings and approximate-nearest-neighbour search.
//!
//! Two embedder backends ship:
//!
//! 1. [`BgeEmbedder`] — a candle-backed BGE-small-en-v1.5 sentence
//!    transformer. ~33M params, 384-dim, ~130MB on disk, lazy-downloaded
//!    via `hf-hub` on first use. CPU by default; build with the `metal`
//!    cargo feature for Metal acceleration on macOS. This is the daemon
//!    default.
//! 2. [`HashEmbedder`] — a feature-hashed bag-of-tokens fallback. Offline,
//!    deterministic, ~zero runtime cost. Used in tests and as an opt-out
//!    when network/disk constraints rule out BGE.
//!
//! Both implement [`Embedder`] and produce L2-normalised vectors so the
//! HNSW index can treat cosine similarity as dot product regardless of
//! which backend produced the vector.
//!
//! The [`VectorIndex`] facade wraps `hnsw_rs` with a side map from internal
//! HNSW ids to memory ids. The graph is rebuilt from the SQLite
//! `embeddings` table on startup; SQLite is the source of truth.

pub mod bge;
pub mod embedder;
pub mod index;
pub mod post_filter;

pub use bge::{BgeEmbedder, BGE_MODEL_ID};
pub use embedder::{Embedder, HashEmbedder};
pub use index::{IndexHit, IndexedPoint, VectorIndex};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum VectorError {
    #[error("dimension mismatch: expected {expected}, got {actual}")]
    DimensionMismatch { expected: usize, actual: usize },
    #[error("hnsw error: {0}")]
    Hnsw(String),
    #[error("embedding model error: {0}")]
    Model(String),
}

pub type Result<T> = std::result::Result<T, VectorError>;

/// Default embedding dimensionality for the hash embedder. Small enough for
/// CPU cosine to stay cheap; wide enough that hash collisions don't dominate.
pub const DEFAULT_DIM: usize = 256;

/// Default model name string the hash embedder tags emitted embeddings with.
pub const DEFAULT_MODEL: &str = "hash-feature-v1";
