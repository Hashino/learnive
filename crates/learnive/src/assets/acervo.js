// learnive — S27f: the acervo gate report ("what's missing"), PDF<->item
// manual matching, and TOC confirmation. Surfaces the backend already built
// in S27c/S27d/S27e (api/acervo.rs) — this file adds no policy of its own.
//
// Deliberately NOT a blocking gate (S27h's job, out of scope here): this
// screen opens (1) right after a document with book/article sources is
// created, as a courtesy stop between confirmation and the first node's
// generation — it auto-advances into node generation the moment every item
// has passed the automatic check, no button to click (2026-08-29 UX
// request: "a aplicação deveria automaticamente prosseguir para a geração
// do conteúdo"; before that, only "Re-check"/"Resolve match" are
// available), and a broken check silently falls through to the old
// behavior (`continueFromAcervoGate`'s catch branch) rather than stalling
// the whole flow; and (2) any time later, from the sidebar `#acervoBtn`, in
// "review" mode (a "Close" button, no auto-advance — this mode is a
// deliberate look, not a step the user is waiting to get past).
// Global-scope functions (no IIFE — `documents.js`'s `createLivingDocument`
// calls `openAcervoGate` directly), same convention as
// `documents.js`/`outline.js`.
//
// Progress (2026-08-29, live UX request: "a tela de checando acervo
// deveria reportar progresso") streams over SSE from `GET .../acervo` (see
// `api/acervo.rs`'s doc comment for the wire event sequence: `items` →
// any number of `phase` → `report` → `done`) instead of one blocking
// request — `renderAcervoChecking`/`setAcervoItemPhase` paint the live
// per-item status, and `renderAcervoReport` (unchanged) replaces that list
// wholesale once the terminal `report` event arrives.

// { mode: "coldstart" | "review", docId, continueNodeId }
let acervoState = { mode: "review", docId: null, continueNodeId: null };
let acervoTocEntries = [];
let acervoTocItemId = null;

function showAcervoView(name) {
  el("acervoReportView").hidden = name !== "report";
  el("acervoMatchesView").hidden = name !== "matches";
  el("acervoTocView").hidden = name !== "toc";
}

// `continueNodeId`: the node `createLivingDocument` would otherwise have
// generated immediately — carried through so "Continue" can still do it.
// `null` in review mode, where there is nothing pending to advance into.
function openAcervoGate(mode, docId, continueNodeId) {
  acervoState = { mode, docId, continueNodeId: continueNodeId || null };
  el("coldstart").hidden = true;
  el("doc").hidden = true;
  el("acervoGate").hidden = false;
  showAcervoView("report");
  // Re-check depends on the report that's about to load — see
  // `updateAcervoGateButtons` — so it starts hidden rather than flashing
  // visible before there's anything to act on.
  el("acervoRecheckBtn").hidden = true;
  el("acervoCloseBtn").hidden = mode !== "review";
  loadAcervoReport();
}

function continueFromAcervoGate() {
  el("acervoGate").hidden = true;
  el("doc").hidden = false;
  if (acervoState.continueNodeId) generateNode(acervoState.continueNodeId);
}

el("acervoCloseBtn").addEventListener("click", () => {
  el("acervoGate").hidden = true;
  el("doc").hidden = false;
});
el("acervoRecheckBtn").addEventListener("click", loadAcervoReport);
el("acervoMatchesBtn").addEventListener("click", () => {
  showAcervoView("matches");
  loadAcervoMatches();
});
el("acervoMatchesBackBtn").addEventListener("click", () => {
  showAcervoView("report");
  loadAcervoReport();
});
el("acervoTocBackBtn").addEventListener("click", () => {
  showAcervoView("report");
  loadAcervoReport();
});

// Re-check only makes sense once something has actually failed the
// automatic pass — a clean library has nothing to re-check (2026-08-29 live
// UX request).
function updateAcervoGateButtons(report) {
  el("acervoRecheckBtn").hidden = report.all_pass;
}

async function loadAcervoReport() {
  el("acervoStatus").textContent = t("acervo.loading");
  el("acervoList").innerHTML = "";
  el("acervoLibraryPath").innerHTML = "";
  el("acervoRecheckBtn").hidden = true;
  try {
    const resp = await api(`/api/documents/${acervoState.docId}/acervo`);
    if (!resp.ok) throw new Error(await resp.text());
    await readSse(resp, (event, data) => {
      if (event === "items") {
        const payload = JSON.parse(data);
        renderLibraryPath(payload.library_path);
        if (acervoState.mode === "coldstart" && payload.items.length === 0) {
          // Nothing to check on this reading list (no book/article sources
          // yet) — never show an empty gate, proceed exactly as before
          // this slice existed.
          continueFromAcervoGate();
          return;
        }
        el("acervoStatus").textContent = t("acervo.checking");
        renderAcervoChecking(payload.items);
      } else if (event === "phase") {
        const payload = JSON.parse(data);
        setAcervoItemPhase(payload.item_id, payload.phase);
      } else if (event === "report") {
        const report = JSON.parse(data);
        if (acervoState.mode === "coldstart" && report.all_pass) {
          // Everything the reading list needs is already in the library —
          // nothing for the user to act on, so don't make them click
          // through a screen that's already green (2026-08-29 UX request).
          continueFromAcervoGate();
          return;
        }
        el("acervoStatus").textContent = report.all_pass ? t("acervo.allGood") : "";
        renderAcervoReport(report);
        refreshAcervoMatchesBtn();
        updateAcervoGateButtons(report);
      } else if (event === "error") {
        throw new Error(data);
      }
    });
  } catch (err) {
    if (acervoState.mode === "coldstart") {
      // Informational only in this slice (S27h makes it a real gate) — a
      // broken check must never block generation.
      continueFromAcervoGate();
      return;
    }
    el("acervoStatus").innerHTML =
      '<span class="error">' + t("acervo.error") + escapeHtml(String(err)) + "</span>";
    // Something clearly needs another look — never strand the user with no
    // way to retry.
    el("acervoRecheckBtn").hidden = false;
  }
}

// The live-progress list — one row per expected item, filled in with the
// item's current check phase as `phase` events arrive. Replaced wholesale
// by `renderAcervoReport` once the terminal `report` event lands.
function renderAcervoChecking(items) {
  const list = el("acervoList");
  list.innerHTML = "";
  for (const item of items) {
    const li = document.createElement("li");
    li.className = "acervo-item";
    li.dataset.itemId = item.item_id;

    const title = document.createElement("div");
    title.className = "acervo-item-title";
    title.textContent = item.title;
    li.appendChild(title);

    const phase = document.createElement("div");
    phase.className = "muted acervo-item-phase";
    phase.textContent = t("acervo.phase.queued");
    li.appendChild(phase);

    list.appendChild(li);
  }
}

function setAcervoItemPhase(itemId, phase) {
  const li = el("acervoList").querySelector(
    `li[data-item-id="${CSS.escape(itemId)}"]`,
  );
  if (!li) return;
  const phaseEl = li.querySelector(".acervo-item-phase");
  if (phaseEl) phaseEl.textContent = t("acervo.phase." + phase);
}

// Shows "Resolve match" only when there's actually something to resolve —
// it used to be shown unconditionally, so a user with a clean library saw a
// button that opened an empty "Nothing needs manual matching right now."
// screen every time (bug reported live, 2026-08-29). Needs its own
// `/acervo/matches` fetch: "ambiguous"/"unmatched" isn't derivable from the
// main report's data, `match_report` (source/acervo.rs) is a second full
// library scan on top of the one `loadAcervoReport` already ran. Accepted
// cost — this screen is already documented as on-demand, CPU-bound, no
// caching (`api/acervo.rs`'s own module doc); doubling it is not a new
// class of cost, and a wrong-but-safe fallback (leave the button visible)
// covers a failed check rather than hiding something the user might need.
async function refreshAcervoMatchesBtn() {
  const btn = el("acervoMatchesBtn");
  try {
    const resp = await api(`/api/documents/${acervoState.docId}/acervo/matches`);
    if (!resp.ok) throw new Error(await resp.text());
    const data = await resp.json();
    btn.hidden = data.ambiguous.length === 0 && data.unmatched_files.length === 0;
  } catch {
    // Unknown is not the same as "not needed" — default to showing it.
    btn.hidden = false;
  }
}

// Shows the absolute `<data>/library/` path a missing PDF must be dropped
// into — the report used to say "Missing" with no indication of where to
// put the file (bug reported live, 2026-08-29). A `file://` link is offered
// on a best-effort basis: most browsers refuse to navigate to `file://`
// from an `http(s)://` origin (this app's own origin, since §3 runs it as a
// real local HTTP server, not a webview), so it may silently do nothing —
// the selectable/copyable path text next to it is the reliable fallback,
// never the other way around.
function renderLibraryPath(path) {
  const container = el("acervoLibraryPath");
  container.innerHTML = "";
  if (!path) return;

  const label = document.createElement("span");
  label.textContent = t("acervo.libraryPath.label");
  container.appendChild(label);

  const link = document.createElement("a");
  link.href = "file://" + path;
  link.textContent = path;
  link.className = "acervo-library-path-link";
  container.appendChild(link);

  const copyBtn = document.createElement("button");
  copyBtn.type = "button";
  copyBtn.className = "acervo-copy-path-btn";
  copyBtn.textContent = t("acervo.libraryPath.copy");
  copyBtn.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(path);
      const original = t("acervo.libraryPath.copy");
      copyBtn.textContent = t("acervo.libraryPath.copied");
      setTimeout(() => {
        copyBtn.textContent = original;
      }, 1500);
    } catch {
      // Clipboard API unavailable/denied (e.g. insecure context) — the
      // path text next to this button is still selectable by hand.
    }
  });
  container.appendChild(copyBtn);
}

function renderAcervoReport(report) {
  const list = el("acervoList");
  list.innerHTML = "";
  if (report.items.length === 0) {
    const li = document.createElement("li");
    li.className = "muted";
    li.textContent = t("acervo.empty");
    list.appendChild(li);
    return;
  }
  for (const item of report.items) {
    const li = document.createElement("li");
    li.className = "acervo-item " + (item.passes ? "acervo-pass" : "acervo-fail");

    const title = document.createElement("div");
    title.className = "acervo-item-title";
    title.textContent = item.title;
    const badge = document.createElement("span");
    badge.className = "acervo-badge " + (item.presence === "found" ? "found" : "missing");
    badge.textContent =
      item.presence === "found" ? t("acervo.status.found") : t("acervo.status.missing");
    title.appendChild(badge);
    li.appendChild(title);

    const bits = [];
    if (item.filename) bits.push(t("acervo.filename", item.filename));
    if (item.identity === "mismatch") {
      bits.push(t("acervo.identityMismatch", item.identity_reason || ""));
    }
    if (item.text_layer === "no_text") bits.push(t("acervo.noTextLayer"));
    if (bits.length) {
      const details = document.createElement("div");
      details.className = "muted acervo-item-details";
      details.textContent = bits.join(" · ");
      li.appendChild(details);
    }

    if (item.presence === "found" && item.needs_toc_confirmation) {
      const actions = document.createElement("div");
      actions.className = "acervo-item-actions";
      const btn = document.createElement("button");
      btn.type = "button";
      btn.textContent = t("acervo.reviewToc");
      btn.addEventListener("click", () => openAcervoToc(item.item_id));
      actions.appendChild(btn);
      li.appendChild(actions);
    }

    list.appendChild(li);
  }
}

// --- Matching screen -----------------------------------------------------

async function loadAcervoMatches() {
  el("acervoMatchesStatus").textContent = t("acervo.loading");
  el("acervoAmbiguousList").innerHTML = "";
  el("acervoUnmatchedList").innerHTML = "";
  try {
    const resp = await api(`/api/documents/${acervoState.docId}/acervo/matches`);
    if (!resp.ok) throw new Error(await resp.text());
    const data = await resp.json();
    el("acervoMatchesStatus").textContent = "";
    renderAcervoMatches(data);
  } catch (err) {
    el("acervoMatchesStatus").innerHTML =
      '<span class="error">' + t("acervo.error") + escapeHtml(String(err)) + "</span>";
  }
}

function renderAcervoMatches(data) {
  const amb = el("acervoAmbiguousList");
  amb.innerHTML = "";
  if (data.ambiguous.length === 0) {
    const p = document.createElement("p");
    p.className = "muted";
    p.textContent = t("acervo.matches.none");
    amb.appendChild(p);
  }
  for (const item of data.ambiguous) {
    const box = document.createElement("div");
    box.className = "acervo-ambiguous-item";
    const h = document.createElement("h3");
    h.textContent = item.title;
    box.appendChild(h);
    if (item.manual_match) {
      const cur = document.createElement("p");
      cur.className = "muted";
      cur.textContent = t("acervo.matches.current", item.manual_match);
      box.appendChild(cur);
    }
    const ul = document.createElement("ul");
    for (const cand of item.candidates) {
      const li = document.createElement("li");
      const label = document.createElement("span");
      const confidence =
        cand.confidence === "strong"
          ? t("acervo.matches.confidenceStrong")
          : t("acervo.matches.confidenceWeak");
      label.textContent = `${cand.filename} (${confidence})`;
      const btn = document.createElement("button");
      btn.type = "button";
      btn.textContent = t("acervo.matches.choose");
      btn.disabled = item.manual_match === cand.filename;
      btn.addEventListener("click", () => chooseAcervoMatch(item.item_id, cand.filename));
      li.append(label, btn);
      ul.appendChild(li);
    }
    box.appendChild(ul);
    amb.appendChild(box);
  }

  const unmatchedList = el("acervoUnmatchedList");
  unmatchedList.innerHTML = "";
  for (const filename of data.unmatched_files) {
    const li = document.createElement("li");
    li.textContent = filename;
    unmatchedList.appendChild(li);
  }
}

async function chooseAcervoMatch(itemId, filename) {
  el("acervoMatchesStatus").textContent = t("status.saving");
  try {
    const resp = await postJson(`/api/documents/${acervoState.docId}/acervo/matches`, {
      item_id: itemId,
      filename,
    });
    if (!resp.ok) throw new Error(await resp.text());
    el("acervoMatchesStatus").textContent = t("acervo.matches.saved");
    await loadAcervoMatches();
  } catch (err) {
    el("acervoMatchesStatus").innerHTML =
      '<span class="error">' + t("acervo.matches.error") + escapeHtml(String(err)) + "</span>";
  }
}

// --- TOC confirmation screen -----------------------------------------------

function acervoTocSourceLabel(source) {
  switch (source) {
    case "embedded":
      return t("acervo.toc.sourceEmbedded");
    case "heuristic":
      return t("acervo.toc.sourceHeuristic");
    case "confirmed":
      return t("acervo.toc.sourceConfirmed");
    default:
      return t("acervo.toc.sourceUnavailable");
  }
}

async function openAcervoToc(itemId) {
  acervoTocItemId = itemId;
  showAcervoView("toc");
  el("acervoTocStatus").textContent = t("acervo.loading");
  acervoTocEntries = [];
  el("acervoTocEntries").innerHTML = "";
  el("acervoTocSourceNote").textContent = "";
  try {
    const resp = await api(`/api/documents/${acervoState.docId}/acervo/toc/${itemId}`);
    if (!resp.ok) throw new Error(await resp.text());
    const data = await resp.json();
    acervoTocEntries = data.entries.map((e) => ({ title: e.title, page: e.page }));
    el("acervoTocStatus").textContent = "";
    el("acervoTocSourceNote").textContent = acervoTocSourceLabel(data.source);
    renderAcervoTocEntries();
  } catch (err) {
    el("acervoTocStatus").innerHTML =
      '<span class="error">' + t("acervo.error") + escapeHtml(String(err)) + "</span>";
  }
}

function renderAcervoTocEntries() {
  const ol = el("acervoTocEntries");
  ol.innerHTML = "";
  acervoTocEntries.forEach((entry, i) => {
    const li = document.createElement("li");
    li.className = "acervo-toc-entry";

    const titleInput = document.createElement("input");
    titleInput.type = "text";
    titleInput.value = entry.title;
    titleInput.placeholder = t("acervo.toc.titlePlaceholder");
    titleInput.addEventListener("input", () => {
      acervoTocEntries[i].title = titleInput.value;
    });

    const pageInput = document.createElement("input");
    pageInput.type = "number";
    pageInput.min = "1";
    pageInput.value = entry.page != null ? entry.page : "";
    pageInput.placeholder = t("acervo.toc.pagePlaceholder");
    pageInput.addEventListener("input", () => {
      const v = pageInput.value.trim();
      acervoTocEntries[i].page = v ? parseInt(v, 10) : null;
    });

    const up = document.createElement("button");
    up.type = "button";
    up.textContent = "↑";
    up.title = t("acervo.toc.moveUp");
    up.setAttribute("aria-label", t("acervo.toc.moveUp"));
    up.disabled = i === 0;
    up.addEventListener("click", () => swapAcervoTocEntries(i, i - 1));

    const down = document.createElement("button");
    down.type = "button";
    down.textContent = "↓";
    down.title = t("acervo.toc.moveDown");
    down.setAttribute("aria-label", t("acervo.toc.moveDown"));
    down.disabled = i === acervoTocEntries.length - 1;
    down.addEventListener("click", () => swapAcervoTocEntries(i, i + 1));

    const remove = document.createElement("button");
    remove.type = "button";
    remove.textContent = "✕";
    remove.title = t("acervo.toc.remove");
    remove.setAttribute("aria-label", t("acervo.toc.remove"));
    remove.addEventListener("click", () => {
      acervoTocEntries.splice(i, 1);
      renderAcervoTocEntries();
    });

    li.append(titleInput, pageInput, up, down, remove);
    ol.appendChild(li);
  });
}

function swapAcervoTocEntries(i, j) {
  const tmp = acervoTocEntries[i];
  acervoTocEntries[i] = acervoTocEntries[j];
  acervoTocEntries[j] = tmp;
  renderAcervoTocEntries();
}

el("acervoTocAddBtn").addEventListener("click", () => {
  acervoTocEntries.push({ title: "", page: null });
  renderAcervoTocEntries();
});

el("acervoTocSaveBtn").addEventListener("click", async () => {
  const entries = acervoTocEntries
    .map((e) => ({ title: e.title.trim(), page: e.page }))
    .filter((e) => e.title);
  if (entries.length === 0) {
    el("acervoTocStatus").innerHTML =
      '<span class="error">' + t("acervo.toc.needsAtLeastOne") + "</span>";
    return;
  }
  el("acervoTocStatus").textContent = t("status.saving");
  try {
    const resp = await putJson(
      `/api/documents/${acervoState.docId}/acervo/toc/${acervoTocItemId}`,
      { entries },
    );
    if (!resp.ok) throw new Error(await resp.text());
    el("acervoTocStatus").textContent = t("acervo.toc.saved");
  } catch (err) {
    el("acervoTocStatus").innerHTML =
      '<span class="error">' + t("acervo.toc.error") + escapeHtml(String(err)) + "</span>";
  }
});

// --- Sidebar entry point (review mode, any time a document is open) ------

el("acervoBtn").addEventListener("click", () => {
  if (!state.docId) return;
  openAcervoGate("review", state.docId, null);
});
