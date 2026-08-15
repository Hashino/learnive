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
  // stays its own leaf.
  const blocks = [...el("prose").querySelectorAll("[data-block-id]")].filter(
    (b) => !b.querySelector("[data-block-id]"),
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
function clearReadingLine() {
  if (readingLineEl) readingLineEl.classList.remove("reading-line");
  readingLineEl = null;
}
function updateReadingLine() {
  readingLineQueued = false;
  if (el("doc").hidden) return clearReadingLine();
  const best = currentReadingBlock();
  if (best === readingLineEl) return;
  clearReadingLine();
  if (best) {
    best.classList.add("reading-line");
    readingLineEl = best;
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
  if (!block || !el("prose").contains(block)) return null;
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
