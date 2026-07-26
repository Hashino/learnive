//! learnive-core — contrato de dados do nó (§4.3) e ancoragem.
//!
//! De propósito **sem** dependências de servidor (tokio/axum): este crate
//! compila para `wasm32`, então a lógica de ancoragem roda idêntica no servidor
//! e no cliente (§3, §4.3 — escrita uma vez em Rust, sem reimplementar em JS e
//! sem drift).
//!
//! Um nó é um arquivo HTML no dialeto da app (§4.2) com duas camadas lógicas:
//!
//! - **conteúdo** — congelado na criação, todo bloco endereçável com
//!   `data-block-id` estável;
//! - **interação** — append-only (anotações, threads de Q&A/remediação) que
//!   *referenciam* IDs da camada de conteúdo, nunca a alteram.

pub mod anchor;
pub mod node;

pub use anchor::{Anchor, QuoteSelector, ResolvedAnchor, resolve_quote};
pub use node::{
    Citation, ContentBlock, ContentLayer, Exercise, InteractionItem, Node, Objective,
    ObjectiveType, ParseError, ThreadKind,
};
