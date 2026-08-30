// learnive — the left sidebar and moving through the graph: outline
// rendering (§S5), opening/revisiting a node, skip and advance, plus
// `boot()` itself (called at the end of node.js, once everything loaded).

// --- Sidebar reveal ---------------------------------------------------
// Slides in when the pointer reaches the left edge, out when it leaves.
// Focus pins it open so keyboard navigation isn't fighting the pointer.
let sidebarPinned = false;
function openSidebar() {
  el("sidebar").classList.add("open");
  // The ask bar yields to the sidebar (see `showAskBar`) — but not while it
  // holds a half-typed question, which `hideAskBar` already protects.
  hideAskBar(false);
}
function closeSidebar() {
  if (!sidebarPinned) el("sidebar").classList.remove("open");
}
// Closes regardless of focus — for navigation *from* the sidebar, where
// the click that pinned it is the same click that's done with it.
function forceCloseSidebar() {
  sidebarPinned = false;
  el("sidebar").classList.remove("open");
}
document.addEventListener("pointermove", (e) => {
  if (e.clientX <= 14) openSidebar();
});
el("sidebar").addEventListener("pointerenter", openSidebar);
el("sidebar").addEventListener("pointerleave", closeSidebar);
el("sidebar").addEventListener("focusin", () => {
  sidebarPinned = true;
  openSidebar();
});
el("sidebar").addEventListener("focusout", () => {
  sidebarPinned = false;
  // Give the next element a tick to take focus before deciding.
  setTimeout(() => {
    if (!el("sidebar").contains(document.activeElement)) closeSidebar();
  }, 0);
});

// Reopens the last document worked on, else leaves the cold start up
// (§S12). Called at the very bottom of this script, not here: it
// touches state declared further down, and running it inline would
// depend on `await` happening to yield past the rest of the file.
async function boot() {
  applyI18n(document);
  await refreshDocumentList();
  const last = localStorage.getItem(LAST_DOC_KEY);
  const pick =
    state.docs.find((d) => d.doc_id === last) || state.docs[0] || null;
  if (pick) await openDocument(pick);
}

// §S15: splits an `OutlineResp.items` response into the main line
// (`state.items`, top-level only — used for edge-based neighbor
// lazy-loading, where "how far from either end of the reading order" only
// makes sense at that granularity) and the full tree (`state.allItems`,
// everything else — skip eligibility, "next available" advance, the revisit
// hint target, and `renderOutline`'s nesting — all search the full tree
// since a decomposed prerequisite or a question-spawned elaboration can be
// the reachable/suggested node, not just a top-level item).
function setOutlineItems(items) {
  state.allItems = items;
  state.items = items.filter((it) => !it.parent_id);
}

// Renders the outline as a tree (§S15, extending §S5's gate state):
// locked items are shown but not clickable, available/demonstrated items
// jump straight to that node. A sub-node (question-spawned or a decomposed
// prerequisite) nests under whatever item its `parent_id` points to;
// dangling `parent_id` (rare — e.g. a `plan` reorder minted a fresh id for
// the item it used to point to) falls back to top-level rather than being
// dropped.
function renderOutline() {
  const byParent = new Map();
  for (const it of state.allItems || state.items) {
    const key = it.parent_id && state.allItems.some((p) => p.id === it.parent_id)
      ? it.parent_id
      : null;
    if (!byParent.has(key)) byParent.set(key, []);
    byParent.get(key).push(it);
  }
  function renderLevel(parentKey) {
    const items = byParent.get(parentKey) || [];
    return items.map((it) => renderItem(it)).join("");
  }
  function renderItem(it) {
    const cls = [
      it.id === state.currentId ? "current" : "",
      "state-" + it.state,
      it.mode === "review" ? "mode-review" : "",
      it.chapter_match_failed ? "match-failed" : "",
    ]
      .filter(Boolean)
      .join(" ");
    const badge = it.chapter_match_failed
      ? " ⚠"
      : it.state === "locked"
        ? " 🔒"
        : it.state === "demonstrated"
          ? " ✓"
          : "";
    const reviewBadge = it.mode === "review" ? " " + t("prereq.reviewBadge") : "";
    const children = renderLevel(it.id);
    return (
      '<li class="' +
      cls +
      '" data-id="' +
      it.id +
      '">' +
      '<span class="outline-row" data-id="' +
      it.id +
      '">' +
      escapeHtml(it.title) +
      reviewBadge +
      badge +
      "</span>" +
      (children ? "<ul>" + children + "</ul>" : "") +
      "</li>"
    );
  }
  // `#outline` is itself the top-level `<ol>` (§S5) — only NESTED levels
  // get their own `<ul>` wrapper (built into `renderItem` above), so a
  // top-level item is a direct `<li>` child exactly like before §S15.
  el("outline").innerHTML = renderLevel(null);
  el("outline")
    .querySelectorAll(".outline-row[data-id]")
    .forEach((row) => {
      const it = state.allItems.find((i) => i.id === row.dataset.id);
      // A chapter S27g's matching pass could not place is "locked" (no
      // resolved_page means nothing to generate) but not stuck: it opens
      // the remediation modal (remediate.js) instead of the ordinary
      // read/generate path.
      if (it && it.chapter_match_failed) {
        row.addEventListener("click", () => openChapterRemediate(it.id));
      } else if (it && it.state !== "locked") {
        row.addEventListener("click", () => openNode(it.id));
      }
    });
}

// §S5 revisit scheduler: shows the server's suggestion (the
// longest-deferred skipped node), clickable, or hides the hint when
// there's nothing to suggest.
function renderRevisitHint() {
  const hint = el("revisitHint");
  const target = state.allItems.find((it) => it.id === state.suggestedRevisit);
  if (!target) {
    hint.hidden = true;
    hint.onclick = null;
    return;
  }
  hint.hidden = false;
  hint.textContent = t("revisit.hint", target.title);
  hint.style.cursor = "pointer";
  hint.onclick = () => openNode(target.id);
}

// Re-reads the outline's gate state from the server (§S5) — called
// after anything that can change it (advancing, skipping).
async function refreshOutline() {
  const resp = await api(`/api/documents/${state.docId}/outline`);
  if (resp.ok) {
    const data = await resp.json();
    setOutlineItems(data.items);
    state.suggestedRevisit = data.suggested_revisit || null;
    renderOutline();
    renderRevisitHint();
  }
}

// Opens an outline item non-destructively: an already-generated node
// (skipped or demonstrated) is just READ (§S5/§4.3 — regenerating it
// would clobber the append-only interaction layer); a never-generated
// one falls through to the normal streaming generation path. A section
// already mounted (visited before, or quietly pulled in by
// `maybeLoadNeighbor` below) is never re-fetched — clicking it just
// scrolls the continuous document to where it already lives (§9).
async function openNode(id, opts = {}) {
  // Navigating from the sidebar means you're done with the sidebar.
  forceCloseSidebar();
  const instant = opts.instant !== false;
  const existing = state.sections.get(id);
  if (existing && existing.el.isConnected) {
    state.currentId = id;
    renderOutline();
    scrollToSection(existing, instant);
    return;
  }
  try {
    const resp = await api(`/api/documents/${state.docId}/nodes/${id}`);
    if (resp.status === 404) {
      return generateNode(id, opts);
    }
    if (!resp.ok) throw new Error(await resp.text());
    const data = await resp.json();
    // A node whose generation stalled mid-loop (an error the SSE stream
    // could only report to the tab that was open at the time, 2026-08-17
    // live report) is on disk with partial content and no `NodeGenerated`
    // event. `prepare`'s regen guard only rejects an already-finished node,
    // so it's always safe to just retry generation here rather than render
    // the leftover partial content as if it were done — same path a
    // never-generated (404) node already takes.
    if (data.complete === false) {
      return generateNode(id, opts);
    }
    await renderExistingNode(id, data, opts);
  } catch (err) {
    // Append, never overwrite via innerHTML — this container can already
    // hold other mounted sections (§S15: a prerequisite tree can have
    // several generated nodes on screen at once), and replacing its
    // innerHTML would wipe them all out from under the user for one
    // node's failure.
    const rec = state.sections.get(state.currentId);
    const target = rec ? rec.controls : el("nodeSections");
    const p = document.createElement("p");
    p.className = "error";
    p.textContent = t("open.failed") + String(err);
    target.appendChild(p);
  }
}

// The document carries half a screen of slack above it so the first
// block can reach the reading line (§9, `#doc` in app.css) — only
// relevant when nothing has been mounted into `#nodeSections` yet (a
// document whose outline exists but whose first node was never
// generated, §S12). Once a section exists, `scrollToSection` (node.js)
// is what "opening a node" actually means.
function parkAtDocumentTop() {
  const doc = el("doc");
  if (doc.hidden) return;
  window.scrollTo({ top: 0, behavior: "auto" });
}

// Renders an already-generated node read back from the server into its
// section, then scrolls to it (instant by default — "the view arriving,
// not moving"; `advanceAfterGrading` asks for smooth instead, §9).
async function renderExistingNode(id, data, opts = {}) {
  state.currentId = id;
  state.nodeId = id;
  renderOutline();
  const rec = await mountExistingSection(id, data);
  scrollToSection(rec, opts.instant !== false);
  scheduleReadingLine();
  armEdgeLoading();
  return rec;
}

// Shows the "skip" control (§S5, "botão pular") only when there's
// another reachable node to skip to — never for a linear doc's last
// (or only) available item.
function renderSkipControl(rec) {
  // §S15: search the full tree, not just the main line — on a
  // prerequisite-gated document every main-line item can be locked while
  // `currentId` sits on a sub-node, which used to hide this control entirely.
  const other = state.allItems.find(
    (it) => it.id !== state.currentId && it.state !== "locked",
  );
  if (!other) return;
  const btn = document.createElement("button");
  btn.type = "button";
  btn.textContent = t("skip.button");
  btn.addEventListener("click", skipCurrentNode);
  rec.controls.appendChild(btn);
}

async function skipCurrentNode() {
  if (!state.currentId) return;
  const skippedId = state.currentId;
  const rec = state.sections.get(skippedId);
  try {
    const resp = await postJson(
      `/api/documents/${state.docId}/nodes/${skippedId}/skip`,
      {},
    );
    if (!resp.ok) throw new Error(await resp.text());
    const data = await resp.json();
    setOutlineItems(data.items);
    state.suggestedRevisit = data.suggested_revisit || null;
    renderOutline();
    renderRevisitHint();
    // §S15: same full-tree search as renderSkipControl above.
    const next = state.allItems.find(
      (it) => it.id !== skippedId && it.state !== "locked",
    );
    if (next) {
      openNode(next.id);
    } else if (rec) {
      rec.controls.innerHTML = '<p class="muted">' + t("skip.none") + "</p>";
    }
  } catch (err) {
    (rec ? rec.result : el("nodeSections")).innerHTML =
      '<span class="error">' +
      t("skip.failed") +
      escapeHtml(String(err)) +
      "</span>";
  }
}

// --- Lazy neighbor-loading (§9) -----------------------------------------
// On a big document, boot only mounts the resume node — not the whole
// graph. As the reader nears either edge of what's currently mounted, pull
// in the next already-generated outline neighbor in that direction so the
// document keeps reading as continuous rather than stopping short. Never
// generates: an unreached neighbor (404, or gated `locked`) just ends the
// lazy-load in that direction — triggering generation on its own would
// spend the learner's money (BYOK) for a node they haven't asked for.
let edgeObserver = null;
const neighborLoadInFlight = new Set();

function armEdgeLoading() {
  if (edgeObserver) edgeObserver.disconnect();
  const sections = [...el("nodeSections").children].filter((c) =>
    c.classList.contains("node-section"),
  );
  if (sections.length === 0) return;
  const first = sections[0];
  const last = sections[sections.length - 1];
  edgeObserver = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        if (!entry.isIntersecting) continue;
        if (entry.target === first) maybeLoadNeighbor(-1);
        if (entry.target === last) maybeLoadNeighbor(1);
      }
    },
    { rootMargin: "600px 0px 600px 0px", threshold: 0 },
  );
  edgeObserver.observe(first);
  if (last !== first) edgeObserver.observe(last);
}

async function maybeLoadNeighbor(dir) {
  const mountedIndices = [...state.sections.values()]
    .filter((s) => s.el.isConnected)
    .map((s) => state.items.findIndex((it) => it.id === s.nodeId))
    .filter((i) => i >= 0);
  if (mountedIndices.length === 0) return;
  const edgeIdx =
    dir < 0 ? Math.min(...mountedIndices) : Math.max(...mountedIndices);
  const neighbor = state.items[edgeIdx + dir];
  if (!neighbor || neighbor.state === "locked") return;
  if (state.sections.has(neighbor.id)) return;
  if (neighborLoadInFlight.has(neighbor.id)) return;
  neighborLoadInFlight.add(neighbor.id);
  try {
    const resp = await api(`/api/documents/${state.docId}/nodes/${neighbor.id}`);
    if (resp.ok) {
      await mountExistingSection(neighbor.id, await resp.json());
      scheduleReadingLine();
      armEdgeLoading();
    }
    // A 404 here just means that neighbor hasn't been generated yet —
    // nothing to load, lazy-loading in this direction stops.
  } catch (err) {
    // Best-effort background load; a failure just leaves that neighbor
    // unmounted for now.
  } finally {
    neighborLoadInFlight.delete(neighbor.id);
  }
}
