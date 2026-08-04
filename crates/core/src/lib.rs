//! learnive-core — node data contract (§4.3) and anchoring.
//!
//! Deliberately **free** of server dependencies (tokio/axum): this crate
//! compiles to `wasm32`, so the anchoring logic runs identically on the server
//! and the client (§3, §4.3 — written once in Rust, no JS reimplementation and
//! no drift).
//!
//! A node is an HTML file in the app dialect (§4.2) with two logical layers:
//!
//! - **content** — frozen at creation, every block addressable by a stable
//!   `data-block-id`;
//! - **interaction** — append-only (annotations, Q&A/remediation threads) that
//!   *reference* content-layer IDs and never mutate them.

pub mod anchor;
pub mod assemble;
pub mod node;

pub use anchor::{Anchor, QuoteSelector, ResolvedAnchor, resolve_quote};
pub use assemble::{
    ensure_block_ids, extract_block_by_id, prose_blocks_only, redact_interactive_blocks,
};
pub use node::{
    Citation, ContentBlock, ContentLayer, Exercise, InteractionItem, Node, Objective,
    ObjectiveType, ParseError, ThreadKind,
};
