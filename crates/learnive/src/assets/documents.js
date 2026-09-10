// learnive — how a document begins and how you get back to it: the cold
// start (§6.1/§S4) and the document list (resume, switch, rename — §S12).

// --- Cold start (§6.1/§S4) ---------------------------------------------
// One topic submission chains two internal calls — propose_objective (the
// objective anchors every later move, §5) then propose_outline — with no
// up-front editing screen in between: the objective stays revisable later,
// from within the document (§5), not gated at cold start. The learner sees
// exactly one confirmation before generation starts: the outline tree
// (§S15/§S16, unified 2026-08-19) — every prerequisite root chained before
// the requested topic's own root, which comes last.
let pendingTopic = null;
// Document name proposed alongside the objective (§S12), carried to
// `POST /api/documents`; the learner renames it from the sidebar.
let pendingName = "";
let pendingObjectiveText = "";
// §S15/§S16: ONE ordered tree from `propose_outline` — every element but
// the last is a prerequisite root, the last is the requested topic's own
// decomposition. Round-tripped verbatim (as `nodes`) to `create_document`
// so the confirmed structure and the generated one match.
let pendingOutlineTree = [];

el("startForm").addEventListener("submit", async (e) => {
  e.preventDefault();
  const topic = el("topic").value.trim();
  if (!topic) return;
  pendingTopic = topic;
  el("startEntry").hidden = true;
  el("startStatus").textContent = t("status.objective");
  try {
    const objResp = await postJson("/api/objective/propose", { topic });
    if (!objResp.ok) throw new Error(await objResp.text());
    const objData = await objResp.json();
    pendingName = objData.title || "";
    pendingObjectiveText = objData.text;

    el("startStatus").textContent = t("status.curriculum");
    const outlineResp = await postJson("/api/outline/propose", {
      topic: pendingTopic,
      objective_text: pendingObjectiveText,
    });
    if (!outlineResp.ok) throw new Error(await outlineResp.text());
    const outlineData = await outlineResp.json();
    el("startStatus").textContent = "";
    pendingOutlineTree = outlineData.nodes || [];
    initPrereqActions(pendingOutlineTree);
    renderOutlineTree();
    el("prereqConfirm").hidden = false;
  } catch (err) {
    el("startEntry").hidden = false;
    el("startStatus").innerHTML =
      '<span class="error">' + t("error.failed") + escapeHtml(String(err)) + "</span>";
  }
});

el("prereqBackBtn").addEventListener("click", () => {
  el("prereqConfirm").hidden = true;
  el("startEntry").hidden = false;
});

el("prereqConfirmBtn").addEventListener("click", async () => {
  await createLivingDocument(pendingObjectiveText, pendingOutlineTree);
});

// §S15 toggle tree: every node defaults to what the server suggested
// (`learn` — never seen anywhere; `review` — already `Demonstrated` in
// another document) and is freely re-toggleable afterwards, even inside a
// branch that just got cascaded (§S15 co-design: the parent click only
// sets the branch's default, it doesn't lock the children). The last
// top-level node (the requested topic's own root) is forced to "learn" at
// render time regardless of what this sets, since it's locked.
function initPrereqActions(nodes) {
  for (const n of nodes) {
    n.action = n.suggested;
    initPrereqActions(n.children || []);
  }
}

function cascadePrereqAction(node, action) {
  node.action = action;
  for (const c of node.children || []) cascadePrereqAction(c, action);
}

function findPrereqNode(nodes, id) {
  for (const n of nodes) {
    if (n.id === id) return n;
    const found = findPrereqNode(n.children || [], id);
    if (found) return found;
  }
  return null;
}

// `lockedAction`: "learn" for the requested topic's own root and everything
// under it (it's what was asked for, so it can't be skipped/reviewed), or
// "skip" for any node under an ancestor that is (or was just cascaded to)
// `skip` — a whole skipped branch is "not taught", so its descendants can't
// be independently re-toggled without first un-skipping the branch. `null`
// means freely toggleable. Forcing `node.action` here (not just visually)
// keeps the data model in sync even for a node whose own `suggested` value
// came back different from cross-document matching before any lock/cascade
// touched it.
function renderOutlineNode(node, lockedAction) {
  if (lockedAction) node.action = lockedAction;
  const knownNote = node.known
    ? ' <span class="muted">(' +
      escapeHtml(t("prereq.knownIn", node.known.doc_name)) +
      ")</span>"
    : "";
  const childLocked = lockedAction || (node.action === "skip" ? "skip" : null);
  const childrenHtml = (node.children || []).length
    ? "<ul>" +
      node.children.map((c) => renderOutlineNode(c, childLocked)).join("") +
      "</ul>"
    : "";
  const segments = ["skip", "review", "learn"]
    .map(
      (a) =>
        '<button type="button" class="prereq-toggle-seg' +
        (node.action === a ? " active" : "") +
        '" data-action="' +
        a +
        '" aria-pressed="' +
        (node.action === a) +
        '"' +
        (lockedAction ? " disabled" : "") +
        ">" +
        t("prereq.action." + a) +
        "</button>",
    )
    .join("");
  return (
    '<li data-id="' +
    node.id +
    '">' +
    '<div class="prereq-row' +
    (lockedAction ? " prereq-locked" : "") +
    '">' +
    '<span class="prereq-title">' +
    escapeHtml(node.title) +
    knownNote +
    "</span>" +
    '<div class="prereq-toggle" data-id="' +
    node.id +
    '" role="group">' +
    segments +
    "</div>" +
    "</div>" +
    childrenHtml +
    "</li>"
  );
}

// Shared by cold start's own `#outlineTree` and the "what are we learning
// next?" prompt (§S15c, `node.js`'s `renderNextTopicPrompt`) — same tree,
// same toggle/cascade behavior, just a different container + backing array
// so the two flows never share mutable state.
function renderPrereqTree(containerEl, tree) {
  containerEl.innerHTML = tree
    .map((n, i) => renderOutlineNode(n, i === tree.length - 1 ? "learn" : null))
    .join("");
  containerEl.querySelectorAll(".prereq-toggle-seg").forEach((btn) => {
    btn.addEventListener("click", () => {
      const id = btn.closest(".prereq-toggle").dataset.id;
      const node = findPrereqNode(tree, id);
      if (node) cascadePrereqAction(node, btn.dataset.action);
      renderPrereqTree(containerEl, tree);
    });
  });
}

function renderOutlineTree() {
  renderPrereqTree(el("outlineTree"), pendingOutlineTree);
}

async function createLivingDocument(objective_text, nodes) {
  el("startStatus").textContent = t("status.curriculum");
  try {
    const resp = await postJson("/api/documents", {
      topic: pendingTopic,
      objective_text,
      name: pendingName,
      nodes,
    });
    if (!resp.ok) throw new Error(await resp.text());
    const data = await resp.json();
    state.docId = data.doc_id;
    setOutlineItems(data.items);
    setCurrentDocument(data.doc_id, data.name);
    el("coldstart").hidden = true;
    el("prereqConfirm").hidden = true;
    renderOutline();
    showOutlinePane();
    await refreshDocumentList();
    // §S15: the main line's own first item is no longer necessarily the
    // first thing to open — a confirmed prerequisite tree can gate it, in
    // which case the first available node is a prerequisite leaf instead.
    const first = state.allItems.find((it) => it.state === "available");
    // S27f: a courtesy stop before the first token generates — reveals
    // #doc and generates `first` itself once done (or immediately, if this
    // reading list has no book/article sources to check). Never blocking:
    // see acervo.js's `openAcervoGate`/`loadAcervoReport`.
    openAcervoGate("coldstart", data.doc_id, first ? first.id : null);
  } catch (err) {
    el("startStatus").innerHTML =
      '<span class="error">' + t("error.failed") + escapeHtml(String(err)) + "</span>";
  }
}

// --- Manual cold start (2026-09-09): pick from the library ------------
// The second cold-start path: instead of the model proposing a reading
// list, the learner picks works straight from the local library, orders
// them, marks the skip/review/learn disposition, and the outline is
// exactly that selection. Zero model calls anywhere in this path — even
// the chapter lists come from data already on disk (a user-confirmed TOC
// or the PDF's own bookmarks; `GET /api/library/{hash}/toc`).
//
// The listing itself streams (SSE): a cold pdftext cache extracts roughly
// one book per minute on a real library, so rows appear as they are read
// and selection can start while the whale is still extracting.
let manualLibrary = [];
// hashes of the checked works, in click order (the initial study order)
let manualSelected = [];
let manualScanning = false;
// The confirmed selection as ConfirmedNode-shaped nodes (plus client-only
// fields, stripped in `manualNodeToPayload` before the create call).
let manualTree = [];

// SSE payloads are JSON-encoded twice (grading.rs's `sse_frame` serializes
// its `&str` as a JSON string) — unwrap the inner layer like acervo.js does.
function manualPayload(data) {
  return typeof data === "string" ? JSON.parse(data) : data;
}

el("manualStartBtn").addEventListener("click", async () => {
  el("startEntry").hidden = true;
  el("libraryPicker").hidden = false;
  if (!manualLibrary.length) await loadLibrary();
});

async function loadLibrary() {
  if (manualScanning) return;
  manualScanning = true;
  el("libraryRecheckBtn").disabled = true;
  manualLibrary = [];
  renderLibraryList();
  const progress = el("libraryProgress");
  progress.hidden = false;
  progress.textContent = t("manual.loading");
  let total = null;
  try {
    const resp = await api("/api/library");
    if (!resp.ok) throw new Error(await resp.text());
    await readSse(resp, (event, data) => {
      if (event === "start") {
        const started = manualPayload(data);
        total = started.total;
        // Same path indicator (with copy button) the acervo check screen
        // shows — this picker is where a user who still needs to drop a
        // PDF into the library lands first.
        renderLibraryPath(started.library_path, el("libraryPickerPath"));
      } else if (event === "entry") {
        manualLibrary.push(manualPayload(data));
        progress.textContent = total == null
          ? t("manual.loading")
          : t("manual.scanning", manualLibrary.length, total) +
            " · " + t("manual.scanningHint");
      }
      renderLibraryList();
    });
  } catch (err) {
    el("libraryList").innerHTML =
      '<li class="muted"><span class="error">' + t("error.failed") + " " + escapeHtml(String(err)) + "</span></li>";
  }
  // A re-check keeps what's still there and forgets what vanished — done
  // here rather than per-entry so a refresh never flickers the selection.
  const known = new Set(manualLibrary.map((e) => e.hash));
  manualSelected = manualSelected.filter((h) => known.has(h));
  manualScanning = false;
  el("libraryRecheckBtn").disabled = false;
  progress.hidden = true;
  renderLibraryList();
}

function libraryStem(filename) {
  return filename.replace(/\.[^.]+$/, "");
}

function librarySameText(a, b) {
  const norm = (s) => s.toLowerCase().replace(/[^a-z0-9]+/g, "");
  return norm(a) === norm(b);
}

// Metadata titles in the wild are often worse than none: the whole
// "Author - Title (year, publisher)" string, or another converter's
// leftover filename ("single.dvi" — seen in a real library). Clean what
// is recognizable and fall back to the filename stem, whose
// "Author - Title" shape lets the author prefix move to the meta line.
function libraryDisplayTitle(e) {
  let title = (e.title || "").trim();
  if (/^[^/\\]+\.(pdf|dvi|djvu|epub|ps|txt|tex)$/i.test(title)) title = "";
  if (!title) {
    const stem = libraryStem(e.filename);
    const parts = stem.split(" - ");
    return (parts.length > 1 ? parts[parts.length - 1] : stem).trim();
  }
  const authors = (e.authors || "").trim();
  if (authors && title.toLowerCase().startsWith(authors.toLowerCase() + " - ")) {
    return title.slice(authors.length + 3).trim();
  }
  return title;
}

function visibleLibraryEntries() {
  const q = el("librarySearch").value.trim().toLowerCase();
  return manualLibrary
    .filter((e) => {
      if (!q) return true;
      const hay = (
        libraryDisplayTitle(e) +
        " " +
        e.title +
        " " +
        (e.authors || "") +
        " " +
        e.filename
      ).toLowerCase();
      return hay.includes(q);
    })
    .map((e) => ({ e, key: libraryDisplayTitle(e) }))
    .sort((a, b) => a.key.localeCompare(b.key, undefined, { sensitivity: "base" }))
    .map((x) => x.e);
}

function renderLibraryList() {
  const list = el("libraryList");
  if (!manualLibrary.length && !manualScanning) {
    list.innerHTML = '<li class="muted">' + escapeHtml(t("manual.empty")) + "</li>";
    updateLibraryFooter();
    return;
  }
  const chosen = new Set(manualSelected);
  const entries = visibleLibraryEntries();
  list.innerHTML = entries
    .map((e) => {
      // The filename only earns its line when it says something the
      // title doesn't — otherwise the row repeats itself twice.
      const title = libraryDisplayTitle(e);
      const showFile = !librarySameText(title, libraryStem(e.filename));
      // Unusable = too long to stay whole-work with no derivable chapter
      // tier: the acervo gate would refuse it outright, so the picker
      // refuses it first — visible, but not pickable.
      const unusable = e.toc === "unusable";
      const meta = [
        (e.authors || "").trim(),
        e.pages + "p",
        showFile ? e.filename : null,
        e.toc === "unusable"
          ? t("manual.tocUnusable")
          : e.toc === "unavailable"
            ? t("manual.tocUnavailable")
            : null,
      ]
        .filter(Boolean)
        .map(escapeHtml)
        .join(" · ");
      return (
        '<li><label class="library-row' +
        (chosen.has(e.hash) ? " selected" : "") +
        (unusable ? " disabled" : "") +
        '">' +
        '<input type="checkbox" data-hash="' +
        e.hash +
        '"' +
        (chosen.has(e.hash) ? " checked" : "") +
        (unusable ? " disabled" : "") +
        ">" +
        '<span class="library-text">' +
        '<span class="library-title">' +
        escapeHtml(title) +
        "</span>" +
        '<span class="library-meta muted">' +
        meta +
        "</span>" +
        "</span>" +
        "</label></li>"
      );
    })
    .join("");
  if (!entries.length) {
    list.innerHTML = '<li class="muted">' + escapeHtml(t("manual.noMatch")) + "</li>";
  }
  list.querySelectorAll('input[type="checkbox"]').forEach((box) => {
    box.addEventListener("change", () => {
      const hash = box.dataset.hash;
      if (box.checked) {
        if (!manualSelected.includes(hash)) manualSelected.push(hash);
      } else {
        manualSelected = manualSelected.filter((h) => h !== hash);
      }
      renderLibraryList();
    });
  });
  updateLibraryFooter();
}

function updateLibraryFooter() {
  const n = manualSelected.length;
  el("libraryCount").textContent = manualLibrary.length
    ? t("manual.selectedCount", n, manualLibrary.length)
    : "";
  el("libraryClearBtn").hidden = n === 0;
  el("libraryContinueBtn").disabled = n === 0;
  el("libraryContinueBtn").textContent = t("manual.continue") + (n ? " (" + n + ")" : "");
}

el("librarySearch").addEventListener("input", () => renderLibraryList());

el("libraryClearBtn").addEventListener("click", () => {
  manualSelected = [];
  renderLibraryList();
});

el("libraryBackBtn").addEventListener("click", () => {
  el("libraryPicker").hidden = true;
  el("startEntry").hidden = false;
});

el("libraryRecheckBtn").addEventListener("click", () => loadLibrary());

el("libraryContinueBtn").addEventListener("click", async () => {
  const picked = manualSelected
    .map((h) => manualLibrary.find((e) => e.hash === h))
    .filter(Boolean);
  manualTree = picked.map((e) => ({
    id: "w" + e.hash.slice(0, 12),
    title: e.title,
    action: "learn",
    item_type: "book",
    bibliography: {
      title: e.title,
      authors: (e.authors || "")
        .split(/[,;]\s*/)
        .map((a) => a.trim())
        .filter(Boolean),
      year: null,
      edition: null,
      identifier: null,
      kind: "book",
    },
    // verification stays null on purpose: `Verified` means checked against
    // external catalogs, and a locally present file needs no external check
    // — the acervo gate validates presence for real after creation.
    verification: null,
    chapter_number: null,
    children: [],
    // client-only, stripped before create:
    hash: e.hash,
    toc: e.toc,
  }));
  el("libraryPicker").hidden = true;
  el("manualConfirm").hidden = false;
  el("manualTree").innerHTML =
    '<li class="muted">' + escapeHtml(t("manual.loadingChapters")) + "</li>";
  // Chapter tiers come from data already on disk; a book with neither a
  // confirmed TOC nor bookmarks nor derivable openers stays whole-work
  // (`manual.tocUnavailable`) or is refused outright when too long —
  // though a refused book can't reach this flow at all, the picker
  // screen blocks selecting it.
  await Promise.all(
    manualTree.map(async (w) => {
      if (w.toc === "unavailable" || w.toc === "unusable") return;
      try {
        const resp = await api("/api/library/" + w.hash + "/toc");
        if (!resp.ok) return;
        const data = await resp.json();
        w.children = (data.entries || []).map((en, i) => ({
          id: w.id + "c" + i,
          title: en.title,
          action: "learn",
          item_type: "chapter",
          chapter_number: en.number,
          children: [],
        }));
      } catch {
        // leave the work whole — a missing chapter tier never blocks
      }
    }),
  );
  renderManualConfirm();
});

// One row of the manual confirmation tree: optional reorder arrows (works
// only — chapters keep their book's order), then the same 3-segment
// skip/review/learn toggle markup the proposed path renders
// (`renderOutlineNode`'s classes, so the styling is shared). A WORK that
// has chapter children gets NO toggle: both `skip` (cascades to the whole
// branch server-side) and `review` (materializes the work alone, children
// omitted) would silently discard the chapter choices the user just made —
// and the picker already asserted "I want this work". The chapters carry
// the choices; the work row is a container with an order.
function manualRowHtml(node, withReorder) {
  const workWithChildren = withReorder && (node.children || []).length > 0;
  const segments = workWithChildren
    ? ""
    : ["skip", "review", "learn"]
        .map(
          (a) =>
            '<button type="button" class="prereq-toggle-seg' +
            (node.action === a ? " active" : "") +
            '" data-action="' +
            a +
            '" aria-pressed="' +
            (node.action === a) +
            '">' +
            t("prereq.action." + a) +
            "</button>",
        )
        .join("");
  const reorder = withReorder
    ? '<span class="manual-reorder">' +
      '<button type="button" class="reorder-btn" data-dir="up" title="' +
      escapeHtml(t("manual.up")) +
      '" aria-label="' +
      escapeHtml(t("manual.up")) +
      '">↑</button>' +
      '<button type="button" class="reorder-btn" data-dir="down" title="' +
      escapeHtml(t("manual.down")) +
      '" aria-label="' +
      escapeHtml(t("manual.down")) +
      '">↓</button>' +
      "</span>"
    : "";
  return (
    '<div class="prereq-row">' +
    reorder +
    '<span class="prereq-title">' +
    escapeHtml(node.title) +
    "</span>" +
    '<div class="prereq-toggle" data-id="' +
    node.id +
    '" role="group">' +
    segments +
    "</div>" +
    "</div>"
  );
}

function renderManualConfirm() {
  el("manualTree").innerHTML = manualTree
    .map((w) => {
      const chapters = (w.children || []).map((c) => "<li>" + manualRowHtml(c, false) + "</li>");
      return (
        "<li>" +
        manualRowHtml(w, true) +
        (chapters.length ? "<ul>" + chapters.join("") + "</ul>" : "") +
        "</li>"
      );
    })
    .join("");
  el("manualTree").querySelectorAll(".prereq-toggle-seg").forEach((btn) => {
    btn.addEventListener("click", () => {
      const id = btn.closest(".prereq-toggle").dataset.id;
      const node = findPrereqNode(manualTree, id);
      if (node) cascadePrereqAction(node, btn.dataset.action);
      renderManualConfirm();
    });
  });
  // Array position IS the prerequisite chain (same convention as the
  // proposed list), so the arrows literally reorder the curriculum.
  el("manualTree").querySelectorAll(".reorder-btn").forEach((btn) => {
    btn.addEventListener("click", () => {
      const li = btn.closest("li");
      const id = li.querySelector(".prereq-toggle").dataset.id;
      const from = manualTree.findIndex((w) => w.id === id);
      const to = btn.dataset.dir === "up" ? from - 1 : from + 1;
      if (from < 0 || to < 0 || to >= manualTree.length) return;
      [manualTree[from], manualTree[to]] = [manualTree[to], manualTree[from]];
      renderManualConfirm();
    });
  });
}

el("manualBackBtn").addEventListener("click", () => {
  el("manualConfirm").hidden = true;
  el("libraryPicker").hidden = false;
});

// Hands the server the exact ConfirmedNode shape `create_document`
// materializes verbatim — same round-trip contract the proposed path uses,
// just built client-side. `hash` is NOT stripped: it rides along as
// `file_hash` (the one thing the picker knows that the server cannot
// re-derive); only `toc` is client-only. A
// work with chapter children is ALWAYS `learn` in the payload (the
// ChaptersProposed container): a stale `skip`/`review` here would cascade
// past the user's chapter choices — the toggle is hidden for such works,
// this normalization is the backstop.
function manualNodeToPayload(n) {
  const hasChildren = (n.children || []).length > 0;
  return {
    id: n.id,
    title: n.title,
    action: n.item_type === "book" && hasChildren ? "learn" : n.action,
    children: (n.children || []).map(manualNodeToPayload),
    item_type: n.item_type,
    bibliography: n.bibliography || undefined,
    verification: null,
    chapter_number: n.chapter_number || undefined,
    // The one field this payload adds over the proposed path's round-trip:
    // WHICH library file the work means (server persists it as a
    // manual-match pairing, so two editions of the same title never leave
    // chapter resolution ambiguous again). Chapters have no `hash`; it
    // drops out of the JSON as undefined.
    file_hash: n.hash || undefined,
  };
}

// Surviving study units in a confirmed manual payload: leaves (whole works
// and chapters) that aren't skipped. A container work contributes only what
// its children contribute — the "book learn + every chapter skip" payload
// materializes to a lone empty container, which is just the all-skip blank
// document again (caught live in the 2026-09-09 fix's own verification).
function manualLeafCount(nodes) {
  return nodes.reduce(
    (sum, n) =>
      sum +
      ((n.children || []).length
        ? manualLeafCount(n.children)
        : n.action === "skip"
          ? 0
          : 1),
    0,
  );
}

el("manualConfirmBtn").addEventListener("click", async () => {
  const nodes = manualTree.map(manualNodeToPayload);
  if (manualLeafCount(nodes) === 0) {
    el("startStatus").innerHTML =
      '<span class="error">' + escapeHtml(t("manual.nothingSelected")) + "</span>";
    return;
  }
  // topic/name/objective: there was no typed topic, so the selection IS
  // the subject. objective_text stays empty — `create_document` falls
  // back to the topic, and the objective stays revisable later (§5).
  const titles = manualTree.map((w) => w.title).join(", ");
  pendingTopic = titles.length > 300 ? titles.slice(0, 300) + "…" : titles;
  pendingName = manualTree.length === 1 ? manualTree[0].title : pendingTopic;
  pendingObjectiveText = "";
  // createLivingDocument hides all of #coldstart on success (the manual
  // screens live inside it); on error they stay visible behind the shared
  // #startStatus error line.
  await createLivingDocument(pendingObjectiveText, nodes);
});

// --- Documents: resume, switch, rename (§S12) -------------------------
// The app used to always cold-start: documents were persisted under
// `<data-dir>/<doc-id>/` from the very first slice, but nothing ever
// read them back, so every reload looked like a fresh install. Boot now
// lists them and reopens the one last worked on.

// Remembers the *chosen* document across reloads. Only a preference —
// the server's list is the source of truth, and a stale id (deleted
// directory) just falls through to the most recently touched document.
const LAST_DOC_KEY = "learnive-doc";

function setCurrentDocument(docId, name) {
  state.docId = docId;
  state.docName = name || "";
  localStorage.setItem(LAST_DOC_KEY, docId);
  el("docName").textContent = state.docName;
  // S27f: the sidebar library-check entry point only makes sense once a
  // document exists.
  el("acervoBtn").hidden = false;
  renderDocList();
}

async function refreshDocumentList() {
  try {
    const resp = await api("/api/documents");
    state.docs = resp.ok ? await resp.json() : [];
  } catch (err) {
    state.docs = [];
  }
  renderDocList();
}

function renderDocList() {
  el("docList").replaceChildren(
    ...state.docs.map((d) => {
      const li = document.createElement("li");
      if (d.doc_id === state.docId) li.className = "current";
      const name = document.createElement("span");
      name.className = "doc-name";
      name.textContent = d.name || d.topic || d.doc_id;
      const meta = document.createElement("span");
      meta.className = "doc-meta";
      meta.textContent = t("doc.count", d.demonstrated, d.total);
      const trash = document.createElement("button");
      trash.type = "button";
      trash.className = "doc-trash";
      trash.title = t("delete.title");
      trash.setAttribute("aria-label", t("delete.title"));
      // "✕", not an emoji glyph (2026-09-01 no-emoji rule) — the title/
      // aria-label carry the "delete" meaning; matches acervo.js's remove.
      trash.textContent = "✕";
      trash.addEventListener("click", (e) => {
        // The row itself opens the document — a delete must not do both.
        e.stopPropagation();
        confirmDeleteDocument(li, d);
      });
      li.append(name, meta, trash);
      li.addEventListener("click", () => openDocument(d));
      return li;
    }),
  );
}

// Deleting is irreversible and erases work, so it asks — in the row, where
// the name and the progress the learner is about to throw away are still
// on screen. A native confirm() would take that context away and put the
// decision in a dialog that says nothing but the question.
function confirmDeleteDocument(li, d) {
  if (li.querySelector(".doc-confirm")) return;
  const box = document.createElement("div");
  box.className = "doc-confirm";
  const label = document.createElement("span");
  label.textContent = t("delete.confirm", d.name || d.topic || d.doc_id);
  const yes = document.createElement("button");
  yes.type = "button";
  yes.className = "danger";
  yes.textContent = t("delete.button");
  const no = document.createElement("button");
  no.type = "button";
  no.textContent = t("delete.cancel");
  box.append(label, yes, no);
  box.addEventListener("click", (e) => e.stopPropagation());
  no.addEventListener("click", () => box.remove());
  yes.addEventListener("click", async () => {
    yes.disabled = true;
    no.disabled = true;
    try {
      const resp = await api(`/api/documents/${d.doc_id}`, { method: "DELETE" });
      if (!resp.ok) throw new Error(await resp.text());
      // Deleting the document you are reading leaves the page showing a
      // document that no longer exists — go back to a clean start.
      if (d.doc_id === state.docId) {
        localStorage.removeItem(LAST_DOC_KEY);
        location.reload();
        return;
      }
      await refreshDocumentList();
      renderDocList();
    } catch (err) {
      label.innerHTML =
        '<span class="error">' +
        t("delete.failed") +
        escapeHtml(String(err)) +
        "</span>";
      no.disabled = false;
      no.textContent = t("delete.close");
    }
  });
  li.appendChild(box);
}

// Opens a document from its summary: restores the outline and resumes
// reading at the last node that actually exists on disk.
// `resume_node_id` is server-side "last MAIN-LINE node with a file"
// (§S15: it deliberately walks main-line items only), so on a
// prerequisite-gated document it can come back null even after a
// sub-node was already generated — the main-line item stays locked
// until its prerequisites clear. So the fallback below can't assume
// "no resume_node_id" means "nothing generated": it goes through
// `openNode`, which reads-if-it-exists and only generates on a 404,
// rather than calling `generateNode` directly and risking a re-generate
// of an already-generated node (the regen guard then errors, and the
// node the user already has disappears from view).
async function openDocument(summary) {
  if (!summary) return;
  setCurrentDocument(summary.doc_id, summary.name);
  el("coldstart").hidden = true;
  el("doc").hidden = false;
  showOutlinePane();
  resetNodeView();
  await refreshOutline();
  if (summary.resume_node_id) {
    await openNode(summary.resume_node_id);
  } else if (state.allItems.length) {
    // Nothing resumable on the main line — open the first non-locked
    // item across the FULL tree (§S15: a prerequisite tree can gate the
    // main line's own first item). `openNode` itself decides whether
    // that means reading an existing node or generating a new one.
    const first = state.allItems.find((it) => it.state === "available");
    if (first) await openNode(first.id);
  } else {
    // Outline itself came back empty — genuinely nothing to show yet.
    el("nodeSections").innerHTML = "";
    parkAtDocumentTop();
  }
  // §S15c: a returning learner whose main line is already fully
  // `demonstrated` gets the same "what are we learning next?" prompt a
  // live grading would have shown (`advanceAfterGrading`, node.js) —
  // otherwise it only ever appears once, right after the grading that
  // produced it, and is gone for good on the next reload.
  if (
    state.allItems.length &&
    state.currentId &&
    !state.allItems.some((it) => it.state === "available")
  ) {
    const rec = state.sections.get(state.currentId);
    if (rec) renderNextTopicPrompt(rec.controls);
  }
}

// Clears everything tied to the previously open document, so switching
// documents can never leave the last one's sections on screen.
function resetNodeView() {
  state.currentId = null;
  state.nodeId = null;
  state.sections.clear();
  el("nodeSections").innerHTML = "";
  if (edgeObserver) edgeObserver.disconnect();
  clearReadingLine();
}

function showOutlinePane() {
  el("outlinePane").hidden = false;
  el("docsPane").hidden = true;
  el("docsBackBtn").textContent = "←";
  el("docsBackBtn").title = t("nav.allDocuments");
  el("docsBackBtn").setAttribute("aria-label", t("nav.allDocuments"));
}
function showDocsPane() {
  el("outlinePane").hidden = true;
  el("docsPane").hidden = false;
  el("docsBackBtn").textContent = "→";
  el("docsBackBtn").title = t("docs.backOutline");
  el("docsBackBtn").setAttribute("aria-label", t("docs.backOutline"));
  refreshDocumentList();
}
el("docsBackBtn").addEventListener("click", () => {
  if (el("docsPane").hidden) showDocsPane();
  else showOutlinePane();
});

el("newDocBtn").addEventListener("click", () => {
  state.docId = null;
  state.docName = "";
  localStorage.removeItem(LAST_DOC_KEY);
  el("docName").textContent = "";
  el("acervoBtn").hidden = true;
  resetNodeView();
  state.items = [];
  state.allItems = [];
  renderOutline();
  showOutlinePane();
  el("doc").hidden = true;
  el("acervoGate").hidden = true;
  el("coldstart").hidden = false;
  el("startEntry").hidden = false;
  el("libraryPicker").hidden = true;
  el("manualConfirm").hidden = true;
  el("prereqConfirm").hidden = true;
  el("startStatus").textContent = "";
  el("topic").value = "";
  el("topic").focus();
});

// Rename in place: the title becomes an input, Enter/blur saves. The
// name is a label — nothing downstream reads it — so this is a plain
// overwrite, not a §S4-style version chain.
el("docName").addEventListener("click", () => {
  if (!state.docId) return;
  // Hold the heading itself, not its id: once it is swapped out for the
  // input it is detached, and `getElementById` would find nothing to
  // put back.
  const heading = el("docName");
  const input = document.createElement("input");
  input.id = "docNameInput";
  input.value = state.docName;
  let settled = false;
  const finish = async (save) => {
    if (settled) return;
    settled = true;
    const next = input.value.trim();
    input.replaceWith(heading);
    if (!save || !next || next === state.docName) return;
    try {
      const resp = await postJson(
        `/api/documents/${state.docId}/name`,
        { name: next },
      );
      if (!resp.ok) throw new Error(await resp.text());
      const data = await resp.json();
      state.docName = data.name;
      heading.textContent = data.name;
      await refreshDocumentList();
    } catch (err) {
      heading.textContent = state.docName;
    }
  };
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      finish(true);
    } else if (e.key === "Escape") {
      finish(false);
    }
  });
  input.addEventListener("blur", () => finish(true));
  heading.replaceWith(input);
  input.focus();
  input.select();
});
