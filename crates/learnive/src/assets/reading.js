// learnive — reading interactions (§S6, §9 "the document is the answer"):
// the reading-line highlight, selection→anchor resolution and the bottom
// ask bar that turns a question into an insertion in the document.

// --- §S6 reading interactions (§9 "the document is the answer") ------
// One bottom ask bar, always in the same place. It anchors the question
// to the current selection when there is one, and otherwise to the
// block on the reading line — the viewport-centre block, read at submit
// time and never persisted (§9); only the resulting anchor is. The
// answer comes back as an insertion into the document at that anchor,
// not as an entry appended to the end of the page.

// Whether the document is in a state where a question can be anchored
// at all: only a finalized node has the `data-block-id`s anchors are
// built from (§4.3), so a still-streaming node has nothing to point at.
let askEligible = false;
function setReadingToolsEnabled(enabled) {
  askEligible = enabled;
  if (!enabled) {
    pendingSelection = null;
    hideAskBar(true);
    cancelDraftAnnotation();
    updateAnnotationPlus();
  }
}

// Returns `{blockId, nodeId}` for the block nearest the reading line,
// or null. `nodeId` is whichever node actually owns that block — the
// top-level node (`#prose` carries its own `data-node-id`) or a
// spliced sub-node (§S8: each `.subnode` wrapper carries its own) —
// never assumed to be `state.nodeId`, since a spliced sub-node's
// blocks live inside `#prose` too.
function currentReadingBlock() {
  // Leaf-most blocks only. An answer carries a block id for the whole item
  // *and* one per paragraph (§4.3), so its wrapper would otherwise compete
  // with its own paragraphs for the reading line and win by being taller —
  // putting the highlight on the entire answer instead of the line you are
  // on. An answer written before per-paragraph ids has no inner block and
  // stays its own leaf. Scanned across every mounted section (§9), not
  // just one — the continuous document can have several on screen at once.
  // Also excludes degenerate zero-height blocks (e.g. a demonstrated node's
  // now-empty exercise slot, kept in the DOM only so an old Q&A thread can
  // still find its anchor, §S6) — nothing to read, so nothing to land on.
  const blocks = [
    ...el("nodeSections").querySelectorAll("[data-block-id]"),
  ].filter(
    (b) => !b.querySelector("[data-block-id]") && b.getBoundingClientRect().height > 0,
  );
  if (blocks.length === 0) return null;
  const center = window.innerHeight / 2;
  let best = blocks[0];
  let bestDist = Infinity;
  blocks.forEach((b) => {
    const rect = b.getBoundingClientRect();
    const dist = Math.abs((rect.top + rect.bottom) / 2 - center);
    if (dist < bestDist) {
      bestDist = dist;
      best = b;
    }
  });
  return best;
}

function currentReadingBlockId() {
  const best = currentReadingBlock();
  if (!best) return null;
  const owner = best.closest("[data-node-id]");
  return {
    blockId: best.dataset.blockId,
    nodeId: owner ? owner.dataset.nodeId : state.nodeId,
    text: best.textContent || "",
  };
}

// --- Reading-line highlight (§9) --------------------------------------
// Marks the block currently on the reading line. Purely ephemeral client
// UI state — never written into the node (§4.3), never sent anywhere; it
// exists so "ask about this" with no selection has a visible referent.
let readingLineEl = null;
let readingLineQueued = false;
let readingLineOverlay = null;
// A sibling overlay in `#doc`, not a class on the block itself — a block
// can clip its own overflow or paint its own opaque background (`pre`
// code blocks), which silently hides a highlight painted on the block.
function getReadingLineOverlay() {
  if (!readingLineOverlay) {
    readingLineOverlay = document.createElement("div");
    readingLineOverlay.className = "reading-line-overlay";
    el("doc").prepend(readingLineOverlay);
  }
  return readingLineOverlay;
}
function clearReadingLine() {
  if (readingLineOverlay) readingLineOverlay.style.display = "none";
  readingLineEl = null;
}
function updateReadingLine() {
  readingLineQueued = false;
  if (el("doc").hidden) return clearReadingLine();
  // Margin notes reposition on every tick regardless of whether the
  // reading-line block itself changed below (scrolling moves them
  // continuously; the reading line only jumps between blocks).
  updateAnnotationPlus();
  repositionNotes();
  const best = currentReadingBlock();
  if (best === readingLineEl) return;
  if (best) {
    const overlay = getReadingLineOverlay();
    const blockRect = best.getBoundingClientRect();
    const docRect = el("doc").getBoundingClientRect();
    overlay.style.top = `${blockRect.top - docRect.top}px`;
    overlay.style.height = `${blockRect.height}px`;
    overlay.style.display = "block";
    readingLineEl = best;
  } else {
    clearReadingLine();
  }
  // The bar's "asking about" label follows the line whenever it isn't
  // pinned to an explicit selection.
  if (!pendingSelection) renderAskContext();
}
function scheduleReadingLine() {
  if (readingLineQueued) return;
  readingLineQueued = true;
  requestAnimationFrame(updateReadingLine);
}
addEventListener("scroll", scheduleReadingLine, { passive: true });
addEventListener("resize", scheduleReadingLine);

// Builds a §4.3 Anchor from the live DOM Selection: the block id of
// the closest ancestor inside #prose, plus an exact quote (with a
// little surrounding context to disambiguate repeats) when text is
// actually selected — the same contract `core::anchor` resolves
// server-side. Also resolves which node owns that block (§S8: could
// be a spliced sub-node, not always the top-level `state.nodeId`).
function anchorFromSelection() {
  const sel = window.getSelection();
  if (!sel || sel.isCollapsed || sel.rangeCount === 0) return null;
  const node = sel.getRangeAt(0).commonAncestorContainer;
  const el0 = node.nodeType === 1 ? node : node.parentElement;
  const block = el0 && el0.closest("[data-block-id]");
  if (!block || !el("nodeSections").contains(block)) return null;
  const exact = sel.toString();
  if (!exact.trim()) return null;
  const text = block.textContent || "";
  const start = text.indexOf(exact);
  const quote = { exact };
  if (start >= 0) {
    const prefix = text.slice(Math.max(0, start - 20), start);
    const suffix = text.slice(start + exact.length, start + exact.length + 20);
    if (prefix) quote.prefix = prefix;
    if (suffix) quote.suffix = suffix;
  }
  const owner = block.closest("[data-node-id]");
  return {
    anchor: { block_id: block.dataset.blockId, quote },
    nodeId: owner ? owner.dataset.nodeId : state.nodeId,
  };
}

// --- The ask bar ------------------------------------------------------
// The selection is captured the moment it is made, not when Send is
// clicked: focusing the textarea collapses the document selection, so
// reading it at submit time would always find nothing. `pendingSelection`
// holds `{anchor, nodeId, quote}` until the question is sent or dropped.
let pendingSelection = null;

function showAskBar() {
  if (!askEligible) return;
  // Not while the sidebar is out: they are the two things that overlay the
  // document, they meet at the bottom-left corner, and the sidebar is a
  // navigation gesture — showing a question box in the middle of it asks
  // about a passage the learner has stopped looking at.
  if (el("sidebar").classList.contains("open")) return;
  el("askBar").classList.add("open");
  renderAskContext();
}
// `force` closes even while focused/typed-in — used when the document
// itself goes away (node switch, generation start).
function hideAskBar(force) {
  if (!force && askBarSticky()) return;
  el("askBar").classList.remove("open");
  el("askStatus").textContent = "";
  if (force) {
    el("askInput").value = "";
    autoGrowAskInput();
  }
}
// The bar refuses to slide away while it holds something the learner
// would lose: focus, half-typed text, or a captured selection.
function askBarSticky() {
  return (
    el("askBar").contains(document.activeElement) ||
    el("askInput").value.trim() !== "" ||
    pendingSelection !== null
  );
}

// What the question will be anchored to, shown above the textbox so the
// anchor is never a guess: the selected quote, or the reading line.
function renderAskContext() {
  const box = el("askContext");
  if (pendingSelection) {
    box.innerHTML =
      'Asking about: <span class="ask-quote">“' +
      escapeHtml(truncate(pendingSelection.quote, 90)) +
      '”</span>';
    return;
  }
  const line = currentReadingBlockId();
  box.textContent = line
    ? "Asking about the line you're on: " + truncate(line.text.trim(), 90)
    : "Asking about this page";
}

function truncate(s, n) {
  s = (s || "").replace(/\s+/g, " ").trim();
  return s.length > n ? s.slice(0, n - 1) + "…" : s;
}

// A selection inside the prose reveals the bar and pins the anchor to
// it. `selectionchange` (not `mouseup`) so keyboard selection and
// double-click-drag both work.
document.addEventListener("selectionchange", () => {
  if (!askEligible) return;
  // Selecting inside the bar's own textarea must not repoint the anchor.
  if (el("askBar").contains(document.activeElement)) return;
  const r = anchorFromSelection();
  if (r) {
    pendingSelection = {
      anchor: r.anchor,
      nodeId: r.nodeId,
      quote: r.anchor.quote.exact,
    };
    showAskBar();
  } else if (pendingSelection && !el("askInput").value.trim()) {
    // Selection dropped and nothing typed yet: fall back to the reading
    // line rather than keeping a stale quote.
    pendingSelection = null;
    renderAskContext();
  }
});

// Reaching the bottom edge of the window also reveals the bar — the
// no-selection path ("ask about the line I'm on").
document.addEventListener("pointermove", (e) => {
  if (!askEligible) return;
  if (e.clientY >= innerHeight - 48) showAskBar();
  else if (!el("askBar").contains(e.target)) hideAskBar(false);
});
el("askBar").addEventListener("pointerleave", () => hideAskBar(false));
el("askInput").addEventListener("blur", () => hideAskBar(false));
el("askInput").addEventListener("input", autoGrowAskInput);
el("askInput").addEventListener("keydown", (e) => {
  // Enter sends, Shift+Enter is a newline — the bar is a one-line
  // question box, not a document editor.
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    el("askBar").requestSubmit();
  } else if (e.key === "Escape") {
    el("askInput").value = "";
    pendingSelection = null;
    autoGrowAskInput();
    hideAskBar(true);
  }
});
function autoGrowAskInput() {
  const t = el("askInput");
  t.style.height = "auto";
  t.style.height = t.scrollHeight + "px";
}

el("askBar").addEventListener("submit", async (e) => {
  e.preventDefault();
  const text = el("askInput").value.trim();
  if (!text || !askEligible) return;
  // The selection wins; with none, the block on the reading line.
  const line = pendingSelection ? null : currentReadingBlockId();
  const anchor = pendingSelection
    ? pendingSelection.anchor
    : line && { block_id: line.blockId };
  const targetNodeId = pendingSelection
    ? pendingSelection.nodeId
    : line && line.nodeId;
  if (!anchor || !targetNodeId) return;

  el("askSend").disabled = true;
  el("askStatus").textContent = "thinking…";
  try {
    const resp = await postJson(
      `/api/documents/${state.docId}/nodes/${targetNodeId}/ask`,
      { question: text, anchor },
    );
    if (!resp.ok) throw new Error(await resp.text());
    const data = await resp.json();
    if (data.kind === "spawn") {
      // §S8: not a toggle — permanently spliced right after the
      // paragraph where the question was asked.
      const anchorEl = blockElement(data.anchor_block);
      if (anchorEl) {
        const sub = buildSubNodeWrapper(
          data.node_id,
          data.title,
          "<strong>You asked:</strong> " + escapeHtml(text),
        );
        sub.proseEl.innerHTML = sanitizeHtml(data.content_html);
        hydrateIslands(sub.proseEl, data.node_id);
        anchorEl.insertAdjacentElement("afterend", sub.wrapper);
        anchorEl.nextElementSibling.scrollIntoView({
          behavior: "smooth",
          block: "center",
        });
      }
    } else {
      // §9: the answer is an edit of the document, woven in right after
      // the passage it was asked about — not an entry appended to a
      // transcript at the end of the page.
      spliceInlineAnswer(data.anchor_block, data.body_html, true, data.id);
    }
    el("askInput").value = "";
    pendingSelection = null;
    autoGrowAskInput();
    hideAskBar(true);
  } catch (err) {
    el("askStatus").innerHTML =
      '<span class="error">' + escapeHtml(String(err)) + "</span>";
  } finally {
    el("askSend").disabled = false;
  }
});

// --- Margin annotations --------------------------------------------------
// One post-it per paragraph, at most one per paragraph for now: a
// persistent "+" follows the reading line onto whichever paragraph doesn't
// have one yet (§9). Click it, type, click away — empty cancels, non-empty
// saves via `POST .../annotate` (the same anchor contract `/ask` uses, no
// selection quote: it's always the whole paragraph on the line). An
// existing note is click-to-edit anywhere, anytime; emptying it on blur
// reverts to what was last saved rather than deleting it, so editing never
// needs its own delete endpoint.
//
// Notes are kept in `notesById`, keyed by the interaction item's stable id
// (§4.3) — hydrating the same item twice (a remount, the `done` handler's
// post-generation refetch) upserts the existing entry instead of building a
// second one, the same trap the interaction-splice bug earlier in this
// session already taught this codebase to watch for.
let notesById = new Map();
let draftAnnotation = null; // {el, textarea, blockId} — not in notesById until saved
let annotationPlusEl = null;

function annotationsLayer() {
  return el("annotationsLayer");
}

// Resolves which node actually owns a block (§S8: could be a spliced
// sub-node), the same way `currentReadingBlockId` does — freshly, at the
// moment of the request, rather than trusted from whenever the note was
// first rendered.
function ownerNodeId(blockId) {
  const target = blockElement(blockId);
  const owner = target && target.closest("[data-node-id]");
  return owner ? owner.dataset.nodeId : state.nodeId;
}

function renderNoteText(entry) {
  entry.el.innerHTML = sanitizeHtml(entry.bodyHtml);
}

function attachNoteClickHandler(entry, id) {
  entry.el.addEventListener("click", (e) => {
    if (e.target.tagName !== "TEXTAREA") startEditingNote(id);
  });
}

// Renders/repositions one already-saved annotation (called from
// `hydrateInteractions`, node.js) — the client-side counterpart to the
// server's `InteractionItem::Annotation`.
function upsertAnnotationNote(item) {
  if (!item.anchor_block) return;
  let entry = notesById.get(item.id);
  if (!entry) {
    const noteEl = document.createElement("div");
    noteEl.className = "annotation-note";
    annotationsLayer().appendChild(noteEl);
    entry = {
      el: noteEl,
      blockId: item.anchor_block,
      bodyHtml: item.body_html,
      editing: false,
    };
    notesById.set(item.id, entry);
    attachNoteClickHandler(entry, item.id);
  } else {
    entry.blockId = item.anchor_block;
    entry.bodyHtml = item.body_html;
  }
  if (!entry.editing) renderNoteText(entry);
  const target = blockElement(item.anchor_block);
  if (target) target.classList.add("annotated");
  scheduleAnnotations();
}

function startEditingNote(id) {
  const entry = notesById.get(id);
  if (!entry || entry.editing) return;
  const tmp = document.createElement("div");
  tmp.innerHTML = entry.bodyHtml;
  const plain = (tmp.textContent || "").trim();
  entry.editing = true;
  entry.el.innerHTML = "";
  const textarea = document.createElement("textarea");
  textarea.rows = 3;
  textarea.value = plain;
  entry.el.appendChild(textarea);
  textarea.focus();
  textarea.setSelectionRange(textarea.value.length, textarea.value.length);
  textarea.addEventListener("blur", () => finishEditingNote(id, plain));
  textarea.addEventListener("keydown", (e) => {
    // Escape reverts and closes, same as leaving it unchanged would.
    if (e.key === "Escape") {
      textarea.value = plain;
      textarea.blur();
    }
  });
}

async function finishEditingNote(id, previousPlain) {
  const entry = notesById.get(id);
  if (!entry) return;
  const textarea = entry.el.querySelector("textarea");
  const text = textarea ? textarea.value.trim() : "";
  entry.editing = false;
  if (!text || text === previousPlain) {
    // Empty reverts to the last saved text rather than deleting the note;
    // unchanged is simply nothing to save.
    renderNoteText(entry);
    return;
  }
  try {
    const resp = await putJson(
      `/api/documents/${state.docId}/nodes/${ownerNodeId(entry.blockId)}/annotations/${id}`,
      { body: text },
    );
    if (!resp.ok) throw new Error(await resp.text());
    const data = await resp.json();
    entry.bodyHtml = data.body_html;
  } catch (err) {
    // Best-effort: keep showing the last successfully saved text.
  }
  renderNoteText(entry);
}

function getAnnotationPlus() {
  if (!annotationPlusEl) {
    annotationPlusEl = document.createElement("button");
    annotationPlusEl.type = "button";
    annotationPlusEl.className = "annotation-plus";
    annotationPlusEl.textContent = "+";
    annotationPlusEl.title = "Add a note";
    annotationPlusEl.addEventListener("click", () => {
      const line = currentReadingBlockId();
      if (line) startNewAnnotation(line.blockId);
    });
    annotationsLayer().appendChild(annotationPlusEl);
  }
  return annotationPlusEl;
}

// Positions the "+" beside the reading-line paragraph, or hides it — no
// eligible paragraph, a draft or edit already open, or that paragraph
// already has its one note (§9: one per paragraph for now — the note
// itself is what you click to change it).
function updateAnnotationPlus() {
  if (el("doc").hidden) return;
  const plus = getAnnotationPlus();
  if (!askEligible || draftAnnotation) {
    plus.style.display = "none";
    return;
  }
  const best = currentReadingBlock();
  if (
    !best ||
    [...notesById.values()].some((n) => n.blockId === best.dataset.blockId)
  ) {
    plus.style.display = "none";
    return;
  }
  plus.style.top = `${blockMidY(best, el("doc").getBoundingClientRect())}px`;
  plus.style.display = "flex";
}

// The paragraph's own vertical centre, relative to `#doc` — the note/button
// is translated -50% (CSS) around this point, so it stays centred on the
// paragraph no matter how tall the note's own text makes it.
function blockMidY(target, docRect) {
  const rect = target.getBoundingClientRect();
  return rect.top - docRect.top + rect.height / 2;
}

function repositionNotes() {
  if (el("doc").hidden) return;
  const docRect = el("doc").getBoundingClientRect();
  for (const note of notesById.values()) {
    const target = blockElement(note.blockId);
    if (!target) {
      note.el.style.display = "none";
      continue;
    }
    note.el.style.top = `${blockMidY(target, docRect)}px`;
    note.el.style.display = "block";
  }
  if (draftAnnotation) {
    const target = blockElement(draftAnnotation.blockId);
    if (target) {
      draftAnnotation.el.style.top = `${blockMidY(target, docRect)}px`;
    }
  }
}

let annotationsQueued = false;
function scheduleAnnotations() {
  if (annotationsQueued) return;
  annotationsQueued = true;
  requestAnimationFrame(() => {
    annotationsQueued = false;
    updateAnnotationPlus();
    repositionNotes();
  });
}

function startNewAnnotation(blockId) {
  if (draftAnnotation) return;
  if ([...notesById.values()].some((n) => n.blockId === blockId)) return;
  const target = blockElement(blockId);
  if (!target) return;
  target.classList.add("annotated");
  const noteEl = document.createElement("div");
  noteEl.className = "annotation-note";
  const textarea = document.createElement("textarea");
  textarea.rows = 3;
  noteEl.appendChild(textarea);
  annotationsLayer().appendChild(noteEl);
  draftAnnotation = { el: noteEl, textarea, blockId };
  scheduleAnnotations();
  updateAnnotationPlus();
  textarea.focus();
  textarea.addEventListener("blur", finishDraftAnnotation);
  textarea.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      textarea.value = "";
      textarea.blur();
    }
  });
}

// Drops an in-progress draft without saving it (e.g. generation starting
// on another node makes reading tools ineligible mid-draft).
function cancelDraftAnnotation() {
  if (!draftAnnotation) return;
  const target = blockElement(draftAnnotation.blockId);
  if (target) target.classList.remove("annotated");
  draftAnnotation.el.remove();
  draftAnnotation = null;
}

async function finishDraftAnnotation() {
  const draft = draftAnnotation;
  if (!draft) return;
  draftAnnotation = null;
  const text = draft.textarea.value.trim();
  if (!text) {
    const target = blockElement(draft.blockId);
    if (target) target.classList.remove("annotated");
    draft.el.remove();
    updateAnnotationPlus();
    return;
  }
  try {
    const resp = await postJson(
      `/api/documents/${state.docId}/nodes/${ownerNodeId(draft.blockId)}/annotate`,
      { body: text, anchor: { block_id: draft.blockId } },
    );
    if (!resp.ok) throw new Error(await resp.text());
    const data = await resp.json();
    const entry = {
      el: draft.el,
      blockId: draft.blockId,
      bodyHtml: data.body_html,
      editing: false,
    };
    notesById.set(data.id, entry);
    attachNoteClickHandler(entry, data.id);
    renderNoteText(entry);
  } catch (err) {
    const target = blockElement(draft.blockId);
    if (target) target.classList.remove("annotated");
    draft.el.remove();
  }
  updateAnnotationPlus();
}
