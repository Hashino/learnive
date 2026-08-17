// learnive — how a document begins and how you get back to it: the cold
// start (§6.1/§S4) and the document list (resume, switch, rename — §S12).

// --- Cold start (§6.1/§S4) ---------------------------------------------
// Two steps: propose a compact, editable objective from the raw topic,
// then confirm (possibly edited) before the outline is generated and
// the document is created — the objective anchors every later move.
let pendingTopic = null;
// Document name proposed alongside the objective (§S12), carried to
// `POST /api/documents`; the learner renames it from the sidebar.
let pendingName = "";

el("startForm").addEventListener("submit", async (e) => {
  e.preventDefault();
  const topic = el("topic").value.trim();
  if (!topic) return;
  pendingTopic = topic;
  el("startStatus").textContent = t("status.objective");
  try {
    const resp = await postJson("/api/objective/propose", { topic });
    if (!resp.ok) throw new Error(await resp.text());
    const data = await resp.json();
    pendingName = data.title || "";
    el("objectiveText").value = data.text;
    el("startStatus").textContent = "";
    el("startForm").hidden = true;
    el("objectiveConfirm").hidden = false;
  } catch (err) {
    el("startStatus").innerHTML =
      '<span class="error">' + t("error.failed") + escapeHtml(String(err)) + "</span>";
  }
});

el("objectiveBackBtn").addEventListener("click", () => {
  el("objectiveConfirm").hidden = true;
  el("startForm").hidden = false;
});

el("objectiveConfirmBtn").addEventListener("click", async () => {
  const objective_text = el("objectiveText").value.trim();
    el("startStatus").textContent = t("status.curriculum");
  try {
    const resp = await postJson("/api/documents", {
      topic: pendingTopic,
      objective_text,
      name: pendingName,
    });
    if (!resp.ok) throw new Error(await resp.text());
    const data = await resp.json();
    state.docId = data.doc_id;
    state.items = data.items;
    setCurrentDocument(data.doc_id, data.name);
    el("coldstart").hidden = true;
    el("doc").hidden = false;
    renderOutline();
    showOutlinePane();
    await refreshDocumentList();
    generateNode(state.items[0].id);
  } catch (err) {
    el("startStatus").innerHTML =
      '<span class="error">' + t("error.failed") + escapeHtml(String(err)) + "</span>";
  }
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
// reading at the last node that actually exists on disk. Never
// generates — `resume_node_id` is server-side "last node with a file",
// so reopening the app costs zero model tokens (§12.2); a document
// whose first node was never generated just shows its outline.
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
  } else if (state.items.length) {
    // No node generated yet — the document opens by generating the first
    // one, rather than parking on a placeholder waiting for a click.
    generateNode(state.items[0].id);
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
  renderOutline();
  showOutlinePane();
  el("doc").hidden = true;
  el("coldstart").hidden = false;
  el("startForm").hidden = false;
  el("objectiveConfirm").hidden = true;
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
