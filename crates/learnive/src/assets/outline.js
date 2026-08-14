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
  await refreshDocumentList();
  const last = localStorage.getItem(LAST_DOC_KEY);
  const pick =
    state.docs.find((d) => d.doc_id === last) || state.docs[0] || null;
  if (pick) await openDocument(pick);
}

// Renders the outline with its gate state (§S5): locked items are
// shown but not clickable, available/demonstrated items jump straight
// to that node (not just "next" — a diamond can leave several
// available at once, even though today's outlines are all linear).
function renderOutline() {
  el("outline").innerHTML = state.items
    .map((it) => {
      const cls = [
        it.id === state.currentId ? "current" : "",
        "state-" + it.state,
      ]
        .filter(Boolean)
        .join(" ");
      const badge =
        it.state === "locked" ? " 🔒" : it.state === "demonstrated" ? " ✓" : "";
      return (
        '<li class="' +
        cls +
        '" data-id="' +
        it.id +
        '">' +
        escapeHtml(it.title) +
        badge +
        "</li>"
      );
    })
    .join("");
  el("outline")
    .querySelectorAll("li[data-id]")
    .forEach((li) => {
      const it = state.items.find((i) => i.id === li.dataset.id);
      if (it && it.state !== "locked") {
        li.addEventListener("click", () => openNode(it.id));
      }
    });
}

// §S5 revisit scheduler: shows the server's suggestion (the
// longest-deferred skipped node), clickable, or hides the hint when
// there's nothing to suggest.
function renderRevisitHint() {
  const hint = el("revisitHint");
  const target = state.items.find((it) => it.id === state.suggestedRevisit);
  if (!target) {
    hint.hidden = true;
    hint.onclick = null;
    return;
  }
  hint.hidden = false;
  hint.textContent = "↺ Consider revisiting: " + target.title;
  hint.style.cursor = "pointer";
  hint.onclick = () => openNode(target.id);
}

// Re-reads the outline's gate state from the server (§S5) — called
// after anything that can change it (advancing, skipping).
async function refreshOutline() {
  const resp = await api(`/api/documents/${state.docId}/outline`);
  if (resp.ok) {
    const data = await resp.json();
    state.items = data.items;
    state.suggestedRevisit = data.suggested_revisit || null;
    renderOutline();
    renderRevisitHint();
  }
}

// Opens an outline item non-destructively: an already-generated node
// (skipped or demonstrated) is just READ (§S5/§4.3 — regenerating it
// would clobber the append-only interaction layer); a never-generated
// one falls through to the normal streaming generation path.
async function openNode(id) {
  // Navigating from the sidebar means you're done with the sidebar.
  forceCloseSidebar();
  try {
    const resp = await api(`/api/documents/${state.docId}/nodes/${id}`);
    if (resp.status === 404) {
      return generateNode(id);
    }
    if (!resp.ok) throw new Error(await resp.text());
    await renderExistingNode(id, await resp.json());
  } catch (err) {
    el("controls").innerHTML =
      '<p class="error">could not open node: ' +
      escapeHtml(String(err)) +
      "</p>";
  }
}

// Renders an already-generated node read back from the server: frozen
// prose, the append-only interaction history, and — if not yet
// demonstrated — the still-active exercise, answerable exactly like a
// freshly generated one.
// The document carries half a screen of slack above it so the first
// block can reach the reading line (§9, `#doc` in app.css). That slack is
// scrolled past whenever a node opens: the learner asked for a node, not
// for half a screen of starfield. Instant, never smooth — this is the
// view arriving, not moving.
function parkAtDocumentTop() {
  const doc = el("doc");
  if (doc.hidden) return;
  const top = doc.getBoundingClientRect().top + window.scrollY;
  window.scrollTo({ top: Math.max(0, top - 16), behavior: "auto" });
}

async function renderExistingNode(id, data) {
  state.currentId = id;
  state.nodeId = id;
  renderOutline();
  el("prose").innerHTML = sanitizeHtml(data.content_html);
  el("prose").dataset.nodeId = id;
  hydrateIslands(el("prose"), id);
  el("result").innerHTML = "";
  el("interactions").innerHTML = "";
  await hydrateInteractions(data.interactions, el("interactions"));
  if (data.has_exercise) {
    renderExercise(id);
    el("controls").innerHTML = "";
    renderSkipControl();
  } else {
    el("exercise").innerHTML = "";
    exerciseFrame = null;
    // A demonstrated node still needs a way forward — this is the node
    // a resumed session lands on most of the time (§S12: resume opens
    // the last node that exists on disk, which is usually one that was
    // just completed), so leaving it a dead end would make reopening
    // the app look like the document had ended.
    el("controls").innerHTML =
      '<p class="muted">Already demonstrated.</p>';
    renderNextControl();
  }
  setReadingToolsEnabled(true);
  armReadToEndWatcher();
  parkAtDocumentTop();
  scheduleReadingLine();
}

// Shows the "skip" control (§S5, "botão pular") only when there's
// another reachable node to skip to — never for a linear doc's last
// (or only) available item.
function renderSkipControl() {
  const other = state.items.find(
    (it) => it.id !== state.currentId && it.state !== "locked",
  );
  if (!other) return;
  const btn = document.createElement("button");
  btn.type = "button";
  btn.textContent = "Skip for now →";
  btn.addEventListener("click", skipCurrentNode);
  el("controls").appendChild(btn);
}

// Appends the "next concept" control (or the end-of-outline note) to
// `#controls`. Shared by the advance-after-grading path and by opening
// an already-demonstrated node, so both offer the same way forward.
function renderNextControl() {
  const next = state.items.find(
    (it) => it.id !== state.currentId && it.state === "available",
  );
  if (!next) {
    const p = document.createElement("p");
    p.className = "muted";
    p.textContent = "You have completed the current outline.";
    el("controls").appendChild(p);
    return;
  }
  const btn = document.createElement("button");
  btn.type = "button";
  btn.textContent = "Next concept →";
  btn.addEventListener("click", () => openNode(next.id));
  el("controls").appendChild(btn);
}

async function skipCurrentNode() {
  if (!state.currentId) return;
  const skippedId = state.currentId;
  try {
    const resp = await postJson(
      `/api/documents/${state.docId}/nodes/${skippedId}/skip`,
      {},
    );
    if (!resp.ok) throw new Error(await resp.text());
    const data = await resp.json();
    state.items = data.items;
    state.suggestedRevisit = data.suggested_revisit || null;
    renderOutline();
    renderRevisitHint();
    const next = state.items.find(
      (it) => it.id !== skippedId && it.state !== "locked",
    );
    if (next) {
      openNode(next.id);
    } else {
      el("controls").innerHTML =
        '<p class="muted">Nothing else available yet.</p>';
    }
  } catch (err) {
    el("result").innerHTML =
      '<span class="error">skip failed: ' +
      escapeHtml(String(err)) +
      "</span>";
  }
}
