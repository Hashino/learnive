use super::*;

use super::generation::NODE_TAIL_BUDGET;
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
/// `sanitizeHtml` in `assets/core.js`), but nothing here should ever depend on
/// that: user text is escaped before it is stored, not just before it is shown.
pub(super) fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Read-only source viewer (§11): serves the corpus's already-normalized text
/// for a citation's `data-source-id`, so `<cite>` has somewhere real to point
/// — the minimal version of the eventual PDF split-view (§11.1), which needs
/// the original file and page-level locations neither acquisition backend nor
/// the corpus currently stores. GET, mutates nothing: the source viewer is
/// read-only by design (§9/§11) — any note the learner wants to make lands in
/// the living document, never on the source itself. Not document-scoped: the
/// corpus (`state.corpus`) is one shared, global store (§4/§11), so this sits
/// beside `/api/documents/...` rather than under it.
pub async fn get_source(
    State(state): State<AppState>,
    Path(source_id): Path<String>,
) -> Result<Json<crate::source::FetchedSource>, ApiError> {
    state
        .corpus
        .load(&source_id)
        .map(Json)
        .map_err(|e| ApiError::NotFound(e.to_string()))
}

/// Resolves a client-supplied anchor against the node (§4.3) — rejecting one
/// that doesn't resolve keeps the interaction layer's "always references real
/// IDs" invariant true by construction, rather than trusting whatever block id
/// the client happens to send.
///
/// `Node::resolve` accepts a content block *or* an interaction item: a
/// follow-up question asked inside an answer anchors to that answer, which is
/// how the learner keeps going deeper without leaving the document.
fn resolve_anchor(node: &Node, anchor: &Anchor) -> Result<(), ApiError> {
    node.resolve(anchor).ok_or_else(|| {
        ApiError::BadRequest("anchor does not resolve against this node".to_string())
    })?;
    Ok(())
}

/// How much of a neighbouring block to carry into the reading context. Enough
/// for the tutor to see where the question sits without re-sending the node
/// (`node_tail` already covers that, §14).
const NEIGHBOUR_BUDGET: usize = 400;

/// Where the learner is reading, as a labelled block for the prompt (§9).
///
/// Not just the selected span: a question about a selection is almost never
/// answerable from the selection alone, so this carries the passage it sits
/// in *and* its neighbours. With no selection at all the anchor is whatever
/// block is nearest the viewport centre (the reading line) and the same
/// surrounding text is what gives the question its subject. `None` only when
/// the anchor names neither a content block nor an interaction item.
fn reading_context(node: &Node, anchor: &Anchor) -> Option<String> {
    let idx = match node
        .content
        .blocks
        .iter()
        .position(|b| b.id == anchor.block_id)
    {
        Some(idx) => idx,
        // Anchored inside an answer (the rabbit hole, §9): the passage the
        // question is about is that answer, and what came *before* it is the
        // passage the answer itself was answering — the thread the learner is
        // following, in the order they read it.
        None => return interaction_reading_context(node, anchor),
    };
    let blocks = &node.content.blocks;
    let mut out = String::new();
    if let Some(q) = &anchor.quote {
        out.push_str(&format!("Selected text: \"{}\"\n", q.exact.trim()));
    }
    if idx > 0 {
        out.push_str(&format!(
            "Preceding passage: {}\n",
            head_chars(&blocks[idx - 1].text, NEIGHBOUR_BUDGET)
        ));
    }
    out.push_str(&format!(
        "Passage the question is about: {}\n",
        blocks[idx].text.trim()
    ));
    if let Some(next) = blocks.get(idx + 1) {
        out.push_str(&format!(
            "Following passage: {}",
            head_chars(&next.text, NEIGHBOUR_BUDGET)
        ));
    }
    Some(out)
}

/// `reading_context` for an anchor that lands in the interaction layer.
///
/// Walks one step back up the chain (answer → what it was anchored to) so the
/// tutor sees the question in the thread that produced it, not floating on its
/// own. Only one step: the whole node's tail is already in the prompt
/// (`node_tail`), and each extra hop costs tokens on the app's hottest path
/// (§14/§12.2).
fn interaction_reading_context(node: &Node, anchor: &Anchor) -> Option<String> {
    let target = node.resolve_interaction(&anchor.block_id)?;
    let mut out = String::new();
    if let Some(q) = &anchor.quote {
        out.push_str(&format!("Selected text: \"{}\"\n", q.exact.trim()));
    }
    if let Some(parent_id) = target.item.anchor_block() {
        let parent_text = node
            .content
            .blocks
            .iter()
            .find(|b| b.id == parent_id)
            .map(|b| b.text.clone())
            .or_else(|| node.interaction_text(parent_id));
        if let Some(parent_text) = parent_text {
            out.push_str(&format!(
                "Preceding passage (what this answer was answering): {}\n",
                head_chars(&parent_text, NEIGHBOUR_BUDGET)
            ));
        }
    }
    out.push_str(&format!(
        "Passage the question is about (an earlier answer in this document): {}",
        target.text.trim()
    ));
    // Anchored at one paragraph of a longer answer: the rest of that answer is
    // the passage's own context, and without it the tutor is reading a
    // sentence out of the middle of its own previous reply.
    if target.item.id() != anchor.block_id {
        out.push_str(&format!(
            "\nThe answer that paragraph belongs to: {}",
            head_chars(&node.interaction_text(target.item.id())?, NODE_TAIL_BUDGET)
        ));
    }
    Some(out)
}

/// The "You asked …" line that heads an answer in the document (§9).
///
/// When the question came from a selection, the selection is part of the
/// header: an answer read weeks later has to say what it was an answer *to*,
/// and by then the quote is the only thing that still identifies it — the
/// passage around it may have been asked about several times over. Long
/// selections are clipped; the header is a label, not a second copy of the
/// text.
pub(super) fn question_header(question: &str, anchor: &Anchor) -> String {
    let mut out = format!(
        "<p class=\"question\"><strong>You asked:</strong> {}",
        escape_html(question)
    );
    if let Some(quote) = &anchor.quote {
        let exact = quote.exact.trim();
        if !exact.is_empty() {
            out.push_str(&format!(
                " <span class=\"about\">about \u{201c}{}\u{201d}</span>",
                escape_html(&head_chars(exact, QUOTE_HEADER_BUDGET))
            ));
        }
    }
    out.push_str("</p>");
    out
}

/// How much of the selection the answer's header shows before clipping.
pub(super) const QUOTE_HEADER_BUDGET: usize = 120;

/// First `max_chars` characters of `s` (char-boundary safe) — the
/// neighbouring-block counterpart of `tail_chars`.
fn head_chars(s: &str, max_chars: usize) -> String {
    let s = s.trim();
    match s.char_indices().nth(max_chars) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// Topic + this node's title from the outline (§S4/§S5) — degrades to an
/// empty title if the outline no longer carries this item (e.g. renamed
/// away by a `plan` approval, §S5), the same graceful-empty-field contract
/// `MoveContext` already uses.
pub(super) fn topic_and_title(
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

/// Discriminated by `kind` (§S8). Both kinds land *in the document*, at
/// `anchor_block` — §9 "o documento é a resposta": `inline` is a short
/// explanatory insertion woven in right after the passage that was asked
/// about, `spawn` a brand-new sub-node spliced there instead. Neither is a
/// reply appended to a transcript at the end of the page.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AskResp {
    Inline {
        /// The interaction item's stable id (§4.3) — the client tags the
        /// spliced answer with it so a follow-up question can anchor to
        /// this answer, the same way it would anchor to a paragraph.
        id: String,
        body_html: String,
        anchor_block: String,
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
    let context = reading_context(&node, &body.anchor);
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

            // Each paragraph of the answer gets its own `data-block-id`, on
            // the same `{id}-b{n}` scheme the content layer uses: an answer is
            // several paragraphs and the learner asks about *one* of them, so
            // it has to be addressable at that grain (§4.3) rather than as one
            // undifferentiated blob.
            let answer_html = ensure_block_ids(&answer_html, &format!("{move_id}-b"));
            let body_html = format!(
                "{}\n<div class=\"answer\">{answer_html}</div>",
                question_header(question, &body.anchor)
            );
            state.store.append_interaction(
                &doc_id,
                &node_id,
                InteractionItem::Thread {
                    id: move_id.clone(),
                    kind: ThreadKind::Qa,
                    anchor_block: Some(body.anchor.block_id.clone()),
                    body_html: body_html.clone(),
                    child_node_id: None,
                },
            )?;

            Ok(Json(AskResp::Inline {
                id: move_id,
                body_html,
                anchor_block: body.anchor.block_id,
            }))
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
                "{}\n<p>↳ spawned a new section: {}</p>",
                question_header(question, &body.anchor),
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
pub(super) fn tail_chars(s: &str, max_chars: usize) -> String {
    match s.char_indices().rev().nth(max_chars.saturating_sub(1)) {
        Some((i, _)) => s[i..].to_string(),
        None => s.to_string(),
    }
}

/// Data ready to generate a node.
pub(super) struct NodePrep {
    pub(super) topic: String,
    pub(super) title: String,
    /// Fed to `MoveContext::outline_context` (§14 budget: `PRIOR_CONTENT_BUDGET`
    /// chars) — the titles of prior outline items PLUS an excerpt of what they
    /// actually said, not titles alone (see `prior_content_context`'s doc
    /// comment for why titles alone let sibling nodes repeat each other).
    pub(super) context: String,
    pub(super) node_id: String,
    /// Retrieved source passages formatted for the prompt (§10). Empty when the
    /// index has nothing relevant yet (acquisition may still be running, §14).
    pub(super) grounding: String,
    /// The document's current objective text (§S4) — empty for a pre-S4
    /// document with no `objective.json`, same graceful-degradation
    /// convention as `grounding`.
    pub(super) objective: String,
    /// Evidence-profile text for `MoveContext::profile` (§7/§S7) —
    /// `profile_for`'s always-fresh evidence table plus any distilled
    /// traits/hypotheses. Empty for a document with no evidence/distillation
    /// yet, same graceful-degradation convention as `grounding`/`objective`.
    pub(super) profile: String,
    /// Current outline item titles, for `generate_node` to detect whether a
    /// `plan` move's proposal is a real structural change (§5).
    pub(super) outline_titles: Vec<String>,
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
/// Also refuses regenerating a node that has already been **finalized**
/// (a `NodeGenerated` event exists for it — `events::aggregate::node_generated`),
/// regardless of gate state: making already-generated outline items
/// clickable, so the learner can revisit a skipped or demonstrated node,
/// opened a path where `finalize` would silently overwrite that file with
/// a freshly assembled node, discarding the content layer the learner has
/// already read and may have anchored questions/annotations against. §5:
/// "conhecimento nunca é editado destrutivamente". The client now always
/// tries `GET .../nodes/{id}` before ever calling generate on an outline
/// click; this check is the server-side backstop, not the primary defense.
///
/// A node file existing is no longer, by itself, reason to refuse: content
/// now persists progressively, one move at a time (§S6 follow-up — see
/// `EventKind::NodeGenerated`'s doc comment), so an on-disk file can be a
/// node that's still mid-generation, or one abandoned mid-stream by a
/// dropped connection or a page reload. Retrying such a node is allowed to
/// proceed and simply overwrite the partial file as it re-generates from
/// move 1 — no attempt at resuming from where the interrupted attempt left
/// off, which would need move-level state (tactics history, `ctx.prior_moves`)
/// this function never persisted. `write_node_content` (store.rs) is what
/// keeps this safe for the interaction layer specifically: any interaction
/// appended against the partial node (a mid-stream `/ask`, now possible —
/// the whole point) survives the retry's overwrite, carried forward rather
/// than clobbered.
pub(super) async fn prepare(
    state: &AppState,
    doc_id: &str,
    item_id: &str,
) -> Result<NodePrep, String> {
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
    if node_generated(event_log.iter().map_err(|e| e.to_string())?, &item.id) {
        return Err(
            "this node was already generated; fetch it instead of regenerating".to_string(),
        );
    }

    let context = prior_content_context(state, doc_id, &outline.items[..idx]);
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

/// §14 budget for `prior_content_context`'s excerpt of prior nodes' actual
/// content — separate from `NODE_TAIL_BUDGET` (the current node's own tail)
/// because this one spans potentially several prior nodes, not one.
const PRIOR_CONTENT_BUDGET: usize = 3000;

/// Titles of every prior outline item PLUS a trailing excerpt of what they
/// actually said (§14 budget: `PRIOR_CONTENT_BUDGET` chars, most recent
/// content wins under `tail_chars`). Titles alone used to be the whole of
/// this — a node's `explain`/`test` move knew the PREVIOUS concept was
/// called "C variable declaration" but nothing about what that node's prose
/// actually covered, so a later node had no way to notice it was about to
/// re-teach the same ground (seen live, 2026-08-15: "C assignment operator"
/// repeating material "C variable declaration" already covered — the two
/// concepts are inseparable in a real for-loop). Skips any prior item never
/// generated (locked, or the learner skipped it) — nothing to excerpt.
fn prior_content_context(
    state: &AppState,
    doc_id: &str,
    prior_items: &[engine::OutlineItem],
) -> String {
    let titles = prior_items
        .iter()
        .map(|i| i.title.as_str())
        .collect::<Vec<_>>()
        .join("; ");
    let mut covered = String::new();
    for i in prior_items {
        if let Ok(node) = state.store.read_node(doc_id, &i.id) {
            covered.push_str("\n## ");
            covered.push_str(&i.title);
            covered.push('\n');
            covered.push_str(node.content.html.trim());
            covered.push('\n');
        }
    }
    if covered.is_empty() {
        return titles;
    }
    format!(
        "Outline so far: {titles}\n\nContent already covered — do NOT repeat this, \
         write only what is genuinely new:{}",
        tail_chars(&covered, PRIOR_CONTENT_BUDGET)
    )
}

/// Compact objective summary for `MoveContext::objective` (§S4) — a document
/// with no `objective.json` yet (pre-S4) or an empty version chain degrades
/// to "", the same way `grounding_for` degrades when nothing is indexed.
pub(super) fn objective_for(state: &AppState, doc_id: &str) -> String {
    let Ok(json) = state.store.read_doc_file(doc_id, "objective.json") else {
        return String::new();
    };
    let Ok(log) = serde_json::from_str::<ObjectiveLog>(&json) else {
        return String::new();
    };
    log.current().map(|v| v.text.clone()).unwrap_or_default()
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
pub(super) fn rung_for(state: &AppState, doc_id: &str, config_prior: AgentPolicy) -> AgentPolicy {
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
pub(super) fn read_profile(state: &AppState, doc_id: &str) -> Option<ProfileProjection> {
    let json = state.store.read_doc_file(doc_id, "profile.json").ok()?;
    serde_json::from_str(&json).ok()
}

/// Compact evidence-profile text for `MoveContext::profile` (§7/§S7): the
/// always-fresh evidence table (`tactic_outcomes`, 0 LLM tokens, recomputed
/// on every call) plus whatever distilled traits/hypotheses `profile.json`
/// currently holds. Degrades to "" on a document with nothing yet, same
/// convention as `grounding_for`/`objective_for`.
pub(super) fn profile_for(state: &AppState, doc_id: &str) -> String {
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
pub(super) fn spawn_profile_distillation(state: AppState, doc_id: String, node_closed: bool) {
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
pub(super) async fn grounding_for(state: &AppState, query: &str) -> String {
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
/// server-only sidecar), then marks it finalized in the event log. Returns
/// the exercise HTML to the client. Content generation already happened
/// move by move in `generate_node`'s loop, each move already tagged and
/// progressively written (§S6 follow-up) — `content_html` here is that
/// already-tagged accumulation, so assembly uses `finalize_node` (which
/// only tags the fresh exercise form) rather than `assemble_node` (which
/// would re-tag/re-render the whole thing, unsafe for already-rendered
/// math, see `wrap_article`'s doc comment in engine.rs).
pub(super) async fn finalize(
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
    let node = engine::finalize_node(
        doc_id,
        &prep.node_id,
        content_html,
        &graded.html,
        &exercise_id,
        &rubric_id,
    )
    .map_err(|e| e.to_string())?;

    // `write_node_content`, not `write_node`: a concurrent `/ask` may have
    // appended an interaction against this node's still-partial content
    // between the last progressive write and this final one (§S6 follow-up
    // — asking no longer waits for the node to finish), and this write must
    // not clobber it.
    state
        .store
        .write_node_content(&node)
        .map_err(|e| e.to_string())?;

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

    // The explicit completion signal `prepare`'s regen guard reads
    // (`events::aggregate::node_generated`) — appended last, after both
    // files land, so a crash between the two writes above never leaves a
    // node that reads as finalized without also having a rubric.
    let event_log = state.store.event_log(doc_id).map_err(|e| e.to_string())?;
    if let Err(e) = event_log.append(
        Some(&prep.node_id),
        EventKind::NodeGenerated {
            move_id: move_id.to_string(),
        },
    ) {
        eprintln!("event log append failed: {e}");
    }

    Ok(())
}
