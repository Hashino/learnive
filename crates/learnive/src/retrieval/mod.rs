//! The embedding layer (§10, §7.1).
//!
//! After the corpus retirement (2026-09-09, user decision) there is no
//! whole-corpus vector index anymore: grounding searches the per-book PDF
//! page indexes the acervo gate builds (`source::search_index_cache`), and
//! this module now holds only the one thing that path still needs — the
//! swappable [`Embedder`] — plus the [`cosine`] it scores with. The same
//! layer is what §10 cross-document retrieval and §7.1 profile memory are
//! meant to reuse when they arrive.
//!
//! **Embedder is swappable** ([`Embedder`]): the default is `model2vec-rs`
//! static embeddings — real semantic vectors, pure-Rust inference (no
//! onnxruntime), model cached locally after first download. A different
//! embedder (a heavier local model, or an embeddings API) is a drop-in
//! behind the same interface; the per-book indexes, persistence and search
//! are identical.
#![allow(dead_code, unused_imports)]

use std::sync::Arc;

use model2vec_rs::model::StaticModel;
use text_splitter::{ChunkConfig, TextSplitter};

/// Default embedding model: potion-base-8M — 256-dim static embeddings, ~30 MB,
/// downloaded once to the HF cache. Good multilingual semantic quality for its size.
pub const DEFAULT_MODEL: &str = "minishlab/potion-base-8M";

/// Target chunk size / overlap in characters (§10). ~700 keeps a chunk to a few
/// sentences — a citable unit — with overlap so a concept spanning a boundary
/// stays retrievable. Used by the acervo gate's per-book page index
/// (`source::acervo::build_index_cache` chunks PAGE text through this).
const CHUNK_SIZE: usize = 700;
const CHUNK_OVERLAP: usize = 120;

/// Splits a page's text into overlapping, sentence-aware chunks
/// (`text-splitter`) — shared by the acervo gate's per-book index builder.
pub(crate) fn chunk_text(text: &str) -> Vec<String> {
    let cfg = ChunkConfig::new(CHUNK_SIZE)
        .with_overlap(CHUNK_OVERLAP)
        .expect("overlap < capacity");
    TextSplitter::new(cfg)
        .chunks(text)
        .map(|s| s.to_string())
        .collect()
}

/// Swappable text→vector embedder (§10). Same-idiom facade as `ai::Provider` /
/// `source::Source`: a new embedder is a new variant, call sites unchanged.
#[derive(Clone)]
pub enum Embedder {
    /// `model2vec-rs` static embeddings — semantic, offline after first download.
    Static { model: Arc<StaticModel>, id: String },
    /// Deterministic, offline, non-semantic bag-of-hashed-words embedding —
    /// no model file, no download, no network. Exists solely so integration
    /// tests (and the keyless demo path, §22) can exercise the real
    /// retrieval/acervo-gate pipeline (S27m's `ensure_document_grounded` +
    /// `ground_node` both hard-require *some* `Embedder`) without paying for
    /// a real model load in every test run. Never selected by
    /// `build_ai`/any real-user config path — added 2026-08-29 alongside
    /// the demo-mode library fixtures in `app::tests`. Cosine similarity between two `Mock` vectors correlates
    /// with shared vocabulary, which is enough for tests to get non-empty,
    /// plausible-looking retrieval hits; it is not a real semantic space.
    Mock,
}

/// Fixed dimensionality of [`Embedder::Mock`]'s vectors — arbitrary, just
/// small enough to be cheap and large enough that hash collisions between
/// unrelated words are rare in test-sized vocabularies.
const MOCK_EMBED_DIM: usize = 64;

fn mock_embed(text: &str) -> Vec<f32> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut v = vec![0f32; MOCK_EMBED_DIM];
    for word in text.split_whitespace() {
        let mut hasher = DefaultHasher::new();
        word.to_lowercase().hash(&mut hasher);
        let idx = (hasher.finish() as usize) % MOCK_EMBED_DIM;
        v[idx] += 1.0;
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
    v
}

impl Embedder {
    /// Loads the static embedding model (downloading + caching it on first use).
    /// Fails only if the model cannot be fetched — the caller then runs without
    /// grounding rather than crashing (grounding is an enhancement, §10).
    pub fn load(model_id: &str) -> Result<Self, String> {
        let model = StaticModel::from_pretrained(model_id, None, None, None)
            .map_err(|e| format!("load embedding model {model_id}: {e}"))?;
        Ok(Embedder::Static {
            model: Arc::new(model),
            id: model_id.to_string(),
        })
    }

    /// Loads the default embedder ([`DEFAULT_MODEL`]).
    pub fn default_model() -> Result<Self, String> {
        Self::load(DEFAULT_MODEL)
    }

    /// Embeds one string.
    pub fn embed(&self, text: &str) -> Vec<f32> {
        self.embed_batch(std::slice::from_ref(&text.to_string()))
            .into_iter()
            .next()
            .unwrap_or_default()
    }

    /// Embeds a batch (the model vectorizes many texts in one call — used at index
    /// build time so a whole corpus is embedded efficiently).
    pub fn embed_batch(&self, texts: &[String]) -> Vec<Vec<f32>> {
        match self {
            Embedder::Static { model, .. } => model.encode(texts),
            Embedder::Mock => texts.iter().map(|t| mock_embed(t)).collect(),
        }
    }

    /// Stable tag persisted with the index, so a reindex is triggered if the
    /// embedder (and thus the vector space) changed since the index was built.
    pub fn tag(&self) -> String {
        match self {
            Embedder::Static { id, .. } => format!("model2vec:{id}"),
            Embedder::Mock => "mock".to_string(),
        }
    }
}

/// Cosine similarity of two vectors. (Kept explicit rather than pulling a linear
/// algebra crate for three lines; the embedding itself is the crate's job.)
pub(crate) fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_basics() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0, "length mismatch → 0");
    }

    /// Real semantic embedding via model2vec. Ignored by default (downloads the
    /// model); run with `cargo test -p learnive embedder_live -- --ignored`.
    #[test]
    #[ignore = "downloads the embedding model"]
    fn embedder_live_ranks_semantically() {
        let e = Embedder::default_model().expect("load model");
        let a = e.embed("the limit of a function as x approaches a value");
        let b = e.embed("a function's limit as the input approaches some value");
        let c = e.embed("photosynthesis converts sunlight into chemical energy");
        assert!(
            cosine(&a, &b) > cosine(&a, &c) + 0.3,
            "paraphrase must rank well above unrelated text"
        );
    }
}
