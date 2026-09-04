//! Curriculum-loop endpoints (§6, §8, §8.2).
//!
//! Flow: cold start (POST proposes an editable objective, POST confirms it and
//! creates the document + outline anchored on it, §6.1/§S4) → node generation,
//! addressed by the outline item's stable id and gated on its prerequisite
//! edges (POST that **streams** the prose token by token in SSE format, and
//! may pause on a `plan` move's proposed outline revision awaiting approval,
//! §5/§S5) → answer (POST grades against the locked rubric; on failure opens
//! remediation §8.2; on success signals to advance). `GET .../outline` reads
//! the graph's current gate state and `POST .../nodes/{id}/skip` defers a
//! reachable node without answering it (§S5).
//!
//! On streaming and §3.1: §3 asks for SSE, but the browser's `EventSource` does
//! not send a token header nor POST, and §3.1 forbids state-changing GET. So we
//! stream the **SSE wire format over a POST** (read via `fetch`): it keeps the §3
//! semantics and honors §3.1 (POST + header token + Origin).

use axum::{
    Json,
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

use learnive_core::{
    Anchor, InteractionItem, Node, ObjectiveType, ThreadKind, ensure_block_ids, render_math,
};

use crate::ai::{Ai, MockProvider, Models, OpenAiCompat, Provider, Tier};
use crate::app::AppState;
use crate::config::{AppConfig, Intent, ProviderKind};
use crate::engine::{
    self, AskDecision, ExpansionState, Grade, NodeMode, ObjectiveGrade, Outline, OutlineItem,
    OutlineItemType, Rubric, SourcePointer,
};
use crate::events::EventKind;
use crate::events::aggregate::{NodeState, node_generated, node_states, revisit_suggestion};
use crate::movement::{self, GeneratedMove, MoveContext, MoveRecord, MoveRender, MoveType};
use crate::objective::{ObjectiveLog, ObjectiveSource};
use crate::secret::SecretStore;
use crate::store::StoreError;

/// API error mapped to an HTTP status.
pub enum ApiError {
    Engine(engine::EngineError),
    Store(StoreError),
    BadRequest(String),
    Internal(String),
    NotFound(String),
    /// §S15b step 6: a refused action, not a malformed request or a server
    /// fault — e.g. deleting a document other documents still reference.
    Conflict(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            // Provider failure or unreadable model output: bad gateway.
            ApiError::Engine(e) => (StatusCode::BAD_GATEWAY, e.to_string()),
            ApiError::Store(StoreError::NotFound(_)) => {
                (StatusCode::NOT_FOUND, "not found".to_string())
            }
            ApiError::Store(StoreError::InvalidId(_)) => {
                (StatusCode::BAD_REQUEST, "invalid identifier".to_string())
            }
            ApiError::Store(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
            ApiError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            ApiError::Conflict(m) => (StatusCode::CONFLICT, m),
        };
        (status, msg).into_response()
    }
}

impl From<engine::EngineError> for ApiError {
    fn from(e: engine::EngineError) -> Self {
        ApiError::Engine(e)
    }
}
impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        ApiError::Store(e)
    }
}

/// The node's server-only sidecar: locked rubric + the exercise + context for
/// grading/remediation. Never served to the client (§8). `move_id` is the
/// currently active graded move's id (§6/§7): grading joins `MoveGraded`
/// back onto the `MoveGenerated` event this id was assigned in.
#[derive(Serialize, Deserialize)]
struct RubricSidecar {
    move_id: String,
    rubric: Rubric,
    exercise_html: String,
    /// The model's worked solution to `exercise_html` (S16) — server-only,
    /// like the rest of this sidecar; `#[serde(default)]` so a sidecar
    /// written before this field existed still deserializes, with `grade()`
    /// degrading to rubric-only grading for it.
    #[serde(default)]
    reference_solution: String,
    title: String,
    topic: String,
}

mod acervo;
mod cold_start;
mod generation;
mod grading;
mod provider;
mod reading;
mod setup;

pub use acervo::*;
pub use cold_start::*;
pub use generation::*;
pub use grading::*;
pub use provider::*;
pub use reading::*;
pub use setup::*;
#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use learnive_core::QuoteSelector;

    #[test]
    fn answer_header_names_the_selection_it_was_asked_on() {
        let plain = question_header("why?", &Anchor::block("b1"), crate::locale::Locale::En);
        assert!(plain.contains("You asked:</strong> why?"));
        assert!(!plain.contains("about"));

        let selected = question_header(
            "why <that>?",
            &Anchor {
                block_id: "b1".to_string(),
                quote: Some(QuoteSelector {
                    exact: "  entropy never decreases  ".to_string(),
                    prefix: None,
                    suffix: None,
                }),
            },
            crate::locale::Locale::En,
        );
        // Question and quote are both escaped — a selection is user-chosen
        // document text and reaches the page through `innerHTML`.
        assert!(selected.contains("why &lt;that&gt;?"));
        assert!(selected.contains("about \u{201c}entropy never decreases\u{201d}"));

        // A long selection is clipped, not reproduced.
        let long = "x".repeat(QUOTE_HEADER_BUDGET + 50);
        let clipped = question_header(
            "?",
            &Anchor {
                block_id: "b1".to_string(),
                quote: Some(QuoteSelector {
                    exact: long,
                    prefix: None,
                    suffix: None,
                }),
            },
            crate::locale::Locale::En,
        );
        assert!(clipped.contains('…'));
        assert!(clipped.len() < QUOTE_HEADER_BUDGET + 120);
    }

    /// Guards the fix noted at the top of `demo_responder`: without the
    /// movement-specific branches ordered first, a `test` move's prompt
    /// (which embeds `EXERCISE_HTML_CONTRACT`, containing the literal
    /// substring "exercise_html") falls into the legacy `exercise_html`
    /// branch and returns the WRONG JSON shape, so every demo call would
    /// fail and burn its repair attempt.
    #[tokio::test]
    async fn demo_mode_answers_the_move_abi_contract() {
        let ai = demo_ai();
        let ctx = MoveContext {
            topic: "fractions".into(),
            item_title: "Equivalent fractions".into(),
            ..Default::default()
        };

        // Explain is a streamed move — pump the token stream, then finish it.
        let stream = movement::generate_move_stream(&ai, MoveType::Explain, &ctx)
            .await
            .unwrap();
        let accumulated = stream
            .map(|r| r.unwrap())
            .collect::<Vec<_>>()
            .await
            .concat();
        let explained = movement::finish_streamed_move(MoveType::Explain, &accumulated);
        assert!(!explained.graded);
        assert!(!explained.html.is_empty());

        // A `test` move is structured — must come back graded with a
        // non-empty rubric, not the legacy exercise shape.
        let tested = movement::generate_move(&ai, MoveType::Test, &ctx)
            .await
            .unwrap();
        assert!(tested.graded);
        assert!(tested.rubric.unwrap().objectives.iter().any(|o| o.transfer));
    }
}
