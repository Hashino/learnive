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
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

use learnive_core::{Anchor, InteractionItem, Node, ThreadKind};

use crate::ai::{Ai, MockProvider, Models, OpenAiCompat, Provider, Tier};
use crate::app::AppState;
use crate::config::{AppConfig, Intent, ProviderKind};
use crate::engine::{self, AskDecision, Grade, ObjectiveGrade, Outline, OutlineItem, Rubric};
use crate::events::EventKind;
use crate::events::aggregate::{
    NodeState, activity_counts, calibrate_rung, ladder_signals, node_states, revisit_suggestion,
    tactic_outcomes,
};
use crate::movement::{
    self, AgentPolicy, GeneratedMove, MoveContext, MoveRecord, MoveRender, MoveType,
};
use crate::objective::{self, ObjectiveLog, ObjectiveSource};
use crate::profile::{self, ProfileProjection};
use crate::secret::SecretStore;
use crate::source::Source;
use crate::store::StoreError;

/// API error mapped to an HTTP status.
pub enum ApiError {
    Engine(engine::EngineError),
    Store(StoreError),
    BadRequest(String),
    Internal(String),
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
    title: String,
    topic: String,
}

// ---------------------------------------------------------------------------
// Cold start (§6.1, §S4): topic → proposed objective → confirmed objective +
// outline. Two calls: `propose_objective` is stateless (nothing persisted,
// just a fast round-trip so the client can show an editable confirm box);
// `create_document` locks the (possibly edited) objective as version 1 and
// only then generates the outline, anchored on it.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ProposeObjectiveReq {
    topic: String,
}

#[derive(Serialize)]
pub struct ProposeObjectiveResp {
    text: String,
    non_goals: Vec<String>,
}

pub async fn propose_objective(
    State(state): State<AppState>,
    Json(body): Json<ProposeObjectiveReq>,
) -> Result<Json<ProposeObjectiveResp>, ApiError> {
    if body.topic.trim().is_empty() {
        return Err(ApiError::BadRequest("empty topic".to_string()));
    }
    let ai = state.ai.load_full();
    let proposal = engine::propose_objective(&ai, &body.topic).await?;
    Ok(Json(ProposeObjectiveResp {
        text: proposal.text,
        non_goals: proposal.non_goals,
    }))
}

#[derive(Deserialize)]
pub struct CreateReq {
    topic: String,
    /// The (possibly user-edited) objective from `propose_objective`. Empty
    /// only for a caller that skipped confirmation entirely (e.g. a direct
    /// API call) — falls back to the raw topic so the objective anchor is
    /// never blank, even though the UI always confirms (§6.1/§S4).
    #[serde(default)]
    objective_text: String,
    #[serde(default)]
    non_goals: Vec<String>,
}

/// One outline item as shown to the client (§S5): the graph's gate, resolved.
/// `state` is `"locked"` (a prerequisite isn't `Demonstrated` yet and the
/// item was never touched), `"available"` (prerequisites met, or already
/// attempted/skipped — i.e. still worth showing as reachable), or
/// `"demonstrated"`.
#[derive(Serialize)]
pub struct OutlineItemView {
    id: String,
    title: String,
    state: &'static str,
}

#[derive(Serialize)]
pub struct OutlineResp {
    items: Vec<OutlineItemView>,
    /// §S5 revisit scheduler: the currently-skipped node deferred longest,
    /// if any (`events::aggregate::revisit_suggestion`) — a spacing
    /// suggestion, not a mandate; the learner can pick any other reachable
    /// item instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    suggested_revisit: Option<String>,
}

/// §S5 revisit scheduler, wired to the response: see
/// `events::aggregate::revisit_suggestion` for the actual heuristic.
fn suggested_revisit(state: &AppState, doc_id: &str) -> Result<Option<String>, ApiError> {
    let event_log = state.store.event_log(doc_id)?;
    Ok(revisit_suggestion(
        event_log
            .iter()
            .map_err(|e| ApiError::Internal(e.to_string()))?,
    ))
}

/// Resolves every item's gate state against the event log in one fold
/// (§S5) — never a separate `progress.json`, so there is nothing to desync
/// (see `events::aggregate::node_states`'s doc comment).
fn outline_view(
    state: &AppState,
    doc_id: &str,
    outline: &Outline,
) -> Result<Vec<OutlineItemView>, ApiError> {
    let event_log = state.store.event_log(doc_id)?;
    let states = node_states(
        event_log
            .iter()
            .map_err(|e| ApiError::Internal(e.to_string()))?,
    );
    Ok(outline
        .items
        .iter()
        // Sub-nodes spawned from a question (§S8) are never shown in the
        // main-line sidebar or picked as the "next available" item — they're
        // reachable only inline, spliced into the parent that spawned them.
        .filter(|item| item.parent_id.is_none())
        .map(|item| {
            let view_state = match states.get(&item.id) {
                Some(NodeState::Demonstrated) => "demonstrated",
                Some(NodeState::Attempted) | Some(NodeState::Skipped) => "available",
                None => {
                    let unlocked = item
                        .prerequisites
                        .iter()
                        .all(|p| matches!(states.get(p), Some(NodeState::Demonstrated)));
                    if unlocked { "available" } else { "locked" }
                }
            };
            OutlineItemView {
                id: item.id.clone(),
                title: item.title.clone(),
                state: view_state,
            }
        })
        .collect())
}

#[derive(Serialize)]
pub struct CreateResp {
    doc_id: String,
    items: Vec<OutlineItemView>,
}

pub async fn create_document(
    State(state): State<AppState>,
    Json(body): Json<CreateReq>,
) -> Result<Json<CreateResp>, ApiError> {
    if body.topic.trim().is_empty() {
        return Err(ApiError::BadRequest("empty topic".to_string()));
    }
    let objective_text = if body.objective_text.trim().is_empty() {
        body.topic.clone()
    } else {
        body.objective_text.clone()
    };

    let ai = state.ai.load_full();
    let outline = engine::generate_outline(&ai, &body.topic, &objective_text).await?;
    let doc_id = engine::new_id();
    state.store.create_document(&doc_id)?;

    let mut objective_log = ObjectiveLog::default();
    objective_log.push(
        objective_text.clone(),
        body.non_goals.clone(),
        ObjectiveSource::ColdStart,
    );
    state.store.write_doc_file(
        &doc_id,
        "objective.json",
        &serde_json::to_string(&objective_log).unwrap_or_default(),
    )?;
    state.store.write_doc_file(
        &doc_id,
        "outline.json",
        &serde_json::to_string(&outline).unwrap_or_default(),
    )?;
    // Acquire a grounding source in the background (§11/§14): the outline returns
    // immediately and content starts streaming ungrounded; citations appear once
    // the source is fetched and indexed. Never blocks the user. Seeded with the
    // confirmed objective text (strictly better grounding input than the raw
    // topic, and the only text guaranteed to already reflect the user's edits).
    spawn_acquisition(state.clone(), objective_text);
    let items = outline_view(&state, &doc_id, &outline)?;
    Ok(Json(CreateResp { doc_id, items }))
}

#[derive(Deserialize)]
pub struct ReviseObjectiveReq {
    text: String,
    #[serde(default)]
    non_goals: Vec<String>,
}

#[derive(Serialize)]
pub struct ObjectiveResp {
    text: String,
    non_goals: Vec<String>,
    version: u32,
}

/// User-initiated objective revision (§5/§S4): "editável a qualquer
/// momento" — the objective is not only revised by an approved `plan` move,
/// the learner can edit it directly. Appends a new `ObjectiveLog` version
/// (never overwrites) and an `ObjectiveRevised` event.
pub async fn revise_objective(
    State(state): State<AppState>,
    Path(doc_id): Path<String>,
    Json(body): Json<ReviseObjectiveReq>,
) -> Result<Json<ObjectiveResp>, ApiError> {
    if body.text.trim().is_empty() {
        return Err(ApiError::BadRequest("empty objective".to_string()));
    }
    let log_json = state.store.read_doc_file(&doc_id, "objective.json")?;
    let mut log: ObjectiveLog =
        serde_json::from_str(&log_json).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    log.push(
        body.text.clone(),
        body.non_goals.clone(),
        ObjectiveSource::UserEdit,
    );
    state.store.write_doc_file(
        &doc_id,
        "objective.json",
        &serde_json::to_string(&log).unwrap_or_default(),
    )?;

    let version = log.current().map(|v| v.version).unwrap_or(0);
    let event_log = state.store.event_log(&doc_id)?;
    if let Err(e) = event_log.append(None, EventKind::ObjectiveRevised { version }) {
        eprintln!("event log append failed: {e}");
    }

    Ok(Json(ObjectiveResp {
        text: body.text,
        non_goals: body.non_goals,
        version,
    }))
}

#[derive(Serialize)]
pub struct ProfileResp {
    traits: Vec<String>,
    hypotheses: Vec<String>,
    /// Always-fresh, 0-LLM-token evidence table (§7) — recomputed on every
    /// read, never itself edited by the user (only what's distilled from it
    /// is: `traits`/`hypotheses`).
    evidence: String,
    distilled_through: u32,
}

/// Read-only inspection of the evidence profile (§7.1: "perfil inspecionável
/// e editável" — the human-in-the-loop fix for drift/bad compaction).
/// `traits`/`hypotheses` are empty before the first distillation.
pub async fn get_profile(
    State(state): State<AppState>,
    Path(doc_id): Path<String>,
) -> Result<Json<ProfileResp>, ApiError> {
    let event_log = state.store.event_log(&doc_id)?;
    let table = tactic_outcomes(
        event_log
            .iter()
            .map_err(|e| ApiError::BadRequest(e.to_string()))?,
    );
    let evidence = profile::evidence_table_text(&table);
    let projection = read_profile(&state, &doc_id).unwrap_or_default();
    Ok(Json(ProfileResp {
        traits: projection.traits,
        hypotheses: projection.hypotheses,
        evidence,
        distilled_through: projection.distilled_through,
    }))
}

#[derive(Deserialize)]
pub struct ReviseProfileReq {
    #[serde(default)]
    traits: Vec<String>,
    #[serde(default)]
    hypotheses: Vec<String>,
}

/// User-initiated profile edit (§7.1: "editável a qualquer momento") — the
/// same human-in-the-loop escape hatch `revise_objective` gives the
/// objective. Overwrites `profile.json` wholesale; `distilled_through` is
/// preserved as-is (a user edit is not a distillation, so it must not reset
/// the ~30-event threshold `should_distill` tracks).
pub async fn revise_profile(
    State(state): State<AppState>,
    Path(doc_id): Path<String>,
    Json(body): Json<ReviseProfileReq>,
) -> Result<Json<ProfileResp>, ApiError> {
    let distilled_through = read_profile(&state, &doc_id)
        .map(|p| p.distilled_through)
        .unwrap_or(0);
    let projection = ProfileProjection {
        traits: body.traits,
        hypotheses: body.hypotheses,
        distilled_through,
    };
    state.store.write_doc_file(
        &doc_id,
        "profile.json",
        &serde_json::to_string(&projection).unwrap_or_default(),
    )?;

    let event_log = state.store.event_log(&doc_id)?;
    let table = tactic_outcomes(
        event_log
            .iter()
            .map_err(|e| ApiError::BadRequest(e.to_string()))?,
    );
    let evidence = profile::evidence_table_text(&table);
    Ok(Json(ProfileResp {
        traits: projection.traits,
        hypotheses: projection.hypotheses,
        evidence,
        distilled_through: projection.distilled_through,
    }))
}

/// Background source acquisition + reindex (§11.1/§10). No-op when grounding is
/// disabled (no retriever). Failures are logged, never surfaced to the user — an
/// ungrounded document is still fully usable.
fn spawn_acquisition(state: AppState, topic: String) {
    let Some(retriever) = state.retriever.clone() else {
        return;
    };
    let source = state.source.clone();
    let corpus = state.corpus.clone();
    tokio::spawn(async move {
        let hit = match source.search(&topic).await {
            Ok(hits) => match hits.into_iter().next() {
                Some(h) => h,
                None => return,
            },
            Err(e) => {
                eprintln!("acquisition search failed: {e}");
                return;
            }
        };
        let doc = match source.fetch(&hit).await {
            Ok(d) => d,
            Err(e) => {
                eprintln!("acquisition fetch failed: {e}");
                return;
            }
        };
        match corpus.store(&doc) {
            Ok(true) => {
                // Reindex so the new source is retrievable for grounding.
                let mut r = retriever.write().await;
                if let Err(e) = r.reindex(&corpus) {
                    eprintln!("reindex after acquisition failed: {e}");
                } else {
                    eprintln!("grounded on \"{}\" ({} chunks)", doc.meta.title, r.len());
                }
            }
            Ok(false) => {} // already in the corpus and indexed
            Err(e) => eprintln!("corpus store failed: {e}"),
        }
    });
}

/// The runtime acquisition backend (§11.1): OpenStax OER by default.
pub fn build_source() -> Source {
    Source::openstax()
}

// ---------------------------------------------------------------------------
// Setup (§12): configure the provider + key (in-app), key in the secret store.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct SetupReq {
    /// `demo` | `openrouter` | `openai_compatible`.
    provider: String,
    /// `free` | `paid` — the single intent that derives both tiers (§12.1).
    intent: String,
    base_url: Option<String>,
    api_key: Option<String>,
    model_fast: Option<String>,
    model_robust: Option<String>,
}

#[derive(Serialize)]
pub struct SetupStatus {
    provider: String,
    intent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    /// Whether a key is stored — the key itself is NEVER returned (§12).
    has_key: bool,
    demo: bool,
    /// The derived active model pair, for display only.
    model_fast: String,
    model_robust: String,
}

fn status_of(config: &AppConfig, secret: &SecretStore) -> SetupStatus {
    let (provider, base_url) = match &config.provider {
        ProviderKind::Demo => ("demo".to_string(), None),
        ProviderKind::OpenRouter => ("openrouter".to_string(), None),
        ProviderKind::OpenAiCompatible { base_url } => {
            ("openai_compatible".to_string(), Some(base_url.clone()))
        }
    };
    let has_key = config
        .key_name()
        .map(|n| secret.get(n).is_some())
        .unwrap_or(false);
    let models = config.models();
    SetupStatus {
        provider,
        intent: match config.intent {
            Intent::Free => "free",
            Intent::Paid => "paid",
        }
        .to_string(),
        base_url,
        has_key,
        demo: matches!(config.provider, ProviderKind::Demo),
        model_fast: models.for_tier(Tier::Fast).to_string(),
        model_robust: models.for_tier(Tier::Robust).to_string(),
    }
}

/// Current setup, for prefilling the form. Never leaks the key.
pub async fn setup_status(State(state): State<AppState>) -> Json<SetupStatus> {
    let config = state.config.read().await.clone();
    Json(status_of(&config, &state.secret))
}

/// Saves the provider/intent (config file) + key (secret store) and hot-swaps the
/// live AI (§12) — no restart. State-changing → POST only, token-guarded (§3.1).
pub async fn save_setup(
    State(state): State<AppState>,
    Json(req): Json<SetupReq>,
) -> Result<Json<SetupStatus>, ApiError> {
    let provider = match req.provider.as_str() {
        "demo" => ProviderKind::Demo,
        "openrouter" => ProviderKind::OpenRouter,
        "openai_compatible" => {
            let base = req
                .base_url
                .clone()
                .filter(|b| !b.trim().is_empty())
                .ok_or_else(|| ApiError::BadRequest("base_url required".into()))?;
            ProviderKind::OpenAiCompatible {
                base_url: base.trim().to_string(),
            }
        }
        other => return Err(ApiError::BadRequest(format!("unknown provider: {other}"))),
    };
    let intent = if req.intent == "paid" {
        Intent::Paid
    } else {
        Intent::Free
    };
    let config = AppConfig {
        provider,
        intent,
        model_fast: req.model_fast.filter(|s| !s.trim().is_empty()),
        model_robust: req.model_robust.filter(|s| !s.trim().is_empty()),
    };

    // Persist config (no secret) and store the key separately (§12).
    config
        .save(&*state.data_dir)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if let (Some(name), Some(key)) = (
        config.key_name(),
        req.api_key.as_ref().filter(|k| !k.trim().is_empty()),
    ) {
        state
            .secret
            .set(name, key.trim())
            .map_err(|e| ApiError::Internal(e.to_string()))?;
    }

    // Apply live: swap config + rebuild the provider + rung (§12 hot-swap).
    // Both come from the same `build_ai` call — see its doc comment on why
    // they must never be derived separately.
    *state.config.write().await = config.clone();
    let (ai, policy) = build_ai(&config, &state.secret);
    state.ai.store(std::sync::Arc::new(ai));
    state.policy.store(std::sync::Arc::new(policy));

    let status = status_of(&config, &state.secret);
    Ok(Json(status))
}

// ---------------------------------------------------------------------------
// Node generation (§6): decide → generate → stream, move by move (§14).
// ---------------------------------------------------------------------------

/// Hard cap on moves generated for one node in one request (§12.2 cost
/// control): the node must still close in a graded check even if L1/L2 keeps
/// picking ungraded moves, so the last allowed iteration forces `test`
/// instead of asking `decide_move` again. L0 never gets close to this (it
/// always closes at move 2: explain, then test).
const MAX_MOVES_PER_NODE: usize = 4;

/// Verbatim §14 context budget fed to `decide_move` (~1.5k chars).
const NODE_TAIL_BUDGET: usize = 1500;

/// Streams the SSE format over a POST. Events: `token` (prose — both streamed
/// moves and, as one full frame, ungraded structured moves, which share the
/// same prose contract §4.4/`movement.rs`), `exercise` (the graded move's
/// form, sandboxed), `done` (node_id), `error`.
///
/// Block-level interactive islands (§4.4/§S11) — a `<figure data-interactive>`
/// the model opens mid-move's HTML, not the whole-move `interactive` flag
/// below — DO have a real sandbox slot as of §S11, for every move type, once
/// the node is finalized and read back (`get_node`'s `redact_interactive_blocks`
/// and `api::block_frame`; `ensure_block_ids` runs on the full concatenated
/// `content_html` at `finalize` time regardless of which move contributed an
/// island). What's gated live, mid-stream, is narrower: `movement::IslandGate`
/// only gates a streamed move's token-by-token output. A structured ungraded
/// move's `html` still goes out as one raw `token` frame — its island isn't
/// hidden from that single frame the way a streamed move's is, though
/// `sanitizeHtml` still strips any `<script>` inside it client-side before
/// it's ever inserted, so nothing executes; it's a cosmetic gap (the island
/// shows empty until the post-`done` refetch swaps in the properly hydrated
/// version), not a security one.
///
/// `interactive:true && graded:false` on the [`GeneratedMove`] struct itself
/// (an L1/L2 **structured** move choosing to make its *entire* output an
/// interactive widget, e.g. `profile`, rather than prose with an island in
/// it) is a different, still-open gap: no code path here reads that flag, so
/// such a move's whole `html` is folded into `content_html` and sanitized as
/// plain prose — any `<script>` it carries at the top level is stripped the
/// same way. Not a bug to fix here; a future slice that wants a whole
/// ungraded move to render as one sandboxed widget needs its own slot in the
/// wire format, distinct from the island mechanism above.
pub async fn generate_node(
    State(state): State<AppState>,
    Path((doc_id, item_id)): Path<(String, String)>,
) -> Response {
    // The fallible work that emits no tokens lives in `prepare`/`finalize`; the
    // generator only holds the `yield`s (async_stream does not rewrite `yield`
    // through a nested macro).
    let stream = async_stream::stream! {
        let prep = match prepare(&state, &doc_id, &item_id).await {
            Ok(p) => p,
            Err(e) => {
                yield Ok::<Bytes, std::io::Error>(sse_frame("error", &e));
                return;
            }
        };

        let ai = state.ai.load_full();
        let config_prior = *state.policy.load_full();
        let event_log = match state.store.event_log(&doc_id) {
            Ok(l) => l,
            Err(e) => {
                yield Ok(sse_frame("error", &e.to_string()));
                return;
            }
        };
        // §9 "mover o degrau por documento": this document's own ladder
        // telemetry (schema violations, move-diversity collapse) can step
        // the config prior down for THIS document, without touching
        // `state.policy` (the global prior every other document still
        // starts from). Computed once per node, not per move, so the whole
        // node's move loop runs at one stable rung and the `rung` field
        // stamped on every `MoveGenerated` below reflects what was actually
        // used, not a stale global value.
        let policy = rung_for(&state, &doc_id, config_prior);

        let mut ctx = MoveContext {
            topic: prep.topic.clone(),
            item_title: prep.title.clone(),
            outline_context: prep.context.clone(),
            grounding: prep.grounding.clone(),
            objective: prep.objective.clone(),
            profile: prep.profile.clone(),
            ..Default::default()
        };
        let mut content_html = String::new();
        let mut graded: Option<(String, GeneratedMove)> = None;

        for i in 0..MAX_MOVES_PER_NODE {
            let move_type = if i == MAX_MOVES_PER_NODE - 1 {
                // Cost guard exhausted without a graded check — force one so
                // the node still closes (every node ends in a check, §6).
                MoveType::Test
            } else {
                match movement::decide_move(&ai, policy, &ctx).await {
                    Ok(mt) => mt,
                    Err(e) => {
                        yield Ok(sse_frame("error", &e.to_string()));
                        return;
                    }
                }
            };

            let generated = match move_type.render() {
                MoveRender::Streamed => {
                    let mut tokens = match movement::generate_move_stream(&ai, move_type, &ctx).await {
                        Ok(s) => s,
                        Err(e) => {
                            yield Ok(sse_frame("error", &e.to_string()));
                            return;
                        }
                    };
                    // §S11: gate an interactive island's raw HTML out of the
                    // `token` frames — the client only ever sees its empty
                    // placeholder; the real content stays in the frozen
                    // accumulator (below) and is fetched later, sandboxed,
                    // from `block_frame`.
                    let mut gate = movement::IslandGate::new();
                    loop {
                        match tokens.next().await {
                            Some(Ok(t)) => {
                                for frame in gate.push(&t) {
                                    yield Ok(sse_frame("token", &frame));
                                }
                            }
                            Some(Err(e)) => {
                                yield Ok(sse_frame("error", &e.to_string()));
                                return;
                            }
                            None => break,
                        }
                    }
                    let (accumulated, trailing) = gate.finish();
                    if let Some(t) = trailing {
                        yield Ok(sse_frame("token", &t));
                    }
                    movement::finish_streamed_move(move_type, &accumulated)
                }
                MoveRender::Structured => {
                    match movement::generate_move(&ai, policy, move_type, &ctx).await {
                        Ok(mv) => mv,
                        Err(e) => {
                            yield Ok(sse_frame("error", &e.to_string()));
                            return;
                        }
                    }
                }
            };

            if generated.repaired {
                // §9 ladder telemetry signal: the first response violated the
                // Move JSON contract and needed a repair round.
                if let Err(e) = event_log.append(
                    Some(&prep.node_id),
                    EventKind::SchemaViolation {
                        move_type: move_type.to_string(),
                        detail: "required one repair round".to_string(),
                    },
                ) {
                    eprintln!("event log append failed: {e}");
                }
            }

            let move_id = engine::new_id();
            if let Err(e) = event_log.append(
                Some(&prep.node_id),
                EventKind::MoveGenerated {
                    move_id: move_id.clone(),
                    move_type: move_type.to_string(),
                    tactics: generated.tactics.clone(),
                    rung: format!("{policy:?}"),
                },
            ) {
                eprintln!("event log append failed: {e}");
            }

            if generated.graded {
                graded = Some((move_id, generated));
                break;
            }

            if move_type == MoveType::Plan
                && !generated.proposed_outline.is_empty()
                && generated.proposed_outline != prep.outline_titles
            {
                // Structural proposal (§5 propose→approve, non-destructive):
                // persist it and end this generation request without a node —
                // approval is a separate user action (`/plan/decide`), never
                // assumed. Nothing before this point was persisted (`finalize`
                // only runs after a graded move), so an unapproved/rejected
                // proposal leaves no trace beyond the event log already
                // appended above; `/plan/decide` appends the resolution.
                let proposal = PlanProposal {
                    move_id,
                    node_id: prep.node_id.clone(),
                    html: generated.html.clone(),
                    proposed: generated.proposed_outline.clone(),
                    resolved: false,
                };
                let payload = serde_json::to_string(&proposal).unwrap_or_default();
                if let Err(e) =
                    state
                        .store
                        .write_doc_file(&doc_id, "outline.proposal.json", &payload)
                {
                    yield Ok(sse_frame("error", &e.to_string()));
                    return;
                }
                yield Ok(sse_frame("plan_proposal", &payload));
                yield Ok(sse_frame("done", ""));
                return;
            }

            // Ungraded: both streamed moves (tokens already yielded above) and
            // structured-but-ungraded moves (one full frame here) render as
            // sanitized prose in the app origin — same contract, same client
            // path (`movement.rs` module docs).
            if matches!(move_type.render(), MoveRender::Structured) {
                yield Ok(sse_frame("token", &generated.html));
            }
            content_html.push_str(&generated.html);
            content_html.push('\n');
            ctx.prior_moves.push(MoveRecord {
                move_type,
                graded: false,
            });
            ctx.node_tail = tail_chars(&content_html, NODE_TAIL_BUDGET);
        }

        let Some((move_id, graded)) = graded else {
            yield Ok(sse_frame(
                "error",
                "could not produce a graded check for this node",
            ));
            return;
        };

        match finalize(&state, &doc_id, &prep, &content_html, &move_id, &graded).await {
            Ok(()) => {
                // The client fetches the exercise sandboxed from its own
                // frame endpoint (§4.4) — this event just signals it's ready
                // and carries the node id needed to build that URL, since
                // `state.nodeId` isn't set client-side until `done` below.
                yield Ok(sse_frame("exercise", &prep.node_id));
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

#[derive(Deserialize)]
pub struct PlanDecisionReq {
    approve: bool,
}

/// The pending (or already-decided) `plan` proposal — `<doc>/outline.proposal.json`.
/// Written by `generate_node` when a `plan` move proposes a structural
/// outline change; read and updated by `decide_plan_proposal`.
#[derive(Serialize, Deserialize)]
struct PlanProposal {
    move_id: String,
    node_id: String,
    html: String,
    proposed: Vec<String>,
    /// Set once a decision has been recorded — guards against replaying a
    /// stale proposal (e.g. reject, then approve the same file again) after
    /// the outline has already moved on.
    #[serde(default)]
    resolved: bool,
}

/// Resolves a `plan` move's proposed outline revision (§5 propose→approve):
/// on approval, rebuilds `outline.json`'s item list from the proposed titles
/// (topic unchanged); on rejection, leaves `outline.json` untouched. Either
/// way appends a `PlanDecided` event (§9 telemetry: acceptance rate joined
/// back to the generating move) and marks the proposal `resolved` so it
/// can't be replayed. Returns the resolved outline view so the client can
/// re-render.
///
/// §S5: a proposed title that matches an existing item's title **reuses that
/// item's id** — the model only returns titles, and re-minting ids on every
/// approval would orphan every already-generated node file on the very next
/// silent reorder. A title with no match (a genuinely new item) mints a
/// fresh id. This can't reassign an existing id to a different title (ids
/// are never looked up by anything but exact title match here), so the
/// worst case is an orphaned node file for a renamed/removed title — that's
/// fine per §5 (nothing destroyed, just unreachable from the current
/// outline), not silent corruption. Rebuilt as a linear chain (§S5's own
/// scope: `plan` proposes titles only, never edges) — one consequence worth
/// naming: a title-only rename mints a fresh id with no prerequisites
/// satisfied, so everything from that point on re-locks even though the
/// learner may have already demonstrated the equivalent concept under the
/// old title. Accepted for S5 (the alternative, fuzzy-matching titles across
/// a rename, needs the model to signal "this is the same concept renamed"
/// explicitly — not something the current `plan` contract carries).
pub async fn decide_plan_proposal(
    State(state): State<AppState>,
    Path(doc_id): Path<String>,
    Json(body): Json<PlanDecisionReq>,
) -> Result<Json<OutlineResp>, ApiError> {
    let proposal_json = state
        .store
        .read_doc_file(&doc_id, "outline.proposal.json")?;
    let mut proposal: PlanProposal =
        serde_json::from_str(&proposal_json).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    if proposal.resolved {
        return Err(ApiError::BadRequest(
            "this proposal was already decided".to_string(),
        ));
    }

    if body.approve {
        // Guarded read-modify-write (§S8): a sub-node spawn (`ask_question`)
        // can insert into `outline.json` from a concurrent request, the same
        // race `interaction_lock` closed for node interaction appends.
        state.store.update_outline_file(&doc_id, |json| {
            let mut outline: Outline = serde_json::from_str(json).map_err(|e| e.to_string())?;
            let ids: Vec<String> = proposal
                .proposed
                .iter()
                .map(|title| {
                    outline
                        .items
                        .iter()
                        .find(|i| &i.title == title)
                        .map(|i| i.id.clone())
                        .unwrap_or_else(engine::new_id)
                })
                .collect();
            // A `plan` move only ever proposes titles for the main line
            // (§S4/§S5) — sub-nodes spawned from a question (§S8) are never
            // among them, so rebuilding `outline.items` from `proposed`
            // wholesale would silently drop them. Preserve them verbatim.
            let sub_nodes: Vec<OutlineItem> = outline
                .items
                .iter()
                .filter(|i| i.parent_id.is_some())
                .cloned()
                .collect();
            outline.items = proposal
                .proposed
                .iter()
                .cloned()
                .zip(ids.iter().cloned())
                .enumerate()
                .map(|(idx, (title, id))| OutlineItem {
                    id,
                    title,
                    prerequisites: if idx == 0 {
                        Vec::new()
                    } else {
                        vec![ids[idx - 1].clone()]
                    },
                    parent_id: None,
                })
                .collect();
            outline.items.extend(sub_nodes);
            serde_json::to_string(&outline).map_err(|e| e.to_string())
        })?;
    }

    let event_log = state.store.event_log(&doc_id)?;
    if let Err(e) = event_log.append(
        Some(&proposal.node_id),
        EventKind::PlanDecided {
            move_id: proposal.move_id.clone(),
            approved: body.approve,
        },
    ) {
        eprintln!("event log append failed: {e}");
    }

    proposal.resolved = true;
    state.store.write_doc_file(
        &doc_id,
        "outline.proposal.json",
        &serde_json::to_string(&proposal).unwrap_or_default(),
    )?;

    let outline_json = state.store.read_doc_file(&doc_id, "outline.json")?;
    let outline: Outline =
        serde_json::from_str(&outline_json).map_err(|e| ApiError::Internal(e.to_string()))?;
    let items = outline_view(&state, &doc_id, &outline)?;
    let suggested_revisit = suggested_revisit(&state, &doc_id)?;
    Ok(Json(OutlineResp {
        items,
        suggested_revisit,
    }))
}

/// Read-only outline + gate state (§S5) — a GET is fine here (§3.1 only
/// forbids state-changing endpoints on GET); the client needs this to decide
/// what to render as reachable and whether to show "skip" at all.
pub async fn get_outline(
    State(state): State<AppState>,
    Path(doc_id): Path<String>,
) -> Result<Json<OutlineResp>, ApiError> {
    let outline_json = state.store.read_doc_file(&doc_id, "outline.json")?;
    let outline: Outline =
        serde_json::from_str(&outline_json).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let items = outline_view(&state, &doc_id, &outline)?;
    let suggested_revisit = suggested_revisit(&state, &doc_id)?;
    Ok(Json(OutlineResp {
        items,
        suggested_revisit,
    }))
}

/// Skips the given node (§S5, "botão pular"): the node stays open (not
/// demonstrated), just deferred — a `NodeSkipped` event, not a mutation of
/// the outline or any node file. Rejected for a node that's still `locked`
/// (skipping something you were never able to reach carries no real
/// signal); rejected for an unknown id. Not gated on "is there actually
/// another available node" — the client only shows the button when there
/// is, and if it's called anyway when this is the only available node, the
/// skip is a harmless no-op (the node was already `Attempted`/`available`,
/// so its resolved state doesn't change). The actual revisit *suggestion*
/// (which skipped node to come back to) is `events::aggregate::
/// revisit_suggestion`, surfaced on every `OutlineResp` — that's the
/// scheduler; there is no separate "it becomes inevitable" state machine
/// because that case is just the availability set having one element, which
/// needs no extra bookkeeping to detect.
pub async fn skip_node(
    State(state): State<AppState>,
    Path((doc_id, item_id)): Path<(String, String)>,
) -> Result<Json<OutlineResp>, ApiError> {
    let outline_json = state.store.read_doc_file(&doc_id, "outline.json")?;
    let outline: Outline =
        serde_json::from_str(&outline_json).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let views = outline_view(&state, &doc_id, &outline)?;
    let this = views
        .iter()
        .find(|v| v.id == item_id)
        .ok_or_else(|| ApiError::BadRequest("unknown outline item".to_string()))?;
    if this.state == "locked" {
        return Err(ApiError::BadRequest(
            "cannot skip a locked node".to_string(),
        ));
    }

    let event_log = state.store.event_log(&doc_id)?;
    if let Err(e) = event_log.append(Some(&item_id), EventKind::NodeSkipped) {
        eprintln!("event log append failed: {e}");
    }
    spawn_profile_distillation(state.clone(), doc_id.clone(), false);

    let items = outline_view(&state, &doc_id, &outline)?;
    let suggested_revisit = suggested_revisit(&state, &doc_id)?;
    Ok(Json(OutlineResp {
        items,
        suggested_revisit,
    }))
}

/// One interaction-layer item, as shown to the client (§4.3) — the same
/// `body_html` the append-only layer already stores, just retagged for
/// display; nothing here is regraded or re-served as gradeable.
#[derive(Serialize)]
struct InteractionView {
    kind: &'static str,
    body_html: String,
    /// Set for a `qa` thread that spawned a sub-node (§S8): the client
    /// fetches `GET .../nodes/{child_node_id}` and splices its content
    /// inline at `anchor_block`, permanently — not a toggle.
    #[serde(skip_serializing_if = "Option::is_none")]
    child_node_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    anchor_block: Option<String>,
}

#[derive(Serialize)]
pub struct NodeView {
    /// Frozen content-layer prose (§4.3) — the exercise form is stripped out
    /// (it renders separately, sandboxed) and never re-embedded here.
    content_html: String,
    /// Whether this node still has an active, answerable exercise (§4.4) —
    /// `false` once `Demonstrated`, so a solved node reads as done rather
    /// than re-prompting. The exercise HTML itself is never sent here; the
    /// client fetches it, sandboxed, from `GET .../exercise-frame`.
    has_exercise: bool,
    interactions: Vec<InteractionView>,
    demonstrated: bool,
    /// This node's outline title (§S8): a sub-node isn't shown in the
    /// sidebar, so the client has no other way to label it once spliced
    /// inline. Degrades to "" if the outline no longer carries this item,
    /// same convention as `topic_and_title`.
    title: String,
}

/// Non-destructive read of an already-generated node (§S5, §4.3) — the
/// counterpart to `prepare`'s "already generated" refusal: revisiting a
/// skipped or demonstrated node must not regenerate it (that would clobber
/// the interaction layer, see `prepare`'s doc comment), so there has to be
/// a way to just look at what's there. GET is fine (§3.1: no state changes).
///
/// The exercise is split back out of the frozen `content.html` (it's stored
/// inline, `blocks` then the exercise markup, per `engine::assemble_node`)
/// rather than duplicated at write time — one stored copy, split on read.
/// The split is by `data-block-id` presence (`prose_blocks_only`), not a
/// search for the `<form` substring: the model doesn't always wrap the
/// exercise in a bare `<form>` (e.g. `<div><p>question</p><form>…`), and a
/// substring split would leave that wrapper's prefix dangling in the prose.
pub async fn get_node(
    State(state): State<AppState>,
    Path((doc_id, node_id)): Path<(String, String)>,
) -> Result<Json<NodeView>, ApiError> {
    let node = state.store.read_node(&doc_id, &node_id)?;
    let event_log = state.store.event_log(&doc_id)?;
    let states = node_states(
        event_log
            .iter()
            .map_err(|e| ApiError::Internal(e.to_string()))?,
    );
    let demonstrated = matches!(states.get(&node_id), Some(NodeState::Demonstrated));

    let content_html = learnive_core::redact_interactive_blocks(&learnive_core::prose_blocks_only(
        &node.content.html,
    ));
    let exercise_html = if demonstrated {
        None
    } else {
        state
            .store
            .read_doc_file(&doc_id, &format!("{node_id}.rubric.json"))
            .ok()
            .and_then(|json| serde_json::from_str::<RubricSidecar>(&json).ok())
            .map(|sidecar| sidecar.exercise_html)
    };

    let interactions = node
        .interaction
        .iter()
        .map(|item| match item {
            InteractionItem::Annotation { body_html, .. } => InteractionView {
                kind: "annotation",
                body_html: body_html.clone(),
                child_node_id: None,
                anchor_block: None,
            },
            InteractionItem::Thread {
                kind: ThreadKind::Qa,
                body_html,
                anchor_block,
                child_node_id,
                ..
            } => InteractionView {
                kind: "qa",
                body_html: body_html.clone(),
                child_node_id: child_node_id.clone(),
                anchor_block: anchor_block.clone(),
            },
            InteractionItem::Thread {
                kind: ThreadKind::Remediation,
                body_html,
                ..
            } => InteractionView {
                kind: "remediation",
                body_html: body_html.clone(),
                child_node_id: None,
                anchor_block: None,
            },
        })
        .collect();

    let title = topic_and_title(&state, &doc_id, &node_id)
        .map(|(_, title)| title)
        .unwrap_or_default();

    Ok(Json(NodeView {
        content_html,
        has_exercise: exercise_html.is_some(),
        interactions,
        demonstrated,
        title,
    }))
}

#[derive(Deserialize)]
pub struct FrameQuery {
    theme: Option<String>,
}

/// Serves the node's currently-active exercise as its own real HTTP response
/// (§4.4), not `iframe.srcdoc` — `srcdoc` documents inherit the parent page's
/// CSP, which would break the moment the app origin's CSP tightens (planned
/// hardening, `security.rs`). This response carries its **own** CSP, set
/// after `engine::render_sandbox_frame`'s doc comment's reasoning; isolation
/// itself still comes from the `<iframe sandbox="allow-scripts">` the client
/// builds around it, not from CSP.
///
/// GET, read-only: looks up the same `.rubric.json` sidecar `get_node`/
/// `answer` already read, so remediation's freshly-overwritten sidecar (§8.2)
/// is served here without any change to the write path.
pub async fn exercise_frame(
    State(state): State<AppState>,
    Path((doc_id, node_id)): Path<(String, String)>,
    Query(query): Query<FrameQuery>,
) -> Result<Response, ApiError> {
    let sidecar_json = state
        .store
        .read_doc_file(&doc_id, &format!("{node_id}.rubric.json"))?;
    let sidecar: RubricSidecar =
        serde_json::from_str(&sidecar_json).map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let theme = query.theme.as_deref().unwrap_or("dark");
    let page = engine::render_sandbox_frame(&sidecar.exercise_html, theme, true);
    Ok(sandbox_frame_response(page))
}

/// Serves a single interactive-island content block (§4.4, §S11) — the
/// generalized counterpart to `exercise_frame`: any `data-interactive` block
/// the model opened mid-move (`<figure data-interactive>`, gated out of the
/// live token stream by `movement::IslandGate`), not just the exercise.
///
/// GET, read-only: reads the node's frozen `content.html` straight from
/// storage (not a sidecar — an island has no server-only rubric to hide, so
/// there's nothing to split off at write time beyond what `redact_interactive_blocks`
/// already keeps out of `content_html`) and extracts the one block by id.
/// `graded: false` — §4.4's structured-answer-artifact requirement is for
/// graded exercises only; a plain visualization just needs the theme/height
/// harness `render_sandbox_frame` always includes.
pub async fn block_frame(
    State(state): State<AppState>,
    Path((doc_id, node_id, block_id)): Path<(String, String, String)>,
    Query(query): Query<FrameQuery>,
) -> Result<Response, ApiError> {
    let node = state.store.read_node(&doc_id, &node_id)?;
    let block_html = learnive_core::extract_block_by_id(&node.content.html, &block_id)
        .ok_or_else(|| ApiError::BadRequest("block not found".to_string()))?;

    let theme = query.theme.as_deref().unwrap_or("dark");
    let page = engine::render_sandbox_frame(&block_html, theme, false);
    Ok(sandbox_frame_response(page))
}

/// Shared response envelope for a sandboxed content frame (§4.4):
/// `exercise_frame` and `block_frame` both serve `engine::render_sandbox_frame`'s
/// output through it. Its own CSP is deliberately permissive relative to the
/// app-origin default (`security.rs`) — this is the only surface allowed to
/// run generated `<script>` at all, and it does so already isolated by the
/// client's `<iframe sandbox="allow-scripts">` with no `allow-same-origin` —
/// no ambient token, cookies, or parent DOM access regardless of what this
/// policy permits. `frame-ancestors 'self'` keeps it from being embedded
/// anywhere but this app's own page. Never cached: an exercise's URL gets
/// reused in place by remediation (§8.2), and a block's frame is cheap
/// enough to just never cache either, avoiding a second caching rule to keep
/// in sync with the write path.
fn sandbox_frame_response(page: String) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .header(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(
                "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
                 img-src data:; connect-src 'none'; form-action 'none'; frame-ancestors 'self'",
            ),
        )
        .body(Body::from(page))
        .expect("valid frame response")
}

// ---------------------------------------------------------------------------
// §S6 — reading interactions ("the document is the answer", §9). All three
// endpoints below only operate on an already-finalized node: the frozen
// content layer they anchor against, and the interaction layer they append
// to, both require the node file to already exist (`store::append_interaction`
// is read-then-write against it). A node still mid-generation has neither yet
// — scoped out of this slice, see `EventKind::NodeReadToEnd`'s doc comment.
// ---------------------------------------------------------------------------

/// Minimal HTML-escaper for embedding user-authored plain text inside
/// server-built HTML (the question/annotation text itself — never the
/// model's reply, which is already HTML under `PROSE_HTML_CONTRACT`). The
/// client also sanitizes every `body_html` at render (defense in depth,
/// `sanitizeHtml` in `index.html`), but nothing here should ever depend on
/// that: user text is escaped before it is stored, not just before it is shown.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Resolves a client-supplied anchor against the node's frozen content layer
/// (§4.3) — rejecting one that doesn't resolve keeps the interaction layer's
/// "always references real content IDs" invariant true by construction,
/// rather than trusting whatever block id the client happens to send.
fn resolve_anchor(node: &Node, anchor: &Anchor) -> Result<(), ApiError> {
    node.content.resolve(anchor).ok_or_else(|| {
        ApiError::BadRequest("anchor does not resolve against this node's content".to_string())
    })?;
    Ok(())
}

/// The exact text an anchor points at: the selected quote if present, else
/// the whole anchored block's text (question-on-the-line, §9 — no
/// selection, context = the reading line's current block).
fn anchor_text(node: &Node, anchor: &Anchor) -> Option<String> {
    match &anchor.quote {
        Some(q) => Some(q.exact.clone()),
        None => node
            .content
            .blocks
            .iter()
            .find(|b| b.id == anchor.block_id)
            .map(|b| b.text.clone()),
    }
}

/// Topic + this node's title from the outline (§S4/§S5) — degrades to an
/// empty title if the outline no longer carries this item (e.g. renamed
/// away by a `plan` approval, §S5), the same graceful-empty-field contract
/// `MoveContext` already uses.
fn topic_and_title(
    state: &AppState,
    doc_id: &str,
    node_id: &str,
) -> Result<(String, String), ApiError> {
    let outline_json = state.store.read_doc_file(doc_id, "outline.json")?;
    let outline: Outline =
        serde_json::from_str(&outline_json).map_err(|e| ApiError::Internal(e.to_string()))?;
    let title = outline
        .items
        .iter()
        .find(|i| i.id == node_id)
        .map(|i| i.title.clone())
        .unwrap_or_default();
    Ok((outline.topic, title))
}

#[derive(Deserialize)]
pub struct AskReq {
    question: String,
    anchor: Anchor,
}

/// Discriminated by `kind` (§S8): `inline` is today's woven-reply behavior
/// unchanged; `spawn` is a brand-new sub-node the client splices permanently
/// into the document right after `anchor_block` — not a toggle, not a
/// separate page.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AskResp {
    Inline {
        body_html: String,
    },
    Spawn {
        node_id: String,
        title: String,
        content_html: String,
        anchor_block: String,
    },
}

/// §S6/§S8: "seleção→pergunta" / "pergunta-na-linha" (§9). The reading-line
/// highlight itself stays ephemeral client UI state, never persisted (§9) —
/// the client sends whichever block was current at ask-time only as this
/// one request's anchor (a whole-block `Anchor`, no `quote`); a real text
/// selection sends the same `Anchor` with a `quote`.
///
/// The tutor decides per question (§7/§S8) whether to answer inline (woven
/// into the document as a `Qa` thread, never a side chat) or spawn a real
/// sub-node — a self-contained elaboration, versioned and present in
/// `outline.json` like any node, parented to this one. Either way a `Qa`
/// thread is appended here; a spawn's thread carries `child_node_id` instead
/// of the answer prose, so a reload (`get_node`) knows to re-splice the same
/// sub-node rather than re-asking.
pub async fn ask_question(
    State(state): State<AppState>,
    Path((doc_id, node_id)): Path<(String, String)>,
    Json(body): Json<AskReq>,
) -> Result<Json<AskResp>, ApiError> {
    let question = body.question.trim();
    if question.is_empty() {
        return Err(ApiError::BadRequest(
            "question must not be empty".to_string(),
        ));
    }
    let node = state.store.read_node(&doc_id, &node_id)?;
    resolve_anchor(&node, &body.anchor)?;
    let context = anchor_text(&node, &body.anchor);
    let (topic, title) = topic_and_title(&state, &doc_id, &node_id)?;
    let node_tail = tail_chars(&node.content.html, NODE_TAIL_BUDGET);

    let ai = state.ai.load_full();
    // Degrades to `Inline` on failure (a bad/unparseable decision, or a
    // provider error after the one bounded repair) rather than propagating —
    // every other optional-context signal in this codebase degrades the same
    // way (`objective_for`, `grounding_for`, `profile_for` all fall back to
    // ""); `/ask` must stay at least as reliable as it was before §S8, not
    // regress into failing outright when the classifier call itself fails.
    let decision = engine::decide_ask_response(
        &ai,
        &topic,
        &title,
        &node_tail,
        context.as_deref(),
        question,
    )
    .await
    .unwrap_or(AskDecision::Inline);

    let move_id = engine::new_id();
    let event_log = state.store.event_log(&doc_id)?;
    if let Err(e) = event_log.append(
        Some(&node_id),
        EventKind::QuestionAsked {
            move_id: move_id.clone(),
            anchor_block: body.anchor.block_id.clone(),
        },
    ) {
        eprintln!("event log append failed: {e}");
    }

    match decision {
        AskDecision::Inline => {
            let answer_html = engine::answer_question(
                &ai,
                &topic,
                &title,
                &node_tail,
                context.as_deref(),
                question,
            )
            .await?;

            let body_html = format!(
                "<p class=\"question\"><strong>You asked:</strong> {}</p>\n<div class=\"answer\">{answer_html}</div>",
                escape_html(question)
            );
            state.store.append_interaction(
                &doc_id,
                &node_id,
                InteractionItem::Thread {
                    id: move_id,
                    kind: ThreadKind::Qa,
                    anchor_block: Some(body.anchor.block_id),
                    body_html: body_html.clone(),
                    child_node_id: None,
                },
            )?;

            Ok(Json(AskResp::Inline { body_html }))
        }
        AskDecision::Spawn { title: sub_title } => {
            let sub_id = engine::new_id();
            let prose = engine::generate_subnode_prose(
                &ai,
                &topic,
                &sub_title,
                &title,
                &node_tail,
                context.as_deref(),
                question,
            )
            .await?;
            let sub_node = engine::assemble_content_node(&doc_id, &sub_id, &prose)?;
            state.store.write_node(&sub_node)?;
            state.store.update_outline_file(&doc_id, |json| {
                let mut outline: Outline = serde_json::from_str(json).map_err(|e| e.to_string())?;
                outline.items.push(OutlineItem {
                    id: sub_id.clone(),
                    title: sub_title.clone(),
                    prerequisites: Vec::new(),
                    parent_id: Some(node_id.clone()),
                });
                serde_json::to_string(&outline).map_err(|e| e.to_string())
            })?;

            let content_html = learnive_core::redact_interactive_blocks(
                &learnive_core::prose_blocks_only(&sub_node.content.html),
            );
            let body_html = format!(
                "<p class=\"question\"><strong>You asked:</strong> {}</p>\n<p>↳ spawned a new section: {}</p>",
                escape_html(question),
                escape_html(&sub_title)
            );
            state.store.append_interaction(
                &doc_id,
                &node_id,
                InteractionItem::Thread {
                    id: move_id,
                    kind: ThreadKind::Qa,
                    anchor_block: Some(body.anchor.block_id.clone()),
                    body_html,
                    child_node_id: Some(sub_id.clone()),
                },
            )?;

            Ok(Json(AskResp::Spawn {
                node_id: sub_id,
                title: sub_title,
                content_html,
                anchor_block: body.anchor.block_id,
            }))
        }
    }
}

#[derive(Deserialize)]
pub struct AnnotateReq {
    body: String,
    anchor: Anchor,
}

#[derive(Serialize)]
pub struct AnnotateResp {
    body_html: String,
}

/// §S6/§9/§11: the living document is the only place for user notes — the
/// source viewer is read-only. No AI call: this is the user's own words,
/// escaped (never trusted as HTML, see `escape_html`) and anchored.
pub async fn annotate(
    State(state): State<AppState>,
    Path((doc_id, node_id)): Path<(String, String)>,
    Json(body): Json<AnnotateReq>,
) -> Result<Json<AnnotateResp>, ApiError> {
    let text = body.body.trim();
    if text.is_empty() {
        return Err(ApiError::BadRequest(
            "annotation must not be empty".to_string(),
        ));
    }
    let node = state.store.read_node(&doc_id, &node_id)?;
    resolve_anchor(&node, &body.anchor)?;

    let event_log = state.store.event_log(&doc_id)?;
    if let Err(e) = event_log.append(
        Some(&node_id),
        EventKind::AnnotationAdded {
            anchor_block: body.anchor.block_id.clone(),
        },
    ) {
        eprintln!("event log append failed: {e}");
    }

    let body_html = format!("<p>{}</p>", escape_html(text));
    state.store.append_interaction(
        &doc_id,
        &node_id,
        InteractionItem::Annotation {
            id: engine::new_id(),
            anchor: body.anchor,
            body_html: body_html.clone(),
        },
    )?;

    Ok(Json(AnnotateResp { body_html }))
}

#[derive(Serialize)]
pub struct AckResp {
    ok: bool,
}

/// §S6 "Ritmo": scroll-to-end is captured as a pure signal event; nothing
/// consumes it yet (see `EventKind::NodeReadToEnd`'s doc comment on why
/// gating the next `decide_move` on it is a separate, deferred slice).
pub async fn read_to_end(
    State(state): State<AppState>,
    Path((doc_id, node_id)): Path<(String, String)>,
) -> Result<Json<AckResp>, ApiError> {
    let event_log = state.store.event_log(&doc_id)?;
    if let Err(e) = event_log.append(Some(&node_id), EventKind::NodeReadToEnd) {
        eprintln!("event log append failed: {e}");
    }
    Ok(Json(AckResp { ok: true }))
}

/// Last `max_chars` characters of `s` (char-boundary safe) — the §14
/// verbatim-tail budget threaded into `MoveContext::node_tail`.
fn tail_chars(s: &str, max_chars: usize) -> String {
    match s.char_indices().rev().nth(max_chars.saturating_sub(1)) {
        Some((i, _)) => s[i..].to_string(),
        None => s.to_string(),
    }
}

/// Data ready to generate a node.
struct NodePrep {
    topic: String,
    title: String,
    context: String,
    node_id: String,
    /// Retrieved source passages formatted for the prompt (§10). Empty when the
    /// index has nothing relevant yet (acquisition may still be running, §14).
    grounding: String,
    /// Compact `objective::summarize` of the document's current objective
    /// (§S4) — empty for a pre-S4 document with no `objective.json`, same
    /// graceful-degradation convention as `grounding`.
    objective: String,
    /// Evidence-profile text for `MoveContext::profile` (§7/§S7) —
    /// `profile_for`'s always-fresh evidence table plus any distilled
    /// traits/hypotheses. Empty for a document with no evidence/distillation
    /// yet, same graceful-degradation convention as `grounding`/`objective`.
    profile: String,
    /// Current outline item titles, for `generate_node` to detect whether a
    /// `plan` move's proposal is a real structural change (§5).
    outline_titles: Vec<String>,
}

/// Loads the outline, resolves the requested item by its stable id, and
/// enforces the §S5 availability gate (fallible work, no `yield`). Locked —
/// some prerequisite isn't yet `Demonstrated` — is refused outright: this is
/// the real enforcement point for "disponibilidade + gate nas arestas", not
/// just a UI affordance. (Prerequisites are monotonic — `Demonstrated` never
/// reverts, S5's `node_states` doc comment — so a node that was ever
/// generated was unlocked at the time and stays unlocked forever; the lock
/// check alone is sufficient, no separate "already touched" bypass needed.)
///
/// Also refuses regenerating a node whose file already exists, full stop,
/// regardless of gate state: making already-generated outline items
/// clickable, so the learner can revisit a skipped or demonstrated node,
/// opened a path where `finalize` would silently overwrite that file with
/// a freshly assembled — and interaction-layer-empty — node, destroying
/// §4.3's append-only history. §5: "conhecimento nunca é editado
/// destrutivamente". The client now always tries `GET .../nodes/{id}`
/// before ever calling generate on an outline click; this check is the
/// server-side backstop, not the primary defense.
async fn prepare(state: &AppState, doc_id: &str, item_id: &str) -> Result<NodePrep, String> {
    let outline_json = state
        .store
        .read_doc_file(doc_id, "outline.json")
        .map_err(|e| e.to_string())?;
    let outline: Outline = serde_json::from_str(&outline_json).map_err(|e| e.to_string())?;
    let idx = outline
        .items
        .iter()
        .position(|i| i.id == item_id)
        .ok_or_else(|| "unknown outline item".to_string())?;
    let item = outline.items[idx].clone();

    let event_log = state.store.event_log(doc_id).map_err(|e| e.to_string())?;
    let states = node_states(event_log.iter().map_err(|e| e.to_string())?);
    let unlocked = item
        .prerequisites
        .iter()
        .all(|p| matches!(states.get(p), Some(NodeState::Demonstrated)));
    if !unlocked {
        return Err("this node is locked: its prerequisites are not yet demonstrated".to_string());
    }
    if state.store.read_node(doc_id, &item.id).is_ok() {
        return Err(
            "this node was already generated; fetch it instead of regenerating".to_string(),
        );
    }

    let context = outline.items[..idx]
        .iter()
        .map(|i| i.title.as_str())
        .collect::<Vec<_>>()
        .join("; ");
    let grounding = grounding_for(state, &format!("{} {}", outline.topic, item.title)).await;
    let objective = objective_for(state, doc_id);
    let profile = profile_for(state, doc_id);
    let outline_titles = outline.items.iter().map(|i| i.title.clone()).collect();
    Ok(NodePrep {
        topic: outline.topic,
        title: item.title,
        context,
        node_id: item.id,
        grounding,
        objective,
        profile,
        outline_titles,
    })
}

/// Compact objective summary for `MoveContext::objective` (§S4) — a document
/// with no `objective.json` yet (pre-S4) or an empty version chain degrades
/// to "", the same way `grounding_for` degrades when nothing is indexed.
fn objective_for(state: &AppState, doc_id: &str) -> String {
    let Ok(json) = state.store.read_doc_file(doc_id, "objective.json") else {
        return String::new();
    };
    let Ok(log) = serde_json::from_str::<ObjectiveLog>(&json) else {
        return String::new();
    };
    log.current().map(objective::summarize).unwrap_or_default()
}

/// §9 "mover o degrau por documento": the config-derived `config_prior` is a
/// ceiling, not the rung itself — this document's own `events.jsonl` can
/// step it down (never up) via `calibrate_rung`. Degrades to `config_prior`
/// unchanged if the log can't be read, same "optional context degrades,
/// never blocks generation" convention as `objective_for`/`grounding_for`/
/// `profile_for` — a telemetry read failing must not fail the whole node
/// generation. Deliberately synchronous and self-contained (constructs and
/// fully drains the log's iterator in one call, never holding it as a local
/// in the caller): `EventLog::iter`'s `Box<dyn Iterator>` isn't `Send`, and
/// `generate_node`'s SSE stream is a real `async` state machine — a `Box<dyn
/// Iterator>` local spanning any of its later `.await`s makes the whole
/// stream `!Send`, which `Body::from_stream` requires. Returning here before
/// any `.await` keeps the non-`Send` value confined to this function's own
/// stack frame.
fn rung_for(state: &AppState, doc_id: &str, config_prior: AgentPolicy) -> AgentPolicy {
    let Ok(event_log) = state.store.event_log(doc_id) else {
        return config_prior;
    };
    let Ok(events) = event_log.iter() else {
        return config_prior;
    };
    calibrate_rung(config_prior, &ladder_signals(events))
}

/// Reads `<doc>/profile.json` (§S7) if it exists yet — `None` for a document
/// with no distillation yet (a fresh document, or one predating this slice),
/// same graceful-degradation convention as `objective_for`.
fn read_profile(state: &AppState, doc_id: &str) -> Option<ProfileProjection> {
    let json = state.store.read_doc_file(doc_id, "profile.json").ok()?;
    serde_json::from_str(&json).ok()
}

/// Compact evidence-profile text for `MoveContext::profile` (§7/§S7): the
/// always-fresh evidence table (`tactic_outcomes`, 0 LLM tokens, recomputed
/// on every call) plus whatever distilled traits/hypotheses `profile.json`
/// currently holds. Degrades to "" on a document with nothing yet, same
/// convention as `grounding_for`/`objective_for`.
fn profile_for(state: &AppState, doc_id: &str) -> String {
    let Ok(event_log) = state.store.event_log(doc_id) else {
        return String::new();
    };
    let Ok(events) = event_log.iter() else {
        return String::new();
    };
    let table = tactic_outcomes(events);
    let evidence_text = profile::evidence_table_text(&table);
    let projection = read_profile(state, doc_id);
    profile::render_for_prompt(&evidence_text, projection.as_ref())
}

/// Fires the rare profile distillation (§7.1) in the background — same
/// fire-and-forget shape as `spawn_acquisition`. `node_closed: true` fires on
/// EVERY node close (the answer→advance path, the hottest transition in the
/// app, §14), so this must never be awaited inline: a synchronous second LLM
/// call on that path would be a latency regression on exactly the path §14
/// singles out, and on the remediation branch it would serialize behind
/// grading too, right when the learner is already stuck.
fn spawn_profile_distillation(state: AppState, doc_id: String, node_closed: bool) {
    tokio::spawn(async move {
        maybe_distill_profile(&state, &doc_id, node_closed).await;
    });
}

/// Best-effort rare profile distillation (§7.1: "destilação rara"). Never
/// surfaces failure to the caller — same convention as event-log append
/// failures elsewhere: a stale profile just means `MoveContext::profile`
/// stays one distillation behind, not a broken request. `node_closed` is
/// whether THIS call is the "fechar nó" trigger (§7.1); the ~30-event
/// fallback is checked regardless, so a document that only skips (never
/// closes a node) still eventually distills. Only ever called via
/// `spawn_profile_distillation` — never awaited on a request path.
async fn maybe_distill_profile(state: &AppState, doc_id: &str, node_closed: bool) {
    let Ok(event_log) = state.store.event_log(doc_id) else {
        return;
    };
    let Ok(events) = event_log.iter() else {
        return;
    };
    let events: Vec<_> = events.collect();
    let total_events = events.len() as u32;

    let existing = read_profile(state, doc_id);
    let distilled_through = existing.as_ref().map(|p| p.distilled_through).unwrap_or(0);
    if !profile::should_distill(node_closed, total_events, distilled_through) {
        return;
    }

    let table = tactic_outcomes(events.iter().cloned());
    let activity = activity_counts(events.into_iter());
    let evidence_text = profile::evidence_table_text(&table);

    let ai = state.ai.load_full();
    match profile::distill(
        &ai,
        &evidence_text,
        &activity,
        total_events,
        existing.as_ref(),
    )
    .await
    {
        Ok(projection) => {
            // Guard against `revise_profile` landing a manual edit while this
            // backgrounded call was in flight (§7.1) — the window is seconds
            // (an LLM call), far wider than `Store::append_interaction`'s
            // ~1ms critical section. If the on-disk profile no longer matches
            // what this call started from, something newer already landed;
            // drop this stale result rather than clobber it. Not airtight (a
            // second race can still land between this check and the write
            // below), but shrinks the window from "seconds" back down to
            // "one read+write", the same residual every sidecar file in this
            // codebase already accepts.
            if read_profile(state, doc_id) != existing {
                eprintln!("profile distillation dropped: a newer edit landed first");
                return;
            }
            if let Err(e) = state.store.write_doc_file(
                doc_id,
                "profile.json",
                &serde_json::to_string(&projection).unwrap_or_default(),
            ) {
                eprintln!("profile write failed: {e}");
            }
        }
        Err(e) => eprintln!("profile distillation failed: {e}"),
    }
}

/// Retrieves grounding passages for a concept and formats them so the model can
/// cite each by its exact id/locator (§10/§4.3). Returns "" when grounding is off
/// or nothing relevant is indexed yet.
async fn grounding_for(state: &AppState, query: &str) -> String {
    let Some(retriever) = &state.retriever else {
        return String::new();
    };
    let hits = {
        let r = retriever.read().await;
        r.retrieve(query, 4)
    };
    hits.iter()
        .map(|h| {
            format!(
                "[id: {} | loc: {} | {} — {}]\n{}",
                h.chunk.source_id,
                h.chunk.locator,
                h.chunk.source_title,
                h.chunk.section_title,
                h.chunk.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Assembles the node from the accumulated moves and persists it (node +
/// server-only sidecar). Returns the exercise HTML to the client. Content
/// generation already happened move by move in `generate_node`'s loop — this
/// is pure assembly, same shape `assemble_node` has always produced
/// (§4.3 content layer is unchanged by the move ABI, only how it's filled).
async fn finalize(
    state: &AppState,
    doc_id: &str,
    prep: &NodePrep,
    content_html: &str,
    move_id: &str,
    graded: &GeneratedMove,
) -> Result<(), String> {
    let rubric = graded
        .rubric
        .clone()
        .ok_or_else(|| "graded move carried no rubric".to_string())?;

    let exercise_id = format!("{}-ex", prep.node_id);
    let rubric_id = format!("{}-ru", prep.node_id);
    let node = engine::assemble_node(
        doc_id,
        &prep.node_id,
        content_html,
        &graded.html,
        &exercise_id,
        &rubric_id,
    )
    .map_err(|e| e.to_string())?;

    state.store.write_node(&node).map_err(|e| e.to_string())?;

    let sidecar = RubricSidecar {
        move_id: move_id.to_string(),
        rubric,
        exercise_html: graded.html.clone(),
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

    Ok(())
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
    /// Remediation EXPLANATION prose (§8.2), sanitized and shown inline.
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
    // Grabbed before any sidecar overwrite below (§8.2 replaces it on failure) —
    // this is the id `MoveGraded` must join back onto.
    let move_id = sidecar.move_id.clone();

    let ai = state.ai.load_full();
    let assessment =
        engine::grade(&ai, &sidecar.rubric, &sidecar.exercise_html, &body.answer).await?;

    let event_log = state.store.event_log(&doc_id)?;
    if let Err(e) = event_log.append(
        Some(&node_id),
        EventKind::MoveGraded {
            move_id,
            grade: reduce_grade(&assessment),
        },
    ) {
        eprintln!("event log append failed: {e}");
    }

    // Advancing requires every objective demonstrated (§8).
    if assessment.all_demonstrated() {
        spawn_profile_distillation(state.clone(), doc_id.clone(), true);
        return Ok(Json(AnswerResp {
            grades: assessment.grades,
            advance: true,
            remediation_html: None,
        }));
    }
    spawn_profile_distillation(state.clone(), doc_id.clone(), false);

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
    // (a) Explanation: a worked solution of the problem they just missed (§8.2),
    // sanitized prose — it does NOT contain the next problem and must not leak it.
    let explanation = engine::remediate(
        &ai,
        &sidecar.title,
        &sidecar.exercise_html,
        &body.answer,
        &assessment.unmet(),
        attempt,
    )
    .await?;

    // (b) A NEW gradeable problem in the sandbox, similar to the failed one and
    // grounded in the same sources (§8/§8.2). Its rubric is freshly locked and the
    // answer is never revealed (EXERCISE_HTML_CONTRACT).
    let grounding = grounding_for(&state, &format!("{} {}", sidecar.topic, sidecar.title)).await;
    let er = engine::generate_remediation_exercise(
        &ai,
        &sidecar.title,
        &sidecar.exercise_html,
        attempt,
        &grounding,
    )
    .await?;

    // The new problem is a fresh graded artifact — its own move_id, so a future
    // submission's MoveGraded joins onto it and not the one just graded above.
    // It stays on the legacy remediation path (not `decide_move`/`generate_move`):
    // L0's rule has no slot for a second graded move in one node, and the new
    // problem is required by construction (§8.2), never a model choice.
    let new_move_id = engine::new_id();
    if let Err(e) = event_log.append(
        Some(&node_id),
        EventKind::MoveGenerated {
            move_id: new_move_id.clone(),
            move_type: MoveType::Test.to_string(),
            tactics: Vec::new(),
            rung: format!("{:?}", *state.policy.load_full()),
        },
    ) {
        eprintln!("event log append failed: {e}");
    }

    // The new problem becomes the node's ACTIVE check: overwrite the server-only
    // rubric sidecar so the next submission grades IT. This is grading state, not
    // user knowledge — the §5 non-destructive rule is upheld by the append-only
    // interaction layer, which retains the full attempt/remediation trajectory.
    let new_sidecar = RubricSidecar {
        move_id: new_move_id,
        rubric: er.rubric,
        exercise_html: er.exercise_html.clone(),
        title: sidecar.title.clone(),
        topic: sidecar.topic.clone(),
    };
    state.store.write_doc_file(
        &doc_id,
        &format!("{node_id}.rubric.json"),
        &serde_json::to_string(&new_sidecar).unwrap_or_default(),
    )?;

    // Append the explanation to the interaction layer (append-only, §4.3),
    // anchored to the original exercise.
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
            body_html: explanation.clone(),
            child_node_id: None,
        },
    )?;

    Ok(Json(AnswerResp {
        grades: assessment.grades,
        advance: false,
        remediation_html: Some(explanation),
    }))
}

/// Formats an SSE frame with JSON-encoded `data` (avoids newline problems).
fn sse_frame(event: &str, data: &str) -> Bytes {
    let json = serde_json::to_string(data).unwrap_or_else(|_| "\"\"".to_string());
    Bytes::from(format!("event: {event}\ndata: {json}\n\n"))
}

/// Reduces a move's per-objective grades to one outcome (§7's evidence table
/// is per-move, not per-objective) — worst case wins: any not-demonstrated
/// objective makes the whole move not-demonstrated, else any partial makes it
/// partial, else it's fully demonstrated.
fn reduce_grade(assessment: &engine::Assessment) -> Grade {
    if assessment
        .grades
        .iter()
        .any(|g| g.grade == Grade::NotDemonstrated)
    {
        Grade::NotDemonstrated
    } else if assessment.grades.iter().any(|g| g.grade == Grade::Partial) {
        Grade::Partial
    } else {
        Grade::Demonstrated
    }
}

// ---------------------------------------------------------------------------
// Provider selection (§12) — any OpenAI-compatible endpoint is swappable. Order:
// a custom base URL (generic BYOK), else OpenRouter (default), else offline demo.
// ---------------------------------------------------------------------------

/// Builds the `Ai` from the environment (§12), together with the policy-ladder
/// rung (§14) that goes with it — the two must never be derived separately:
/// deriving the rung from `config` alone would desync from which `Ai` this
/// function actually returns whenever an env-var override is active (a real
/// provider while `config.provider` is still `Demo`). Precedence:
/// 1. `LEARNIVE_API_BASE_URL` (+ optional `LEARNIVE_API_KEY`) — any OpenAI-compatible
///    `chat/completions` endpoint: Inception's Mercury, OpenCode Zen, a local model.
/// 2. `LEARNIVE_OPENROUTER_KEY` — the default OpenRouter path.
/// 3. Otherwise, offline demo mode.
///
/// Rung: a real provider (any of the paths above) derives L1/L2 from the
/// free/paid intent (§12.1); the demo fallback is always L0 (demo mode = L0,
/// PLAN.md) — deterministic content, no AI call for `decide_move`.
pub fn build_ai(config: &AppConfig, secret: &SecretStore) -> (Ai, AgentPolicy) {
    let real_provider_policy = match config.intent {
        Intent::Free => AgentPolicy::L1,
        Intent::Paid => AgentPolicy::L2,
    };

    // 1. Environment override wins (dev / `.env`; CLAUDE.md: the real env wins).
    if let Ok(base_url) = std::env::var("LEARNIVE_API_BASE_URL")
        && !base_url.is_empty()
    {
        let key = std::env::var("LEARNIVE_API_KEY")
            .ok()
            .filter(|k| !k.is_empty());
        return (
            Ai::new(
                Provider::OpenAiCompat(OpenAiCompat::new(base_url, key)),
                models_from_env(),
            ),
            real_provider_policy,
        );
    }
    if let Ok(key) = std::env::var("LEARNIVE_OPENROUTER_KEY")
        && !key.is_empty()
    {
        return (
            Ai::new(
                Provider::OpenAiCompat(OpenAiCompat::openrouter(Some(key))),
                models_from_env(),
            ),
            real_provider_policy,
        );
    }

    // 2. The provider configured in /setup, with its key from the secret store
    //    (§12). Models are derived from the free/paid intent (§12.1).
    match &config.provider {
        ProviderKind::OpenRouter => {
            if let Some(key) = secret.get("openrouter") {
                return (
                    Ai::new(
                        Provider::OpenAiCompat(OpenAiCompat::openrouter(Some(key))),
                        config.models(),
                    ),
                    real_provider_policy,
                );
            }
        }
        ProviderKind::OpenAiCompatible { base_url } => {
            return (
                Ai::new(
                    Provider::OpenAiCompat(OpenAiCompat::new(base_url.clone(), secret.get("api"))),
                    config.models(),
                ),
                real_provider_policy,
            );
        }
        ProviderKind::Demo => {}
    }

    // 3. Nothing configured → demo.
    eprintln!("No provider configured — DEMO MODE. Open /setup to configure a provider.");
    (demo_ai(), AgentPolicy::L0)
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

pub(crate) fn demo_responder(req: &crate::ai::ChatRequest) -> String {
    let text = req
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    // Branch on distinctive phrases from each prompt (engine::prompt /
    // movement::prompt). Keep these in sync with the prompt wording. The
    // movement.rs (S2) checks come FIRST: its structured generate_move
    // prompt embeds EXERCISE_HTML_CONTRACT for `test`, whose text literally
    // contains "exercise_html" and would otherwise be caught by the legacy
    // branch below with the wrong JSON shape.
    if text.contains("cold start of a living curriculum") {
        // engine::prompt::propose_objective (§6.1/§S4) contract.
        return r#"{"text":"Learn the essentials of the requested topic, well enough to explain and apply it","non_goals":[]}"#.to_string();
    }
    if text.contains("choosing the next move") {
        // movement::decide_move (L1/L2) contract.
        return r#"{"move_type":"explain","rationale":"demo: start with an explanation"}"#
            .to_string();
    }
    if text.contains("Move JSON contract") {
        // movement::generate_move (structured path only — test/profile/plan/
        // other) contract. Branch by the move-type marker embedded in its
        // system prompt ("generating a \"test\" move").
        if text.contains("\"test\" move") {
            return r#"{"html":"<form><p>Apply the concept to a new case:</p><textarea name=\"answer\" rows=\"4\" required></textarea><p><button type=\"submit\">Submit answer</button></p></form>","interactive":false,"graded":true,"tactics":["worked-example"],"objectives":[{"id":"o1","kind":"application","description":"Apply the concept to a new case","criteria":"The answer transfers the concept to a scenario not covered in the text","transfer":true}]}"#.to_string();
        }
        if text.contains("\"plan\" move") {
            // No structural change proposed — demo mode never has enough
            // signal to justify one, so the loop just continues (§S4: "no
            // proposal, no approval needed").
            return r#"{"html":"<p>No structural changes needed yet in <strong>demo mode</strong>.</p>","interactive":false,"graded":false,"tactics":[],"outline":[]}"#.to_string();
        }
        return r#"{"html":"<h2>Core concept</h2><p>This is a structured move generated in <strong>demo mode</strong> via the move ABI.</p>","interactive":false,"graded":false,"tactics":["analogy"],"objectives":[]}"#.to_string();
    }
    if text.contains("distill a learner's evidence profile") {
        // profile::prompt::distill (§7/§S7) contract — checked before the
        // generic branches below since it's also a JSON-envelope call.
        return r#"{"traits":["demo mode: not enough graded evidence yet for a real trait"],"hypotheses":["would a more concrete worked example change the outcome?"]}"#.to_string();
    }
    if text.contains("Decide how to answer it: INLINE") {
        // engine::prompt::decide_ask_response (§7/§S8) contract — demo mode
        // never has real signal to justify spawning a new section, so it
        // always answers inline, same behavior as before this slice.
        return r#"{"spawn":false,"title":""}"#.to_string();
    }
    if text.contains("<!--tactics:") {
        // movement::generate_move_stream (streamed path — explain/ask/
        // confront/integrate/revisit) prompt: plain HTML, no JSON envelope,
        // with a trailing tactics sentinel per the contract.
        return "<h2>Core concept</h2><p>This is explanatory prose generated in \
                <strong>demo mode</strong> via the move ABI.</p>\n\
                <!--tactics: analogy-->"
            .to_string();
    }
    if text.contains("JSON array of strings") {
        r#"["Introduction to the topic", "Core concept", "Practical application"]"#.to_string()
    } else if text.contains("exercise_html") {
        r#"{"exercise_html":"<form><p>Explain the concept in your own words and apply it to a new case:</p><textarea name=\"answer\" rows=\"4\" required></textarea><p><button type=\"submit\">Submit answer</button></p></form>","objectives":[{"id":"o1","kind":"application","description":"Apply the concept to a new case","criteria":"The answer transfers the concept to a scenario not covered in the text","transfer":true}]}"#.to_string()
    } else if text.contains("locked rubric") {
        // Demo: a blank/empty answer fails (so the "fail on purpose" flow reaches
        // remediation §8.2 keyless); any real content is graded as demonstrated so
        // the loop still advances end to end.
        let blank = text.contains("\"answer\":\"\"") || text.contains("Student's answer: {}");
        if blank {
            r#"{"grades":[{"objective_id":"o1","grade":"not_demonstrated","feedback":"No answer given — nothing to assess."}]}"#.to_string()
        } else {
            r#"{"grades":[{"objective_id":"o1","grade":"demonstrated","feedback":"Good transfer of the concept to a new case."}]}"#.to_string()
        }
    } else if text.contains("Remediation session") {
        // Explanation only — a worked solution of the missed problem. The NEW
        // practice problem is generated separately (matches the "exercise_html"
        // branch above) and rendered in its own sandbox.
        "<p><strong>Worked solution.</strong> Let's redo the problem you missed, step by step: first identify what's given, then apply the core idea, then check the result. The slip was in the middle step — the core idea applies before the final comparison, not after.</p>".to_string()
    } else {
        // Prose (default).
        "<h2>Core concept</h2><p>This is an explanatory paragraph generated in <strong>demo mode</strong> (no AI key). Configure a provider for real content, grounded in sources.</p><p>Each node is atomic and ends in a comprehension check.</p>".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    /// Guards the fix noted at the top of `demo_responder`: without the
    /// movement-specific branches ordered first, a `test` move's prompt
    /// (which embeds `EXERCISE_HTML_CONTRACT`, containing the literal
    /// substring "exercise_html") falls into the legacy `exercise_html`
    /// branch and returns the WRONG JSON shape, so every L1/L2 demo call
    /// would fail and burn its repair attempt.
    #[tokio::test]
    async fn demo_mode_answers_the_move_abi_contract_l1_and_l0() {
        let ai = demo_ai();
        let ctx = MoveContext {
            topic: "fractions".into(),
            item_title: "Equivalent fractions".into(),
            ..Default::default()
        };

        // L1: decide_move is an AI call against the demo responder.
        let decided = movement::decide_move(&ai, AgentPolicy::L1, &ctx)
            .await
            .unwrap();
        assert_eq!(decided, MoveType::Explain);

        // Explain is a streamed move — pump the token stream, then finish it.
        let stream = movement::generate_move_stream(&ai, decided, &ctx)
            .await
            .unwrap();
        let accumulated = stream
            .map(|r| r.unwrap())
            .collect::<Vec<_>>()
            .await
            .concat();
        let explained = movement::finish_streamed_move(decided, &accumulated);
        assert!(!explained.graded);
        assert!(!explained.html.is_empty());

        // L0: a `test` move is structured — must come back graded with a
        // non-empty rubric, not the legacy exercise shape.
        let tested = movement::generate_move(&ai, AgentPolicy::L0, MoveType::Test, &ctx)
            .await
            .unwrap();
        assert!(tested.graded);
        assert!(tested.rubric.unwrap().objectives.iter().any(|o| o.transfer));
    }

    /// §9 "mover o degrau por documento" — exercises the real production
    /// wiring (`rung_for` → `state.store.event_log` → `EventLog::iter` →
    /// `calibrate_rung`) against a real on-disk `Store`/`events.jsonl`, not
    /// just `calibrate_rung`'s own pure-logic unit tests in `events.rs`.
    /// Guards the `Send` fix: this exact call shape (constructing, fully
    /// draining, and dropping the log's iterator inside one synchronous
    /// function) is what keeps `generate_node`'s SSE stream `Send`; a
    /// regression that reintroduces the boxed iterator as a local held
    /// across an `.await` would fail to compile, not just fail this test.
    #[test]
    fn rung_for_demotes_from_document_telemetry_but_floors_at_l0() {
        use crate::config::AppConfig;
        use crate::secret::SecretStore;
        use crate::source::{Corpus, Source};
        use arc_swap::ArcSwap;
        use std::collections::HashSet;
        use std::sync::Arc;
        use tokio::sync::RwLock;

        let dir = std::env::temp_dir().join(format!("learnive-test-{}", engine::new_id()));
        let state = AppState {
            token: Arc::from("t"),
            allowed_origins: Arc::new(HashSet::new()),
            allowed_hosts: Arc::new(HashSet::new()),
            store: crate::store::Store::open(&dir).unwrap(),
            ai: Arc::new(ArcSwap::from_pointee(demo_ai())),
            policy: Arc::new(ArcSwap::from_pointee(AgentPolicy::L0)),
            config: Arc::new(RwLock::new(AppConfig::default())),
            secret: Arc::new(SecretStore::open(&dir)),
            data_dir: Arc::from(dir.to_string_lossy().as_ref()),
            source: Arc::new(Source::Mock(crate::source::MockSource::new())),
            corpus: Corpus::open(&dir).unwrap(),
            retriever: None,
        };
        // No history yet: the prior passes through unchanged.
        assert_eq!(rung_for(&state, "d1", AgentPolicy::L2), AgentPolicy::L2);

        let log = state.store.event_log("d1").unwrap();
        for (move_type, violated) in [
            ("explicar", false),
            ("explicar", true),
            ("explicar", false),
            ("explicar", true),
            ("explicar", false),
        ] {
            log.append(
                None,
                crate::events::EventKind::MoveGenerated {
                    move_id: engine::new_id(),
                    move_type: move_type.to_string(),
                    tactics: vec![],
                    rung: "L2".to_string(),
                },
            )
            .unwrap();
            if violated {
                log.append(
                    None,
                    crate::events::EventKind::SchemaViolation {
                        move_type: move_type.to_string(),
                        detail: "required a repair round".to_string(),
                    },
                )
                .unwrap();
            }
        }

        // 2/5 violations (40%) is past the 1-in-3 threshold: steps down once.
        assert_eq!(rung_for(&state, "d1", AgentPolicy::L2), AgentPolicy::L1);
        // The prior itself is untouched — a different document would still
        // start from L2, exactly why this is per-document and not global.
        assert_eq!(*state.policy.load_full(), AgentPolicy::L0);

        // L0 has nowhere lower to go, regardless of how bad the telemetry is.
        assert_eq!(rung_for(&state, "d1", AgentPolicy::L0), AgentPolicy::L0);

        std::fs::remove_dir_all(&dir).ok();
    }
}
