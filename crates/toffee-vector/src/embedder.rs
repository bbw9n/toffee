//! Embedder trait + `HashEmbedder` fallback.
//!
//! [`HashEmbedder`] is a deterministic feature-hashing baseline. It tokenises
//! on word boundaries, lowercases, and projects each token to a fixed-dim
//! sparse vector via a fast non-cryptographic hash; vectors are L2-normalised
//! so dot product == cosine similarity.
//!
//! The semantic backend ([`crate::BgeEmbedder`]) is the daemon default;
//! `HashEmbedder` is kept for tests and as an offline fallback when BGE
//! weights are unavailable.

use crate::{Result, DEFAULT_DIM, DEFAULT_MODEL};

pub trait Embedder: Send + Sync + 'static {
    /// Stable model identifier stored alongside each embedding.
    fn model(&self) -> &str;
    /// Vector dimensionality.
    fn dim(&self) -> usize;
    /// Embed one piece of text. Returned vector should be L2-normalised
    /// (callers will use cosine via dot product).
    fn embed(&self, text: &str) -> Result<Vec<f32>>;
    /// Batched convenience. Default impl loops; impls with batchable kernels
    /// can override.
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
}

/// Feature-hashed embedder.
#[derive(Debug, Clone)]
pub struct HashEmbedder {
    dim: usize,
    model: String,
}

impl Default for HashEmbedder {
    fn default() -> Self {
        Self::new(DEFAULT_DIM)
    }
}

impl HashEmbedder {
    pub fn new(dim: usize) -> Self {
        HashEmbedder {
            dim,
            model: DEFAULT_MODEL.to_string(),
        }
    }

    fn tokenize(text: &str) -> impl Iterator<Item = String> + '_ {
        text.split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .map(|t| t.to_ascii_lowercase())
    }
}

impl Embedder for HashEmbedder {
    fn model(&self) -> &str {
        &self.model
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut v = vec![0.0f32; self.dim];
        let mut n_tokens = 0usize;
        // Per-token bag-of-words feature hashing with sign.
        for tok in Self::tokenize(text) {
            n_tokens += 1;
            let h = fnv1a(tok.as_bytes());
            let bucket = (h as usize) % self.dim;
            let sign = if (h >> 32) & 1 == 0 { 1.0 } else { -1.0 };
            v[bucket] += sign;

            // Also project the 3-char prefix and suffix so short morphological
            // variants ("Pest" vs "pest") share buckets. This is what makes
            // the hash embedder behave better than pure bag-of-words.
            for piece in ngram_pieces(&tok) {
                let h2 = fnv1a(piece.as_bytes());
                let b = (h2 as usize) % self.dim;
                let s = if (h2 >> 32) & 1 == 0 { 1.0 } else { -1.0 };
                v[b] += s * 0.5;
            }
        }
        if n_tokens == 0 {
            // Avoid NaN from normalising a zero vector. Returning all-zeros
            // means cosine similarity with everything is 0 — acceptable.
            return Ok(v);
        }
        l2_normalize(&mut v);
        Ok(v)
    }
}

fn ngram_pieces(tok: &str) -> impl Iterator<Item = &str> {
    let bytes = tok.as_bytes();
    let prefix_end = bytes.len().min(3);
    let suffix_start = bytes.len().saturating_sub(3);
    let pieces: Vec<&str> = if bytes.len() <= 3 {
        // Single piece — no need to duplicate.
        vec![&tok[..bytes.len()]]
    } else {
        vec![&tok[..prefix_end], &tok[suffix_start..]]
    };
    pieces.into_iter()
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn l2_normalize(v: &mut [f32]) {
    let norm_sq: f32 = v.iter().map(|x| x * x).sum();
    let norm = norm_sq.sqrt();
    if norm > 1e-12 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cos(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn embeds_have_unit_norm() {
        let e = HashEmbedder::new(256);
        let v = e.embed("the parser uses Pest").unwrap();
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "norm = {}", norm);
    }

    #[test]
    fn identical_texts_have_cosine_one() {
        let e = HashEmbedder::new(256);
        let a = e.embed("the parser uses Pest").unwrap();
        let b = e.embed("THE parser USES pest").unwrap();
        assert!((cos(&a, &b) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn related_texts_rank_above_unrelated() {
        let e = HashEmbedder::new(256);
        let q = e.embed("which parser library do we use").unwrap();
        let related = e.embed("the parser uses Pest").unwrap();
        let unrelated = e.embed("user prefers concise responses").unwrap();
        assert!(
            cos(&q, &related) > cos(&q, &unrelated),
            "related={} unrelated={}",
            cos(&q, &related),
            cos(&q, &unrelated)
        );
    }

    #[test]
    fn empty_text_returns_zero_vector_without_panic() {
        let e = HashEmbedder::new(256);
        let v = e.embed("").unwrap();
        assert!(v.iter().all(|x| *x == 0.0));
    }

    #[test]
    fn batch_matches_loop() {
        let e = HashEmbedder::new(64);
        let texts = ["alpha", "beta", "alpha"];
        let batch = e.embed_batch(&texts).unwrap();
        let loops: Vec<_> = texts.iter().map(|t| e.embed(t).unwrap()).collect();
        assert_eq!(batch, loops);
        // alpha == alpha
        assert!((cos(&batch[0], &batch[2]) - 1.0).abs() < 1e-4);
    }
}
