// learnive — the node itself: streamed generation (§6, §14), splicing
// answers and sub-nodes into the reading flow (§S8), sandboxed interactive
// islands (§4.4), and the answer → grading → remediation/advance loop
// (§8, §8.2). Last script on the page, so it is the one that boots the app.

// Builds one node's `.node-section` shell (§9): its own prose, exercise,
// read-to-end sentinel, interaction layer and controls — the continuous
// document's real unit. Registered in `state.sections` but NOT inserted
// into `#nodeSections` here; callers place it (`insertSectionInOrder`).
function buildSection(nodeId) {
  const section = document.createElement("section");
  section.className = "node-section";
  section.dataset.nodeId = nodeId;
  const prose = document.createElement("article");
  prose.className = "prose";
  const exercise = document.createElement("div");
  exercise.className = "exercise";
  // Scroll-to-end sentinel (§S6 "Ritmo"): crossing this logs the signal
  // event. Placed after the exercise so it marks the end of the node's
  // actual content, not the interaction history below.
  const sentinel = document.createElement("div");
  sentinel.className = "read-end-sentinel";
  // Append-only interaction layer (§4.3): attempts + remediation
  // accumulate here and are never overwritten within a node.
  const interactions = document.createElement("div");
  interactions.className = "interactions";
  // Transient status only (evaluating…, errors).
  const result = document.createElement("div");
  result.className = "result";
  const controls = document.createElement("div");
  controls.className = "controls";
  section.append(prose, exercise, sentinel, interactions, result, controls);
  const rec = {
    nodeId,
    el: section,
    prose,
    exercise,
    sentinel,
    interactions,
    result,
    controls,
    exerciseFrame: null,
    readEndFired: false,
    readEndObserver: null,
  };
  state.sections.set(nodeId, rec);
  return rec;
}

// Places a section's element among the other mounted sections in OUTLINE
// order, not load order (§S5's graph, plus §S8 lazy neighbor-loading in
// outline.js, mean a later node can be mounted before an earlier one).
function insertSectionInOrder(rec) {
  const idx = state.items.findIndex((it) => it.id === rec.nodeId);
  let before = null;
  let bestIdx = Infinity;
  for (const s of state.sections.values()) {
    if (s === rec || !s.el.isConnected) continue;
    const i = state.items.findIndex((it) => it.id === s.nodeId);
    if (i > idx && i < bestIdx) {
      bestIdx = i;
      before = s.el;
    }
  }
  if (before) el("nodeSections").insertBefore(rec.el, before);
  else el("nodeSections").appendChild(rec.el);
}

// Scrolls a section's top to just under the reading position — instant
// for the view "arriving" (an outline click, a freshly generated node),
// smooth for content flowing in while the learner is already reading
// (auto-advance appending the next node below the one just graded).
function scrollToSection(rec, instant) {
  if (!rec || !rec.el.isConnected) return;
  const top = rec.el.getBoundingClientRect().top + window.scrollY;
  window.scrollTo({
    top: Math.max(0, top - 16),
    behavior: instant ? "auto" : "smooth",
  });
}

// Renders an already-generated node's content into its section — used
// both for direct navigation (`openNode`/`renderExistingNode`) and for
// background neighbor-loading (`maybeLoadNeighbor` in outline.js), which
// must NOT touch `state.currentId`/`state.nodeId`: those track what the
// learner actually navigated to, not what quietly mounted nearby.
async function mountExistingSection(id, data) {
  let rec = state.sections.get(id);
  if (!rec) {
    rec = buildSection(id);
    insertSectionInOrder(rec);
  }
  rec.prose.innerHTML = sanitizeHtml(data.content_html);
  rec.prose.dataset.nodeId = id;
  hydrateIslands(rec.prose, id);
  rec.result.innerHTML = "";
  // The exercise slot's own §4.3 block id (reused from exercise_id) has to
  // be in place *before* hydrating interactions below: an old Q&A thread
  // anchored to the exercise (§S6) needs `blockElement` to find it to
  // re-splice inline, even once the node is demonstrated and the form
  // itself is gone — `exercise_block_id` now outlives demonstration for
  // exactly this reason (see its doc comment server-side).
  if (data.exercise_block_id) {
    rec.exercise.dataset.blockId = data.exercise_block_id;
  } else {
    delete rec.exercise.dataset.blockId;
  }
  rec.interactions.innerHTML = "";
  await hydrateInteractions(data.interactions, rec.interactions);
  if (data.exercise_block_id && !data.demonstrated) {
    renderExerciseInto(rec, id);
    // The exercise reads on the reading line (§9) and is a valid "ask
    // about this" anchor (§S6) like any other block, not a client-only
    // stand-in — but only while it's still live (above).
    rec.controls.innerHTML = "";
    renderSkipControl(rec);
    // Only one node has a live gradeable exercise at a time (the graph
    // gate keeps it that way in practice, §S5) — whichever section that
    // is becomes the answer-routing target, main nav or background load.
    state.nodeId = id;
  } else {
    rec.exercise.innerHTML = "";
    rec.exerciseFrame = null;
    // Deliberately NOT auto-advancing here: this is the node a resumed
    // session lands on most of the time (§S12: resume opens the last node
    // that exists on disk, which is usually one just completed), and the
    // sidebar lets a demonstrated node be reopened at any time to reread
    // it. Either case firing a generation request on its own would spend
    // the learner's money (BYOK) without them having asked for anything —
    // auto-advance only happens right after a live grading (§8, below).
    // No "already demonstrated" label either: the frozen attempt record
    // (exercise + answer + grade, §4.3) is right there in the interaction
    // layer above — the document itself says so, nothing left to add.
    rec.controls.innerHTML = "";
  }
  setReadingToolsEnabled(true);
  armReadToEndWatcher(rec);
  return rec;
}

// Finds a content block by id, anywhere in the reading flow — any
// mounted section's prose or a spliced sub-node's (§S8).
function blockElement(blockId) {
  return blockId
    ? el("nodeSections").querySelector('[data-block-id="' + blockId + '"]')
    : null;
}

// Reading depth of an element in the flow: 0 for the node's own prose,
// 1 for an answer about a paragraph, n+1 for an answer about a depth-n
// answer. Drives both the insertion point below and the indent in CSS.
function answerDepth(elm) {
  return elm && elm.dataset.depth ? Number(elm.dataset.depth) : 0;
}

// Inserts an answered question into the document at its anchor (§9).
//
// The anchor is a content block, another answer, or one paragraph of
// another answer: asking about an answer is how the learner falls into a
// rabbit hole, and the thread has to keep reading in order. So the
// insertion point is after the anchor and after everything already
// hanging off it — its own follow-ups (deeper) and the answers asked
// before this one at the same depth — but never past a sibling that
// belongs to a shallower part of the page. Anchored at a paragraph, the
// follow-up lands right under that paragraph, inside the answer it is
// about. `id` is the interaction item's stable id (§4.3): tagging it as
// a block id is what makes this answer askable as a whole, in this
// session and after a reload (its paragraphs carry their own ids, from
// the server). Falls back to the interaction panel only if the anchor is
// not on the page (a node the answer doesn't belong to).
function spliceInlineAnswer(blockId, bodyHtml, scroll, id, fallbackEl) {
  const div = document.createElement("div");
  div.className = "qa inline-answer";
  div.innerHTML = sanitizeHtml(bodyHtml);
  if (id) div.dataset.blockId = id;
  const anchorEl = blockElement(blockId);
  if (!anchorEl) {
    // The anchor's section isn't mounted (shouldn't normally happen —
    // sections are never unmounted once loaded, §9) — degrade to the end
    // of the whole document rather than lose the answer.
    (fallbackEl || el("nodeSections")).appendChild(div);
    return;
  }
  // Depth comes from the answer the anchor belongs to, not from the anchor
  // element itself: anchoring on one paragraph of an answer is still asking
  // about that answer, one level deeper.
  const owner = anchorEl.closest(".inline-answer");
  const depth = answerDepth(owner) + 1;
  div.dataset.depth = String(depth);
  div.style.setProperty("--depth", String(depth));
  let after = anchorEl;
  while (
    after.nextElementSibling &&
    after.nextElementSibling.classList.contains("inline-answer") &&
    answerDepth(after.nextElementSibling) >= depth
  ) {
    after = after.nextElementSibling;
  }
  after.insertAdjacentElement("afterend", div);
  if (scroll) div.scrollIntoView({ behavior: "smooth", block: "center" });
}

// §S8: a spawned sub-node's DOM shell — its own prose container and
// its own (initially empty) interaction container, so a question
// asked *inside* a sub-node nests exactly the same way, recursively.
function buildSubNodeWrapper(subNodeId, title, questionHtml) {
  const wrapper = document.createElement("div");
  wrapper.className = "subnode";
  wrapper.dataset.nodeId = subNodeId;
  const titleEl = document.createElement("div");
  titleEl.className = "subnode-title";
  titleEl.textContent = title || "";
  wrapper.appendChild(titleEl);
  // §7: the selection-plus-question that triggered this section is the
  // highest-signal input in the app — it must survive on the page, not
  // just live in the parent's interaction log where nothing renders it.
  if (questionHtml) {
    const qEl = document.createElement("div");
    qEl.className = "subnode-question";
    qEl.innerHTML = sanitizeHtml(questionHtml);
    wrapper.appendChild(qEl);
  }
  const proseEl = document.createElement("div");
  proseEl.className = "subnode-prose";
  const interEl = document.createElement("div");
  interEl.className = "subnode-interactions";
  wrapper.appendChild(proseEl);
  wrapper.appendChild(interEl);
  return { wrapper, proseEl, interEl };
}

// Pulls the "<p class="question">You asked: ...</p>" fragment out of a
// parent-side spawn marker's `body_html` (§S8) so a reload can show the
// same question text a live session showed — without re-parsing HTML
// by hand.
function extractQuestionHtml(markerBodyHtml) {
  const tmp = document.createElement("div");
  tmp.innerHTML = sanitizeHtml(markerBodyHtml || "");
  const q = tmp.querySelector(".question");
  return q ? q.innerHTML : "";
}

// Fetches and permanently splices a sub-node (§S8) right after
// `anchorBlockId`, then recursively hydrates ITS OWN interactions
// (including further nested spawns) into its own containers — a
// reload must reconstruct exactly what a live session already spliced.
async function spliceSubNodeFromServer(anchorBlockId, subNodeId, questionHtml) {
  const anchorEl = document.querySelector(
    '[data-block-id="' + anchorBlockId + '"]',
  );
  if (!anchorEl) return;
  try {
    const resp = await api(
      `/api/documents/${state.docId}/nodes/${subNodeId}`,
    );
    if (!resp.ok) return;
    const data = await resp.json();
    const sub = buildSubNodeWrapper(subNodeId, data.title, questionHtml);
    sub.proseEl.innerHTML = sanitizeHtml(data.content_html);
    hydrateIslands(sub.proseEl, subNodeId);
    anchorEl.insertAdjacentElement("afterend", sub.wrapper);
    await hydrateInteractions(data.interactions, sub.interEl);
  } catch (err) {
    // Best-effort: a failed sub-node fetch just leaves that spot
    // unspliced rather than breaking the rest of the reload.
  }
}

// Renders a node's interaction layer into `interactionsEl`. Every `qa`
// item goes back into the *document* instead, at the block it was
// anchored to (§9) — as a spliced sub-node if it spawned one (§S8),
// otherwise as the inline answer it was rendered as when it was first
// asked. A reload must reconstruct the same document a live session
// showed, not degrade it into a transcript at the bottom of the page.
// Grades and remediation (§8.2) are the ones that genuinely belong in
// the panel: they are the check's history, not part of the text.
async function hydrateInteractions(interactions, interactionsEl) {
  for (const item of interactions || []) {
    // Margin post-it (§9), not a panel entry — rendered/repositioned into
    // `#annotationsLayer` (reading.js), keyed by id so re-hydrating the
    // same item (a remount, the `done` handler's refetch) upserts instead
    // of duplicating.
    if (item.kind === "annotation") {
      upsertAnnotationNote(item);
      continue;
    }
    if (item.kind === "qa" && item.child_node_id) {
      await spliceSubNodeFromServer(
        item.anchor_block,
        item.child_node_id,
        extractQuestionHtml(item.body_html),
      );
      continue;
    }
    if (item.kind === "qa" && item.anchor_block) {
      spliceInlineAnswer(
        item.anchor_block,
        item.body_html,
        false,
        item.id,
        interactionsEl,
      );
      continue;
    }
    const div = document.createElement("div");
    div.className = item.kind;
    div.innerHTML = sanitizeHtml(item.body_html);
    interactionsEl.appendChild(div);
  }
}

// §S6 "Ritmo": crossing the sentinel after the exercise logs the
// scroll-to-end signal once per node. A pure signal today — nothing
// reacts to it yet (see the server's `EventKind::NodeReadToEnd` doc
// comment for why gating the next move on it is a separate slice).
// Per-section (§9): every mounted node arms its own observer against its
// own sentinel, independent of whichever node the learner is currently
// answering — sections are never torn down, so there is no single
// "current" watcher to swap.
function armReadToEndWatcher(rec) {
  rec.readEndFired = false;
  if (rec.readEndObserver) rec.readEndObserver.disconnect();
  // IntersectionObserver fires once immediately with whatever the
  // current state already is — for a node short enough to fit the
  // viewport, that's an "intersecting" callback before the learner has
  // scrolled a pixel, which would log the signal before they've read
  // anything. Ignore that first callback; only a later transition
  // (an actual scroll bringing the sentinel into view) counts.
  let firstCallback = true;
  rec.readEndObserver = new IntersectionObserver(
    (entries) => {
      const wasFirst = firstCallback;
      firstCallback = false;
      if (rec.readEndFired || wasFirst) return;
      if (entries.some((e) => e.isIntersecting)) {
        rec.readEndFired = true;
        postJson(
          `/api/documents/${state.docId}/nodes/${rec.nodeId}/read-to-end`,
          {},
        ).catch(() => {});
      }
    },
    { threshold: 0.1 },
  );
  rec.readEndObserver.observe(rec.sentinel);
}

// --- Node generation with streaming (§6, §14) --------------------------
// `instant` controls the initial scroll to the new section: instant for
// the view "arriving" (an outline click on a never-generated node), smooth
// when it's auto-advance appending the next node right after a live grade
// (§9 — content flowing in while the learner is already reading, not a
// jump cut).
async function generateNode(id, { instant = true } = {}) {
  state.currentId = id;
  let rec = state.sections.get(id);
  if (!rec) {
    rec = buildSection(id);
    insertSectionInOrder(rec);
  }
  renderOutline();
  rec.prose.innerHTML = "";
  rec.exercise.innerHTML = "";
  rec.exerciseFrame = null;
  rec.interactions.innerHTML = "";
  rec.result.innerHTML = "";
  rec.controls.innerHTML = '<p class="muted">' + t("status.generating") + "</p>";
  // Deliberately NOT disabling reading tools here (§9 continuous document):
  // every OTHER mounted section is fully readable right now, and the very
  // first call ever made (cold start, nothing mounted yet) leaves
  // `askEligible` at its initial `false` regardless. This new section's own
  // content has no `data-block-id`s yet, so it briefly reads as unanchorable
  // on its own — `currentReadingBlock`/`anchorFromSelection` (reading.js)
  // skip block-id-less elements by construction — but that window is now
  // only until this move's `move_settled` event (below), not the whole
  // node's `done`.
  clearReadingLine();
  scrollToSection(rec, instant);

  // A settled move becomes real, permanent DOM siblings inside `rec.prose`
  // (below) rather than a string folded into one big re-render: an earlier
  // move's interactive island, once hydrated (`hydrateIslands`), holds a
  // live iframe with its own state — rebuilding the whole subtree from a
  // string on every later move's settle would tear that iframe down and
  // recreate it. `live` is the one exception: a trailing, always-emptied
  // element holding only the CURRENT move's raw, still-streaming, untagged
  // text, replaced wholesale on every `token` (cheap — it's never held
  // more than one move's worth of text, and it has no `data-block-id` yet
  // for anything to anchor to or hydrate).
  const live = document.createElement("div");
  live.className = "streaming-move";
  rec.prose.appendChild(live);

  let prose = "";
  try {
    const resp = await api(
      `/api/documents/${state.docId}/nodes/${id}/generate`,
      { method: "POST" },
    );
    if (!resp.ok) throw new Error(await resp.text());
    await readSse(resp, (event, data) => {
      if (event === "token") {
        // Re-rendering the accumulated text tolerates partial tags during
        // the stream. Sanitized: LLM-generated prose goes to the app
        // origin, so it must NOT carry script/handlers (§3.1/§4.4). Only
        // interactive blocks run — and only inside the sandbox iframe.
        prose += data;
        live.innerHTML = sanitizeHtml(prose);
      } else if (event === "move_settled") {
        // This move is now tagged with real, permanent `data-block-id`s
        // and already persisted (§S6 follow-up) — insert it as real
        // siblings just before `live`, then clear `live` for whatever
        // move streams next. The node id is known from the very first
        // settled move, not just at `done`, so reading tools and "ask
        // about this" can target this section immediately.
        prose = "";
        live.insertAdjacentHTML("beforebegin", sanitizeHtml(data));
        live.innerHTML = "";
        rec.prose.dataset.nodeId = id;
        hydrateIslands(rec.prose, id);
        setReadingToolsEnabled(true);
        scheduleReadingLine();
      } else if (event === "research") {
        // §S13: the agent is fetching grounding for this concept before
        // writing it — surfaced as status text over the existing
        // "generating…" placeholder, not as document content.
        rec.controls.innerHTML =
          '<p class="muted">' + escapeHtml(data) + "</p>";
      } else if (event === "exercise") {
        renderExerciseInto(rec, data);
      } else if (event === "plan_proposal") {
        // A `plan` move proposed a structural outline change (§5) —
        // this generation request ends here, unfinished, awaiting the
        // learner's decision (nothing was persisted for this attempt).
        renderPlanProposal(rec, JSON.parse(data));
      } else if (event === "done") {
        // Empty data means a plan_proposal paused this request instead
        // of finishing a node — renderPlanProposal already set up the
        // approve/reject controls, so leave them alone.
        if (data) {
          state.nodeId = data;
          rec.prose.dataset.nodeId = data;
          rec.controls.innerHTML = "";
          renderSkipControl(rec);
          // Every move's prose already carries its real `data-block-id`s
          // and is already persisted (§S6 follow-up, `move_settled` above)
          // by the time `done` fires — this refetch is no longer what
          // *creates* the anchors, just confirmation that the server's
          // record matches, and the one place the exercise's own block id
          // (only assigned at `finalize`, once the graded move exists)
          // becomes known. Fire-and-forget: nothing here depends on it,
          // and a failure just leaves the already-settled prose in place.
          //
          // `view.content_html` is the content layer only (§4.3) — it
          // carries no trace of any question asked mid-generation (§S6
          // follow-up made that possible for the first time), since an
          // answer lives in the *interaction* layer, spliced into the DOM
          // separately by `/ask`'s own response handler. Replacing
          // `rec.prose.innerHTML` wholesale would silently erase that
          // splice, so this re-runs `hydrateInteractions` afterwards —
          // the same re-splice `mountExistingSection` already does for a
          // node loaded fresh from disk.
          api(`/api/documents/${state.docId}/nodes/${data}`)
            .then((resp) => (resp.ok ? resp.json() : null))
            .then(async (view) => {
              if (view) {
                rec.prose.innerHTML = sanitizeHtml(view.content_html);
                rec.prose.dataset.nodeId = data;
                hydrateIslands(rec.prose, data);
                if (view.exercise_block_id) {
                  rec.exercise.dataset.blockId = view.exercise_block_id;
                }
                rec.interactions.innerHTML = "";
                await hydrateInteractions(view.interactions, rec.interactions);
                scheduleReadingLine();
              }
            })
            .catch(() => {});
          setReadingToolsEnabled(true);
          armReadToEndWatcher(rec);
          scheduleReadingLine();
          armEdgeLoading();
        }
      } else if (event === "error") {
        throw new Error(data);
      }
    });
  } catch (err) {
    rec.controls.innerHTML =
      '<p class="error">' +
      t("gen.error") +
      escapeHtml(String(err)) +
      "</p>";
  }
}

// Renders the generated exercise in a SANDBOX iframe (§3.1/§4.4): no
// allow-same-origin, so the generated HTML never sees the token or the
// parent DOM; it returns the answer only via postMessage.
function renderExerciseInto(rec, nodeId) {
  const iframe = buildExerciseIframe(nodeId);
  rec.exercise.replaceChildren(iframe);
  rec.exerciseFrame = iframe;
}

// Shows a `plan` move's proposed outline revision (§5 propose→approve,
// §S4). The rationale is sanitized prose (app origin, same contract as
// the node's own prose); the proposed titles are plain text. `#planProposal`
// stays a single shared element (index.html) rather than one per section —
// a `plan` move can only fire on the frontier node currently generating,
// which is always the last section, so its fixed position after
// `#nodeSections` already lines up right where it needs to be.
function renderPlanProposal(rec, proposal) {
  rec.controls.innerHTML = "";
  el("planRationale").innerHTML = sanitizeHtml(proposal.html);
  el("planProposedOutline").innerHTML = proposal.proposed
    .map((t) => "<li>" + escapeHtml(t) + "</li>")
    .join("");
  el("planProposal").hidden = false;
}

el("planApproveBtn").addEventListener("click", () => decidePlanProposal(true));
el("planRejectBtn").addEventListener("click", () => decidePlanProposal(false));

// Either way the proposal is resolved and the interrupted generation
// attempt is retried from scratch — any of its moves that had already
// settled (§S6 follow-up) live in an unfinalized node the retry simply
// overwrites, never read as real content until a future `finalize` runs.
async function decidePlanProposal(approve) {
  el("planProposal").hidden = true;
  try {
    const resp = await postJson(
      `/api/documents/${state.docId}/plan/decide`,
      { approve },
    );
    if (!resp.ok) throw new Error(await resp.text());
    const data = await resp.json();
    setOutlineItems(data.items);
    renderOutline();
  } catch (err) {
    const rec = state.sections.get(state.currentId);
    (rec ? rec.result : el("nodeSections")).innerHTML =
      '<span class="error">plan decision failed: ' +
      escapeHtml(String(err)) +
      "</span>";
  }
  generateNode(state.currentId);
}

// Builds a sandboxed exercise iframe (used for the node's check AND for the
// remediation loop's new practice problems, §8.2) — both are the same node
// id's currently-active exercise (remediation overwrites the rubric sidecar
// in place, §8.2), so both are just this one URL.
//
// A real `src=` fetch, not `srcdoc`: `srcdoc` documents inherit the parent
// page's CSP, which would break once the app origin's CSP drops
// `unsafe-inline` (planned hardening) — the frame endpoint
// (`api::exercise_frame`) sets its own, separate CSP and builds the whole
// harness (theme/height/answer-collection) server-side instead.
// §S11: finds every still-empty interactive-island placeholder (§4.4)
// inside `container` — an `ensure_block_ids`-tagged
// `<figure data-interactive data-block-id>`, redacted server-side
// (`redact_interactive_blocks`) so it never carries raw script — and
// swaps it for its own sandboxed iframe, fetched from its own frame
// endpoint. Same pattern as the exercise, just per content block
// instead of per node. `:not([data-hydrated])` makes repeated calls
// (e.g. every `renderExistingNode`) idempotent, never double-wrapping.
function hydrateIslands(container, nodeId) {
  const islands = container.querySelectorAll(
    "[data-interactive][data-block-id]:not([data-hydrated])",
  );
  for (const el of islands) {
    el.dataset.hydrated = "1";
    const iframe = document.createElement("iframe");
    iframe.className = "sandbox island";
    iframe.title = t("iframe.interactive");
    iframe.setAttribute("sandbox", "allow-scripts");
    const url = new URL(
      `/api/documents/${state.docId}/nodes/${nodeId}/blocks/${el.dataset.blockId}/frame`,
      location.origin,
    );
    url.searchParams.set("token", TOKEN);
    url.searchParams.set("theme", currentTheme());
    url.searchParams.set("lang", I18N_LANG);
    iframe.src = url.toString();
    el.replaceChildren(iframe);
  }
}

function buildExerciseIframe(nodeId) {
  const iframe = document.createElement("iframe");
  iframe.className = "sandbox";
    iframe.title = t("iframe.exercise");
  iframe.setAttribute("sandbox", "allow-scripts");
  const url = new URL(
    `/api/documents/${state.docId}/nodes/${nodeId}/exercise-frame`,
    location.origin,
  );
  url.searchParams.set("token", TOKEN);
  url.searchParams.set("theme", currentTheme());
  url.searchParams.set("lang", I18N_LANG);
  iframe.src = url.toString();
  return iframe;
}

// Messages from the sandbox iframe (§4.4): the answer artifact, and the
// reported content height used to size the box to fit (no internal scroll).
window.addEventListener("message", (e) => {
  const d = e.data;
  if (!d) return;
  if (d.type === "learnive-answer") {
    submitAnswer(d.answer);
  } else if (d.type === "learnive-height" && typeof d.height === "number") {
    // Size whichever sandbox frame reported (node exercise or a remediation
    // practice problem) — match by its content window.
    for (const f of document.querySelectorAll("iframe.sandbox")) {
      if (f.contentWindow === e.source) {
        f.style.height = Math.max(d.height + 2, 40) + "px";
        break;
      }
    }
  }
});

// --- Answer → grading → remediation/advance (§8, §8.2) ----------------
async function submitAnswer(answer) {
  if (!state.nodeId) return;
  const rec = state.sections.get(state.nodeId);
  if (!rec) return;
  // Transient status only — never wipes the interaction log below.
  rec.result.innerHTML = '<p class="muted">' + t("status.evaluating") + "</p>";
  try {
    const resp = await postJson(
      `/api/documents/${state.docId}/nodes/${state.nodeId}/answer`,
      { answer },
    );
    if (!resp.ok) throw new Error(await resp.text());
    const data = await resp.json();
    rec.result.innerHTML = "";
    // The just-answered exercise is retired in favor of its own frozen
    // record (`attempt_html`) — same string the server just appended to the
    // interaction layer, so there is no separate live iframe left around to
    // go stale (the original bug class this replaces) and nothing rendered
    // here that a reload wouldn't reconstruct identically (§4.3).
    rec.exercise.innerHTML = "";
    rec.exerciseFrame = null;
    appendAttempt(rec, data.attempt_html);
    if (data.advance) {
      rec.controls.innerHTML = "";
      await advanceAfterGrading();
    } else if (data.remediation_html) {
      appendRemediation(rec, data.remediation_html);
    }
  } catch (err) {
    rec.result.innerHTML =
      '<p class="error">' + t("answer.error") + escapeHtml(String(err)) + "</p>";
  }
}

// Append-only interaction layer (§4.3): renders EXACTLY the HTML the server
// already persisted for this attempt (frozen exercise + answer + feedback) —
// never a client-only reconstruction that reload couldn't reproduce.
function appendAttempt(rec, html) {
  const div = document.createElement("div");
  div.className = "attempt";
  div.innerHTML = sanitizeHtml(html);
  rec.interactions.appendChild(div);
}

// §6/§9: the document keeps writing itself forward as each concept is
// demonstrated — no manual "next concept" click, and no artificial pause
// either: the grade feedback just appended stays on the page for good
// (§4.3, nothing here gets wiped), so scrolling straight on to the next
// section reads as the document continuing, not as losing what just
// happened. `openNode`'s smooth scroll (an existing, already-rendered
// section) or `generateNode`'s (a fresh one streaming in) both carry the
// grade box past the viewport at reading pace rather than cutting to it.
async function advanceAfterGrading() {
  await refreshOutline();
  // §S15: search the full tree, not just the main line — a demonstrated
  // node can unlock a sibling prerequisite sub-node next, not just the
  // next main-line item, and `state.items` excludes every sub-node by
  // construction (`setOutlineItems`, outline.js).
  const next = state.allItems.find(
    (it) => it.id !== state.currentId && it.state === "available",
  );
  if (next) {
    await openNode(next.id, { instant: false });
  } else {
    const rec = state.sections.get(state.currentId);
    const p = document.createElement("p");
    p.className = "muted";
    p.textContent = t("completed");
    (rec ? rec.controls : el("nodeSections")).appendChild(p);
  }
}

// Remediation (§8.2): a worked EXPLANATION of the missed problem (sanitized
// prose) followed by a NEW practice problem that runs in its own SANDBOX
// iframe — submittable and gradeable exactly like the node's check, with the
// answer never revealed. Server-side that new problem becomes the node's
// active rubric, so its submission (via postMessage → submitAnswer) grades
// the new problem and either advances or remediates again.
// --- Source viewer (§11) -----------------------------------------------
// A citation (`<cite data-source-id data-locator>`, §4.3/§10) opens the
// corpus's already-normalized text on the right (`#sourcePanel`) — the seam
// the eventual PDF split-view (§11.1) will occupy once the acquisition
// layer stores original files and page locations. Read-only: nothing here
// is ever written back.
const sourceCache = new Map();

// Delegated, not bound per-citation: citations arrive incrementally as
// prose streams in (§14), so binding at insert time would miss most of them.
document.addEventListener("click", (e) => {
  const cite = e.target.closest("cite[data-source-id]");
  if (!cite) return;
  openSourcePanel(cite.dataset.sourceId, cite.dataset.locator || "");
});

async function openSourcePanel(sourceId, locator) {
  el("sourcePanel").classList.add("open");
  el("sourceTitle").textContent = t("source.loading");
  el("sourceMeta").textContent = "";
  el("sourceBody").innerHTML = "";
  try {
    let source = sourceCache.get(sourceId);
    if (!source) {
      const resp = await api(
        `/api/sources/${encodeURIComponent(sourceId)}`,
      );
      if (!resp.ok) throw new Error(await resp.text());
      source = await resp.json();
      sourceCache.set(sourceId, source);
    }
    renderSource(source, locator);
  } catch (err) {
    el("sourceTitle").textContent = t("source.unavailable");
    el("sourceMeta").textContent = String(err);
  }
}

function renderSource(source, locator) {
  el("sourceTitle").textContent = source.meta.title;
  const bits = [];
  if (source.meta.authors && source.meta.authors.length) {
    bits.push(source.meta.authors.join(", "));
  }
  if (source.meta.license) bits.push(source.meta.license);
  el("sourceMeta").textContent = bits.join(" · ");

  const body = el("sourceBody");
  let current = null;
  for (const section of source.sections || []) {
    const sec = document.createElement("section");
    if (section.locator === locator) {
      sec.className = "current";
      current = sec;
    }
    const h = document.createElement("h3");
    h.textContent = section.title || section.locator;
    const p = document.createElement("p");
    p.textContent = section.text;
    sec.appendChild(h);
    sec.appendChild(p);
    body.appendChild(sec);
  }
  (current || body.firstElementChild)?.scrollIntoView({ block: "start" });
}

function closeSourcePanel() {
  el("sourcePanel").classList.remove("open");
}
el("sourceCloseBtn").addEventListener("click", closeSourcePanel);
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && el("sourcePanel").classList.contains("open")) {
    closeSourcePanel();
  }
});

function appendRemediation(rec, explanationHtml) {
  // Every previously-active exercise iframe is already gone by the time this
  // runs — `submitAnswer` clears the section's exercise div and appends that
  // attempt's own frozen record before ever calling here (§4.3), so there is
  // no stale iframe left to disable: a submission can only ever reach the
  // ONE iframe this function is about to build.
  const wrap = document.createElement("div");
  wrap.className = "remediation";
  const head = document.createElement("div");
  head.innerHTML = "<h3>" + t("review.title") + "</h3>" + sanitizeHtml(explanationHtml);
  wrap.appendChild(head);
  // The server always regenerates a fresh exercise on a failed answer
  // (§8.2) — same node id, same frame URL, sidecar overwritten in place.
  const label = document.createElement("p");
  label.className = "muted";
  label.textContent = t("review.try");
  wrap.appendChild(label);
  const iframe = buildExerciseIframe(state.nodeId);
  rec.exerciseFrame = iframe;
  wrap.appendChild(iframe);
  rec.interactions.appendChild(wrap);
}

boot();
