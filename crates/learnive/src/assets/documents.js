// learnive — how a document begins and how you get back to it: the cold
// start (§6.1/§S4) and the document list (resume, switch, rename — §S12).

// --- Cold start (§6.1/§S4) ---------------------------------------------
// One topic submission chains two internal calls — propose_objective (the
// objective anchors every later move, §5) then propose_prerequisites — with
// no up-front editing screen in between: the objective stays revisable
// later, from within the document (§5), not gated at cold start. The
// learner sees at most one confirmation before generation starts: the
// prerequisite tree (§S15), and only when it's non-empty.
let pendingTopic = null;
// Document name proposed alongside the objective (§S12), carried to
// `POST /api/documents`; the learner renames it from the sidebar.
let pendingName = "";
let pendingObjectiveText = "";
let pendingPrereqTree = [];

el("startForm").addEventListener("submit", async (e) => {
  e.preventDefault();
  const topic = el("topic").value.trim();
  if (!topic) return;
  pendingTopic = topic;
  el("startForm").hidden = true;
  el("startStatus").textContent = t("status.objective");
  try {
    const objResp = await postJson("/api/objective/propose", { topic });
    if (!objResp.ok) throw new Error(await objResp.text());
    const objData = await objResp.json();
    pendingName = objData.title || "";
    pendingObjectiveText = objData.text;

    el("startStatus").textContent = t("status.curriculum");
    const prereqResp = await postJson("/api/prerequisites/propose", {
      topic: pendingTopic,
      objective_text: pendingObjectiveText,
    });
    if (!prereqResp.ok) throw new Error(await prereqResp.text());
    const prereqData = await prereqResp.json();
    el("startStatus").textContent = "";
    if (!prereqData.tree || prereqData.tree.length === 0) {
      await createLivingDocument(pendingObjectiveText, []);
      return;
    }
    pendingPrereqTree = prereqData.tree;
    initPrereqActions(pendingPrereqTree);
    renderPrereqTree();
    el("prereqConfirm").hidden = false;
  } catch (err) {
    el("startForm").hidden = false;
    el("startStatus").innerHTML =
      '<span class="error">' + t("error.failed") + escapeHtml(String(err)) + "</span>";
  }
});

el("prereqBackBtn").addEventListener("click", () => {
  el("prereqConfirm").hidden = true;
  el("startForm").hidden = false;
});

el("prereqConfirmBtn").addEventListener("click", async () => {
  await createLivingDocument(pendingObjectiveText, pendingPrereqTree);
});

// §S15 toggle tree: every node defaults to what the server suggested
// (`learn` — never seen anywhere; `review` — already `Demonstrated` in
// another document) and is freely re-toggleable afterwards, even inside a
// branch that just got cascaded (§S15 co-design: the parent click only
// sets the branch's default, it doesn't lock the children).
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

// `lockedBySkip`: true when an ancestor is (or was just cascaded to) `skip`
// — a whole skipped branch is "not taught", so its descendants can't be
// independently re-toggled without first un-skipping the branch. Forcing
// `node.action = "skip"` here (not just visually) keeps the data model in
// sync even for a node whose own `suggested` value came back non-skip from
// cross-document matching before any cascade touched it.
function renderPrereqNode(node, lockedBySkip) {
  if (lockedBySkip) node.action = "skip";
  const knownNote = node.known
    ? ' <span class="muted">(' +
      escapeHtml(t("prereq.knownIn", node.known.doc_name)) +
      ")</span>"
    : "";
  const childLocked = lockedBySkip || node.action === "skip";
  const childrenHtml = (node.children || []).length
    ? "<ul>" +
      node.children.map((c) => renderPrereqNode(c, childLocked)).join("") +
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
        (lockedBySkip ? " disabled" : "") +
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
    (lockedBySkip ? " prereq-locked" : "") +
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

function renderPrereqTree() {
  el("prereqTree").innerHTML = pendingPrereqTree
    .map((n) => renderPrereqNode(n, false))
    .join("");
  el("prereqTree")
    .querySelectorAll(".prereq-toggle-seg")
    .forEach((btn) => {
      btn.addEventListener("click", () => {
        const id = btn.closest(".prereq-toggle").dataset.id;
        const node = findPrereqNode(pendingPrereqTree, id);
        if (node) cascadePrereqAction(node, btn.dataset.action);
        renderPrereqTree();
      });
    });
}

async function createLivingDocument(objective_text, prerequisites) {
  el("startStatus").textContent = t("status.curriculum");
  try {
    const resp = await postJson("/api/documents", {
      topic: pendingTopic,
      objective_text,
      name: pendingName,
      prerequisites,
    });
    if (!resp.ok) throw new Error(await resp.text());
    const data = await resp.json();
    state.docId = data.doc_id;
    setOutlineItems(data.items);
    state.transcriptHtml = data.transcript_html || null;
    setCurrentDocument(data.doc_id, data.name);
    el("coldstart").hidden = true;
    el("prereqConfirm").hidden = true;
    el("doc").hidden = false;
    renderOutline();
    renderTranscriptBlock();
    showOutlinePane();
    await refreshDocumentList();
    // §S15: the main line's own first item is no longer necessarily the
    // first thing to open — a confirmed prerequisite tree can gate it, in
    // which case the first available node is a prerequisite leaf instead.
    const first = state.allItems.find((it) => it.state === "available");
    if (first) generateNode(first.id);
  } catch (err) {
    el("startStatus").innerHTML =
      '<span class="error">' + t("error.failed") + escapeHtml(String(err)) + "</span>";
  }
}

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
      trash.textContent = "🗑";
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
  renderTranscriptBlock();
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
}

// Clears everything tied to the previously open document, so switching
// documents can never leave the last one's sections on screen.
function resetNodeView() {
  state.currentId = null;
  state.nodeId = null;
  state.sections.clear();
  el("nodeSections").innerHTML = "";
  if (edgeObserver) edgeObserver.disconnect();
  el("planProposal").hidden = true;
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
  resetNodeView();
  state.items = [];
  state.allItems = [];
  renderOutline();
  showOutlinePane();
  el("doc").hidden = true;
  el("coldstart").hidden = false;
  el("startForm").hidden = false;
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
