//! Canned acquisition backend — demo mode and tests, no network.
//!
//! Lets the whole grounding pipeline (§10/§11) run keyless and offline, exactly
//! as `ai::MockProvider` lets the generation loop run keyless. The content is
//! deliberately shaped like a real OER textbook excerpt (title, chapter/section
//! locators, a CC license) so downstream code exercises the real code paths.

use super::{
    FetchedSource, LocalPdfSource, Origin, SearchHit, Section, SourceError, SourceKind, SourceMeta,
    corpus_id,
};

/// Bibliographic identity of demo mode's two canned library fixtures —
/// `(title, author)`. The single source of truth for both `demo_responder`'s
/// scripted reading-list JSON (`api::provider`) and [`seed_demo_library`]
/// below, so the two can never drift apart the way two independently typed
/// string literals eventually would.
pub(crate) const DEMO_BOOK_1: (&str, &str) = ("Demo Foundations", "Demo Author");
pub(crate) const DEMO_BOOK_2: (&str, &str) = ("Demo Document", "Demo Author");

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

/// Writes a minimal but acervo-gate-passing PDF fixture: `PAGE_COUNT` pages
/// (8, matching `source::acervo`'s own `MIN_PLAUSIBLE_BOOK_PAGES`, so a
/// `"kind":"book"` expected item doesn't trip the "too short to plausibly be
/// the claimed book" identity check), `/Info` Title+Author metadata, and the
/// same text
/// on the first page (belt-and-suspenders: identity matches on metadata OR
/// first-page text, either is enough). Promoted out of `app::tests` (S27i,
/// PLAN.md, 2026-08-30) so [`seed_demo_library`] can call the same builder
/// the router-test harness already relied on, instead of a third
/// hand-rolled copy.
pub(crate) fn write_book_pdf(path: &std::path::Path, title: &str, author: &str) {
    use lopdf::{Document, Object, Stream, dictionary};

    const PAGE_COUNT: usize = 8;

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Courier",
    });
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font_id },
    });
    let content = format!("BT /F1 12 Tf 20 700 Td ({title}, by {author}.) Tj ET");
    let content_id = doc.add_object(Stream::new(dictionary! {}, content.into_bytes()));
    let mut page_ids = Vec::with_capacity(PAGE_COUNT);
    for _ in 0..PAGE_COUNT {
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        page_ids.push(page_id);
    }
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => page_ids.iter().map(|&id| id.into()).collect::<Vec<Object>>(),
            "Count" => PAGE_COUNT as i64,
        }),
    );
    let info_id = doc.add_object(dictionary! {
        "Title" => Object::string_literal(title),
        "Author" => Object::string_literal(author),
    });
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);
    doc.trailer.set("Info", info_id);
    doc.save(path).expect("save book pdf fixture");
}

/// Eagerly, synchronously seeds `<data_dir>/library/` with demo mode's two
/// canned PDF fixtures (S27i, PLAN.md, 2026-08-30) so a **live**
/// `LEARNIVE_DEMO=1` run — not just the router-test harness — can pass the
/// acervo gate and exercise the real citation → source-panel → native-PDF-
/// viewer path end to end.
///
/// PLAN.md originally specified this as "`MockSource` writes the fixture
/// into `<data>/library/` on first call" — that design was racy and wrong:
/// `MockSource::fetch` only ever runs inside `api::cold_start::acquire`,
/// which `spawn_acquisition` fires via a detached `tokio::spawn` and never
/// awaits before `ensure_document_grounded` runs (`api/cold_start.rs`). The
/// acervo gate would then race the fixture write and lose most of the time
/// — exactly the failure mode `app/tests.rs`'s own `test_state_with_ai` doc
/// comment already flagged as a reason to seed the library directly instead
/// of going through `Source::Mock`/`acquire()` at all. This function is that
/// same eager-seed pattern, promoted from the test harness to a real
/// (non-`#[cfg(test)]`) call site: `app::AppState::new`, gated on
/// `LEARNIVE_DEMO` and run *before* any request can reach the acervo gate.
///
/// Only-if-absent: `data_dir` can be a real `LEARNIVE_DATA_DIR` (the user's
/// actual library), so this must never overwrite or duplicate a file that's
/// already there.
pub(crate) fn seed_demo_library(data_dir: impl AsRef<std::path::Path>) -> std::io::Result<()> {
    let library = LocalPdfSource::open(data_dir)?;
    for (filename, (title, author)) in [
        ("demo-foundations.pdf", DEMO_BOOK_1),
        ("demo-document.pdf", DEMO_BOOK_2),
    ] {
        let path = library.root().join(filename);
        if !path.exists() {
            write_book_pdf(&path, title, author);
        }
    }
    Ok(())
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
