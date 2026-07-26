//! File storage (§4, §4.1).
//!
//! **Files are the source of truth** — no proprietary binary database, in the
//! Obsidian spirit. Layout:
//!
//! ```text
//! <root>/
//!   <doc-id>/            # one directory per living document
//!     <node-id>.html     # one HTML file per concept node (§4.1)
//! ```
//!
//! The directory only serves loose human navigation; the real graph
//! relationships live in `[[links]]`/attributes *inside* the content (§4), not
//! in the filesystem position. Writes are atomic (tmp + rename) so a node is not
//! corrupted if a stream/generation is interrupted (§16, resumability).
//!
//! Not yet consumed at runtime by everything — the curriculum loop (Phase 1)
//! uses this module; hence the temporary `allow(dead_code)`.
#![allow(dead_code)]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use learnive_core::{Node, ParseError};

/// Storage errors.
#[derive(Debug)]
pub enum StoreError {
    /// ID with an unsafe character (path-traversal defense).
    InvalidId(String),
    /// Nonexistent document or node.
    NotFound(PathBuf),
    /// I/O failure.
    Io(io::Error),
    /// Malformed node HTML.
    Parse(ParseError),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::InvalidId(id) => write!(f, "unsafe identifier: {id:?}"),
            StoreError::NotFound(p) => write!(f, "not found: {}", p.display()),
            StoreError::Io(e) => write!(f, "I/O error: {e}"),
            StoreError::Parse(e) => write!(f, "malformed node: {e}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> Self {
        StoreError::Io(e)
    }
}

impl From<ParseError> for StoreError {
    fn from(e: ParseError) -> Self {
        StoreError::Parse(e)
    }
}

type Result<T> = std::result::Result<T, StoreError>;

/// Storage rooted at a data directory.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Opens (creating if needed) the storage at the given root.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// Creates a living document's directory. Idempotent.
    pub fn create_document(&self, doc_id: &str) -> Result<()> {
        ensure_safe_id(doc_id)?;
        fs::create_dir_all(self.doc_dir(doc_id))?;
        Ok(())
    }

    /// Lists the living-document IDs (subdirectories of the root).
    pub fn list_documents(&self) -> Result<Vec<String>> {
        let mut docs = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && let Some(name) = entry.file_name().to_str()
            {
                docs.push(name.to_string());
            }
        }
        docs.sort();
        Ok(docs)
    }

    /// Writes a node to `<doc>/<node>.html` (atomic write). Creates the document
    /// directory if it does not exist yet.
    pub fn write_node(&self, node: &Node) -> Result<()> {
        ensure_safe_id(&node.doc_id)?;
        ensure_safe_id(&node.node_id)?;
        fs::create_dir_all(self.doc_dir(&node.doc_id))?;
        write_atomic(
            &self.node_path(&node.doc_id, &node.node_id),
            &node.to_html(),
        )?;
        Ok(())
    }

    /// Reads and parses a node.
    pub fn read_node(&self, doc_id: &str, node_id: &str) -> Result<Node> {
        ensure_safe_id(doc_id)?;
        ensure_safe_id(node_id)?;
        let path = self.node_path(doc_id, node_id);
        let html = match fs::read_to_string(&path) {
            Ok(h) => h,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(StoreError::NotFound(path));
            }
            Err(e) => return Err(e.into()),
        };
        Ok(Node::parse(&html)?)
    }

    /// Lists a document's node IDs (the `*.html` files).
    pub fn list_nodes(&self, doc_id: &str) -> Result<Vec<String>> {
        ensure_safe_id(doc_id)?;
        let dir = self.doc_dir(doc_id);
        let mut nodes = Vec::new();
        let read = match fs::read_dir(&dir) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(StoreError::NotFound(dir)),
            Err(e) => return Err(e.into()),
        };
        for entry in read {
            let entry = entry?;
            let name = entry.file_name();
            if let Some(name) = name.to_str()
                && let Some(id) = name.strip_suffix(".html")
            {
                nodes.push(id.to_string());
            }
        }
        nodes.sort();
        Ok(nodes)
    }

    /// Append-only convenience (§4.3): loads the node, appends an interaction
    /// item, and rewrites. Never touches the content layer.
    pub fn append_interaction(
        &self,
        doc_id: &str,
        node_id: &str,
        item: learnive_core::InteractionItem,
    ) -> Result<()> {
        let mut node = self.read_node(doc_id, node_id)?;
        node.push_interaction(item);
        self.write_node(&node)
    }

    /// Writes a **server-only** sidecar file inside the document's directory
    /// (e.g. `outline.json`, `<node>.rubric.json`). Never served to the client —
    /// it is what keeps the locked rubric (§8) invisible to the student.
    pub fn write_doc_file(&self, doc_id: &str, filename: &str, contents: &str) -> Result<()> {
        ensure_safe_id(doc_id)?;
        ensure_safe_filename(filename)?;
        fs::create_dir_all(self.doc_dir(doc_id))?;
        write_atomic(&self.doc_dir(doc_id).join(filename), contents)?;
        Ok(())
    }

    /// Reads a sidecar file from the document's directory.
    pub fn read_doc_file(&self, doc_id: &str, filename: &str) -> Result<String> {
        ensure_safe_id(doc_id)?;
        ensure_safe_filename(filename)?;
        let path = self.doc_dir(doc_id).join(filename);
        match fs::read_to_string(&path) {
            Ok(s) => Ok(s),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(StoreError::NotFound(path)),
            Err(e) => Err(e.into()),
        }
    }

    fn doc_dir(&self, doc_id: &str) -> PathBuf {
        self.root.join(doc_id)
    }

    fn node_path(&self, doc_id: &str, node_id: &str) -> PathBuf {
        self.doc_dir(doc_id).join(format!("{node_id}.html"))
    }
}

/// Rejects IDs that could escape the data directory or collide with the file
/// suffix. ASCII alphanumeric, `-` and `_` only.
fn ensure_safe_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(StoreError::InvalidId(id.to_string()))
    }
}

/// Like `ensure_safe_id`, but allows a single extension suffix (e.g.
/// `outline.json`, `n1.rubric.json`). Still blocks `/` and `..`.
fn ensure_safe_filename(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !name.contains("..");
    if ok {
        Ok(())
    } else {
        Err(StoreError::InvalidId(name.to_string()))
    }
}

/// Writes by saving to a temporary file and renaming over the target — rename is
/// atomic on the same filesystem, so a half-written file never remains.
fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use learnive_core::{Anchor, InteractionItem};

    fn sample_node(doc_id: &str, node_id: &str) -> Node {
        let html = format!(
            r#"<article data-node-id="{node_id}" data-doc-id="{doc_id}">
  <section data-layer="content"><p data-block-id="b1">content</p></section>
  <section data-layer="interaction"></section>
</article>"#
        );
        Node::parse(&html).unwrap()
    }

    #[test]
    fn write_then_read_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();

        let node = sample_node("algebra", "n1");
        store.write_node(&node).unwrap();

        let read = store.read_node("algebra", "n1").unwrap();
        assert_eq!(read.node_id, "n1");
        assert_eq!(read.doc_id, "algebra");
        assert_eq!(read.content.blocks.len(), 1);
    }

    #[test]
    fn lists_documents_and_nodes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();

        store.write_node(&sample_node("algebra", "n1")).unwrap();
        store.write_node(&sample_node("algebra", "n2")).unwrap();
        store.write_node(&sample_node("history", "n1")).unwrap();

        assert_eq!(store.list_documents().unwrap(), vec!["algebra", "history"]);
        assert_eq!(store.list_nodes("algebra").unwrap(), vec!["n1", "n2"]);
    }

    #[test]
    fn append_interaction_persists_and_freezes_content() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        store.write_node(&sample_node("algebra", "n1")).unwrap();
        let frozen = store.read_node("algebra", "n1").unwrap().content.html;

        store
            .append_interaction(
                "algebra",
                "n1",
                InteractionItem::Annotation {
                    id: "a1".to_string(),
                    anchor: Anchor::block("b1"),
                    body_html: "<p>note</p>".to_string(),
                },
            )
            .unwrap();

        let reloaded = store.read_node("algebra", "n1").unwrap();
        assert_eq!(reloaded.interaction.len(), 1);
        assert_eq!(reloaded.content.html, frozen);
    }

    #[test]
    fn rejects_path_traversal_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        assert!(matches!(
            store.read_node("../etc", "passwd"),
            Err(StoreError::InvalidId(_))
        ));
        assert!(matches!(
            store.create_document("a/b"),
            Err(StoreError::InvalidId(_))
        ));
    }

    #[test]
    fn doc_files_round_trip_and_reject_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();

        store
            .write_doc_file("algebra", "n1.rubric.json", r#"{"objectives":[]}"#)
            .unwrap();
        assert_eq!(
            store.read_doc_file("algebra", "n1.rubric.json").unwrap(),
            r#"{"objectives":[]}"#
        );

        // Sidecars do not show up as nodes.
        store.write_node(&sample_node("algebra", "n1")).unwrap();
        assert_eq!(store.list_nodes("algebra").unwrap(), vec!["n1"]);

        assert!(matches!(
            store.read_doc_file("algebra", "../secret"),
            Err(StoreError::InvalidId(_))
        ));
        assert!(matches!(
            store.write_doc_file("algebra", ".hidden", "x"),
            Err(StoreError::InvalidId(_))
        ));
    }

    #[test]
    fn missing_node_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        store.create_document("algebra").unwrap();
        assert!(matches!(
            store.read_node("algebra", "ghost"),
            Err(StoreError::NotFound(_))
        ));
    }
}
