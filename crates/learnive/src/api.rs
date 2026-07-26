//! Curriculum-loop endpoints (§6, §8, §8.2).
//!
//! Flow: cold start (POST creates document + outline) → node generation (POST
//! that **streams** the prose token by token in SSE format) → answer (POST grades
//! against the locked rubric; on failure opens remediation §8.2; on success
//! signals to advance).
//!
//! On streaming and §3.1: §3 asks for SSE, but the browser's `EventSource` does
//! not send a token header nor POST, and §3.1 forbids state-changing GET. So we
//! stream the **SSE wire format over a POST** (read via `fetch`): it keeps the §3
//! semantics and honors §3.1 (POST + header token + Origin).

use axum::{
    Json,
    body::{Body, Bytes},
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

use learnive_core::{InteractionItem, ThreadKind};

use crate::ai::{Ai, MockProvider, Models, OpenAiCompat, Provider, Tier};
use crate::app::AppState;
use crate::engine::{self, ObjectiveGrade, Outline, Rubric};
use crate::store::StoreError;

/// API error mapped to an HTTP status.
pub enum ApiError {
    Engine(engine::EngineError),
    Store(StoreError),
    BadRequest(String),
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
/// grading/remediation. Never served to the client (§8).
#[derive(Serialize, Deserialize)]
struct RubricSidecar {
    rubric: Rubric,
    exercise_html: String,
    title: String,
    topic: String,
}

// ---------------------------------------------------------------------------
// Cold start (§6.1): topic → outline.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CreateReq {
    topic: String,
}

#[derive(Serialize)]
pub struct CreateResp {
    doc_id: String,
    titles: Vec<String>,
}

pub async fn create_document(
    State(state): State<AppState>,
    Json(body): Json<CreateReq>,
) -> Result<Json<CreateResp>, ApiError> {
    if body.topic.trim().is_empty() {
        return Err(ApiError::BadRequest("empty topic".to_string()));
    }
    let outline = engine::generate_outline(&state.ai, &body.topic).await?;
    let doc_id = engine::new_id();
    state.store.create_document(&doc_id)?;
    state.store.write_doc_file(
        &doc_id,
        "outline.json",
        &serde_json::to_string(&outline).unwrap_or_default(),
    )?;
    Ok(Json(CreateResp {
        doc_id,
        titles: outline.items.iter().map(|i| i.title.clone()).collect(),
    }))
}

// ---------------------------------------------------------------------------
// Node generation (§6) with prose streaming (§14).
// ---------------------------------------------------------------------------

/// Streams the SSE format over a POST. Events: `token` (prose, repeated),
/// `exercise` (form, after the prose), `done` (node_id), `error`.
pub async fn generate_node(
    State(state): State<AppState>,
    Path((doc_id, index)): Path<(String, usize)>,
) -> Response {
    // The fallible work that emits no tokens lives in `prepare`/`finalize`; the
    // generator only holds the `yield`s (async_stream does not rewrite `yield`
    // through a nested macro).
    let stream = async_stream::stream! {
        let prep = match prepare(&state, &doc_id, index).await {
            Ok(p) => p,
            Err(e) => {
                yield Ok::<Bytes, std::io::Error>(sse_frame("error", &e));
                return;
            }
        };

        // Prose: robust, streamed token by token (TTFT, §14).
        let mut prose = String::new();
        let prose_prompt = engine::prompt::prose(&prep.topic, &prep.title, &prep.context);
        match state.ai.stream(Tier::Robust, prose_prompt).await {
            Ok(mut s) => {
                while let Some(tok) = s.next().await {
                    match tok {
                        Ok(t) => {
                            prose.push_str(&t);
                            yield Ok(sse_frame("token", &t));
                        }
                        Err(e) => {
                            yield Ok(sse_frame("error", &e.to_string()));
                            return;
                        }
                    }
                }
            }
            Err(e) => {
                yield Ok(sse_frame("error", &e.to_string()));
                return;
            }
        }

        // Exercise + rubric in a separate call, assemble and persist (§8/§14).
        match finalize(&state, &doc_id, &prep, &prose).await {
            Ok(exercise_html) => {
                yield Ok(sse_frame("exercise", &exercise_html));
                yield Ok(sse_frame("done", &prep.node_id));
            }
            Err(e) => {
                yield Ok(sse_frame("error", &e));
                return;
            }
        }
    };

    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .expect("valid stream response")
}

/// Data ready to generate a node.
struct NodePrep {
    topic: String,
    title: String,
    context: String,
    node_id: String,
}

/// Loads the outline and resolves the requested item (fallible work, no `yield`).
async fn prepare(state: &AppState, doc_id: &str, index: usize) -> Result<NodePrep, String> {
    let outline_json = state
        .store
        .read_doc_file(doc_id, "outline.json")
        .map_err(|e| e.to_string())?;
    let outline: Outline = serde_json::from_str(&outline_json).map_err(|e| e.to_string())?;
    let item = outline
        .items
        .get(index)
        .cloned()
        .ok_or_else(|| "index out of the outline".to_string())?;
    let context = outline.items[..index]
        .iter()
        .map(|i| i.title.as_str())
        .collect::<Vec<_>>()
        .join("; ");
    Ok(NodePrep {
        topic: outline.topic,
        title: item.title,
        context,
        node_id: format!("n{index}"),
    })
}

/// Generates exercise + rubric, assembles the node and persists (node +
/// server-only sidecar). Returns the exercise HTML to the client. Idempotent by
/// `node_id` (§16).
async fn finalize(
    state: &AppState,
    doc_id: &str,
    prep: &NodePrep,
    prose: &str,
) -> Result<String, String> {
    let er = engine::generate_exercise_and_rubric(&state.ai, &prep.topic, &prep.title, prose)
        .await
        .map_err(|e| e.to_string())?;

    let exercise_id = format!("{}-ex", prep.node_id);
    let rubric_id = format!("{}-ru", prep.node_id);
    let node = engine::assemble_node(
        doc_id,
        &prep.node_id,
        prose,
        &er.exercise_html,
        &exercise_id,
        &rubric_id,
    )
    .map_err(|e| e.to_string())?;

    state.store.write_node(&node).map_err(|e| e.to_string())?;

    let sidecar = RubricSidecar {
        rubric: er.rubric,
        exercise_html: er.exercise_html.clone(),
        title: prep.title.clone(),
        topic: prep.topic.clone(),
    };
    state
        .store
        .write_doc_file(
            doc_id,
            &format!("{}.rubric.json", prep.node_id),
            &serde_json::to_string(&sidecar).unwrap_or_default(),
        )
        .map_err(|e| e.to_string())?;

    Ok(er.exercise_html)
}

// ---------------------------------------------------------------------------
// Answer → grading (§8) → remediation (§8.2) or advance.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct AnswerReq {
    answer: String,
}

#[derive(Serialize)]
pub struct AnswerResp {
    grades: Vec<ObjectiveGrade>,
    advance: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    remediation_html: Option<String>,
}

pub async fn answer(
    State(state): State<AppState>,
    Path((doc_id, node_id)): Path<(String, String)>,
    Json(body): Json<AnswerReq>,
) -> Result<Json<AnswerResp>, ApiError> {
    let sidecar_json = state
        .store
        .read_doc_file(&doc_id, &format!("{node_id}.rubric.json"))?;
    let sidecar: RubricSidecar =
        serde_json::from_str(&sidecar_json).map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let assessment = engine::grade(
        &state.ai,
        &sidecar.rubric,
        &sidecar.exercise_html,
        &body.answer,
    )
    .await?;

    // Advancing requires every objective demonstrated (§8).
    if assessment.all_demonstrated() {
        return Ok(Json(AnswerResp {
            grades: assessment.grades,
            advance: true,
            remediation_html: None,
        }));
    }

    // Remediation (§8.2): similarity grows with the number of attempts.
    let node = state.store.read_node(&doc_id, &node_id)?;
    let attempt = node
        .interaction
        .iter()
        .filter(|i| {
            matches!(
                i,
                InteractionItem::Thread {
                    kind: ThreadKind::Remediation,
                    ..
                }
            )
        })
        .count() as u32
        + 1;
    let html = engine::remediate(
        &state.ai,
        &sidecar.title,
        &sidecar.exercise_html,
        &body.answer,
        &assessment.unmet(),
        attempt,
    )
    .await?;

    // Append-only in the interaction layer (§4.3), anchored to the exercise.
    let anchor = node
        .content
        .exercise
        .as_ref()
        .map(|e| e.exercise_id.clone());
    state.store.append_interaction(
        &doc_id,
        &node_id,
        InteractionItem::Thread {
            id: engine::new_id(),
            kind: ThreadKind::Remediation,
            anchor_block: anchor,
            body_html: html.clone(),
        },
    )?;

    Ok(Json(AnswerResp {
        grades: assessment.grades,
        advance: false,
        remediation_html: Some(html),
    }))
}

/// Formats an SSE frame with JSON-encoded `data` (avoids newline problems).
fn sse_frame(event: &str, data: &str) -> Bytes {
    let json = serde_json::to_string(data).unwrap_or_else(|_| "\"\"".to_string());
    Bytes::from(format!("event: {event}\ndata: {json}\n\n"))
}

// ---------------------------------------------------------------------------
// Provider selection (§12) — any OpenAI-compatible endpoint is swappable. Order:
// a custom base URL (generic BYOK), else OpenRouter (default), else offline demo.
// ---------------------------------------------------------------------------

/// Builds the `Ai` from the environment (§12). Precedence:
/// 1. `LEARNIVE_API_BASE_URL` (+ optional `LEARNIVE_API_KEY`) — any OpenAI-compatible
///    `chat/completions` endpoint: Inception's Mercury, OpenCode Zen, a local model.
/// 2. `LEARNIVE_OPENROUTER_KEY` — the default OpenRouter path.
/// 3. Otherwise, offline demo mode.
pub fn build_ai() -> Ai {
    // Generic OpenAI-compatible provider. `base_url` is the part before
    // `/chat/completions` (e.g. `https://api.inceptionlabs.ai/v1`).
    if let Ok(base_url) = std::env::var("LEARNIVE_API_BASE_URL")
        && !base_url.is_empty()
    {
        let key = std::env::var("LEARNIVE_API_KEY")
            .ok()
            .filter(|k| !k.is_empty());
        return Ai::new(
            Provider::OpenAiCompat(OpenAiCompat::new(base_url, key)),
            models_from_env(),
        );
    }

    match std::env::var("LEARNIVE_OPENROUTER_KEY") {
        Ok(key) if !key.is_empty() => Ai::new(
            Provider::OpenAiCompat(OpenAiCompat::openrouter(Some(key))),
            models_from_env(),
        ),
        _ => {
            eprintln!(
                "No AI key configured — running in DEMO MODE (canned content). \
                 Configure a provider (see .env.example) for real content."
            );
            demo_ai()
        }
    }
}

/// Reads the fast/robust model pair from the environment (§12.1). Defaults are
/// OpenRouter model ids; for other providers set both explicitly (e.g. `mercury-2`).
fn models_from_env() -> Models {
    let fast = std::env::var("LEARNIVE_MODEL_FAST").unwrap_or_else(|_| "openai/gpt-4o-mini".into());
    let robust = std::env::var("LEARNIVE_MODEL_ROBUST").unwrap_or_else(|_| "openai/gpt-4o".into());
    Models::new(fast, robust)
}

/// Demo-mode `Ai`: a mock that answers differently per sub-task, closing the
/// whole loop offline.
pub fn demo_ai() -> Ai {
    Ai::new(
        Provider::Mock(MockProvider::scripted(demo_responder)),
        Models::single("demo"),
    )
}

fn demo_responder(req: &crate::ai::ChatRequest) -> String {
    let text = req
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    // Branch on distinctive phrases from each prompt (engine::prompt). Keep these
    // in sync with the prompt wording.
    if text.contains("JSON array of strings") {
        r#"["Introduction to the topic", "Core concept", "Practical application"]"#.to_string()
    } else if text.contains("exercise_html") {
        r#"{"exercise_html":"<form><p>Explain the concept in your own words and apply it to a new case:</p><textarea name=\"answer\" rows=\"4\"></textarea></form>","objectives":[{"id":"o1","kind":"application","description":"Apply the concept to a new case","criteria":"The answer transfers the concept to a scenario not covered in the text","transfer":true}]}"#.to_string()
    } else if text.contains("locked rubric") {
        // Demo: always demonstrated, so the loop advances end to end.
        r#"{"grades":[{"objective_id":"o1","grade":"demonstrated","feedback":"Good transfer of the concept to a new case."}]}"#.to_string()
    } else if text.contains("Remediation session") {
        "<p>Let's review with a worked example and a new, similar problem.</p>".to_string()
    } else {
        // Prose (default).
        "<h2>Core concept</h2><p>This is an explanatory paragraph generated in <strong>demo mode</strong> (no AI key). Configure a provider for real content, grounded in sources.</p><p>Each node is atomic and ends in a comprehension check.</p>".to_string()
    }
}
