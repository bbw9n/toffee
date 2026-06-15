//! BGE-small-en-v1.5 embedder backed by candle + tokenizers.
//!
//! On first construction, the model is lazy-downloaded via `hf-hub` into
//! `cache_dir` (typically `$XDG_DATA_HOME/toffee/models/`). After that, the
//! embedder loads from disk and runs forward passes locally — CPU by
//! default, Metal when the `metal` cargo feature is enabled.
//!
//! Embeddings are CLS-pooled and L2-normalised, matching `HashEmbedder` so
//! the HNSW index can treat cosine similarity as dot product regardless of
//! which backend produced the vector.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
use hf_hub::api::sync::ApiBuilder;
use hf_hub::{Repo, RepoType};
use tokenizers::{
    PaddingParams, PaddingStrategy, Tokenizer, TruncationDirection, TruncationParams,
    TruncationStrategy,
};

use crate::{Embedder, Result, VectorError};

const REPO_ID: &str = "BAAI/bge-small-en-v1.5";
/// Stable identifier stamped on every embedding row so the worker knows
/// whether a memory's stored vector matches the currently-active model.
pub const BGE_MODEL_ID: &str = "bge-small-en-v1.5";
const MAX_SEQ_LEN: usize = 512;

pub struct BgeEmbedder {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
    dim: usize,
    // candle modules are Send + Sync, but Metal command queues serialize
    // ops anyway and some operations have observed contention under
    // concurrent forward passes. Serialising at the embedder boundary keeps
    // semantics predictable; forward latency dominates so the lock is not
    // a bottleneck in practice.
    forward_lock: Mutex<()>,
}

impl BgeEmbedder {
    /// Construct a new BGE-small embedder. `cache_dir` is used as the
    /// hf-hub cache root; missing weight files are downloaded on demand.
    pub fn new(cache_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(cache_dir)
            .map_err(|e| VectorError::Model(format!("create cache dir: {e}")))?;
        let api = ApiBuilder::new()
            .with_cache_dir(cache_dir.to_path_buf())
            .build()
            .map_err(|e| VectorError::Model(format!("hf-hub init: {e}")))?;
        let repo = api.repo(Repo::new(REPO_ID.to_string(), RepoType::Model));
        let config_path = repo
            .get("config.json")
            .map_err(|e| VectorError::Model(format!("download config.json: {e}")))?;
        let tokenizer_path = repo
            .get("tokenizer.json")
            .map_err(|e| VectorError::Model(format!("download tokenizer.json: {e}")))?;
        let weights_path = repo
            .get("model.safetensors")
            .map_err(|e| VectorError::Model(format!("download model.safetensors: {e}")))?;
        Self::load_from_paths(&config_path, &tokenizer_path, &weights_path)
    }

    /// Load from explicit file paths. Exposed for tests and for callers
    /// that manage their own download strategy.
    pub fn load_from_paths(
        config_path: &Path,
        tokenizer_path: &Path,
        weights_path: &Path,
    ) -> Result<Self> {
        let config_json = std::fs::read_to_string(config_path)
            .map_err(|e| VectorError::Model(format!("read config: {e}")))?;
        let config: Config = serde_json::from_str(&config_json)
            .map_err(|e| VectorError::Model(format!("parse config: {e}")))?;

        let mut tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|e| VectorError::Model(format!("load tokenizer: {e}")))?;
        let truncation = TruncationParams {
            max_length: MAX_SEQ_LEN,
            strategy: TruncationStrategy::LongestFirst,
            stride: 0,
            direction: TruncationDirection::Right,
        };
        tokenizer
            .with_truncation(Some(truncation))
            .map_err(|e| VectorError::Model(format!("set truncation: {e}")))?;
        // Padding params are harmless on single-sequence encode and would
        // matter the moment we add a batched fast-path.
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            ..PaddingParams::default()
        }));

        let device = pick_device();
        // SAFETY: from_mmaped_safetensors mmaps the weight file; the
        // mapping is owned by the resulting VarBuilder / BertModel and
        // outlives any tensor view into it. No aliasing.
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path.to_path_buf()], DTYPE, &device)
                .map_err(|e| VectorError::Model(format!("load weights: {e}")))?
        };
        let model = BertModel::load(vb, &config)
            .map_err(|e| VectorError::Model(format!("build model: {e}")))?;

        Ok(BgeEmbedder {
            model,
            tokenizer,
            device,
            dim: config.hidden_size,
            forward_lock: Mutex::new(()),
        })
    }

    fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| VectorError::Model(format!("tokenize: {e}")))?;
        let token_ids: Vec<u32> = encoding.get_ids().to_vec();
        let attention: Vec<u32> = encoding.get_attention_mask().to_vec();

        let _guard = self
            .forward_lock
            .lock()
            .map_err(|_| VectorError::Model("forward lock poisoned".into()))?;

        let token_ids_t = Tensor::new(token_ids.as_slice(), &self.device)
            .and_then(|t| t.unsqueeze(0))
            .map_err(|e| VectorError::Model(format!("tensor token_ids: {e}")))?;
        let attention_t = Tensor::new(attention.as_slice(), &self.device)
            .and_then(|t| t.unsqueeze(0))
            .map_err(|e| VectorError::Model(format!("tensor attention_mask: {e}")))?;
        let token_type_ids = token_ids_t
            .zeros_like()
            .map_err(|e| VectorError::Model(format!("tensor token_type_ids: {e}")))?;

        let hidden = self
            .model
            .forward(&token_ids_t, &token_type_ids, Some(&attention_t))
            .map_err(|e| VectorError::Model(format!("forward: {e}")))?;

        // hidden has shape [1, T, H]. CLS = position 0 along T.
        // narrow(dim=1, start=0, len=1) → [1, 1, H], then squeeze twice.
        let cls = hidden
            .narrow(1, 0, 1)
            .and_then(|t| t.squeeze(1))
            .and_then(|t| t.squeeze(0))
            .map_err(|e| VectorError::Model(format!("cls pool: {e}")))?;
        let mut v: Vec<f32> = cls
            .to_vec1::<f32>()
            .map_err(|e| VectorError::Model(format!("cls to_vec: {e}")))?;
        l2_normalize(&mut v);
        Ok(v)
    }
}

impl Embedder for BgeEmbedder {
    fn model(&self) -> &str {
        BGE_MODEL_ID
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        if text.trim().is_empty() {
            // Match HashEmbedder: empty input returns a zero vector rather
            // than running a forward pass on [CLS][SEP].
            return Ok(vec![0.0; self.dim]);
        }
        self.embed_one(text)
    }
}

fn pick_device() -> Device {
    #[cfg(all(feature = "metal", target_os = "macos"))]
    {
        if let Ok(d) = Device::new_metal(0) {
            return d;
        }
        tracing::warn!("metal feature enabled but new_metal(0) failed; falling back to CPU");
    }
    Device::Cpu
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

/// Default cache directory for BGE weights. Callers in the daemon should
/// use `toffee_core::paths::data_dir().join("models")`; this helper is
/// here so library users don't need to depend on `toffee-core` just to
/// find the conventional location.
pub fn default_cache_dir() -> PathBuf {
    if let Ok(p) = std::env::var("TOFFEE_MODELS_DIR") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    // Fall back to a sibling of $XDG_DATA_HOME/toffee/. Real callers
    // should pass an explicit path; this is just a last-resort default.
    dirs_data_home()
        .unwrap_or_else(std::env::temp_dir)
        .join("toffee")
        .join("models")
}

fn dirs_data_home() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("XDG_DATA_HOME") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    let home = std::env::var("HOME").ok().filter(|s| !s.is_empty())?;
    #[cfg(target_os = "macos")]
    {
        Some(PathBuf::from(home).join("Library/Application Support"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Some(PathBuf::from(home).join(".local/share"))
    }
}
