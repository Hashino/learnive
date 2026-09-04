use super::*;

use std::fs;

use tokio::task::spawn_blocking;

use super::generation::NODE_TAIL_BUDGET;
use crate::source;
// ---------------------------------------------------------------------------
// §S6 — reading interactions ("the document is the answer", §9). All three
// endpoints below anchor against the node file on disk, which since the §S6
// follow-up (progressive per-move persistence) can be a partial, still
// mid-generation node just as well as a finalized one — `write_node_content`
// preserves whatever interaction layer already exists across the next move's
// write. A mid-draft `/ask` (or annotation, or the read-to-end sentinel
// below) is exactly what §S18's `ObservationFrame` reads back on the node's
// next per-move `/generate` call.
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

/// Meta companion to [`get_library_pdf`] (S27n): title/authors for the
/// source panel's header, read straight from the persisted
/// `LibraryFileIndex` record — no PDF re-parse at request time, since
/// `load_candidates` already extracted this during acervo validation. `404`
/// with the identical "no library PDF matches this citation" message
/// `get_library_pdf` uses, for the same reason (unknown hash).
pub async fn get_library_meta(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<Json<source::acervo::LibraryFileRecord>, ApiError> {
    let data_dir = state.data_dir.to_string();
    spawn_blocking(
        move || -> Result<source::acervo::LibraryFileRecord, ApiError> {
            let index_root = std::path::PathBuf::from(&data_dir).join("index");
            let file_index = source::acervo::LibraryFileIndex::open(&index_root).map_err(|e| {
                ApiError::Internal(format!("could not open library file index: {e}"))
            })?;
            file_index.get(&hash).ok_or_else(|| {
                ApiError::NotFound("no library PDF matches this citation".to_string())
            })
        },
    )
    .await
    .map_err(|e| ApiError::Internal(format!("library lookup task failed: {e}")))?
    .map(Json)
}

/// Serves the canonical PDF for a source matched from the local library
/// (§11.1), keyed by content hash — the same key `ground_node` already
/// embeds in `<cite data-source-id>` fundamentação blocks (S27n, PLAN.md).
/// Distinct from `get_source_asset`: that route reads `state.corpus`, which
/// after S28 5b only ever holds LibGen/SciHub-acquired material — a document
/// grounded against the local library was never written there, so citations
/// on real generated documents 404 against it. This route resolves the hash
/// via `source::acervo::LibraryFileIndex` (written during acervo validation,
/// which `ensure_document_grounded` always runs before a document is allowed
/// to generate — so the index is guaranteed populated by the time a citation
/// naming that hash can exist) and reads the matched file straight from
/// `<data>/library/`. GET, read-only, same rationale as the other source
/// endpoints.
pub async fn get_library_pdf(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<Response, ApiError> {
    let data_dir = state.data_dir.to_string();
    let bytes = spawn_blocking(move || -> Result<Vec<u8>, ApiError> {
        let index_root = std::path::PathBuf::from(&data_dir).join("index");
        let file_index = source::acervo::LibraryFileIndex::open(&index_root)
            .map_err(|e| ApiError::Internal(format!("could not open library file index: {e}")))?;
        let record = file_index.get(&hash).ok_or_else(|| {
            ApiError::NotFound("no library PDF matches this citation".to_string())
        })?;
        let library = source::LocalPdfSource::open(&data_dir)
            .map_err(|e| ApiError::Internal(format!("could not open local library: {e}")))?;
        fs::read(library.root().join(&record.filename))
            .map_err(|e| ApiError::NotFound(format!("matched library file is missing: {e}")))
    })
    .await
    .map_err(|e| ApiError::Internal(format!("library lookup task failed: {e}")))??;
    Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/pdf"),
            ),
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
    // way (`objective_for`, `grounding_for` all fall back to ""); `/ask`
    // must stay at least as reliable as it was before §S8, not regress into
    // failing outright when the classifier call itself fails.
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
    // context (grounding/citations) any other move gets, with its
    // `MoveGenerated` event appended right below.
    let mut ctx = MoveContext {
        topic: topic.clone(),
        item_title: title.clone(),
        node_tail,
        objective: objective_for(&state, &doc_id),
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
                    tactics: Vec::new(),
                    rung: "deterministic".to_string(),
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
                    tactics: Vec::new(),
                    rung: "deterministic".to_string(),
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
                    chapter_number: None,
                    resolved_page: None,
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
    /// S33-4: this node is the LAST Learn-mode `Node` child of a `Chapter`
    /// that was actually decomposed into MORE THAN ONE such child — the
    /// template's cue to insert `integrate` between `explain` and `test`
    /// (§8's integration, scoped to the chapter the learner just finished).
    /// Computed from the outline shape, never guessed; false for a one-node
    /// chapter (nothing to combine) and for a review node (a review
    /// reactivates, it doesn't integrate).
    pub(super) chapter_close: bool,
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
/// research interception appends one) but never reaches
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
/// NEXT per-move request: the template rule (`movement::next_move`)
/// doesn't recognize a 3rd move and errored with "this node's moves are
/// already complete".
///
/// Restricted to TEMPLATE move types (S33): pre-S33 logs may record
/// `ask`/`plan`/`confront`/`profile`, which deserialize as `MoveType::
/// Other` — leaving one in a resumed `prior_moves` would wedge `next_move`
/// (no template arm matches it) and stall the node. A stale partial node
/// with only non-template moves simply restarts its template from the top.
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
        .filter(|mt| {
            matches!(
                mt,
                MoveType::Explain | MoveType::Test | MoveType::Integrate | MoveType::Revisit
            )
        })
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

// ---------------------------------------------------------------------------
// S33-3 spaced review (n·2ᵏ) — outline plumbing. The schedule itself is pure
// zero-token arithmetic over the event log (`events::aggregate::due_review`);
// this side turns chapters into scheduler input, a due (chapter, level) into
// a review item id, and materializes that item on the GENERATE path (never
// on a GET — §3.1 forbids state-changing GETs, so the outline response only
// *suggests*, and the client's openNode → generate POST is what makes the
// review node real).
// ---------------------------------------------------------------------------

/// A review node's id: `{chapter_id}_review{level}`. An underscore, not
/// `::`, because node ids must pass `store::ensure_safe_id` (alphanumeric
/// plus `-`/`_`) on their way to node files. The chapter is identified by
/// exact match against the outline's known chapter ids — never by string
/// splitting — so a chapter id that happened to contain `_review` can't
/// produce a collision.
pub(super) fn parse_review_id(outline: &Outline, id: &str) -> Option<(String, u32)> {
    crate::events::aggregate::parse_review_node_id(&review_chapters(outline), id)
}

/// Scheduler input for every `Chapter` in the outline: its non-review
/// direct children (a decomposed chapter), or the chapter item's own id
/// when it has none (a genuine `NoSplit` chapter generated as a single
/// node carries its own grades). Review children are excluded on purpose —
/// they are the scheduler's own output, not curriculum progress, and their
/// completion must neither satisfy a close nor advance the counter
/// (a completed review must not push the next review further out).
pub(super) fn review_chapters(outline: &Outline) -> Vec<crate::events::aggregate::ReviewChapter> {
    use crate::events::aggregate::ReviewChapter;
    outline
        .items
        .iter()
        .filter(|i| i.item_type == OutlineItemType::Chapter)
        .map(|ch| {
            let members: Vec<String> = outline
                .items
                .iter()
                .filter(|i| {
                    i.parent_id.as_deref() == Some(ch.id.as_str()) && i.mode != NodeMode::Review
                })
                .map(|i| i.id.clone())
                .collect();
            ReviewChapter {
                id: ch.id.clone(),
                members: if members.is_empty() {
                    vec![ch.id.clone()]
                } else {
                    members
                },
            }
        })
        .collect()
}

/// The due-review view for the current outline/log — every `OutlineResp`
/// carries it (the GET side of the scheduler; see `OutlineResp::due_review`).
pub(super) fn due_review_view(
    state: &AppState,
    doc_id: &str,
    outline: &Outline,
) -> Result<Option<super::cold_start::DueReviewView>, ApiError> {
    let chapters = review_chapters(outline);
    if chapters.is_empty() {
        return Ok(None);
    }
    let log = state
        .store
        .event_log(doc_id)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let due = crate::events::aggregate::due_review(
        log.iter().map_err(|e| ApiError::Internal(e.to_string()))?,
        &chapters,
    );
    let Some(due) = due else {
        return Ok(None);
    };
    let Some(chapter) = outline.items.iter().find(|i| i.id == due.chapter_id) else {
        return Ok(None);
    };
    Ok(Some(super::cold_start::DueReviewView {
        item_id: crate::events::aggregate::review_node_id(&due.chapter_id, due.level),
        chapter_id: due.chapter_id,
        level: due.level,
        title: chapter.title.clone(),
    }))
}

/// The generate-path half of the scheduler: a request to generate
/// `{chapter}_review{level}` whose item isn't materialized yet appends it to
/// the outline — but ONLY if the scheduler agrees this exact review is due
/// (a stale or hand-typed id is refused, not silently generated). The
/// materialized item is a `Node` child of its chapter in `NodeMode::Review`
/// with no prerequisites: nothing gates a scheduled review, and every bit of
/// downstream grounding machinery (`resolve_grounding_source` walking to the
/// chapter's book, `chapter_page_range` finding the chapter,
/// `interleave_titles` picking up the chapter's demonstrated nodes as
/// siblings) works through the parent link unchanged.
async fn maybe_materialize_review(
    state: &AppState,
    doc_id: &str,
    outline: &mut Outline,
    item_id: &str,
) -> Result<(), String> {
    let Some((chapter_id, level)) = parse_review_id(outline, item_id) else {
        return Ok(());
    };
    if outline.items.iter().any(|i| i.id == item_id) {
        return Ok(());
    }
    let chapters = review_chapters(outline);
    let log = state.store.event_log(doc_id).map_err(|e| e.to_string())?;
    let due =
        crate::events::aggregate::due_review(log.iter().map_err(|e| e.to_string())?, &chapters);
    let due_now = matches!(&due, Some(d) if d.chapter_id == chapter_id && d.level == level);
    if !due_now {
        let hint = match due {
            Some(d) => {
                let title = outline
                    .items
                    .iter()
                    .find(|i| i.id == d.chapter_id)
                    .map(|i| i.title.as_str())
                    .unwrap_or("");
                format!("the review due now is \"{title}\" (level {})", d.level)
            }
            None => "no chapter review is due".to_string(),
        };
        return Err(format!("this review is not due yet ({hint})"));
    }
    let chapter = outline
        .items
        .iter()
        .find(|i| i.id == chapter_id)
        .cloned()
        .ok_or_else(|| "unknown review chapter".to_string())?;
    let item = OutlineItem {
        id: item_id.to_string(),
        title: chapter.title.clone(),
        prerequisites: Vec::new(),
        parent_id: Some(chapter_id.clone()),
        mode: NodeMode::Review,
        source_doc_id: None,
        item_type: OutlineItemType::Node,
        expansion: ExpansionState::default(),
        source: None,
        chapter_number: None,
        resolved_page: None,
    };
    let persisted = item.clone();
    state
        .store
        .update_outline_file(doc_id, |json| {
            // Read-modify-write under the store's outline lock; if another
            // request materialized the same review first, keep theirs.
            let mut o: Outline = serde_json::from_str(json).map_err(|e| e.to_string())?;
            if o.items.iter().any(|i| i.id == item_id) {
                return Ok(json.to_string());
            }
            o.items.push(persisted);
            serde_json::to_string(&o).map_err(|e| e.to_string())
        })
        .map_err(|e| e.to_string())?;
    outline.items.push(item);
    Ok(())
}

/// The context for a chapter-scoped review node: the chapter's covered
/// nodes as material to REACTIVATE. The framing is deliberately the inverse
/// of `prior_content_context`'s "already covered — do NOT repeat this": a
/// review exists to re-cover exactly this ground, and that instruction
/// would make the model dodge the chapter's own material. Per-node
/// excerpts, oldest first, each budgeted so a wide chapter can't blow the
/// prompt (§14).
const REVIEW_NODE_BUDGET: usize = 700;

fn review_context(state: &AppState, doc_id: &str, outline: &Outline, item: &OutlineItem) -> String {
    let Some(chapter_id) = item.parent_id.as_deref() else {
        return String::new();
    };
    let mut out = String::new();
    for child in outline
        .items
        .iter()
        .filter(|i| i.parent_id.as_deref() == Some(chapter_id) && i.id != item.id)
    {
        let owner = child.source_doc_id.as_deref().unwrap_or(doc_id);
        let Ok(node) = state.store.read_node(owner, &child.id) else {
            continue;
        };
        out.push_str("\n## ");
        out.push_str(&child.title);
        out.push('\n');
        out.push_str(&tail_chars(node.content.html.trim(), REVIEW_NODE_BUDGET));
        out.push('\n');
    }
    format!(
        "This node reviews a chapter the learner already completed. Its nodes          covered the material below — reactivate and test it, don't avoid it as          already-said:\n{out}"
    )
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
    let mut outline: Outline = serde_json::from_str(&outline_json).map_err(|e| e.to_string())?;
    // S33-3: a review id whose item isn't materialized yet materializes here,
    // on the POST path, gated on the scheduler agreeing it's due — see
    // `maybe_materialize_review`. Runs before the `idx` lookup, which would
    // otherwise refuse the id as "unknown outline item".
    if parse_review_id(&outline, item_id).is_some()
        && !outline.items.iter().any(|i| i.id == item_id)
    {
        maybe_materialize_review(state, doc_id, &mut outline, item_id).await?;
    }
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
    // Read before the first redirect (hoisted 2026-09-01): the redirect
    // needs to know which children were already generated — found live as
    // a chapter click after its first children were done, where the
    // redirect resolved to an already-generated child and the regen guard
    // below refused with "already generated".
    let event_log = state.store.event_log(doc_id).map_err(|e| e.to_string())?;
    let generated_ids: std::collections::HashSet<String> = event_log
        .iter()
        .map_err(|e| e.to_string())?
        .filter_map(|e| match e.kind {
            EventKind::NodeGenerated { .. } => e.node_id.clone(),
            _ => None,
        })
        .collect();
    let is_generated = |id: &str| generated_ids.contains(id);
    // S27g item 2: a `Chapter` that already gained real `Node` children on
    // an EARLIER visit is a container by the time this request even
    // starts — redirect before the refusal below ever sees it. This must
    // run here, not only after the split-attempt block further down,
    // because the client's move-pause/continue loop (§S18) re-enters
    // `prepare` with this SAME chapter id on every request of a child's
    // multi-move generation, long after `expansion` has already flipped to
    // `Expanded` and the split-attempt block stops running. See
    // `redirect_into_chapter_child`'s doc for the full account.
    let item = redirect_into_chapter_child(&outline, item, &is_generated);
    // S27g (2026-08-29): a `Book`/`Article` item whose children are
    // topic-scoped `Chapter` proposals is a container, not content — see
    // `engine::is_generable`'s doc comment. Checked here, not just in the
    // client's view state, because the view state is advisory: this is the
    // enforcement point that actually stops a stale/tampered client request
    // from generating "the whole book" content on top of its own chapters.
    if !engine::is_generable(&outline, &item) {
        return Err(
            "this outline item is a container (its topics were split into chapters); read one of its chapters instead"
                .to_string(),
        );
    }
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

    // `event_log` was created at the top of this function (the redirect's
    // generated-children check needs it) — reused here, same binding.
    // §S15b step 3: a prerequisite can itself be a REFERENCE's real id
    // (`materialize_outline_node`'s `Skip` arm returns `known.node_id` as
    // the chain's exit gate) — its `Demonstrated` state lives in the
    // owner's log, not this document's own, so the gate check below needs
    // the same owner-fold `outline_view` uses. Everything else in this
    // function (`resumed_moves`, `research_attempted`
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
    // `effective_state`, not a plain `states.get`, so a prerequisite that is
    // itself a non-generable `Book`/`Article` container (S27g) resolves
    // through its chapter children instead of never satisfying the gate —
    // see that function's doc comment for the permanent-lock trap this
    // avoids.
    let unlocked = item.prerequisites.iter().all(|p| {
        matches!(
            engine::effective_state(&outline, &states, p),
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
    // S27m (reshaped 2026-08-29): the acervo gate is a document-level
    // precondition, checked once against the whole reading list, before any
    // per-node lookup — see `ensure_document_grounded`'s doc comment for why
    // this replaced the original per-item version.
    if let Err(reason) = ensure_document_grounded(state, doc_id, &outline).await {
        if let Err(e) = event_log.append(
            Some(&item.id),
            EventKind::GenerationBlocked {
                reason: reason.clone(),
            },
        ) {
            eprintln!("event log append failed: {e}");
        }
        return Err(reason);
    }
    // Re-read after the gate (2026-08-30, staleness bug caught before S27g
    // item 2 was built on top of it): `ensure_document_grounded` persists
    // chapter-number resolution (item 1) and, once item 2 exists, a
    // chapter's first-visit split, through `store.update_outline_file` — a
    // separate read-modify-write against disk that the `outline` binding
    // above (read once, before the gate ran) never sees. Without this,
    // `chapter_page_range` inside `ground_node` below would see
    // `resolved_page: None` on exactly the request that just resolved it —
    // item 1's narrowing silently missing its own chapter's first visit —
    // and item 2's split would have no page range to work from on the one
    // visit it's specified to run on. Every downstream use of
    // `outline`/`item` switches to this fresh copy from here on.
    let outline_json = state
        .store
        .read_doc_file(doc_id, "outline.json")
        .map_err(|e| e.to_string())?;
    let outline: Outline = serde_json::from_str(&outline_json).map_err(|e| e.to_string())?;
    let item = outline
        .items
        .iter()
        .find(|i| i.id == item_id)
        .cloned()
        .ok_or_else(|| "unknown outline item".to_string())?;

    // S27g item 2 (PLAN.md, "tenta é literal"): the FIRST visit to a
    // `Chapter` (page range already resolved by item 1 — the hard
    // prerequisite the split's own input depends on) tries splitting it
    // into atomic `Node` sub-topics before anything else runs. This must
    // happen before the container-refusal check below, not inside
    // `ensure_document_grounded` above: that gate iterates every
    // `ChaptersProposed` item in the WHOLE outline unconditionally (it's
    // zero-token, so that's fine there), but a split costs at most one
    // model call — running it from there would fire on every unvisited
    // chapter the first time ANY chapter in the book is opened, which is
    // exactly the speculative spend §14 killed prefetch over. Scoping it to
    // this one already-resolved `item`, gated on `NotExpanded`, is what
    // keeps it to "one call per chapter, at that chapter's own arrival" —
    // `try_split_chapter` leaves `expansion` at `Expanded` whenever the
    // attempt genuinely concluded (split or genuinely single-topic), so a
    // concluded chapter never re-attempts. S33-2: a FAILED attempt (provider
    // error) now also stays `NotExpanded` — the refusal further down refuses
    // whole-chapter generation, and the next visit retries the split.
    let (outline, item) = if item.item_type == OutlineItemType::Chapter
        && item.expansion == ExpansionState::NotExpanded
        && item.resolved_page.is_some()
    {
        let _ = try_split_chapter(state, doc_id, &outline, &item).await;
        let outline_json = state
            .store
            .read_doc_file(doc_id, "outline.json")
            .map_err(|e| e.to_string())?;
        let outline: Outline = serde_json::from_str(&outline_json).map_err(|e| e.to_string())?;
        let item = outline
            .items
            .iter()
            .find(|i| i.id == item_id)
            .cloned()
            .ok_or_else(|| "unknown outline item".to_string())?;
        (outline, item)
    } else {
        (outline, item)
    };
    // The split attempt above may have JUST turned `item` into a
    // container this very call — redirect again for that case (the
    // top-of-function redirect only caught an EARLIER visit's split).
    let item = redirect_into_chapter_child(&outline, item, &is_generated);
    // Mirrors the container-refusal check near the top of this function —
    // reached now only by a `Book`/`Article` container (never a split
    // `Chapter`, redirected above) or the defensive case where a `Chapter`
    // somehow has no children despite `is_generable` reporting false.
    if !engine::is_generable(&outline, &item) {
        let reason = "this outline item is a container (its topics were split into nodes); read one of its children instead"
            .to_string();
        if let Err(e) = event_log.append(
            Some(&item.id),
            EventKind::GenerationBlocked {
                reason: reason.clone(),
            },
        ) {
            eprintln!("event log append failed: {e}");
        }
        return Err(reason);
    }
    // Bug reported live 2026-09-01: a `Chapter` `source::match_chapter`
    // could not place in its book's TOC (`engine::chapter_match_failed`,
    // shared with `cold_start::outline_view`'s remediation badge) used to
    // fall straight through to `ground_node`'s unscoped full-book-search
    // fallback below and generate real content anyway — so a learner could
    // open a node with real prose already on it and still be offered
    // "restart this document" / "skip this chapter" by the client's
    // remediation modal, which only checks the SAME flag `outline_view`
    // computes. Refusing here, before `ground_node` ever runs, closes that
    // contradiction: the flag and the enforcement now agree, and the only
    // way past an unmatched chapter is the remediation modal itself (pick
    // the page by hand, skip the book, or restart cold start) — never
    // silent degraded generation.
    if engine::chapter_match_failed(&outline.items, &item) {
        let reason = "this chapter could not be matched against its book's table of contents; resolve it from the library check before it can generate".to_string();
        if let Err(e) = event_log.append(
            Some(&item.id),
            EventKind::GenerationBlocked {
                reason: reason.clone(),
            },
        ) {
            eprintln!("event log append failed: {e}");
        }
        return Err(reason);
    }
    // S33-2: the chapter/section split is a MANDATORY structural step, not
    // best-effort — a chapter that is still a whole-chapter generation
    // target at this point is refused instead of generating the entire
    // chapter as one node. Two ways to still be here, both terminal for
    // THIS request (both retry later, zero tokens):
    //
    // - the chapter's page never resolved (`chapter_match_failed` above was
    //   false only because the parent book hasn't finished its own TOC
    //   matching pass yet — the same remediation paths that fix a match
    //   failure fix this, they just haven't run to completion);
    // - the split attempt ran (this very call, or a prior one) and deferred
    //   — the library file, its hash, or the split model call itself failed.
    //   `expansion` stays `NotExpanded` exactly so this branch (or the
    //   split attempt, which re-runs first on every visit) sees it again.
    //
    // `NoSplit` (genuinely single-topic) and `Split` (children materialized,
    // redirected above) both mark `Expanded` and pass.
    if item.item_type == OutlineItemType::Chapter && item.resolved_page.is_none() {
        let reason = "this chapter has not been placed in its book's table of contents yet; it generates only after that matching pass resolves it".to_string();
        if let Err(e) = event_log.append(
            Some(&item.id),
            EventKind::GenerationBlocked {
                reason: reason.clone(),
            },
        ) {
            eprintln!("event log append failed: {e}");
        }
        return Err(reason);
    }
    if item.item_type == OutlineItemType::Chapter && item.expansion == ExpansionState::NotExpanded {
        let reason = "this chapter has not been split into nodes yet; the split is retried on the next visit".to_string();
        if let Err(e) = event_log.append(
            Some(&item.id),
            EventKind::GenerationBlocked {
                reason: reason.clone(),
            },
        ) {
            eprintln!("event log append failed: {e}");
        }
        return Err(reason);
    }
    let grounding = match ground_node(state, &outline, &item).await {
        Ok(text) => text,
        Err(reason) => {
            // S27m minimum floor (PLAN.md, 2026-08-29): nothing is persisted
            // to the frozen content layer — `prepare` returning `Err` here
            // means `generate_node` never reaches its move loop at all — the
            // event log records why, and the caller's `error` SSE frame is
            // what the learner sees (never model prose papering over the
            // gap, never a silent skip).
            if let Err(e) = event_log.append(
                Some(&item.id),
                EventKind::GenerationBlocked {
                    reason: reason.clone(),
                },
            ) {
                eprintln!("event log append failed: {e}");
            }
            return Err(reason);
        }
    };
    let objective = objective_for(state, doc_id);
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
    // S33-3: a review scoped to a chapter gets the chapter's covered nodes
    // as material to reactivate — NOT `prior_content_context`'s
    // "already covered, do NOT repeat" framing, which would make the model
    // dodge exactly the ground the review exists to re-cover. A §S15
    // learner-chosen review sub-node (parent is a plain Node, not a
    // chapter) keeps the ordinary context.
    let context = if review_mode {
        let parent_is_chapter = item.parent_id.as_deref().is_some_and(|pid| {
            outline
                .items
                .iter()
                .find(|i| i.id == pid)
                .is_some_and(|p| p.item_type == OutlineItemType::Chapter)
        });
        if parent_is_chapter {
            review_context(state, doc_id, &outline, &item)
        } else {
            context
        }
    } else {
        context
    };
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
    // S33-4: the last of a decomposed chapter's Learn-mode node children
    // closes that chapter — that, and only that, node integrates. `review_
    // mode` wins by construction here (the guard above), matching
    // `MoveContext::chapter_close`'s contract.
    let chapter_close = !review_mode
        && item.item_type == OutlineItemType::Node
        && item
            .parent_id
            .as_deref()
            .and_then(|pid| outline.items.iter().find(|i| i.id == pid))
            .is_some_and(|p| p.item_type == OutlineItemType::Chapter)
        && {
            let mut siblings = outline.items.iter().filter(|i| {
                i.parent_id == item.parent_id
                    && i.item_type == OutlineItemType::Node
                    && i.mode != NodeMode::Review
            });
            siblings.next_back().is_some_and(|last| last.id == item.id) && siblings.count() >= 1
        };
    Ok(NodePrep {
        topic: outline.topic,
        title: item.title,
        context,
        node_id: item.id,
        grounding,
        objective,
        children_titles,
        review_mode,
        parent_title,
        later_titles,
        resumed_moves,
        resumed_content_html,
        resumed_move_index,
        research_attempted,
        scaffolding,
        interleave_titles,
        chapter_close,
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

/// S27m (PLAN.md, 2026-08-29) — "o nó se funda no livro dele, ou não
/// nasce", **reshaped same day** after a live regression report: the
/// original version of this gate ran per-node, resolving and checking only
/// the ONE book a given item resolves to, from inside `prepare` (before
/// `generate_node`'s move loop starts). That made `research`'s LibGen
/// download unreachable for every bibliographically-sourced node —
/// `MoveType::Research` can only ever be selected from inside the move
/// loop, so a node whose book was missing could never even reach the move
/// that might have fixed it.
///
/// The correct shape, per the user: "nodes are only generated after the
/// source acquisition is already done... unless something is wrong in the
/// code" — the acervo gate is a PRECONDITION FOR THE DOCUMENT, evaluated
/// once against the whole reading list ([`engine::expected_items`]), not a
/// per-item lookup resolved lazily at generation time. `research` stays
/// reachable for what this gate never claimed to cover: a book a live
/// question needs that was never on the reading list at all. Where that
/// download should land (the real local library vs. today's separate
/// `Corpus`/`Retriever` path) is a still-open follow-up — see PLAN.md's
/// S27m entry.
///
/// Legacy/demo/pre-S27e documents with no bibliographic items at all are
/// untouched (`Ok(())` immediately) — S27m's own scope note is explicit
/// that only the bibliographically-sourced path is being fixed, not every
/// document in the app.
///
/// Piece 1 (S27c's index-cache build) lives here too now, for every
/// approved item that doesn't have one yet — triggered by this mutating
/// POST path rather than the read-only gate-report GET (§3.1).
/// Flattens a PDF's embedded bookmark tree into the flat shape
/// [`source::match_chapter`] expects, depth-first, parent before children —
/// used by [`ensure_document_grounded`]'s chapter-resolution pass for a book
/// whose TOC check came back `Embedded` (real bookmarks), which never
/// populates `TocConfirmStore` (that store is the S27k deduction path's
/// output only).
///
/// **Splits the printed number out of the raw bookmark title**
/// ([`source::split_printed_number`]) rather than leaving `number: None` —
/// an earlier version of this function didn't, and shipped a live bug
/// (2026-08-31): with `number` always `None`, `match_chapter`'s number-first
/// tier can never fire, so every embedded-outline match falls straight to
/// name similarity — which Stewart's own real bookmark defeats (`"2 -
/// Limits and derivatives"` vs. a proposed "Limits and Continuity" chapter
/// numbered `"2"`: the number matches exactly, but "derivatives" and
/// "continuity" alone don't clear the name-similarity floor, so the chapter
/// silently stayed unresolved and the next `explain` move's grounding
/// search fell back to unscoped, sometimes triggering an unhelpful
/// `research` move on top). Splitting the number out first restores the
/// number-first tier's veto-protected match, the same one the deduction
/// path already gets from `resolve_toc`/`propose_toc`.
fn flatten_embedded_outline(
    entries: &[source::OutlineEntry],
    out: &mut Vec<source::ConfirmedTocEntry>,
) {
    for e in entries {
        let (number, title) = source::split_printed_number(&e.title);
        out.push(source::ConfirmedTocEntry {
            title,
            number,
            page: Some(e.page),
            inferred: true,
        });
        flatten_embedded_outline(&e.children, out);
    }
}

/// The generation-refusal text for a report that failed any hard-blocking
/// check — identical for a freshly computed report and a memoized one, since
/// both must refuse exactly the same way (the message is what the event log
/// records and the library-check panel explains).
fn acervo_refusal(report: &source::acervo::AcervoReport) -> String {
    let failing: Vec<String> = report
        .failing_items()
        .iter()
        .map(|r| {
            format!(
                "\"{}\" ({})",
                r.expected.title,
                r.blocking_failures().join(", ")
            )
        })
        .collect();
    format!(
        "the acervo gate isn't clear yet — check it before this document can generate: {}",
        failing.join("; ")
    )
}

pub(super) async fn ensure_document_grounded(
    state: &AppState,
    doc_id: &str,
    outline: &Outline,
) -> Result<(), String> {
    let expected = engine::expected_items(outline);
    if expected.is_empty() {
        return Ok(());
    }

    // FAST PATH (2026-09-01; reshaped 2026-09-02, see `acervo_cache`): the
    // validation below re-reads every library PDF (hash + metadata + TOC/text
    // checks) and this gate runs on EVERY `/generate` — found live as minutes
    // of dead TTFT. The report is a pure function of (library files,
    // manual matches, TOC confirmations, expected items), so it's memoized
    // under exactly that pair; the gate's own outline writes (chapter
    // resolution, splits) do NOT invalidate it, which S29's doc-fingerprint
    // design got wrong — every resolution re-validated the whole library.
    let signature = acervo_signature(state).await?;
    let items: Vec<source::ExpectedItem> = expected.into_iter().map(|(_, item)| item).collect();
    let expected_fp = expected_items_fingerprint(&items);
    let cached_report = state
        .acervo_cache
        .lock()
        .await
        .get(&(signature, expected_fp))
        .cloned();
    if let Some(report) = cached_report {
        // Cache hit — but a hit is a memoized VERDICT, not a pass: a failing
        // report is cached just like a passing one (both are pure functions of
        // the key), and this path must refuse exactly like the fresh path
        // below. Found live 2026-09-03: after one refused `/generate`, every
        // later one hit this path, sailed past the failing verdict, and died
        // one layer down in `ground_node` — which reads a resolve failure
        // after "the gate passed" as an internal inconsistency and shows the
        // learner "this should not happen" instead of the honest acervo
        // refusal.
        if !report.all_pass() {
            return Err(acervo_refusal(&report));
        }
        // S27n's `LibraryFileIndex` write lives inside the (skipped)
        // validation, and citation deep-links need it. Filling it is nearly
        // free when it's already complete (a scan of tiny record files) and
        // only touches genuinely new files otherwise.
        ensure_library_file_index(state, &report).await?;
        // S34-A (2026-09-03, doc rmklzfy56r): a memo hit used to return
        // here — before the S27k/S27g structural passes ever ran on THIS
        // document, and if the first validation had been memoized by the
        // report panel's auto-open, no later `/generate` ever ran them at
        // all: every chapter stayed `resolved_page: None` forever, the
        // split never fired, and grounding fell back to whole-book
        // retrieval. The passes are functions of the OUTLINE, not of the
        // validation — run them on the memoized report's needs list,
        // exactly as the fresh path does below.
        let needs_toc_deduction: Vec<source::ExpectedItem> = report
            .items
            .iter()
            .filter(|r| {
                matches!(
                    r.toc,
                    source::TocCheck::Heuristic { .. } | source::TocCheck::Unavailable
                )
            })
            .map(|r| r.expected.clone())
            .collect();
        let has_outline_work = !needs_toc_deduction.is_empty()
            || outline
                .items
                .iter()
                .any(|i| matches!(i.expansion, ExpansionState::ChaptersProposed));
        if has_outline_work {
            let library = source::LocalPdfSource::open(state.data_dir.as_ref())
                .map_err(|e| format!("could not open local library: {e}"))?;
            let manual = source::ManualMatchStore::open(state.data_dir.as_ref())
                .map_err(|e| format!("could not open manual-match store: {e}"))?;
            let toc_confirm_dir = std::path::PathBuf::from(state.data_dir.as_ref())
                .join("index")
                .join("toc");
            resolve_outline_structure(
                state,
                doc_id,
                outline,
                &library,
                &manual,
                &toc_confirm_dir,
                &needs_toc_deduction,
            )
            .await?;
        }
        return Ok(());
    }
    let gate_started = std::time::Instant::now();

    let library = source::LocalPdfSource::open(state.data_dir.as_ref())
        .map_err(|e| format!("could not open local library: {e}"))?;
    let manual = source::ManualMatchStore::open(state.data_dir.as_ref())
        .map_err(|e| format!("could not open manual-match store: {e}"))?;
    let index_cache_dir = std::path::PathBuf::from(state.data_dir.as_ref())
        .join("index")
        .join("library");
    let toc_confirm_dir = std::path::PathBuf::from(state.data_dir.as_ref())
        .join("index")
        .join("toc");

    // `validate_acervo` re-parses every PDF in the library (`pdf-extract` +
    // a `lopdf` metadata pass, per `api::acervo`'s own module doc) — real,
    // synchronous, CPU-bound work, run here in `spawn_blocking` for the same
    // reason every handler in `api::acervo` already does: it must never
    // block the async runtime's worker threads.
    let lib_for_validate = library.clone();
    let items_for_validate = items.clone();
    let idx_dir_for_validate = index_cache_dir.clone();
    let toc_dir_for_validate = toc_confirm_dir.clone();
    let file_index_root = std::path::PathBuf::from(state.data_dir.as_ref()).join("index");
    let report = spawn_blocking(move || -> Result<source::acervo::AcervoReport, String> {
        // This is the mutating POST path (unlike `api::acervo`'s read-only
        // gate-report GET, which passes `None`) — S27n's `LibraryFileIndex`
        // write is safe here, and by the time any citation can reference a
        // hash, this call has already run and populated it.
        let file_index = source::acervo::LibraryFileIndex::open(&file_index_root)
            .map_err(|e| format!("could not open library file index: {e}"))?;
        source::validate_acervo(
            &lib_for_validate,
            &items_for_validate,
            &idx_dir_for_validate,
            &toc_dir_for_validate,
            Some(&file_index),
        )
        .map_err(|e| format!("could not validate the acervo gate: {e}"))
    })
    .await
    .map_err(|e| format!("acervo validation task panicked: {e}"))??;

    // Verdict cached (pass OR fail — both are pure functions of the key;
    // fixing the library changes the fingerprint and misses, so a stale
    // verdict can't outlive its inputs): later `/generate`s AND report-panel
    // opens skip the whole re-read. I/O errors above never reach an insert.
    state
        .acervo_cache
        .lock()
        .await
        .insert((signature, expected_fp), report.clone());

    if !report.all_pass() {
        return Err(acervo_refusal(&report));
    }

    // S34-A (bug reported live 2026-09-03, doc rmklzfy56r): BOTH structural
    // passes below (S27k TOC deduction + S27g chapter-name resolution) are
    // functions of the OUTLINE and of per-book TOC state — not of the
    // validation verdict — so they must run on the memo-hit path too, not
    // only here. They live in `resolve_outline_structure`; this fresh path
    // hands it the needs list the fresh report just computed.
    let needs_toc_deduction: Vec<source::ExpectedItem> = report
        .items
        .iter()
        .filter(|r| {
            matches!(
                r.toc,
                source::TocCheck::Heuristic { .. } | source::TocCheck::Unavailable
            )
        })
        .map(|r| r.expected.clone())
        .collect();
    resolve_outline_structure(
        state,
        doc_id,
        outline,
        &library,
        &manual,
        &toc_confirm_dir,
        &needs_toc_deduction,
    )
    .await?;

    let missing_index: Vec<&source::ExpectedItem> = report
        .items
        .iter()
        .filter(|r| matches!(r.index, source::IndexCheck::Missing))
        .map(|r| &r.expected)
        .collect();
    if missing_index.is_empty() {
        eprintln!(
            "acervo gate: full validation passed in {:.1}s (memoized until the library, manual matches, TOC, or expected items change)",
            gate_started.elapsed().as_secs_f32()
        );
        return Ok(());
    }

    let Some(embedder) = (match &state.retriever {
        Some(r) => Some(r.read().await.embedder().clone()),
        None => None,
    }) else {
        return Err("no embedding model is loaded — cannot index the library".to_string());
    };

    for item in &missing_index {
        // An ambiguous match (more than one plausible candidate file) is
        // already surfaced by the S27f matching screen; `report.all_pass()`
        // above only proves a candidate was FOUND for presence purposes,
        // not that it's uniquely resolved — this can legitimately skip
        // here, and `ground_node` below will then correctly report the gap
        // as an internal inconsistency instead of silently indexing the
        // wrong file.
        let Some(filename) = source::resolve_matched_filename(&library, &manual, item)
            .map_err(|e| format!("could not scan the local library: {e}"))?
        else {
            continue;
        };
        let path = library.root().join(&filename);
        let (hash, pdf) =
            source::read_pdf_cached(&path, source::pdftext_cache_dir(state.data_dir.as_ref()))
                .map_err(|e| format!("could not read {filename}: {e}"))?;
        source::build_index_cache(&pdf, &hash, &index_cache_dir, &embedder)
            .map_err(|e| format!("could not index {filename}: {e}"))?;
    }

    eprintln!(
        "acervo gate: validation + {} index build(s) took {:.1}s (memoized until the library, manual matches, TOC, or expected items change)",
        missing_index.len(),
        gate_started.elapsed().as_secs_f32()
    );
    Ok(())
}

/// The gate's two STRUCTURAL passes — the parts whose input is the outline
/// and per-book TOC state, not the validation verdict — so they run
/// identically on the fresh path and the memo-hit path of
/// [`ensure_document_grounded`]. Found live 2026-09-03 (S34-A, doc
/// rmklzfy56r): both used to live only after the fresh validation, and the
/// memo-hit early return skipped them forever — a document whose FIRST
/// validation was memoized (the report panel's auto-open does exactly that,
/// moments after creation) never resolved any chapter's page, so the
/// chapter split (`resolved_page.is_some()` gate) never fired, the
/// chapter-match refusal never applied (the book never reached
/// `Expanded`), and `ground_node` fell back to whole-book retrieval with
/// thin, banner-flagged grounding.
///
/// Pass 1 — S27k: the printed-contents-page deduction, for every expected
/// item whose `TocCheck` came back `Heuristic`/`Unavailable` (`needs`
/// comes from the caller, derived from whichever report is authoritative
/// on THIS call — fresh or memoized). Lives here, not inside
/// `source::acervo::check_toc` (that module stays free of `Ai`/tokio).
/// Never gates anything (`TocCheck` is never in `blocking_failures`, per
/// SPEC's "nenhum PDF é rejeitado por não ter bookmarks") and never
/// hard-fails the document: a missing/unconfigured provider, an unreadable
/// contents page, or a resolution below `is_resolution_acceptable`'s floor
/// all degrade silently to the existing heading-heuristic/user-
/// confirmation net.
///
/// Pass 2 — S27g (2026-08-30): chapter-name resolution — for every
/// Book/Article item whose `propose_outline`-time `Chapter` children
/// haven't been matched against this book's real table of contents yet
/// (`ExpansionState::ChaptersProposed`), resolve each proposed chapter's
/// number/name against the book's confirmed TOC (S27k — which pass 1 may
/// have just populated) and record the resolved physical page on the child
/// item. Degrades silently, the same convention as the rest of this
/// cascade: a book whose TOC isn't confirmed yet (no embedded outline,
/// deduction not yet acceptable, nobody's answered the S27f confirmation
/// screen) is left untouched and retried on a later call; a chapter that
/// matches nothing keeps generating unnarrowed (its proposed title stands
/// in, `resolved_page: None`) rather than being blocked — this step is
/// citation quality, not gating (scope was already fixed at cold start,
/// PLAN.md's 2026-08-29 revision: "a SELEÇÃO de escopo acontece na lista
/// de leitura").
async fn resolve_outline_structure(
    state: &AppState,
    doc_id: &str,
    outline: &Outline,
    library: &source::LocalPdfSource,
    manual: &source::ManualMatchStore,
    toc_confirm_dir: &std::path::Path,
    needs_toc_deduction: &[source::ExpectedItem],
) -> Result<(), String> {
    if !needs_toc_deduction.is_empty() {
        let toc_confirm = source::TocConfirmStore::open_at(toc_confirm_dir)
            .map_err(|e| format!("could not open TOC-confirmation store: {e}"))?;
        let ai = state.ai.load_full();
        for item in needs_toc_deduction {
            let Ok(Some(filename)) = source::resolve_matched_filename(library, manual, item) else {
                continue;
            };
            let path = library.root().join(&filename);
            let Ok((hash, pdf)) =
                source::read_pdf_cached(&path, source::pdftext_cache_dir(state.data_dir.as_ref()))
            else {
                continue;
            };
            let Some(range) = source::toc::find_contents_pages(&pdf) else {
                continue;
            };
            let contents_pages = source::toc::contents_page_chunks(&pdf, range);
            let Ok(llm_entries) = engine::propose_toc(&ai, &contents_pages).await else {
                continue;
            };
            let resolution = source::toc::resolve_toc(&pdf, &llm_entries, range.1);
            if source::toc::is_resolution_acceptable(&resolution) {
                let _ = toc_confirm.put_deduced(&hash, &resolution);
            }
        }
    }

    let needs_chapter_match: Vec<&OutlineItem> = outline
        .items
        .iter()
        .filter(|i| matches!(i.expansion, ExpansionState::ChaptersProposed))
        .collect();
    if needs_chapter_match.is_empty() {
        return Ok(());
    }
    let toc_confirm = source::TocConfirmStore::open_at(toc_confirm_dir)
        .map_err(|e| format!("could not open TOC-confirmation store: {e}"))?;
    // (book id, [(chapter id, resolved physical page)]) — computed here,
    // outside the outline-mutating closure below, since resolution needs
    // blocking file reads this closure (locked, synchronous) must not do.
    type ChapterResolutions = Vec<(String, Vec<(String, Option<usize>)>)>;
    let mut resolutions: ChapterResolutions = Vec::new();
    for book in &needs_chapter_match {
        let Some(ptr) = &book.source else { continue };
        let expected = source::ExpectedItem {
            title: ptr.item.title.clone(),
            authors: ptr.item.authors.clone(),
            kind: ptr.item.kind,
        };
        let Ok(Some(filename)) = source::resolve_matched_filename(library, manual, &expected)
        else {
            continue;
        };
        let path = library.root().join(&filename);
        let Ok((hash, pdf)) =
            source::read_pdf_cached(&path, source::pdftext_cache_dir(state.data_dir.as_ref()))
        else {
            continue;
        };
        // S27o follow-up (bug reported live 2026-08-31): a book with
        // real embedded PDF bookmarks (`TocCheck::Embedded` — most
        // well-formed textbooks, including Stewart/K&R/SICP per
        // `toc_bench`'s own measurements) never gets its hash into
        // `toc_confirm` — only the S27k deduction path
        // (`Heuristic`/`Unavailable`) ever calls `put`/`put_deduced`.
        // Falling straight to `continue` here left every embedded-TOC
        // book's chapters permanently unresolved (`resolved_page` stuck
        // at `None` forever) AND, because `resolutions` never gained an
        // entry for the book, `expansion` never left `ChaptersProposed`
        // — so this whole block re-ran, uselessly, on EVERY `/generate`
        // request for the document's entire lifetime (the reported
        // "grounding starts empty, Research fires every time" bug).
        // Prefer the PDF's own embedded bookmarks when present; only
        // fall back to the confirmed-deduction store when there are
        // none to flatten.
        let entries: Vec<source::ConfirmedTocEntry> = if !pdf.outline.is_empty() {
            let mut flat = Vec::new();
            flatten_embedded_outline(&pdf.outline, &mut flat);
            flat
        } else {
            let Some(confirmed) = toc_confirm.get(&hash) else {
                continue;
            };
            confirmed.entries
        };
        let chapters: Vec<(String, Option<usize>)> = outline
            .items
            .iter()
            .filter(|i| i.parent_id.as_deref() == Some(book.id.as_str()))
            .map(|chapter| {
                let page = source::match_chapter(
                    &entries,
                    chapter.chapter_number.as_deref(),
                    &chapter.title,
                )
                .and_then(|hit| hit.page);
                (chapter.id.clone(), page)
            })
            .collect();
        resolutions.push((book.id.clone(), chapters));
    }
    if !resolutions.is_empty() {
        state
            .store
            .update_outline_file(doc_id, |json| {
                let mut outline: Outline = serde_json::from_str(json).map_err(|e| e.to_string())?;
                for (book_id, chapters) in &resolutions {
                    if let Some(book_item) = outline.items.iter_mut().find(|i| &i.id == book_id) {
                        book_item.expansion = ExpansionState::Expanded;
                    }
                    for (chapter_id, page) in chapters {
                        let Some(page) = page else { continue };
                        if let Some(chapter_item) =
                            outline.items.iter_mut().find(|i| &i.id == chapter_id)
                        {
                            chapter_item.resolved_page = Some(*page);
                        }
                    }
                }
                serde_json::to_string(&outline).map_err(|e| e.to_string())
            })
            .map_err(|e| format!("could not persist chapter resolution: {e}"))?;
    }
    Ok(())
}

/// Cheap filesystem fingerprint of every LIBRARY-side input
/// [`ensure_document_grounded`] (and the report endpoint) validate against:
/// library PDFs, manual matches, TOC confirmations — each hashed by entry
/// name/len/mtime. Deliberately NOT this document's `outline.json` (S29's
/// version folded it in, which made the gate re-validate the whole library
/// after its own chapter-resolution/split writes bumped the file — observed
/// live as a 133s re-validation on the generate right after a chapter
/// resolved): the outline's influence on the report flows entirely through
/// the expected items, hashed separately by
/// [`expected_items_fingerprint`]. A stat-walk, not a content hash: the
/// expensive content identity lives inside `validate_acervo`, which this
/// fingerprint only decides whether to run.
pub(super) async fn acervo_signature(state: &AppState) -> Result<u64, String> {
    let data_dir = std::path::PathBuf::from(state.data_dir.as_ref());
    spawn_blocking(move || {
        use std::hash::Hasher;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        let mut fold_dir = |dir: &std::path::Path| {
            let mut entries: Vec<(std::ffi::OsString, std::fs::Metadata)> = std::fs::read_dir(dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .filter_map(|e| e.metadata().ok().map(|m| (e.file_name(), m)))
                        .collect()
                })
                .unwrap_or_default();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            for (name, m) in entries {
                hasher.write(dir.to_string_lossy().as_bytes());
                hasher.write(name.to_string_lossy().as_bytes());
                hasher.write(&m.len().to_le_bytes());
                let nanos = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);
                hasher.write(&nanos.to_le_bytes());
            }
        };
        fold_dir(&data_dir.join("library"));
        fold_dir(&data_dir.join("index").join("manual_matches"));
        fold_dir(&data_dir.join("index").join("toc"));
        // S32 (2026-09-03): the validation's parses are served from the
        // pdftext cache, so the report is a function of those entries too —
        // `read_pdf_cached`'s one-time /Info backfill of legacy entries
        // rewrites them (flipping presence/identity from Missing to Found
        // for books whose title lives only in /Info metadata), and a memo
        // keyed without this directory would keep serving the pre-backfill
        // verdict until restart. Cheap: one readdir of small JSON files.
        fold_dir(&data_dir.join("index").join("pdftext"));
        Ok(hasher.finish())
    })
    .await
    .map_err(|e| format!("acervo signature join failed: {e}"))?
}

/// Folds the expected items (title, authors, kind — in order, order is the
/// report's own shape) into the cache key alongside
/// [`acervo_signature`]. Two documents citing the same books share one
/// cached report; a `plan`-move that adds a book changes this and misses.
pub(super) fn expected_items_fingerprint(items: &[source::ExpectedItem]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for item in items {
        item.title.hash(&mut hasher);
        for a in &item.authors {
            a.hash(&mut hasher);
        }
        std::mem::discriminant(&item.kind).hash(&mut hasher);
    }
    hasher.finish()
}

/// Cache-hit counterpart of the validation's S27n `LibraryFileIndex` write
/// (which lives inside the skipped `validate_acervo` call): makes sure every
/// library file the cached report FOUND has a record, so citation deep-links
/// (`GET /api/library/{hash}`, `.../pdf`) resolve even when the gate never
/// re-runs the full validation. Cheap by construction: scans the record
/// directory (tiny JSONs) for filenames already covered and only reads +
/// hashes + metadata-parses a file that's genuinely missing a record — on
/// the steady state that loop body never runs.
async fn ensure_library_file_index(
    state: &AppState,
    report: &source::acervo::AcervoReport,
) -> Result<(), String> {
    let found: Vec<String> = report
        .items
        .iter()
        .filter_map(|r| match &r.presence {
            source::PresenceCheck::Found { filename } => Some(filename.clone()),
            _ => None,
        })
        .collect();
    if found.is_empty() {
        return Ok(());
    }
    let index_root = std::path::PathBuf::from(state.data_dir.as_ref()).join("index");
    let library_root = std::path::PathBuf::from(state.data_dir.as_ref()).join("library");
    spawn_blocking(move || -> Result<(), String> {
        use std::collections::HashSet;
        let file_index = source::acervo::LibraryFileIndex::open(&index_root)
            .map_err(|e| format!("could not open library file index: {e}"))?;
        let mut covered: HashSet<String> = HashSet::new();
        for entry in std::fs::read_dir(file_index.dir()).map_err(|e| e.to_string())? {
            let Ok(entry) = entry else { continue };
            let Ok(bytes) = std::fs::read(entry.path()) else {
                continue;
            };
            if let Ok(record) = serde_json::from_slice::<source::acervo::LibraryFileRecord>(&bytes)
            {
                covered.insert(record.filename);
            }
        }
        for filename in found {
            if covered.contains(&filename) {
                continue;
            }
            let path = library_root.join(&filename);
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let hash = source::acervo::content_hash(&bytes);
            let (title, authors) = source::acervo::read_info_metadata(&path);
            if let Err(e) = file_index.set(&hash, &filename, title.as_deref(), authors.as_deref()) {
                eprintln!("library file index ensure failed for {filename}: {e}");
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| format!("library file index ensure task failed: {e}"))?
}

/// A `Chapter` that has real `Node` children (S27g item 2 — split just now,
/// or on an earlier visit) is a container: resolves to its first child
/// instead of leaving the caller to hit the container-refusal error.
/// `prepare` calls this at **two** points — right after its initial
/// outline read, and again after the split-attempt block further down —
/// because the client's move-pause/continue loop (§S18) keeps POSTing to
/// this SAME chapter id across several requests while one child generates:
/// the first call redirects a chapter that was ALREADY split on an earlier
/// visit (`expansion` no longer `NotExpanded`, so the split-attempt block
/// itself never runs), the second catches a split that just happened this
/// very call. Missing either point reintroduces the container-refusal
/// error mid-generation — confirmed live while building this, not just
/// reasoned about: the error only ever appeared starting on request 2 of a
/// multi-move node.
///
/// Children are materialized in prerequisite-chained order
/// (`materialize_split_children`), so "first child" is well-defined and,
/// in practice, is always the one currently being generated.
///
/// `already_generated` (backed by the caller's event log) skips children
/// that are done — found live 2026-09-01: `advanceAfterGrading` could hand
/// this function an already-split chapter after its first children were
/// generated, and the blind "first child" then resolved to a finished
/// node, which the regen guard refused with "already generated" instead of
/// resuming the chapter's next child. Mid-generation the in-flight child
/// has no `NodeGenerated` event yet (that lands at `finalize`), so it
/// still resolves correctly across the §S18 continue loop. All children
/// generated → no redirect, and the caller's container-refusal names the
/// real situation ("read one of its children instead"). `Book`/`Article`
/// containers are untouched (the `item_type` guard) — that refusal
/// predates this slice and stays a hard error for them.
fn redirect_into_chapter_child(
    outline: &Outline,
    item: OutlineItem,
    already_generated: &dyn Fn(&str) -> bool,
) -> OutlineItem {
    if item.item_type != OutlineItemType::Chapter || engine::is_generable(outline, &item) {
        return item;
    }
    outline
        .items
        .iter()
        .find(|i| i.parent_id.as_deref() == Some(item.id.as_str()) && !already_generated(&i.id))
        .cloned()
        .unwrap_or(item)
}

/// Walks from `item` up through `parent_id` to the nearest `Chapter`
/// ancestor — `item` itself if it already is one, `None` if nothing between
/// it and the document root is a `Chapter` (a `Node` hung straight off a
/// `Book`, or off nothing at all). S27g item 2 (chapter→node splitting)
/// isn't built yet, so today this is almost always `None` in practice; this
/// function is written for the shape once that lands, and degrades to no
/// narrowing until then — never an error.
fn nearest_chapter<'a>(outline: &'a Outline, item: &OutlineItem) -> Option<&'a OutlineItem> {
    if item.item_type == OutlineItemType::Chapter {
        return outline.items.iter().find(|i| i.id == item.id);
    }
    let mut current = item.parent_id.clone();
    while let Some(pid) = current {
        let parent = outline.items.iter().find(|i| i.id == pid)?;
        if parent.item_type == OutlineItemType::Chapter {
            return Some(parent);
        }
        current = parent.parent_id.clone();
    }
    None
}

/// The page range (S27g item 1, PLAN.md, 2026-08-30) to narrow
/// [`source::search_index_cache`] to, derived from `OutlineItem::resolved_page`
/// — written by `source::match_chapter` during acervo validation and never
/// read anywhere until now. `None` whenever narrowing isn't possible (no
/// `Chapter` ancestor, or that chapter's page never resolved): the caller
/// then searches the whole book exactly as before this slice, the same
/// zero-token-cost fallback `search_index_cache` itself applies when a
/// range excludes every cached chunk.
///
/// The upper bound comes from the **next sibling chapter's own
/// `resolved_page`** (same `parent_id`, the smallest resolved page greater
/// than this chapter's own) minus one, so a chapter's range never bleeds
/// into the next one's text. `None` when there is no such sibling (the last
/// chapter, or an unmatched one) — `search_index_cache` reads that as "to
/// the end of the book", which is correct for the last chapter and, for an
/// unmatched sibling, no worse than not knowing where it ends.
///
/// Deliberately reads `resolved_page` as a **physical** page number without
/// any offset conversion: `source::toc::ResolvedTocEntry::page` (both the
/// embedded-`/Outlines` path and the S27k heuristic-deduction path) and
/// `CachedChunk::page` both document the same 1-based physical-page
/// convention, confirmed by reading both doc comments before writing this
/// function — a printed/label-page mismatch was the one thing that would
/// have made this narrowing silently ground a chapter in the wrong text.
fn chapter_page_range(outline: &Outline, item: &OutlineItem) -> Option<(usize, Option<usize>)> {
    let chapter = nearest_chapter(outline, item)?;
    let start = chapter.resolved_page?;
    let end = outline
        .items
        .iter()
        .filter(|i| {
            i.item_type == OutlineItemType::Chapter
                && i.parent_id == chapter.parent_id
                && i.id != chapter.id
        })
        .filter_map(|i| i.resolved_page)
        .filter(|&p| p > start)
        .min()
        .map(|next| next.saturating_sub(1));
    Some((start, end))
}

/// What [`try_split_chapter`] decided, and what the caller should do about
/// `expansion` (S27g item 2).
enum ChapterSplitOutcome {
    /// Couldn't even attempt — the library file, its hash, or the PDF
    /// itself didn't resolve. `expansion` must stay `NotExpanded`: nothing
    /// was tried, so nothing was learned, and a later visit (after whatever
    /// infrastructure gap closes) should get to try again at zero token
    /// cost — the same degrade-and-retry convention item 1's own
    /// chapter-matching pass already uses.
    Deferred,
    /// A real attempt ran and legitimately ended with no split — the TOC
    /// has no sub-entries under this chapter and the model (parsed, no
    /// provider error) returned no subsections: a genuinely single-topic
    /// chapter. Caller sets `expansion = Expanded`: this IS the "already
    /// tried, it's one node" outcome. (S33-2: provider ERRORS no longer
    /// land here — see the `Deferred` doc; an unparseable model response
    /// does, because it still cost the attempt and retrying it on every
    /// visit would burn a call per visit on a chapter that may simply not
    /// have sub-structure to find.)
    NoSplit,
    /// A real attempt produced children, already persisted with
    /// `expansion = Expanded`. The caller doesn't need the new ids from
    /// here — the container-redirect step right after this call re-reads
    /// the outline and finds them the same way it would on any later visit
    /// (`prepare`'s doc comment on that step explains why it has to run on
    /// every visit regardless).
    Split,
}

/// S27g item 2's split attempt, scoped to one already-page-resolved
/// `Chapter`. Tries, in order: (1) the confirmed TOC's own sub-entries
/// under this chapter's number (`source::sub_entries_within`) — zero token,
/// real structure, tried first because "a app não adivinha estrutura que
/// existe de verdade"; (2) failing that, one Fast-tier model call fed
/// structural signal only (heading-shaped lines over the chapter's own page
/// range, or, lacking any, a short prose sample spread across that range —
/// never the chapter's full text, see `engine::propose_chapter_split`'s doc
/// for why). At most one model call, ever, per chapter — enforced by the
/// caller only invoking this on `ExpansionState::NotExpanded`, which this
/// function always clears to `Expanded` on every path except
/// [`ChapterSplitOutcome::Deferred`].
async fn try_split_chapter(
    state: &AppState,
    doc_id: &str,
    outline: &Outline,
    chapter: &OutlineItem,
) -> ChapterSplitOutcome {
    let Some(ptr) = engine::resolve_grounding_source(outline, chapter) else {
        return ChapterSplitOutcome::Deferred;
    };
    let expected = source::ExpectedItem {
        title: ptr.item.title.clone(),
        authors: ptr.item.authors.clone(),
        kind: ptr.item.kind,
    };
    let Ok(library) = source::LocalPdfSource::open(state.data_dir.as_ref()) else {
        return ChapterSplitOutcome::Deferred;
    };
    let Ok(manual) = source::ManualMatchStore::open(state.data_dir.as_ref()) else {
        return ChapterSplitOutcome::Deferred;
    };
    let Ok(Some(filename)) = source::resolve_matched_filename(&library, &manual, &expected) else {
        return ChapterSplitOutcome::Deferred;
    };
    let path = library.root().join(&filename);
    let Ok((hash, pdf)) =
        source::read_pdf_cached(&path, source::pdftext_cache_dir(state.data_dir.as_ref()))
    else {
        return ChapterSplitOutcome::Deferred;
    };

    let Some((start, end)) = chapter_page_range(outline, chapter) else {
        // The hard prerequisite this item depends on (item 1's page
        // resolution) is what the caller already checked via
        // `resolved_page.is_some()` — reaching here despite that would mean
        // `chapter_page_range` itself couldn't find `chapter` as its own
        // nearest `Chapter` ancestor, which cannot happen for `chapter`
        // being passed as both `item` and its own ancestor. Deferred rather
        // than panicking: defensive, not expected.
        return ChapterSplitOutcome::Deferred;
    };
    let end = end.unwrap_or(pdf.page_texts.len());
    if start == 0 || start > pdf.page_texts.len() {
        return ChapterSplitOutcome::Deferred;
    }
    let end = end.min(pdf.page_texts.len());
    let slice = &pdf.page_texts[start - 1..end];

    let toc_confirm_dir = std::path::PathBuf::from(state.data_dir.as_ref())
        .join("index")
        .join("toc");
    let sub_titles: Vec<String> = (|| {
        let toc_confirm = source::TocConfirmStore::open_at(&toc_confirm_dir).ok()?;
        let confirmed = toc_confirm.get(&hash)?;
        let matched = source::match_chapter(
            &confirmed.entries,
            chapter.chapter_number.as_deref(),
            &chapter.title,
        )?;
        let subs = source::sub_entries_within(&confirmed.entries, matched.number.as_deref());
        (!subs.is_empty()).then(|| subs.into_iter().map(|e| e.title.clone()).collect())
    })()
    .unwrap_or_default();

    let sub_titles = if !sub_titles.is_empty() {
        sub_titles
    } else {
        let signal = {
            let headings = source::acervo::heuristic_toc_over(slice);
            if !headings.is_empty() {
                headings.join("\n")
            } else {
                sampled_prose(slice)
            }
        };
        let ai = state.ai.load_full();
        match engine::propose_chapter_split(&ai, &chapter.title, &signal).await {
            Ok(titles) => titles,
            // S33-2: the split is a structural PREREQUISITE for generating
            // this chapter, so a failed attempt (free-tier 429 included —
            // §12.2's "recovery costs zero tokens" means the retry happens
            // on a later visit, not with an extra call now) must NOT be
            // read as "nothing to split" and must not let the chapter fall
            // through to whole-chapter generation. Deferred keeps
            // `expansion` at `NotExpanded`; the caller refuses generation
            // and the next visit retries — the zero-token TOC shortcut
            // runs first every time, so a chapter the TOC can split never
            // re-pays the model call at all.
            Err(e) => {
                eprintln!(
                    "chapter split attempt failed for \"{}\": {e}",
                    chapter.title
                );
                return ChapterSplitOutcome::Deferred;
            }
        }
    };

    if sub_titles.is_empty() {
        return match mark_chapter_expanded(state, doc_id, &chapter.id, &[]).await {
            Ok(()) => ChapterSplitOutcome::NoSplit,
            Err(e) => {
                // Couldn't even persist "this chapter stays one node" — no
                // model call was spent deciding that (the TOC shortcut is
                // zero-token, and any model call above already returned
                // before this branch on success), so it's safe to retry a
                // later visit instead of silently pretending it landed.
                eprintln!(
                    "could not persist chapter split outcome for \"{}\": {e}",
                    chapter.title
                );
                ChapterSplitOutcome::Deferred
            }
        };
    }

    let incoming_gate = chapter.prerequisites.first().cloned();
    let children = materialize_split_children(&chapter.id, &sub_titles, incoming_gate);
    match mark_chapter_expanded(state, doc_id, &chapter.id, &children).await {
        Ok(()) => ChapterSplitOutcome::Split,
        Err(e) => {
            eprintln!(
                "could not persist chapter split for \"{}\": {e}",
                chapter.title
            );
            ChapterSplitOutcome::Deferred
        }
    }
}

/// Every-Nth-page-opening sample of a chapter's own prose, spread across
/// its whole page range rather than truncated to its first pages — the
/// fallback signal for [`try_split_chapter`] when the chapter has no
/// heading-shaped lines to work from. Bounded per-page and overall so a
/// long chapter still produces a small, cheap prompt
/// (`engine::propose_chapter_split` caps its input again regardless, this
/// just keeps the string built here from being wastefully large).
fn sampled_prose(pages: &[String]) -> String {
    const PER_PAGE_CHARS: usize = 400;
    const TOTAL_CHAR_CAP: usize = 6000;
    let mut out = String::new();
    for page in pages {
        if out.len() >= TOTAL_CHAR_CAP {
            break;
        }
        let sample: String = page.chars().take(PER_PAGE_CHARS).collect();
        if sample.trim().is_empty() {
            continue;
        }
        out.push_str(sample.trim());
        out.push('\n');
    }
    out
}

/// Sequential sibling-prerequisite chaining, mirroring
/// `api::cold_start::materialize_outline_tree`'s convention (each item's
/// sole prerequisite is the id of the item immediately before it) without
/// pulling in that function's unrelated `ConfirmedNode`/`PrereqAction`
/// machinery — S27g item 2 mints plain titles, not confirmed proposal
/// nodes. `resolved_page: None` on every child is deliberate, not an
/// omission: `nearest_chapter`/`chapter_page_range` already narrow through
/// to the parent `Chapter`'s own `resolved_page`, so a per-child value
/// would be dead data no code reads.
fn materialize_split_children(
    chapter_id: &str,
    titles: &[String],
    incoming_gate: Option<String>,
) -> Vec<OutlineItem> {
    let mut gate = incoming_gate;
    let mut out = Vec::with_capacity(titles.len());
    for title in titles {
        let id = engine::new_id();
        out.push(OutlineItem {
            id: id.clone(),
            title: title.clone(),
            prerequisites: gate.into_iter().collect(),
            parent_id: Some(chapter_id.to_string()),
            mode: NodeMode::Learn,
            source_doc_id: None,
            item_type: OutlineItemType::Node,
            expansion: ExpansionState::NotExpanded,
            source: None,
            chapter_number: None,
            resolved_page: None,
        });
        gate = Some(id);
    }
    out
}

/// Persists a chapter split's outcome: appends `children` (if any) and sets
/// `chapter_id`'s own `expansion = Expanded` — always together, in the same
/// `update_outline_file` pass, so the two can never land inconsistently
/// (children present but the chapter still `NotExpanded`, or vice versa).
/// When `children` is non-empty, also rewrites the chapter's own
/// `prerequisites` to `[last_child_id]`, mirroring
/// `materialize_outline_node`'s "this node's own prerequisites become its
/// LAST child's id" convention — not load-bearing (`prepare` checks
/// `is_generable` before ever reading a container's own `prerequisites`),
/// kept only for consistency with that existing pattern.
async fn mark_chapter_expanded(
    state: &AppState,
    doc_id: &str,
    chapter_id: &str,
    children: &[OutlineItem],
) -> Result<(), String> {
    let children = children.to_vec();
    let chapter_id = chapter_id.to_string();
    state
        .store
        .update_outline_file(doc_id, move |json| {
            let mut outline: Outline = serde_json::from_str(json).map_err(|e| e.to_string())?;
            if let Some(last) = children.last() {
                if let Some(chapter) = outline.items.iter_mut().find(|i| i.id == chapter_id) {
                    chapter.expansion = ExpansionState::Expanded;
                    chapter.prerequisites = vec![last.id.clone()];
                }
                outline.items.extend(children.clone());
            } else if let Some(chapter) = outline.items.iter_mut().find(|i| i.id == chapter_id) {
                chapter.expansion = ExpansionState::Expanded;
            }
            serde_json::to_string(&outline).map_err(|e| e.to_string())
        })
        .map_err(|e| e.to_string())
}

/// The per-node half of S27m's gate: once [`ensure_document_grounded`] has
/// passed for the whole document, resolves and retrieves THIS item's own
/// grounding passages. A failure here after the document gate already
/// passed reads as an internal inconsistency, not a "go fix your library"
/// state — the error messages say so deliberately.
///
/// An item with **no** bibliographic ancestor at all (legacy/demo/pre-S27e
/// documents, or a spawned sub-node under a plain `Node` parent) is
/// deliberately untouched: falls through to the old unscoped-similarity
/// [`grounding_for`], exactly as it behaved before S27m.
async fn ground_node(
    state: &AppState,
    outline: &Outline,
    item: &OutlineItem,
) -> Result<String, String> {
    let Some(ptr) = engine::resolve_grounding_source(outline, item) else {
        return Ok(grounding_for(state, &item.title).await);
    };

    let expected = source::ExpectedItem {
        title: ptr.item.title.clone(),
        authors: ptr.item.authors.clone(),
        kind: ptr.item.kind,
    };

    let library = source::LocalPdfSource::open(state.data_dir.as_ref())
        .map_err(|e| format!("could not open local library: {e}"))?;
    let manual = source::ManualMatchStore::open(state.data_dir.as_ref())
        .map_err(|e| format!("could not open manual-match store: {e}"))?;
    let index_cache_dir = std::path::PathBuf::from(state.data_dir.as_ref())
        .join("index")
        .join("library");

    let filename = source::resolve_matched_filename(&library, &manual, &expected)
        .map_err(|e| format!("could not scan the local library: {e}"))?;
    let Some(filename) = filename else {
        return Err(format!(
            "internal error: \"{}\" passed the document acervo gate but has no resolved file in the library — this should not happen",
            expected.title
        ));
    };

    let Some(embedder) = (match &state.retriever {
        Some(r) => Some(r.read().await.embedder().clone()),
        None => None,
    }) else {
        return Err("no embedding model is loaded — cannot ground this node".to_string());
    };

    let path = library.root().join(&filename);
    let bytes = fs::read(&path).map_err(|e| format!("could not read {filename}: {e}"))?;
    let hash = source::acervo::content_hash(&bytes);

    let page_range = chapter_page_range(outline, item);
    let hits = source::search_index_cache(
        &index_cache_dir,
        &hash,
        &embedder,
        &item.title,
        4,
        page_range,
    )
    .map_err(|e| format!("could not search the index for {filename}: {e}"))?;
    if hits.is_empty() {
        return Err(format!(
            "internal error: \"{}\" produced no retrievable content — this should not happen after the acervo gate passed",
            expected.title
        ));
    }

    Ok(hits
        .iter()
        .map(|(page, text, _score)| {
            format!(
                "[id: {} | loc: p:{page} | {}]\n{}",
                hash, expected.title, text
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n"))
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

    // Rendered here, not at assembly: the graded move never passes through
    // `tag_move_html` (only the streamed loop's ungraded moves do — see
    // `generation::generate_node`'s comment), so this is the exercise's ONLY
    // math pass, exactly once, before the same string lands in the node's
    // content layer (`finalize_node`) and the rubric sidecar (`exercise_frame`
    // and every frozen attempt serve it verbatim). Found live 2026-09-01 as
    // raw LaTeX in the settled exercise iframe. Same reasoning as the
    // remediation explanation in `grading::answer`.
    let exercise_html = render_math(&graded.html);

    let exercise_id = format!("{}-ex", prep.node_id);
    let rubric_id = format!("{}-ru", prep.node_id);
    let node = engine::finalize_node(
        doc_id,
        &prep.node_id,
        content_html,
        &exercise_html,
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
        exercise_html,
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
