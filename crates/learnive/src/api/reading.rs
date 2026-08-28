use super::*;

use super::generation::NODE_TAIL_BUDGET;
// ---------------------------------------------------------------------------
// §S6 — reading interactions ("the document is the answer", §9). All three
// endpoints below anchor against the node file on disk, which since the §S6
// follow-up (progressive per-move persistence) can be a partial, still
// mid-generation node just as well as a finalized one — `write_node_content`
// preserves whatever interaction layer already exists across the next move's
// write. A mid-draft `/ask` (or annotation, or the read-to-end sentinel
// below) is exactly what §S18's `ObservationFrame` reads back on the node's
// next per-move `/generate` call (`events::aggregate::observation_frame`).
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

/// Read-only source viewer (§11): serves the corpus's meta + table of
/// contents for a citation's `data-source-id`, so `<cite>` has somewhere real
/// to point. GET, mutates nothing: the source viewer is read-only by design
/// (§9/§11) — any note the learner wants to make lands in the living
/// document, never on the source itself. Not document-scoped: the corpus
/// (`state.corpus`) is one shared, global store (§4/§11), so this sits beside
/// `/api/documents/...` rather than under it.
pub async fn get_source(
    State(state): State<AppState>,
    Path(source_id): Path<String>,
) -> Result<Json<crate::source::SourceIndex>, ApiError> {
    state
        .corpus
        .load_index(&source_id)
        .map(Json)
        .map_err(|e| ApiError::NotFound(e.to_string()))
}

/// Serves the canonical PDF artifact for a source (§4/§11: PDF is the sole
/// canonical, displayed format — a section's extracted text is index-only).
/// Read-only, same rationale as the other source endpoints. The filename is
/// always `source.pdf` (`SourceMeta.pdf_asset`); `Content-Type` is fixed,
/// never sniffed from the bytes.
pub async fn get_source_asset(
    State(state): State<AppState>,
    Path((source_id, filename)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let mime = match filename.rsplit('.').next() {
        Some("pdf") => "application/pdf",
        _ => return Err(ApiError::BadRequest("unsupported asset type".to_string())),
    };
    let bytes = state
        .corpus
        .load_asset(&source_id, &filename)
        .map_err(|e| ApiError::NotFound(e.to_string()))?;
    Ok((
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(mime)),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            ),
            (
                header::HeaderName::from_static("x-content-type-options"),
                HeaderValue::from_static("nosniff"),
            ),
        ],
        bytes,
    )
        .into_response())
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
pub(super) fn question_header(
    question: &str,
    anchor: &Anchor,
    locale: crate::locale::Locale,
) -> String {
    let asked = crate::locale::pick(locale, "You asked:", "Você perguntou:");
    let about = crate::locale::pick(locale, "about", "sobre");
    let mut out = format!(
        "<p class=\"question\"><strong>{asked}</strong> {}",
        escape_html(question)
    );
    let quote = anchor
        .quote
        .as_ref()
        .map(|q| q.exact.trim())
        .filter(|q| !q.is_empty());
    if let Some(exact) = quote {
        out.push_str(&format!(
            " <span class=\"about\">{about} \u{201c}{}\u{201d}</span>",
            escape_html(&head_chars(exact, QUOTE_HEADER_BUDGET))
        ));
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

/// Parses `doc_id`'s `outline.json` (§S15b) — the one place every owner-
/// resolution call site below goes through, so a parse-shape change never
/// has to be repeated at each call site.
pub(super) fn read_outline(state: &AppState, doc_id: &str) -> Result<Outline, ApiError> {
    let outline_json = state.store.read_doc_file(doc_id, "outline.json")?;
    serde_json::from_str(&outline_json).map_err(|e| ApiError::Internal(e.to_string()))
}

/// Resolves which document actually owns `node_id` (§S15b) before any node
/// read/write/interaction-append: a plain local node resolves to `doc_id`
/// itself, a referenced item (`OutlineItem::source_doc_id: Some`) resolves
/// to the owner. Every call site that used to pass `&doc_id` straight into
/// `read_node`/`write_node`/`append_interaction`/`update_annotation` must
/// resolve through this first, so Q&A/annotation/exercise on a reference
/// converge onto the SAME file the owning document reads, instead of
/// silently forking a second, local copy of the node's interaction layer.
///
/// A second case (§S15b step 5): `node_id` may not be a local item at all
/// but a sub-node spawned, mid-question, under a REFERENCED item — those
/// are written straight into the owner's own outline (never the visitor's,
/// see `ask_question`'s `Spawn` arm), so they never appear in `doc_id`'s
/// own `outline.json`. When a direct lookup misses, every local reference's
/// owner outline is checked in turn for the id — one hop only, since a
/// reference always points at the true owner directly, never at another
/// reference (`Node.doc_id` is never rewritten).
pub(super) fn owner_of_node(state: &AppState, doc_id: &str, node_id: &str) -> String {
    let Ok(outline) = read_outline(state, doc_id) else {
        return doc_id.to_string();
    };
    if outline.items.iter().any(|i| i.id == node_id) {
        return engine::owner_of(&outline, doc_id, node_id);
    }
    for owner in outline
        .items
        .iter()
        .filter_map(|i| i.source_doc_id.as_deref())
    {
        if let Ok(owner_outline) = read_outline(state, owner)
            && owner_outline.items.iter().any(|i| i.id == node_id)
        {
            return owner.to_string();
        }
    }
    doc_id.to_string()
}

/// Folds `doc_id`'s own event log into `node_states`, then folds in every
/// distinct owner referenced by `outline` too (§S15b step 3) — a
/// referenced item's real state (in particular `Demonstrated`) lives in
/// the OWNER's log, since `answer`/`ask_question`/`annotate`/
/// `practice_node` all route their events there via `owner_of_node`. Used
/// by both `outline_view` (the sidebar's own state per item) and
/// `prepare`'s gate check (a chain item can be gated on a REFERENCE's real
/// id — `materialize_outline_node`'s `Skip` arm returns `known.node_id` as
/// the exit gate — so the same fold has to apply there too, or a document
/// gated behind a reference would stay locked forever even after the
/// owner demonstrates it). Ids are opaque random strings (`engine::new_id`),
/// so merging owners' states into one flat map by id carries no realistic
/// cross-document collision risk.
pub(super) fn folded_node_states(
    state: &AppState,
    doc_id: &str,
    outline: &Outline,
) -> Result<std::collections::HashMap<String, NodeState>, ApiError> {
    let event_log = state.store.event_log(doc_id)?;
    let mut states = node_states(
        event_log
            .iter()
            .map_err(|e| ApiError::Internal(e.to_string()))?,
    );
    let owners: std::collections::HashSet<&str> = outline
        .items
        .iter()
        .filter_map(|i| i.source_doc_id.as_deref())
        .collect();
    for owner in owners {
        if let Ok(owner_log) = state.store.event_log(owner)
            && let Ok(iter) = owner_log.iter()
        {
            crate::events::aggregate::merge_node_states(&mut states, node_states(iter));
        }
    }
    Ok(states)
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
    headers: HeaderMap,
    Json(body): Json<AskReq>,
) -> Result<Json<AskResp>, ApiError> {
    let locale = crate::locale::Locale::from_header(&headers);
    let question = body.question.trim();
    if question.is_empty() {
        return Err(ApiError::BadRequest(
            "question must not be empty".to_string(),
        ));
    }
    let owner_id = owner_of_node(&state, &doc_id, &node_id);
    let node = state.store.read_node(&owner_id, &node_id)?;
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
    // §S15b step 3: a reference's events belong to the OWNER's log — same
    // convergence the node/interaction reads and writes above already
    // apply, so the owner's own evidence table (§7) and `outline_view`'s
    // fold (below) see every question asked from ANY document, not just
    // this one.
    let event_log = state.store.event_log(&owner_id)?;
    if let Err(e) = event_log.append(
        Some(&node_id),
        EventKind::QuestionAsked {
            move_id: move_id.clone(),
            anchor_block: body.anchor.block_id.clone(),
            question: question.to_string(),
        },
    ) {
        eprintln!("event log append failed: {e}");
    }

    // §S17: both outcomes below now generate through the move ABI
    // (`movement::generate_move_complete`) instead of the standalone
    // `engine::answer_question`/`generate_subnode_prose` calls — the same
    // context (profile/grounding/citations) and tactic self-labels any
    // other move gets, joined to this node's evidence table via the
    // `MoveGenerated` event appended right below.
    let mut ctx = MoveContext {
        topic: topic.clone(),
        item_title: title.clone(),
        node_tail,
        objective: objective_for(&state, &doc_id),
        profile: profile_for(&state, &doc_id),
        grounding: grounding_for(&state, &title).await,
        locale,
        question: Some(question.to_string()),
        reading_context: context.clone(),
        ..Default::default()
    };

    match decision {
        AskDecision::Inline => {
            let generated = movement::generate_move_complete(&ai, MoveType::Respond, &ctx).await?;
            if let Err(e) = event_log.append(
                Some(&node_id),
                EventKind::MoveGenerated {
                    move_id: move_id.clone(),
                    move_type: MoveType::Respond.to_string(),
                    tactics: generated.tactics.clone(),
                    rung: format!("{:?}", *state.policy.load_full()),
                },
            ) {
                eprintln!("event log append failed: {e}");
            }

            // Each paragraph of the answer gets its own `data-block-id`, on
            // the same `{id}-b{n}` scheme the content layer uses: an answer is
            // several paragraphs and the learner asks about *one* of them, so
            // it has to be addressable at that grain (§4.3) rather than as one
            // undifferentiated blob. This body never passes through
            // `assemble_node`/`assemble_content_node` (it lands in the
            // interaction layer, not the content layer), so `render_math`
            // has to run explicitly here — the same reason `tag_move_html`
            // exists for the content-layer per-move path.
            let answer_html =
                ensure_block_ids(&render_math(&generated.html), &format!("{move_id}-b"));
            let body_html = format!(
                "{}\n<div class=\"answer\">{answer_html}</div>",
                question_header(question, &body.anchor, locale)
            );
            state.store.append_interaction(
                &owner_id,
                &node_id,
                InteractionItem::Thread {
                    id: move_id.clone(),
                    kind: ThreadKind::Qa,
                    anchor_block: Some(body.anchor.block_id.clone()),
                    body_html: body_html.clone(),
                    child_node_id: None,
                    asked_in: Some(doc_id.clone()),
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
            ctx.spawned_section_title = Some(sub_title.clone());
            let generated = movement::generate_move_complete(&ai, MoveType::Respond, &ctx).await?;
            if let Err(e) = event_log.append(
                Some(&node_id),
                EventKind::MoveGenerated {
                    move_id: move_id.clone(),
                    move_type: MoveType::Respond.to_string(),
                    tactics: generated.tactics.clone(),
                    rung: format!("{:?}", *state.policy.load_full()),
                },
            ) {
                eprintln!("event log append failed: {e}");
            }
            // §S15b step 5: written under the OWNER's doc_id, not the
            // document the question was asked from — `node_id`'s own
            // interaction-layer append (below) already resolves to the
            // owner, and a sub-node stamped with the visitor's doc_id would
            // leave a `child_node_id` pointer the owner's own outline (and
            // any third document referencing the same node) can never
            // reach: reading the owner would show a thread pointing at a
            // node file that doesn't exist there.
            let sub_node = engine::assemble_content_node(&owner_id, &sub_id, &generated.html)?;
            state.store.write_node(&sub_node)?;
            state.store.update_outline_file(&owner_id, |json| {
                let mut outline: Outline = serde_json::from_str(json).map_err(|e| e.to_string())?;
                outline.items.push(OutlineItem {
                    id: sub_id.clone(),
                    title: sub_title.clone(),
                    prerequisites: Vec::new(),
                    parent_id: Some(node_id.clone()),
                    mode: NodeMode::Learn,
                    source_doc_id: None,
                    // A §S8 spawned sub-node is always a directly generable
                    // concept node (S27e).
                    item_type: OutlineItemType::Node,
                    expansion: ExpansionState::NotExpanded,
                    source: None,
                });
                serde_json::to_string(&outline).map_err(|e| e.to_string())
            })?;

            let content_html = learnive_core::redact_interactive_blocks(
                &learnive_core::prose_blocks_only(&sub_node.content.html),
            );
            let body_html = format!(
                "{}\n<p>↳ spawned a new section: {}</p>",
                question_header(question, &body.anchor, locale),
                escape_html(&sub_title)
            );
            state.store.append_interaction(
                &owner_id,
                &node_id,
                InteractionItem::Thread {
                    id: move_id,
                    kind: ThreadKind::Qa,
                    anchor_block: Some(body.anchor.block_id.clone()),
                    body_html,
                    child_node_id: Some(sub_id.clone()),
                    asked_in: Some(doc_id.clone()),
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
    id: String,
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
    let owner_id = owner_of_node(&state, &doc_id, &node_id);
    let node = state.store.read_node(&owner_id, &node_id)?;
    resolve_anchor(&node, &body.anchor)?;

    let event_log = state.store.event_log(&owner_id)?;
    if let Err(e) = event_log.append(
        Some(&node_id),
        EventKind::AnnotationAdded {
            anchor_block: body.anchor.block_id.clone(),
        },
    ) {
        eprintln!("event log append failed: {e}");
    }

    let body_html = format!("<p>{}</p>", escape_html(text));
    let id = engine::new_id();
    state.store.append_interaction(
        &owner_id,
        &node_id,
        InteractionItem::Annotation {
            id: id.clone(),
            anchor: body.anchor,
            body_html: body_html.clone(),
            asked_in: Some(doc_id.clone()),
        },
    )?;

    Ok(Json(AnnotateResp { id, body_html }))
}

#[derive(Deserialize)]
pub struct UpdateAnnotationReq {
    body: String,
}

/// Edits an existing annotation's text in place (`Store::update_annotation`)
/// — the one deliberate exception to §4.3 append-only, scoped to the user's
/// own margin notes. No event log entry: `AnnotationAdded` already tallies
/// the note's existence for the profile (§7), and an edit isn't a new one.
pub async fn update_annotation(
    State(state): State<AppState>,
    Path((doc_id, node_id, annotation_id)): Path<(String, String, String)>,
    Json(body): Json<UpdateAnnotationReq>,
) -> Result<Json<AnnotateResp>, ApiError> {
    let text = body.body.trim();
    if text.is_empty() {
        return Err(ApiError::BadRequest(
            "annotation must not be empty".to_string(),
        ));
    }
    let body_html = format!("<p>{}</p>", escape_html(text));
    let owner_id = owner_of_node(&state, &doc_id, &node_id);
    state
        .store
        .update_annotation(&owner_id, &node_id, &annotation_id, body_html.clone())?;
    Ok(Json(AnnotateResp {
        id: annotation_id,
        body_html,
    }))
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
    /// §S15: titles of this node's own children (`parent_id` pointing at
    /// it) — fed to `MoveContext::children_titles` so a `test` move on a
    /// node with a decomposed prerequisite tree can integrate them.
    pub(super) children_titles: Vec<String>,
    /// §S15: this item's `NodeMode` is `Review` — fed to
    /// `MoveContext::review_mode` so the move prompts ask for a short pass
    /// (definition + a couple of exercises) instead of full generation.
    pub(super) review_mode: bool,
    /// §S15: the title of this item's PARENT in the prerequisite tree
    /// (`item.parent_id`), when it has one — fed to `MoveContext::parent_title`
    /// so a sub-node's prompt can be told to stay inside its own narrow scope
    /// instead of drifting into the parent's broader concept. Every prompt
    /// already gets `topic` (the whole document's subject) alongside
    /// `item_title` (this node's own, narrower one); with nothing telling the
    /// model to keep those apart, a prerequisite sub-node's content leaned on
    /// the document topic and taught the parent's own material instead
    /// (observed live, 2026-08-17 — a "Defining and calling functions in
    /// Rust" review sub-node ended up teaching recursion's base case/
    /// recursive step, the parent node's own concept).
    pub(super) parent_title: Option<String>,
    /// Titles of every other outline item with no node generated for it yet
    /// — fed to `MoveContext::later_titles` so a move can be told these
    /// belong to a separate, later node and are off-limits here. See
    /// `MoveContext::later_titles`'s doc comment for the live bug this
    /// closes.
    pub(super) later_titles: Vec<String>,
    /// §14 resilience: moves already generated, persisted, and logged by a
    /// prior, interrupted `generate_node` attempt for this same node (empty
    /// on a node's first-ever attempt) — seeds `generate_node`'s
    /// `ctx.prior_moves` and its move-loop start index, so a retry after a
    /// mid-node failure (e.g. a later move exceeding `COMPLETE_BUDGET`,
    /// PLAN.md's "Geração de nó não é resiliente") picks up after the last
    /// successful move instead of regenerating (and re-paying for) it.
    pub(super) resumed_moves: Vec<MoveRecord>,
    /// The content HTML already progressively persisted for those moves
    /// (`engine::strip_build_marker`-cleaned so re-wrapping doesn't stack a
    /// second build marker on top) — empty exactly when `resumed_moves` is.
    pub(super) resumed_content_html: String,
    /// The move-loop's resume index (`resumed_move_index`) — total moves
    /// already logged, including `research`, unlike `resumed_moves` which
    /// excludes it. See `resumed_move_index`'s doc comment for why these two
    /// counts differ and why the loop must start at this one, not
    /// `resumed_moves.len()`.
    pub(super) resumed_move_index: usize,
    /// §S18: what the learner did since the last move settled in this node
    /// (`events::aggregate::observation_frame`) — empty on a node's first
    /// move, or when this `/generate` call reopened before anything new
    /// happened. Feeds `MoveContext.observation`.
    pub(super) observation: crate::events::aggregate::ObservationFrame,
    /// §S18: whether `research` has ever been logged for this node —
    /// reconstructed the same way `resumed_move_index` is, so the one-
    /// attempt-per-node cap (`MoveContext::research_attempted`'s doc
    /// comment) survives across per-move requests, not just within one.
    pub(super) research_attempted: bool,
    /// §S23: the zero-cost scaffolding parameter, folded fresh from the
    /// event log on every `/generate` call — fed to `MoveContext::scaffolding`.
    pub(super) scaffolding: crate::events::aggregate::ScaffoldingLevel,
    /// §S23: titles of already-demonstrated prerequisites or graph-close
    /// siblings — fed to `MoveContext::interleave_titles` so a `test` move
    /// can be told to mix one in and require distinguishing it, distinct
    /// from `children_titles`' "combine what this node's own children
    /// taught".
    pub(super) interleave_titles: Vec<String>,
}

/// §14 resilience: reconstructs the ungraded moves a prior, interrupted
/// `generate_node` attempt already completed for `node_id`, from the
/// `MoveGenerated` events it logged (`generation.rs` only appends one after
/// a move's content call actually succeeds, so a move recorded here was
/// real and already progressively persisted via `write_node_content` — a
/// move whose generation call itself failed, like the QA-observed
/// `COMPLETE_BUDGET` timeout, never reaches that append at all, leaving no
/// trace to reconstruct, which is exactly what we want: retrying naturally
/// re-attempts only the move that actually failed).
///
/// Excludes `research` on purpose: it's a real logged move (`generation.rs`'s
/// `MoveType::Research` branch appends one) but never reaches
/// `ctx.prior_moves.push` in a live single-request run either — it `continue`s
/// the loop before that point — so a resumed reconstruction that included it
/// would disagree with what a live run's own `prior_moves` ever contains.
///
/// Also excludes `respond` (§S18, live-caught 2026-08-21): `ask_question`
/// logs a `MoveGenerated{move_type: "respond"}` event tagged with the SAME
/// `node_id` it's a question about (§7 evidence bookkeeping), but that event
/// comes from a completely separate handler, never from `generate_node`'s
/// loop — a live loop's own `ctx.prior_moves` never contains a `respond`
/// entry either. Before §S18 this only mattered on a genuine crash-resume
/// (rare); §S18 makes "ask a question while the node is paused between
/// moves" the common case, so leaving `respond` in here reliably broke the
/// NEXT per-move request: L0's fixed [explain, test] rule
/// (`movement::l0_next_move`) doesn't recognize a 3rd move and errored with
/// "this node's moves are already complete".
fn resumed_ungraded_moves(
    events: impl Iterator<Item = crate::events::Event>,
    node_id: &str,
) -> Vec<MoveRecord> {
    events
        .filter(|e| e.node_id.as_deref() == Some(node_id))
        .filter_map(|e| match e.kind {
            EventKind::MoveGenerated { move_type, .. } => {
                movement::parse::move_type_name(&move_type).ok()
            }
            _ => None,
        })
        .filter(|mt| !matches!(mt, MoveType::Research | MoveType::Respond))
        .map(|move_type| MoveRecord {
            move_type,
            graded: false,
        })
        .collect()
}

/// §14 resilience: total moves a prior, interrupted `generate_node` attempt
/// already logged for `node_id`, INCLUDING `research` — every one of them,
/// research included, consumed one iteration of the move loop's `i`, which
/// `engine::tag_move_html` bakes into that move's block-id prefix. This,
/// not `resumed_moves.len()`, is the correct index for a resumed attempt's
/// loop to start at: starting at the (research-excluding) `resumed_moves`
/// count instead would reuse an index a prior iteration already tagged
/// content with, colliding block ids between the resumed content already on
/// disk and the next freshly generated move's.
///
/// Excludes `respond` (§S18, live-caught 2026-08-21, same root cause as the
/// exclusion in `resumed_ungraded_moves`): unlike `research`, a `respond`
/// move never comes from THIS node's own `generate_node` loop at all — it's
/// appended by `ask_question`, a wholly separate handler, and never consumed
/// one of this node's own `i` iterations or tagged content with this node's
/// block-id prefix. Counting it here overcounts the resume index by one per
/// question asked while the node sits paused between moves.
///
/// Does NOT exclude `explain`/`test` the same way for §8.2 remediation
/// (`api/grading.rs::answer`, also a wholly separate handler that appends
/// its own `MoveGenerated` under this node_id) — checked, not overlooked:
/// remediation only ever fires after this node's graded `Test` move has
/// already settled, and settling that move runs `finalize` in the same
/// request, which appends `NodeGenerated` before any submission can reach
/// remediation. `prepare`'s finalized-node refusal (this file, "Also
/// refuses regenerating a node that has already been finalized") means a
/// node carrying remediation events can never reach this function through
/// `/generate` again — so no exclusion is needed today. If that refusal is
/// ever loosened (e.g. to let remediation reopen a finalized node in
/// place), this function needs the same `move_type`-based exclusion
/// `respond` already has, or remediation reintroduces this exact stall.
fn resumed_move_index(events: impl Iterator<Item = crate::events::Event>, node_id: &str) -> usize {
    events
        .filter(|e| e.node_id.as_deref() == Some(node_id))
        .filter(|e| {
            matches!(
                &e.kind,
                EventKind::MoveGenerated { move_type, .. } if move_type != "respond"
            )
        })
        .count()
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
/// dropped connection, a page reload, or a later move exceeding
/// `COMPLETE_BUDGET` (PLAN.md's "Geração de nó não é resiliente"). Retrying
/// such a node now **resumes**: `resumed_moves`/`resumed_content_html` (§14
/// resilience, `resumed_ungraded_moves`) reconstruct the tactics history and
/// already-persisted prose from the prior attempt's `MoveGenerated` events
/// and partial node file, so `generate_node` picks up its move loop after
/// the last successful move instead of re-paying for it. `write_node_content`
/// (store.rs) is what keeps this safe for the interaction layer specifically:
/// any interaction appended against the partial node (a mid-stream `/ask`,
/// now possible — the whole point) survives the retry's overwrite, carried
/// forward rather than clobbered.
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
    // §S15: this is also what refuses a confirmed prereq-tree `skip` — its
    // id was never materialized into `outline.items` at all
    // (`cold_start::materialize_prereq_node`), so it can only ever land
    // here as "unknown outline item", never reach the gate checks below.
    let idx = outline
        .items
        .iter()
        .position(|i| i.id == item_id)
        .ok_or_else(|| "unknown outline item".to_string())?;
    let item = outline.items[idx].clone();
    // §S15b: a reference is by construction already generated (in the
    // OWNER, which is the only place `Demonstrated` evidence for it can
    // exist) — `prepare` is exclusively the pre-generation setup, so
    // refusing here is both correct and cheaper than an owner-aware
    // `node_generated` check: it also closes the accidental-duplicate-
    // generation risk a reference reached here would otherwise create
    // (a fresh LOCAL node file under this document's own id, orphaned
    // dead weight the moment `owner_of` resolution — used everywhere
    // else — keeps pointing at the owner's real file regardless).
    if item.source_doc_id.is_some() {
        return Err(
            "this node is a reference to another document; read it, don't generate it".to_string(),
        );
    }

    let event_log = state.store.event_log(doc_id).map_err(|e| e.to_string())?;
    // §S15b step 3: a prerequisite can itself be a REFERENCE's real id
    // (`materialize_outline_node`'s `Skip` arm returns `known.node_id` as
    // the chain's exit gate) — its `Demonstrated` state lives in the
    // owner's log, not this document's own, so the gate check below needs
    // the same owner-fold `outline_view` uses. Everything else in this
    // function (`resumed_moves`, `observation`, `research_attempted`
    // below) stays on the plain local `event_log`: it's this DOCUMENT's
    // own generation history of THIS item, which is always local (a
    // reference is refused above, before this point).
    let states = folded_node_states(state, doc_id, &outline)
        .map_err(|_| "failed to read the event log".to_string())?;
    // §S15: a `Skipped` prerequisite satisfies the gate exactly like
    // `Demonstrated` — a deliberate, permanent "I don't need this" is not
    // supposed to lock the rest of the document forever, and once real
    // trees (siblings, integration exercises gated on ALL children) exist,
    // that's not just cosmetic: a skipped branch would otherwise leave its
    // parent's gate impossible to ever satisfy.
    let unlocked = item.prerequisites.iter().all(|p| {
        matches!(
            states.get(p),
            Some(NodeState::Demonstrated) | Some(NodeState::Skipped)
        )
    });
    if !unlocked {
        return Err("this node is locked: its prerequisites are not yet demonstrated".to_string());
    }
    if node_generated(event_log.iter().map_err(|e| e.to_string())?, &item.id) {
        return Err(
            "this node was already generated; fetch it instead of regenerating".to_string(),
        );
    }

    let context = prior_content_context(state, doc_id, &outline.items[..idx]);
    let grounding = grounding_for(state, &item.title).await;
    let objective = objective_for(state, doc_id);
    let profile = profile_for(state, doc_id);
    let outline_titles = outline.items.iter().map(|i| i.title.clone()).collect();
    // §S15: titles of this node's own prerequisite-tree/question-spawned
    // children (`parent_id` pointing back at it) — fed to the `test` move so
    // a node with children can be told to integrate what they taught rather
    // than testing each in isolation again (`purpose(MoveType::Test)`).
    let children_titles = outline
        .items
        .iter()
        .filter(|i| i.parent_id.as_deref() == Some(item.id.as_str()))
        .map(|i| i.title.clone())
        .collect();
    let review_mode = item.mode == NodeMode::Review;
    let parent_title = item.parent_id.as_deref().and_then(|pid| {
        outline
            .items
            .iter()
            .find(|i| i.id == pid)
            .map(|i| i.title.clone())
    });
    let later_titles: Vec<String> = outline
        .items
        .iter()
        .filter(|i| i.id != item.id && !states.contains_key(&i.id))
        .map(|i| i.title.clone())
        .collect();
    let resumed_moves =
        resumed_ungraded_moves(event_log.iter().map_err(|e| e.to_string())?, &item.id);
    let resumed_move_index =
        resumed_move_index(event_log.iter().map_err(|e| e.to_string())?, &item.id);
    let resumed_content_html = if resumed_moves.is_empty() {
        String::new()
    } else {
        let owner = item.source_doc_id.as_deref().unwrap_or(doc_id);
        state
            .store
            .read_node(owner, &item.id)
            .map(|node| engine::strip_build_marker(&node.content.html).to_string())
            .unwrap_or_default()
    };
    let observation = crate::events::aggregate::observation_frame(
        event_log.iter().map_err(|e| e.to_string())?,
        &item.id,
    );
    let research_attempted = crate::events::aggregate::research_attempted(
        event_log.iter().map_err(|e| e.to_string())?,
        &item.id,
    );
    let scaffolding =
        crate::events::aggregate::scaffolding_level(event_log.iter().map_err(|e| e.to_string())?);
    // §S23: nearby ⇒ an already-demonstrated prerequisite of this item, or
    // a sibling sharing its parent — distinct from `children_titles`
    // (this item's own decomposed children).
    let interleave_titles: Vec<String> = outline
        .items
        .iter()
        .filter(|i| i.id != item.id)
        .filter(|i| {
            let is_prereq = item.prerequisites.iter().any(|p| p == &i.id);
            let is_sibling = item.parent_id.is_some() && i.parent_id == item.parent_id;
            (is_prereq || is_sibling) && matches!(states.get(&i.id), Some(NodeState::Demonstrated))
        })
        .map(|i| i.title.clone())
        .collect();
    Ok(NodePrep {
        topic: outline.topic,
        title: item.title,
        context,
        node_id: item.id,
        grounding,
        objective,
        profile,
        outline_titles,
        children_titles,
        review_mode,
        parent_title,
        later_titles,
        resumed_moves,
        resumed_content_html,
        resumed_move_index,
        observation,
        research_attempted,
        scaffolding,
        interleave_titles,
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
        let owner = i.source_doc_id.as_deref().unwrap_or(doc_id);
        if let Ok(node) = state.store.read_node(owner, &i.id) {
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
///
/// Callers pass the node's own concept title alone, never `{topic} {title}` —
/// prepending the raw curriculum topic was found (2026-08-20, live QA on an
/// "Epistemologia" document) to dominate the embedding query over the node's
/// own title, pulling in grounding for the topic itself even on a
/// prerequisite node meant to teach something standalone and different from
/// it ("Familiarity with the concept of philosophy" retrieved only
/// Epistemology passages, never the corpus's own Introduction to Philosophy
/// source, which scored higher once the topic prefix was dropped). A node's
/// own title is already a self-contained concept name (`propose_outline`'s
/// contract, `engine/prompt.rs`) — the topic doesn't need to ride along.
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
        reference_solution: graded.reference_solution.clone(),
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
