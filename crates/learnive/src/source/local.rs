//! Local PDF library — the always-present fallback acquisition tier (§11.1,
//! PLAN.md S27a).
//!
//! Unlike the remote backends (`LibGen`/`SciHub`/`Mock`), a local library is not
//! **searched**, it's **matched**: the user drops PDFs into one global
//! `<data>/library/` directory (not nested under any living document — a book
//! used by two documents lives here once), and a reading-list item is matched
//! against what's on disk by bibliographic identity, not looked up by a free-text
//! query string (PLAN.md S28 item 5: *"biblioteca local não se pesquisa, se
//! casa (arquivo ↔ item esperado). O facade muda de forma, não só de
//! variante"*). That's why [`LocalPdfSource`] does not implement
//! `Source::search`/`Source::fetch` in the search-then-download shape those
//! methods assume — see [`super::Source::search`]/[`super::Source::fetch`]'s
//! `LocalPdf` arm, which returns `SourceError::Unsupported` and points here.
//!
//! **Scope of this slice (S27a) — deliberately minimal ("sem UI ainda"):**
//! prove the app can see a manually-placed PDF sitting in the library
//! directory. Just a directory scan yielding filename + file size — no PDF
//! parsing, no embedded-metadata extraction, no text/TOC/page-map reading.
//! That's S27b (text/TOC/pages) and S27c (the six-check acervo gate, which is
//! where real bibliographic identity matching happens). Building any of that
//! here would be the exact "adivinhar" the S27 narrative warns against:
//! matching by more than filename+size needs to actually look at the file.

use std::fs;
use std::path::{Path, PathBuf};

/// One PDF file found in the local library, with the cheap identity signal
/// available without parsing the PDF (full identity extraction is S27b/S27c's
/// job, not this slice's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryEntry {
    /// Filename within `<data>/library/`. **Not a contract** — S27's matching
    /// design explicitly rejects filename-as-identity (renaming a book by
    /// hand every time is repeated manual work and breaks on a typo); this is
    /// only what a cheap directory scan can see before any content is read.
    pub filename: String,
    pub size_bytes: u64,
}

/// The local PDF library (§11.1's fallback acquisition tier) — global,
/// singular, rooted at `<data>/library/`, not per-document (explicit user
/// decision, PLAN.md S27 "Decisões de implementação").
#[derive(Debug, Clone)]
pub struct LocalPdfSource {
    root: PathBuf,
}

impl LocalPdfSource {
    /// Opens (creating if needed) the library directory under a data
    /// directory. Mirrors `Corpus::open`'s idiom (`source/corpus.rs`).
    pub fn open(data_dir: impl AsRef<Path>) -> std::io::Result<Self> {
        let root = data_dir.as_ref().join("library");
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// The library directory's path, for callers that need it (e.g. a future
    /// step that opens a matched file for text/TOC extraction).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Scans the library directory for PDF files (top-level only, no
    /// subdirectory recursion — nothing in the plan asks for a nested layout
    /// yet), returning a listing sorted by filename for stable output. This is
    /// the whole capability this slice builds: proof the app can see a
    /// manually-placed PDF, nothing more.
    pub fn scan(&self) -> std::io::Result<Vec<LibraryEntry>> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            let is_pdf = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
            if !is_pdf {
                continue;
            }
            let metadata = entry.metadata()?;
            if !metadata.is_file() {
                continue;
            }
            let Some(filename) = path.file_name().and_then(|f| f.to_str()) else {
                continue;
            };
            entries.push(LibraryEntry {
                filename: filename.to_string(),
                size_bytes: metadata.len(),
            });
        }
        entries.sort_by(|a, b| a.filename.cmp(&b.filename));
        Ok(entries)
    }

    /// Looks up one library entry by exact filename — the cheapest possible
    /// "matching" primitive (lookup by identity, not by query). Real
    /// bibliographic matching (title/authors/embedded metadata against an
    /// expected reading-list item) is S27c's acervo gate; this is only a
    /// building block for it.
    pub fn get(&self, filename: &str) -> Option<LibraryEntry> {
        self.scan()
            .ok()?
            .into_iter()
            .find(|e| e.filename == filename)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal, structurally valid one-page PDF (the well-known "Hello
    /// World" template) — small enough to embed verbatim, real enough that a
    /// later slice's `%PDF` sniff/parse would accept it. This slice's own
    /// code never looks past the filename, so any bytes would do; using real
    /// PDF bytes keeps the fixture honest for whoever extends this test next.
    const MINIMAL_PDF: &[u8] = b"%PDF-1.1\n\
1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 300 144] >>\nendobj\n\
3 0 obj\n<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Times-Roman >> >> >> /Contents 4 0 R >>\nendobj\n\
4 0 obj\n<< /Length 44 >>\nstream\nBT /F1 18 Tf 0 0 Td (Hello World) Tj ET\nendstream\nendobj\n\
trailer\n<< /Root 1 0 R /Size 5 >>\n%%EOF";

    #[test]
    fn open_creates_the_library_directory() {
        let tmp = std::env::temp_dir().join(format!(
            "learnive-local-pdf-test-open-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let data_dir = tmp.join("data");
        assert!(!data_dir.join("library").exists());

        let _lib = LocalPdfSource::open(&data_dir).expect("open should create the dir");
        assert!(data_dir.join("library").is_dir());

        fs::remove_dir_all(&tmp).ok();
    }

    /// The bar the plan sets for S27a: "a test proves the app can see a
    /// manually-placed PDF". Places a real PDF fixture directly into
    /// `<data>/library/` (as a user would, by hand) and proves `scan` sees it
    /// with the right filename and size — no acquisition code path involved.
    #[test]
    fn scan_sees_a_manually_placed_pdf() {
        let tmp = std::env::temp_dir().join(format!(
            "learnive-local-pdf-test-scan-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let data_dir = tmp.join("data");
        let lib = LocalPdfSource::open(&data_dir).expect("open library");

        // Nothing placed yet: an empty, but working, scan.
        assert_eq!(lib.scan().expect("scan empty library"), Vec::new());

        // A non-PDF file dropped alongside must not show up.
        fs::write(lib.root().join("notes.txt"), b"not a pdf").unwrap();

        // The user manually places a PDF into the library directory.
        let pdf_path = lib
            .root()
            .join("Sipser - Introduction to the Theory of Computation.pdf");
        fs::write(&pdf_path, MINIMAL_PDF).unwrap();

        let seen = lib.scan().expect("scan after placing a pdf");
        assert_eq!(
            seen.len(),
            1,
            "only the PDF should be listed, not notes.txt"
        );
        assert_eq!(
            seen[0].filename,
            "Sipser - Introduction to the Theory of Computation.pdf"
        );
        assert_eq!(seen[0].size_bytes, MINIMAL_PDF.len() as u64);

        // The lookup-by-identity primitive finds it by exact filename too.
        let found = lib
            .get("Sipser - Introduction to the Theory of Computation.pdf")
            .expect("get should find the placed pdf");
        assert_eq!(found.size_bytes, MINIMAL_PDF.len() as u64);
        assert!(lib.get("does-not-exist.pdf").is_none());

        fs::remove_dir_all(&tmp).ok();
    }

    /// `Source::LocalPdf` fits the enum's shared facade even though it opts
    /// out of `search`/`fetch` (they're the wrong shape for matching) — the
    /// enum variant still has to be constructible and matchable like any
    /// other `Source`.
    #[tokio::test]
    async fn source_facade_reports_unsupported_for_search_and_fetch() {
        let tmp = std::env::temp_dir().join(format!(
            "learnive-local-pdf-test-facade-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let lib = LocalPdfSource::open(tmp.join("data")).expect("open library");
        let source = crate::source::Source::LocalPdf(lib);

        assert!(matches!(
            source.search("anything").await,
            Err(crate::source::SourceError::Unsupported(_))
        ));
        let dummy_hit = crate::source::SearchHit {
            title: "x".into(),
            authors: vec![],
            kind: crate::source::SourceKind::Book,
            origin: crate::source::Origin::Mock,
            license: String::new(),
            handle: "x".into(),
            pages: None,
            size_bytes: None,
        };
        assert!(matches!(
            source.fetch(&dummy_hit).await,
            Err(crate::source::SourceError::Unsupported(_))
        ));

        fs::remove_dir_all(&tmp).ok();
    }
}
