use super::cold_start::{acquire, outline_view};
use super::grading::sse_frame;
use super::reading::due_review_view;
use super::reading::{finalize, grounding_for, prepare, tail_chars, topic_and_title};
use super::*;

// ---------------------------------------------------------------------------
// Node generation (§6): the deterministic template → generate → stream, move by move (§14).
//
// §S18: the loop closes by MOVE, not by node. One `/generate` request
// produces at most one real (non-`research`) move, then ends the stream —
// see `generate_node`'s doc comment for the event that signals this
// (`move_paused`) vs. node-complete (`done`). The client reopens the request
// once the learner has read that move (today: crossing the read-to-end
// sentinel, `node.js`'s `armReadToEndWatcher`). State that used to live in
// the loop's local `ctx` across iterations — `prior_moves`, `node_tail`, the
// move-loop index — is now reconstructed on every call by `prepare` from the
// event log + persisted `content.html` (the same resume machinery §14 built
// for crash recovery; see `resumed_move_index`'s doc comment), not carried
// forward in memory. `research` is the one exception: it produces nothing
// the learner reads, so it still loops internally within the same request
// (capped at one attempt via `ctx.research_attempted`) before the request's
// one real move happens.
// ---------------------------------------------------------------------------

/// Hard cap on moves generated for one node, enforced ACROSS requests
/// (§S18) rather than within a single one (§12.2 cost control). The
/// deterministic template closes a node in 2-3 moves, so it never comes
/// near this; the slots exist for the research interception (which
/// consumes one when a node starts ungrounded) and as a last-resort cost
/// guard: once `prepare`'s reconstructed index reaches the last allowed
/// slot, that request forces `Test` so the node still closes in a graded
/// check (every node ends in one, §6).
const MAX_MOVES_PER_NODE: usize = 4;

/// Verbatim §14 context budget for the move loop's own node tail (~1.5k chars).
pub(super) const NODE_TAIL_BUDGET: usize = 1500;

/// Streams the SSE format over a POST — one real move per request as of
/// §S18 (module docs above). Events: `token` (prose — both streamed moves
/// and, as one full frame, ungraded structured moves, which share the same
/// prose contract §4.4/`movement.rs`), `move_settled` (§S6 follow-up — fires
/// once, right after `token`, carrying this move's HTML re-tagged with real,
/// permanent `data-block-id`s and progressively persisted; the client swaps
/// it in for the untagged version it just streamed and can enable reading/
/// asking against it immediately), `move_paused` (§S18 — node_id; this
/// move settled but the node isn't done: no graded check yet, and this
/// request is ending here on purpose so the learner can read/interact before
/// the client reopens `/generate` for the next move), `exercise` (the graded
/// move's form, sandboxed), `done` (node_id — the node's graded move landed
/// and `finalize` wrote its complete content layer), `error`.
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
pub async fn generate_node(
    State(state): State<AppState>,
    Path((doc_id, item_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let locale = crate::locale::Locale::from_header(&headers);
    // The fallible work that emits no tokens lives in `prepare`/`finalize`; the
    // generator only holds the `yield`s (async_stream does not rewrite `yield`
    // through a nested macro). `prepare`'s phases (library gate, chapter
    // split, passage selection) report through `status` — forwarded to the
    // client as `status` SSE frames via this channel while `prepare` itself
    // runs on a task, because a `yield` cannot happen inside its callback.
    // UX feedback (live 2026-09-04): a cold first generate used to show a
    // bare "generating…" for a minute or more while nothing content-shaped
    // was running yet.
    let stream = async_stream::stream! {
        let (status_tx, mut status_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let prepare_state = state.clone();
        let prepare_doc_id = doc_id.clone();
        let prepare_item_id = item_id.clone();
        let mut prepare_task = tokio::spawn(async move {
            prepare(
                &prepare_state,
                &prepare_doc_id,
                &prepare_item_id,
                |msg| {
                    let _ = status_tx.send(msg.to_string());
                },
                locale,
            )
            .await
        });
        let prep = loop {
            tokio::select! {
                Some(msg) = status_rx.recv() => {
                    yield Ok::<Bytes, std::io::Error>(sse_frame("status", &msg));
                }
                joined = &mut prepare_task => {
                    break match joined {
                        Ok(Ok(p)) => p,
                        Ok(Err(e)) => {
                            yield Ok(sse_frame("error", &e));
                            return;
                        }
                        Err(e) => {
                            yield Ok(sse_frame("error", &e.to_string()));
                            return;
                        }
                    };
                }
            }
        };

        let ai = state.ai.load_full();
        let event_log = match state.store.event_log(&doc_id) {
            Ok(l) => l,
            Err(e) => {
                yield Ok(sse_frame("error", &e.to_string()));
                return;
            }
        };
        // §14 resilience: a prior, interrupted attempt at this same node may
        // have already completed and persisted some ungraded moves
        // (`prepare`'s `resumed_moves`/`resumed_content_html`) — seed the
        // move loop's state from them so it picks up after the last
        // successful move instead of regenerating (and re-paying for) it.
        let mut content_html = prep.resumed_content_html.clone();
        let resumed_tail = if content_html.is_empty() {
            String::new()
        } else {
            tail_chars(&content_html, NODE_TAIL_BUDGET)
        };
        let mut ctx = MoveContext {
            topic: prep.topic.clone(),
            item_title: prep.title.clone(),
            outline_context: prep.context.clone(),
            grounding: prep.grounding.clone(),
            objective: prep.objective.clone(),
            children_titles: prep.children_titles.clone(),
            review_mode: prep.review_mode,
            parent_title: prep.parent_title.clone(),
            later_titles: prep.later_titles.clone(),
            prior_moves: prep.resumed_moves.clone(),
            node_tail: resumed_tail,
            locale,
            research_attempted: prep.research_attempted,
            scaffolding: prep.scaffolding,
            interleave_titles: prep.interleave_titles.clone(),
            chapter_close: prep.chapter_close,
            ..Default::default()
        };
        let mut graded: Option<(String, GeneratedMove)> = None;

        // Clamped so a resumed attempt always retries at least the final,
        // forced-`Test` iteration rather than erroring out with no graded
        // check at all — reachable only if every one of `MAX_MOVES_PER_NODE`
        // moves, including that forced `Test`, was already logged by a prior
        // attempt whose graded move then failed to `finalize` (a narrower,
        // separate failure mode from the reported one, where `Test`'s own
        // generation call times out and is never logged at all). `Test`'s
        // content is never in `resumed_content_html` regardless — a graded
        // move's success breaks the loop before the progressive-persistence
        // block runs — so reusing that index here can't collide.
        let start = prep.resumed_move_index.min(MAX_MOVES_PER_NODE - 1);
        for i in start..MAX_MOVES_PER_NODE {
            // S33: move choice is the deterministic template
            // (`movement::next_move`) — the model never picks. One
            // structural interception remains: §S13/S30's research move,
            // Rust-forced now instead of menu-offered — a node that starts
            // with no grounding and no acquisition attempt yet acquires
            // first (one attempt per node, `ctx.research_attempted`), then
            // the template resumes. Like any other move it consumes one of
            // `MAX_MOVES_PER_NODE`'s slots; the forced-`Test` last slot
            // always outranks it, so a node can never spend its whole
            // budget without reaching a graded check.
            let move_type = if i == MAX_MOVES_PER_NODE - 1 {
                // Cost guard exhausted without a graded check — force one so
                // the node still closes (every node ends in a check, §6).
                MoveType::Test
            } else if ctx.grounding.trim().is_empty() && !ctx.research_attempted {
                MoveType::Research
            } else {
                match movement::next_move(&ctx) {
                    Ok(mt) => mt,
                    Err(e) => {
                        yield Ok(sse_frame("error", &e.to_string()));
                        return;
                    }
                }
            };

            if move_type == MoveType::Research {
                // §S13: acquires grounding for this concept, then loops back
                // to the template — never reaches `render()` (see
                // `MoveType::Research`'s doc comment). Capped at one attempt
                // per node across requests via `ctx.research_attempted`
                // (reconstructed from the log in `prepare`), so a source
                // that genuinely can't be found costs exactly one slot.
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
                        rung: "deterministic".to_string(),
                    },
                ) {
                    eprintln!("event log append failed: {e}");
                }
                ctx.research_attempted = true;
                if outcome.grounded {
                    ctx.grounding = grounding_for(&state, &ctx.item_title).await;
                }
                continue;
            }

            let generated = match move_type.render() {
                MoveRender::Streamed => {
                    let mut tokens = match movement::generate_move_stream(&ai, move_type, &ctx)
                        .await
                    {
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
                    match movement::generate_move(&ai, move_type, &ctx).await {
                        Ok(mv) => mv,
                        Err(e) => {
                            yield Ok(sse_frame("error", &e.to_string()));
                            return;
                        }
                    }
                }
            };

            // §S21: post-generation grounding-verification gate, run for
            // BOTH render paths above — right after the move's content is
            // fully settled and before it's used any further (event log, §S6
            // progressive persistence below). `applies` is the same
            // cheap grounded/in-scope check `verify` makes internally;
            // checked again here only to gate the status frame, so a plain
            // ungrounded/out-of-scope move never emits one. `verify` itself
            // never errors this request — a verification failure degrades
            // to the visible "grounding unconfirmed" banner (never a silent
            // drop, same principle as S27's existence verification) rather
            // than propagating. One check call only (2026-09-01, no more
            // corrective regeneration — see `movement::grounding`'s module
            // doc).
            let generated = if movement::grounding::applies(move_type, &ctx.grounding) {
                let checking_en = "Checking grounding…";
                let checking_pt = "Verificando fundamentação…";
                yield Ok(sse_frame(
                    "grounding_check",
                    crate::locale::pick(locale, checking_en, checking_pt),
                ));
                movement::grounding::verify(&ai, move_type, &ctx, generated).await
            } else {
                generated
            };

            if generated.repaired {
                // Kept as a zero-cost diagnostic after S33: the first
                // response violated the Move JSON contract and needed a
                // repair round.
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
                    rung: "deterministic".to_string(),
                },
            ) {
                eprintln!("event log append failed: {e}");
            }

            if generated.graded {
                graded = Some((move_id, generated));
                break;
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
            // either (`engine::wrap_move`'s doc comment).
            let tagged = engine::tag_move_html(&prep.node_id, i, &generated.html);
            content_html.push_str(&tagged);
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
            // §S18: the loop now closes by MOVE, not by node — one real
            // (non-`research`) move settles per request, then the request
            // ends here. The learner reads it (crossing the read-to-end
            // sentinel reopens `/generate` for the next template move);
            // `prepare`'s resume machinery reconstructs `prior_moves`/
            // `node_tail`/the loop index from the event log + persisted
            // content on that next call for this same node.
            yield Ok(sse_frame("move_paused", &prep.node_id));
            return;
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
    let due_review = due_review_view(&state, &doc_id, &outline)?;
    Ok(Json(OutlineResp { items, due_review }))
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
    let items = outline_view(&state, &doc_id, &outline)?;
    let due_review = due_review_view(&state, &doc_id, &outline)?;
    Ok(Json(OutlineResp { items, due_review }))
}

/// Terminal remediation for a chapter `source::match_chapter` could not
/// place against the book's confirmed table of contents (S27g's matching
/// pass ran — `ChaptersProposed` → `Expanded` — and still left
/// `resolved_page: None`): the user picks the page directly, skips the
/// whole book, or restarts cold start. This handler is the "skip the whole
/// book" arm.
///
/// Deliberately its own endpoint rather than looping [`skip_node`] over the
/// book's children client-side: a `Chapter` chains on the one before it
/// (S27g), so every chapter past the first is `"locked"` before the reader
/// reaches it, and `skip_node` refuses a locked node on principle ("skipping
/// something you were never able to reach carries no real signal" — true
/// for an ordinary skip, false here, where the user is abandoning the whole
/// book on purpose). This appends `NodeSkipped` unconditionally for every
/// direct `Chapter` child, no locked check.
///
/// Nothing is appended for the book item itself: [`engine::effective_state`]
/// already synthesizes `Demonstrated` for a container once every direct
/// child is `Demonstrated`/`Skipped` — the exact "fully skipped book" case
/// its own doc comment names — so skipping every child is already sufficient
/// for the reading list to unlock past it.
pub async fn skip_book(
    State(state): State<AppState>,
    Path((doc_id, item_id)): Path<(String, String)>,
) -> Result<Json<OutlineResp>, ApiError> {
    let outline_json = state.store.read_doc_file(&doc_id, "outline.json")?;
    let outline: engine::Outline =
        serde_json::from_str(&outline_json).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let book = outline
        .items
        .iter()
        .find(|i| i.id == item_id)
        .ok_or_else(|| ApiError::BadRequest("unknown outline item".to_string()))?;
    if !matches!(
        book.item_type,
        engine::OutlineItemType::Book | engine::OutlineItemType::Article
    ) {
        return Err(ApiError::BadRequest(
            "only a book or article can be skipped this way".to_string(),
        ));
    }
    let children: Vec<String> = outline
        .items
        .iter()
        .filter(|i| i.parent_id.as_deref() == Some(item_id.as_str()))
        .map(|i| i.id.clone())
        .collect();
    if children.is_empty() {
        return Err(ApiError::BadRequest(
            "this book has no chapters to skip yet".to_string(),
        ));
    }

    let event_log = state.store.event_log(&doc_id)?;
    for child_id in &children {
        if let Err(e) = event_log.append(Some(child_id), EventKind::NodeSkipped) {
            eprintln!("event log append failed: {e}");
        }
    }
    let items = outline_view(&state, &doc_id, &outline)?;
    let due_review = due_review_view(&state, &doc_id, &outline)?;
    Ok(Json(OutlineResp { items, due_review }))
}

#[derive(Deserialize)]
pub struct ResolvedPageReq {
    pub page: usize,
}

/// Terminal remediation, "pick the page yourself" arm — see [`skip_book`]'s
/// doc comment for the other two arms of the same decision. Sets a
/// `Chapter` item's `resolved_page` directly, the same field
/// `source::match_chapter` would have written on a successful automatic
/// match; nothing downstream needs to know this one came from the user
/// instead (citation deep-links, once S27j exists, read this field either
/// way).
pub async fn set_resolved_page(
    State(state): State<AppState>,
    Path((doc_id, item_id)): Path<(String, String)>,
    Json(req): Json<ResolvedPageReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .store
        .update_outline_file(&doc_id, |json| {
            let mut outline: engine::Outline =
                serde_json::from_str(json).map_err(|e| e.to_string())?;
            let item = outline
                .items
                .iter_mut()
                .find(|i| i.id == item_id)
                .ok_or_else(|| "unknown outline item".to_string())?;
            if item.item_type != engine::OutlineItemType::Chapter {
                return Err("only a chapter's page can be set this way".to_string());
            }
            item.resolved_page = Some(req.page);
            serde_json::to_string(&outline).map_err(|e| e.to_string())
        })
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok(Json(serde_json::json!({ "ok": true })))
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
    /// §S15b step 4: the ORIGIN document's display name, present only when
    /// it differs from the document currently being read — a node's own
    /// document reading its own interactions never sets this, only a
    /// reference does. The client renders it as a discreet marker; `None`
    /// means "render nothing", not "unknown".
    #[serde(skip_serializing_if = "Option::is_none")]
    asked_in: Option<String>,
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
    let owner_id = super::reading::owner_of_node(&state, &doc_id, &node_id);
    let node = state.store.read_node(&owner_id, &node_id)?;
    // §S15b step 3: events always land in the owner's log, so `demonstrated`
    // must read from there too — reading `doc_id`'s own log would find no
    // `MoveGraded` for a reference and report a demonstrated node as live.
    let event_log = state.store.event_log(&owner_id)?;
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

    // §S15b step 4: resolves an interaction's `asked_in` doc id into a
    // display name, but only when it differs from THIS document — a plain
    // local node (the overwhelming common case) never carries the marker.
    let asked_in_marker = |origin: &Option<String>| -> Option<String> {
        let origin = origin.as_ref()?;
        if origin == &doc_id {
            return None;
        }
        Some(super::cold_start::document_name(&state, origin, ""))
    };

    let interactions = node
        .interaction
        .iter()
        .map(|item| match item {
            InteractionItem::Annotation {
                id,
                anchor,
                body_html,
                asked_in,
            } => InteractionView {
                id: id.clone(),
                kind: "annotation",
                body_html: body_html.clone(),
                child_node_id: None,
                anchor_block: Some(anchor.block_id.clone()),
                asked_in: asked_in_marker(asked_in),
            },
            InteractionItem::Thread {
                id,
                kind: ThreadKind::Qa,
                body_html,
                anchor_block,
                child_node_id,
                asked_in,
            } => InteractionView {
                id: id.clone(),
                kind: "qa",
                body_html: body_html.clone(),
                child_node_id: child_node_id.clone(),
                anchor_block: anchor_block.clone(),
                asked_in: asked_in_marker(asked_in),
            },
            InteractionItem::Thread {
                id,
                kind: ThreadKind::Remediation,
                body_html,
                asked_in,
                ..
            } => InteractionView {
                id: id.clone(),
                kind: "remediation",
                body_html: body_html.clone(),
                child_node_id: None,
                anchor_block: None,
                asked_in: asked_in_marker(asked_in),
            },
            InteractionItem::Thread {
                id,
                kind: ThreadKind::Attempt,
                body_html,
                asked_in,
                ..
            } => InteractionView {
                id: id.clone(),
                kind: "attempt",
                body_html: body_html.clone(),
                child_node_id: None,
                anchor_block: None,
                asked_in: asked_in_marker(asked_in),
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
    let owner_id = super::reading::owner_of_node(&state, &doc_id, &node_id);
    let sidecar_json = state
        .store
        .read_doc_file(&owner_id, &format!("{node_id}.rubric.json"))?;
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
    let owner_id = super::reading::owner_of_node(&state, &doc_id, &node_id);
    let node = state.store.read_node(&owner_id, &node_id)?;
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
