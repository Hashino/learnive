use super::cold_start::{acquire, outline_view, suggested_revisit};
use super::grading::sse_frame;
use super::reading::{
    finalize, grounding_for, prepare, rung_for, spawn_profile_distillation, tail_chars,
    topic_and_title,
};
use super::*;

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
pub(super) const NODE_TAIL_BUDGET: usize = 1500;

/// Streams the SSE format over a POST. Events: `token` (prose — both streamed
/// moves and, as one full frame, ungraded structured moves, which share the
/// same prose contract §4.4/`movement.rs`), `move_settled` (§S6 follow-up —
/// fires once per ungraded move, right after `token`, carrying that move's
/// HTML re-tagged with real, permanent `data-block-id`s and progressively
/// persisted; the client swaps it in for the untagged version it just
/// streamed and can enable reading/asking against it immediately, without
/// waiting for the whole node), `exercise` (the graded move's form,
/// sandboxed), `done` (node_id), `error`.
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
    headers: HeaderMap,
) -> Response {
    let locale = crate::locale::Locale::from_header(&headers);
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
            children_titles: prep.children_titles.clone(),
            review_mode: prep.review_mode,
            parent_title: prep.parent_title.clone(),
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

            if move_type == MoveType::Research {
                // §S13: acquires grounding for this concept, then loops back
                // to decide the REAL next move — never reaches `render()`
                // (see `MoveType::Research`'s doc comment). Capped at one
                // attempt per node-generation call via `ctx.research_attempted`
                // (withheld from the menu once true, `movement::prompt::menu`),
                // so a source that genuinely can't be found costs exactly one
                // of `MAX_MOVES_PER_NODE`'s slots, never a loop.
                let looking_en = format!("Looking for sources on {}…", ctx.item_title);
                let looking_pt = format!("Procurando fontes sobre {}…", ctx.item_title);
                yield Ok(sse_frame(
                    "research",
                    crate::locale::pick(locale, &looking_en, &looking_pt),
                ));
                let outcome =
                    acquire(&state, &format!("{} {}", ctx.topic, ctx.item_title)).await;
                let status = match &outcome.source_title {
                    Some(title) => {
                        let en = format!("Found a source: {title}");
                        let pt = format!("Fonte encontrada: {title}");
                        crate::locale::pick(locale, &en, &pt).to_string()
                    }
                    None => crate::locale::pick(
                        locale,
                        "No adequate source found — continuing ungrounded",
                        "Nenhuma fonte adequada encontrada — continuando sem fonte",
                    )
                    .to_string(),
                };
                yield Ok(sse_frame("research", &status));
                if let Err(e) = event_log.append(
                    Some(&prep.node_id),
                    EventKind::MoveGenerated {
                        move_id: engine::new_id(),
                        move_type: move_type.to_string(),
                        tactics: Vec::new(),
                        rung: format!("{policy:?}"),
                    },
                ) {
                    eprintln!("event log append failed: {e}");
                }
                ctx.research_attempted = true;
                if outcome.grounded {
                    ctx.grounding =
                        grounding_for(&state, &format!("{} {}", ctx.topic, ctx.item_title)).await;
                }
                continue;
            }

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
                // persist it and end this generation request without
                // finalizing a node — approval is a separate user action
                // (`/plan/decide`), never assumed. No `NodeGenerated` event
                // fires here (only `finalize` appends one), so `prepare`'s
                // regen guard does not treat this node as done. If an
                // earlier move in this same request already progressively
                // persisted (§S6 follow-up), that partial content-only node
                // is left on disk rather than deleted — inert, not a
                // rollback in the literal §5 sense, but self-healing: the
                // next real generation attempt for this node id (whether
                // this proposal is rejected, or approved and this item's id
                // is reused) overwrites it cleanly from move 1, and nothing
                // reads a non-finalized node as real content in the
                // meantime. `/plan/decide` appends the resolution event.
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
            // §S6 follow-up: tag and persist this move now rather than
            // waiting for the whole node (through the graded move) to
            // finish. `tag_move_html` is the ONLY place this move's HTML is
            // ever run through `render_math`/`ensure_block_ids` — `finalize`
            // later only concatenates already-tagged moves, never re-runs
            // either (`engine::wrap_article`'s doc comment). The client
            // gets the tagged fragment directly in the event payload (no
            // refetch needed) so it can splice it in and enable reading/
            // asking against it immediately.
            let tagged = engine::tag_move_html(&prep.node_id, i, &generated.html);
            content_html.push_str(&tagged);
            content_html.push('\n');
            match engine::assemble_partial_node(&doc_id, &prep.node_id, &content_html) {
                Ok(partial) => {
                    if let Err(e) = state.store.write_node_content(&partial) {
                        eprintln!("progressive node write failed: {e}");
                    }
                }
                Err(e) => eprintln!("progressive node assembly failed: {e}"),
            }
            // §4.4/§3.1: `tagged` is the frozen copy IslandGate kept for
            // storage, raw island script and all (never sent live in
            // `token` frames — the gate already redacted those). This SSE
            // frame is a second place that same raw script could otherwise
            // leak to the app origin, so redact it here too; the unredacted
            // `tagged` still went into `content_html`/the progressive write
            // above, since the stored content layer needs the real script
            // for `blocks/{id}/frame` to serve later.
            yield Ok(sse_frame(
                "move_settled",
                &learnive_core::redact_interactive_blocks(&tagged),
            ));
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
            // (§S4/§S5) — sub-nodes spawned from a question (§S8) and
            // prerequisite-tree items (§S15) are never among them, so
            // rebuilding `outline.items` from `proposed` wholesale would
            // silently drop them. Preserve them verbatim.
            let sub_nodes: Vec<OutlineItem> = outline
                .items
                .iter()
                .filter(|i| i.parent_id.is_some())
                .cloned()
                .collect();
            // §S15: the old main-line item 0 carries the prerequisite tree's
            // root ids as its own `prerequisites` (nothing else can be there
            // — idx 0 has no chain predecessor). Carried forward onto the new
            // item 0 so a `plan` reorder doesn't silently unlock content that
            // was gated behind an unfinished prerequisite. If item 0's title
            // changed, the roots' own `parent_id` still points at the old id
            // — the same accepted degradation a title-only rename already
            // causes for chain prerequisites (see this fn's doc comment).
            let carried_prereqs = outline
                .items
                .iter()
                .find(|i| i.parent_id.is_none())
                .map(|i| i.prerequisites.clone())
                .unwrap_or_default();
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
                        carried_prereqs.clone()
                    } else {
                        vec![ids[idx - 1].clone()]
                    },
                    parent_id: None,
                    mode: NodeMode::Learn,
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
        transcript_html: None,
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
    let transcript_html = cold_start_transcript(&state, &doc_id);
    Ok(Json(OutlineResp {
        items,
        suggested_revisit,
        transcript_html,
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
        transcript_html: None,
    }))
}

/// One interaction-layer item, as shown to the client (§4.3) — the same
/// `body_html` the append-only layer already stores, just retagged for
/// display; nothing here is regraded or re-served as gradeable.
#[derive(Serialize)]
struct InteractionView {
    /// The item's stable id (§4.3). Sent because the client tags the
    /// answer it splices into the document with it: that id is what a
    /// follow-up question anchors to when the learner asks about the
    /// answer itself, and it has to survive a reload unchanged.
    id: String,
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
    /// The exercise's own §4.3 block id, whenever the node has an exercise
    /// at all — present before AND after `demonstrated`, so an old Q&A
    /// thread anchored to it (§S6) can always re-resolve on reload. It is
    /// NOT "is the exercise still live": the client derives that from
    /// `demonstrated` and only renders the sandboxed form (fetched from
    /// `GET .../exercise-frame`) or grants it the reading line (§9) while
    /// unsolved. The exercise HTML itself is never sent here.
    #[serde(skip_serializing_if = "Option::is_none")]
    exercise_block_id: Option<String>,
    interactions: Vec<InteractionView>,
    demonstrated: bool,
    /// This node's outline title (§S8): a sub-node isn't shown in the
    /// sidebar, so the client has no other way to label it once spliced
    /// inline. Degrades to "" if the outline no longer carries this item,
    /// same convention as `topic_and_title`.
    title: String,
    /// Whether generation actually reached `finalize` (a `NodeGenerated`
    /// event exists) rather than stopping mid-loop after an error the
    /// `generate_node` SSE stream could only report to the tab that was
    /// open at the time (2026-08-17 live report: a node can end up on disk
    /// with only its first move's content, no rubric, and no way for a
    /// later page load to tell that apart from a real finished node). Same
    /// `node_generated` check `prepare`'s regen guard already uses — when
    /// this is `false`, `prepare` is guaranteed to accept a retry for this
    /// node (its prerequisites don't change by generation stalling), so the
    /// client can safely offer one instead of rendering `content_html` as
    /// if it were finished.
    complete: bool,
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
    // The exercise's own `data-block-id` (§4.3, `ensure_form_ids` reuses its
    // exercise_id). NOT gated on `demonstrated` (unlike `exercise_html`): a
    // Q&A thread asked about the exercise while it was still live (§S6)
    // stays anchored to this id forever, and re-splicing it on reload needs
    // the id to keep existing even once the form itself is gone — losing it
    // used to strand that thread in the bottom interaction panel with the
    // wrong styling. The client tells "live exercise" from "solved, id kept
    // only for old anchors" via `demonstrated`, not via this field's
    // presence.
    let exercise_block_id = node
        .content
        .exercise
        .as_ref()
        .map(|e| e.exercise_id.clone());

    let interactions = node
        .interaction
        .iter()
        .map(|item| match item {
            InteractionItem::Annotation {
                id,
                anchor,
                body_html,
            } => InteractionView {
                id: id.clone(),
                kind: "annotation",
                body_html: body_html.clone(),
                child_node_id: None,
                anchor_block: Some(anchor.block_id.clone()),
            },
            InteractionItem::Thread {
                id,
                kind: ThreadKind::Qa,
                body_html,
                anchor_block,
                child_node_id,
            } => InteractionView {
                id: id.clone(),
                kind: "qa",
                body_html: body_html.clone(),
                child_node_id: child_node_id.clone(),
                anchor_block: anchor_block.clone(),
            },
            InteractionItem::Thread {
                id,
                kind: ThreadKind::Remediation,
                body_html,
                ..
            } => InteractionView {
                id: id.clone(),
                kind: "remediation",
                body_html: body_html.clone(),
                child_node_id: None,
                anchor_block: None,
            },
            InteractionItem::Thread {
                id,
                kind: ThreadKind::Attempt,
                body_html,
                ..
            } => InteractionView {
                id: id.clone(),
                kind: "attempt",
                body_html: body_html.clone(),
                child_node_id: None,
                anchor_block: None,
            },
        })
        .collect();

    let title = topic_and_title(&state, &doc_id, &node_id)
        .map(|(_, title)| title)
        .unwrap_or_default();
    let complete = node_generated(
        event_log
            .iter()
            .map_err(|e| ApiError::Internal(e.to_string()))?,
        &node_id,
    );

    Ok(Json(NodeView {
        content_html,
        exercise_block_id,
        interactions,
        demonstrated,
        title,
        complete,
    }))
}

#[derive(Deserialize)]
pub struct FrameQuery {
    theme: Option<String>,
    lang: Option<String>,
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
    let locale = crate::locale::Locale::from_str_opt(query.lang.as_deref());
    let page = engine::render_sandbox_frame(&sidecar.exercise_html, theme, true, locale);
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
    let locale = crate::locale::Locale::from_str_opt(query.lang.as_deref());
    let page = engine::render_sandbox_frame(&block_html, theme, false, locale);
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
