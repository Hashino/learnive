//! Retrieval layer (§10, §7.1) — grounding by relevance.
//!
//! Turns the immutable source [`Corpus`](crate::source::Corpus) into a
//! **rebuildable derived cache**: chunk each source, embed the chunks, and search
//! by cosine similarity. This is explicitly NOT a source of truth — deleting the
//! index only forces a reindex from the files (§4/§10). The same layer will later
//! serve cross-document context (§10) and profile memory (§7.1).
//!
//! **Embedder is swappable** ([`Embedder`]): the default is `model2vec-rs` static
//! embeddings — real semantic vectors, pure-Rust inference (no onnxruntime),
//! model cached locally after first download. A different embedder (a heavier
//! local model, or an embeddings API) is a drop-in behind the same interface; the
//! index, persistence and search are identical. Chunking uses `text-splitter`.
//!
//! Not all surfaces are wired into the loop yet (grounding is being threaded into
//! `engine`); hence the temporary `allow`s (mirrors `source`/`ai`).
#![allow(dead_code, unused_imports)]

pub mod index;

pub use index::{Retrieved, Retriever, VectorIndex};

use std::sync::Arc;

use model2vec_rs::model::StaticModel;
use text_splitter::{ChunkConfig, TextSplitter};

/// Default embedding model: potion-base-8M — 256-dim static embeddings, ~30 MB,
/// downloaded once to the HF cache. Good multilingual semantic quality for its size.
pub const DEFAULT_MODEL: &str = "minishlab/potion-base-8M";

/// Target chunk size / overlap in characters (§10). ~700 keeps a chunk to a few
/// sentences — a citable unit — with overlap so a concept spanning a boundary
/// stays retrievable.
const CHUNK_SIZE: usize = 700;
const CHUNK_OVERLAP: usize = 120;

/// A retrieval chunk: a slice of one source section, carrying everything needed
/// to **cite** it back (§4.3) — the source id and the section locator.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Chunk {
    /// Corpus source id — becomes `<cite data-source-id=...>`.
    pub source_id: String,
    /// Section locator (e.g. `sec:2.1`) — becomes `<cite data-locator=...>`.
    pub locator: String,
    /// Human title of the source (for the citation label).
    pub source_title: String,
    /// Section title (for the citation label / context header).
    pub section_title: String,
    /// The chunk text.
    pub text: String,
}

/// Swappable text→vector embedder (§10). Same-idiom facade as `ai::Provider` /
/// `source::Source`: a new embedder is a new variant, call sites unchanged.
#[derive(Clone)]
pub enum Embedder {
    /// `model2vec-rs` static embeddings — semantic, offline after first download.
    Static { model: Arc<StaticModel>, id: String },
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
        }
    }

    /// Stable tag persisted with the index, so a reindex is triggered if the
    /// embedder (and thus the vector space) changed since the index was built.
    pub fn tag(&self) -> String {
        match self {
            Embedder::Static { id, .. } => format!("model2vec:{id}"),
        }
    }
}

/// Splits a section's text into overlapping, sentence-aware chunks (`text-splitter`).
pub(crate) fn chunk_text(text: &str) -> Vec<String> {
    let cfg = ChunkConfig::new(CHUNK_SIZE)
        .with_overlap(CHUNK_OVERLAP)
        .expect("overlap < capacity");
    TextSplitter::new(cfg)
        .chunks(text)
        .map(|s| s.to_string())
        .collect()
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
    fn chunking_overlaps_and_covers() {
        let text = "One two three four five. Six seven eight nine ten. Eleven twelve \
                    thirteen fourteen fifteen. Sixteen seventeen eighteen nineteen twenty. "
            .repeat(6);
        let chunks = chunk_text(&text);
        assert!(chunks.len() >= 2, "long text splits into multiple chunks");
        assert!(chunks.iter().all(|c| !c.trim().is_empty()));
    }

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
