//! Embeddings and approximate-nearest-neighbour search.
//!
//! Phase 3 ships two things:
//!
//! 1. An [`Embedder`] trait with a default [`HashEmbedder`] implementation.
//!    The hash embedder is a feature-hashed bag-of-tokens — fast, offline,
//!    deterministic, and good enough for lexical recall while a real
//!    sentence-transformer (BGE-small / MiniLM via `candle`) is gated behind
//!    a follow-up cargo feature.
//! 2. A [`VectorIndex`] facade around `hnsw_rs` that owns the in-memory HNSW
//!    graph and a side map from internal HNSW ids to memory ids. The graph
//!    is rebuilt from the SQLite `embeddings` table on startup; SQLite is
//!    the source of truth.

pub mod embedder;
pub mod index;
pub mod post_filter;

pub use embedder::{Embedder, HashEmbedder};
pub use index::{IndexHit, IndexedPoint, VectorIndex};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum VectorError {
    #[error("dimension mismatch: expected {expected}, got {actual}")]
    DimensionMismatch { expected: usize, actual: usize },
    #[error("hnsw error: {0}")]
    Hnsw(String),
}

pub type Result<T> = std::result::Result<T, VectorError>;

/// Default embedding dimensionality for the hash embedder. Small enough for
/// CPU cosine to stay cheap; wide enough that hash collisions don't dominate.
pub const DEFAULT_DIM: usize = 256;

/// Default model name string we tag emitted embeddings with.
pub const DEFAULT_MODEL: &str = "hash-feature-v1";
