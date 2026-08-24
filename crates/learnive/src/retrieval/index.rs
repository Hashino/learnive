//! Rebuildable vector index (§10).
//!
//! Persisted at `<data>/index/vectors.json` — a **derived cache**, never a source
//! of truth: it is reconstructed from the corpus files on demand, and deleting it
//! only forces a reindex (§4/§10). Flat cosine search over the chunks; a binary
//! ANN store (HNSW/sqlite) is the scale upgrade, behind this same API.

use std::fs;
use std::path::{Path, PathBuf};

use super::{Chunk, Embedder, chunk_text, cosine};
use crate::source::{Corpus, CorpusError};

/// A chunk plus its embedding, as stored in the index.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct IndexedChunk {
    #[serde(flatten)]
    chunk: Chunk,
    vector: Vec<f32>,
}

/// A search result: the chunk and its cosine score in `[-1, 1]`.
#[derive(Debug, Clone)]
pub struct Retrieved {
    pub chunk: Chunk,
    pub score: f32,
}

/// The persisted vector index (embedder tag + indexed chunks).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct VectorIndex {
    /// Which embedder built this — a mismatch forces a rebuild (vector spaces differ).
    embedder_tag: String,
    chunks: Vec<IndexedChunk>,
}

impl VectorIndex {
    /// Builds an index over the whole corpus with the given embedder. Chunks are
    /// collected first and embedded in one batch (the model vectorizes many texts
    /// far faster than one-at-a-time).
    pub fn build(corpus: &Corpus, embedder: &Embedder) -> Result<Self, CorpusError> {
        let mut chunks: Vec<Chunk> = Vec::new();
        for meta in corpus.list()? {
            let source = corpus.load(&meta.id)?;
            for section in &source.sections {
                for text in chunk_text(&section.text) {
                    chunks.push(Chunk {
                        source_id: meta.id.clone(),
                        locator: section.locator.clone(),
                        source_title: meta.title.clone(),
                        section_title: section.title.clone(),
                        text,
                    });
                }
            }
        }
        let texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
        let vectors = if texts.is_empty() {
            Vec::new()
        } else {
            embedder.embed_batch(&texts)
        };
        let indexed = chunks
            .into_iter()
            .zip(vectors)
            .map(|(chunk, vector)| IndexedChunk { chunk, vector })
            .collect();
        Ok(Self {
            embedder_tag: embedder.tag(),
            chunks: indexed,
        })
    }

    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// Top-`k` chunks by cosine similarity to the query vector, above `min_score`.
    /// At most one chunk per (source, locator) so results cite distinct passages.
    pub fn search(&self, query_vec: &[f32], k: usize, min_score: f32) -> Vec<Retrieved> {
        let mut scored: Vec<Retrieved> = self
            .chunks
            .iter()
            .map(|c| Retrieved {
                chunk: c.chunk.clone(),
                score: cosine(query_vec, &c.vector),
            })
            .filter(|r| r.score > min_score)
            .collect();
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::with_capacity(k);
        for r in scored {
            if seen.insert((r.chunk.source_id.clone(), r.chunk.locator.clone())) {
                out.push(r);
                if out.len() == k {
                    break;
                }
            }
        }
        out
    }
}

/// Facade joining an [`Embedder`] with a persisted [`VectorIndex`]. The rest of
/// the app grounds a query by relevance without knowing how vectors are made or
/// stored — the §10 seam that a hosted/ANN backend slots into.
pub struct Retriever {
    embedder: Embedder,
    index: VectorIndex,
    path: PathBuf,
}

impl Retriever {
    /// Opens the index from disk, rebuilding it if missing, unreadable, or built
    /// by a different embedder. Always returns a usable retriever (empty if the
    /// corpus is empty — grounding then simply contributes nothing).
    pub fn open(
        data_dir: impl AsRef<Path>,
        corpus: &Corpus,
        embedder: Embedder,
    ) -> Result<Self, CorpusError> {
        let dir = data_dir.as_ref().join("index");
        fs::create_dir_all(&dir)?;
        let path = dir.join("vectors.json");

        let loaded = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<VectorIndex>(&b).ok())
            .filter(|idx| idx.embedder_tag == embedder.tag());

        let index = match loaded {
            Some(idx) => idx,
            None => {
                let idx = VectorIndex::build(corpus, &embedder)?;
                persist(&path, &idx)?;
                idx
            }
        };
        Ok(Self {
            embedder,
            index,
            path,
        })
    }

    /// Forces a rebuild from the corpus (call after acquiring a new source, §11).
    pub fn reindex(&mut self, corpus: &Corpus) -> Result<(), CorpusError> {
        self.index = VectorIndex::build(corpus, &self.embedder)?;
        persist(&self.path, &self.index)?;
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// The embedder instance backing this index — reused (never a second
    /// model load) by callers that need a raw text→vector embedding outside
    /// the corpus index itself, e.g. §S15's cross-document prerequisite
    /// matching (a separate, small, in-memory comparison over node titles,
    /// not the corpus `VectorIndex`).
    pub fn embedder(&self) -> &Embedder {
        &self.embedder
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// Retrieves the top-`k` passages relevant to `query` for grounding (§10).
    pub fn retrieve(&self, query: &str, k: usize) -> Vec<Retrieved> {
        if self.index.is_empty() || query.trim().is_empty() {
            return Vec::new();
        }
        let qv = self.embedder.embed(query);
        // A small positive floor drops near-orthogonal noise.
        self.index.search(&qv, k, 0.01)
    }
}

/// Atomic write of the index (tmp + rename), mirroring the store/corpus.
fn persist(path: &Path, index: &VectorIndex) -> Result<(), CorpusError> {
    let json = serde_json::to_vec(index).map_err(|e| CorpusError::Decode(e.to_string()))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &json)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(source: &str, locator: &str) -> Chunk {
        Chunk {
            source_id: source.into(),
            locator: locator.into(),
            source_title: "Calculus".into(),
            section_title: locator.into(),
            text: format!("text for {source} {locator}"),
        }
    }

    /// Synthetic index — exercises ranking/dedup/top-k offline (no embedder).
    fn synthetic() -> VectorIndex {
        VectorIndex {
            embedder_tag: "test".into(),
            chunks: vec![
                IndexedChunk {
                    chunk: chunk("calc", "sec:2.1"),
                    vector: vec![1.0, 0.0, 0.0],
                },
                IndexedChunk {
                    // Same (source, locator) as the first → must be deduped.
                    chunk: chunk("calc", "sec:2.1"),
                    vector: vec![0.9, 0.1, 0.0],
                },
                IndexedChunk {
                    chunk: chunk("calc", "sec:3.1"),
                    vector: vec![0.2, 1.0, 0.0],
                },
            ],
        }
    }

    #[test]
    fn search_ranks_dedups_and_caps() {
        let idx = synthetic();
        let hits = idx.search(&[1.0, 0.0, 0.0], 5, 0.01);
        assert_eq!(hits.len(), 2, "3 chunks, one deduped by (source,locator)");
        assert_eq!(hits[0].chunk.locator, "sec:2.1", "closest ranks first");
        assert!(hits[0].score > hits[1].score);
        // top-k cap.
        assert_eq!(idx.search(&[1.0, 0.0, 0.0], 1, 0.01).len(), 1);
        // min_score filters the orthogonal chunk.
        let strict = idx.search(&[1.0, 0.0, 0.0], 5, 0.5);
        assert!(strict.iter().all(|r| r.chunk.locator == "sec:2.1"));
    }

    #[test]
    fn persist_and_reload_offline() {
        let dir = std::env::temp_dir().join(format!("learnive-idx-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vectors.json");
        let idx = synthetic();
        persist(&path, &idx).unwrap();

        let bytes = fs::read(&path).unwrap();
        let reloaded: VectorIndex = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(reloaded.len(), idx.len());
        assert_eq!(reloaded.embedder_tag, "test");
        fs::remove_dir_all(&dir).ok();
    }

    /// Full pipeline with the real model2vec embedder + corpus. Ignored by
    /// default (downloads the model); run with
    /// `cargo test -p learnive retriever_live -- --ignored --nocapture`.
    #[test]
    #[ignore = "downloads the embedding model"]
    fn retriever_live_end_to_end() {
        use crate::source::{FetchedSource, Origin, Section, SourceKind, SourceMeta};

        let dir = std::env::temp_dir().join(format!("learnive-idxlive-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        let corpus = Corpus::open(&dir).unwrap();
        corpus
            .store(&FetchedSource {
                meta: SourceMeta {
                    id: "calc-abc123".into(),
                    title: "Calculus".into(),
                    authors: vec![],
                    kind: SourceKind::Book,
                    license: "CC BY 4.0".into(),
                    origin: Origin::OpenStax,
                    handle: String::new(),
                    complete: true,
                    pdf_asset: None,
                },
                sections: vec![
                    Section {
                        locator: "sec:2.1".into(),
                        title: "Limits".into(),
                        text: "A limit describes the value that a function approaches as the \
                               input approaches some value. Limits are the foundation of calculus."
                            .into(),
                        html: String::new(),
                    },
                    Section {
                        locator: "sec:3.1".into(),
                        title: "Derivatives".into(),
                        text: "A derivative measures the instantaneous rate of change of a \
                               function, defined via a limit of average rates of change."
                            .into(),
                        html: String::new(),
                    },
                ],
                pdf: None,
            })
            .unwrap();

        let embedder = Embedder::default_model().expect("load model");
        let retr = Retriever::open(&dir, &corpus, embedder).unwrap();
        assert!(!retr.is_empty());

        let hits = retr.retrieve("how fast is the function changing at a point", 2);
        assert_eq!(
            hits[0].chunk.locator, "sec:3.1",
            "rate-of-change → derivatives"
        );
        assert_eq!(hits[0].chunk.source_id, "calc-abc123");

        // Persisted: a second open loads without rebuilding.
        let embedder2 = Embedder::default_model().expect("load model");
        let retr2 = Retriever::open(&dir, &corpus, embedder2).unwrap();
        assert_eq!(retr2.len(), retr.len());

        fs::remove_dir_all(&dir).ok();
    }
}
