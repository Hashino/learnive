//! Canned acquisition backend — demo mode and tests, no network.
//!
//! Lets the whole grounding pipeline (§10/§11) run keyless and offline, exactly
//! as `ai::MockProvider` lets the generation loop run keyless. The content is
//! deliberately shaped like a real OER textbook excerpt (title, chapter/section
//! locators, a CC license) so downstream code exercises the real code paths.

use super::{
    FetchedSource, Origin, SearchHit, Section, SourceError, SourceKind, SourceMeta, corpus_id,
};

/// A no-network source backend returning a single plausible OER book per query.
#[derive(Debug, Clone, Default)]
pub struct MockSource;

impl MockSource {
    pub fn new() -> Self {
        Self
    }

    pub async fn search(&self, query: &str) -> Result<Vec<SearchHit>, SourceError> {
        let query = query.trim();
        if query.is_empty() {
            return Err(SourceError::NoResult);
        }
        // One canned "OpenStax-like" hit whose title echoes the query, so tests
        // can assert the topic flowed through.
        Ok(vec![SearchHit {
            title: format!("Introduction to {query}"),
            authors: vec!["OpenStax".into()],
            kind: SourceKind::Book,
            origin: Origin::Mock,
            license: "CC BY 4.0".into(),
            handle: format!("mock:{query}"),
            pages: None,
            size_bytes: None,
        }])
    }

    pub async fn fetch(&self, hit: &SearchHit) -> Result<FetchedSource, SourceError> {
        let topic = hit
            .handle
            .strip_prefix("mock:")
            .unwrap_or(&hit.title)
            .to_string();
        let id = corpus_id(&hit.title, "mock");
        let sections = vec![
            {
                let text = format!(
                    "{topic} is a foundational idea. This section introduces its core \
                     definition and the vocabulary used to talk about it, grounding the \
                     learner before any exercise. A worked intuition precedes the formal \
                     statement so the concept is met concretely first."
                );
                Section {
                    locator: "chap:1;sec:1".into(),
                    title: format!("What is {topic}?"),
                    text,
                }
            },
            {
                let text = format!(
                    "Building on the definition, this section shows where {topic} is used \
                     and connects it to adjacent concepts, so the learner integrates it \
                     rather than memorizing it in isolation."
                );
                Section {
                    locator: "chap:1;sec:2".into(),
                    title: format!("Why {topic} matters"),
                    text,
                }
            },
        ];
        Ok(FetchedSource {
            meta: SourceMeta {
                id,
                title: hit.title.clone(),
                authors: hit.authors.clone(),
                kind: hit.kind,
                license: hit.license.clone(),
                origin: hit.origin.clone(),
                pdf_asset: None,
            },
            sections,
            pdf: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Source;

    #[tokio::test]
    async fn search_then_fetch_yields_normalized_source() {
        let src = Source::Mock(MockSource::new());
        let hits = src.search("limits").await.unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].title.contains("limits"));
        assert_eq!(hits[0].license, "CC BY 4.0");

        let fetched = src.fetch(&hits[0]).await.unwrap();
        assert_eq!(fetched.sections.len(), 2);
        assert_eq!(fetched.sections[0].locator, "chap:1;sec:1");
        assert!(fetched.char_len() > 0);
        assert!(fetched.meta.id.starts_with("introduction-to-limits-"));
    }

    #[tokio::test]
    async fn blank_query_has_no_result() {
        let src = Source::Mock(MockSource::new());
        assert!(matches!(
            src.search("   ").await,
            Err(SourceError::NoResult)
        ));
    }
}
