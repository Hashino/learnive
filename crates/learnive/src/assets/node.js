// learnive — the node itself: streamed generation (§6, §14), splicing
// answers and sub-nodes into the reading flow (§S8), sandboxed interactive
// islands (§4.4), and the answer → grading → remediation/advance loop
// (§8, §8.2). Last script on the page, so it is the one that boots the app.

// Finds a content block by id, anywhere in the reading flow — the
// top-level node's prose or a spliced sub-node's (§S8).
function blockElement(blockId) {
  return blockId
    ? el("prose").querySelector('[data-block-id="' + blockId + '"]')
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
// The anchor is a content block *or* another answer: asking about an
// answer is how the learner falls into a rabbit hole, and the thread has
// to keep reading in order. So the insertion point is after the anchor
// and after everything already hanging off it — its own follow-ups
// (deeper) and the answers asked before this one at the same depth —
// but never past a sibling that belongs to a shallower part of the page.
// `id` is the interaction item's stable id (§4.3): tagging it as a block
// id is what makes this answer itself askable, in this session and after
// a reload. Falls back to the interaction panel only if the anchor is
// not on the page (a node the answer doesn't belong to).
function spliceInlineAnswer(blockId, bodyHtml, scroll, id) {
  const div = document.createElement("div");
  div.className = "qa inline-answer";
  div.innerHTML = sanitizeHtml(bodyHtml);
  if (id) div.dataset.blockId = id;
  const anchorEl = blockElement(blockId);
  if (!anchorEl) {
    el("interactions").appendChild(div);
    return;
  }
  const depth = answerDepth(anchorEl) + 1;
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
    if (item.kind === "qa" && item.child_node_id) {
      await spliceSubNodeFromServer(
        item.anchor_block,
        item.child_node_id,
        extractQuestionHtml(item.body_html),
      );
      continue;
    }
    if (item.kind === "qa" && item.anchor_block) {
      spliceInlineAnswer(item.anchor_block, item.body_html, false, item.id);
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
let readEndFired = false;
let readEndObserver = null;
function armReadToEndWatcher() {
  readEndFired = false;
  if (readEndObserver) readEndObserver.disconnect();
  // IntersectionObserver fires once immediately with whatever the
  // current state already is — for a node short enough to fit the
  // viewport, that's an "intersecting" callback before the learner has
  // scrolled a pixel, which would log the signal before they've read
  // anything. Ignore that first callback; only a later transition
  // (an actual scroll bringing the sentinel into view) counts.
  let firstCallback = true;
  readEndObserver = new IntersectionObserver(
    (entries) => {
      const wasFirst = firstCallback;
      firstCallback = false;
      if (readEndFired || !state.nodeId || wasFirst) return;
      if (entries.some((e) => e.isIntersecting)) {
        readEndFired = true;
        postJson(
          `/api/documents/${state.docId}/nodes/${state.nodeId}/read-to-end`,
          {},
        ).catch(() => {});
      }
    },
    { threshold: 0.1 },
  );
  readEndObserver.observe(el("readEndSentinel"));
}

// --- Node generation with streaming (§6, §14) -------------------------
async function generateNode(id) {
  state.currentId = id;
  state.nodeId = null;
  renderOutline();
  el("prose").innerHTML = "";
  el("exercise").innerHTML = "";
  exerciseFrame = null;
  el("interactions").innerHTML = "";
  el("result").innerHTML = "";
  el("controls").innerHTML = '<p class="muted">generating…</p>';
  setReadingToolsEnabled(false);
  clearReadingLine();

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
        el("prose").innerHTML = sanitizeHtml(prose);
      } else if (event === "exercise") {
        renderExercise(data);
      } else if (event === "plan_proposal") {
        // A `plan` move proposed a structural outline change (§5) —
        // this generation request ends here, unfinished, awaiting the
        // learner's decision (nothing was persisted for this attempt).
        renderPlanProposal(JSON.parse(data));
      } else if (event === "done") {
        // Empty data means a plan_proposal paused this request instead
        // of finishing a node — renderPlanProposal already set up the
        // approve/reject controls, so leave them alone.
        if (data) {
          state.nodeId = data;
          el("prose").dataset.nodeId = data;
          el("controls").innerHTML = "";
          renderSkipControl();
          // The just-streamed prose has no data-block-id attributes —
          // those are assigned server-side at finalize (§4.3), which
          // only happens once the whole node, including the graded
          // move, is done. Reading tools anchor on those ids (§S6),
          // so swap in the finalized, block-tagged content now that
          // it exists. Fire-and-forget: nothing here depends on it,
          // and a failure just leaves the streamed prose in place.
          api(`/api/documents/${state.docId}/nodes/${data}`)
            .then((resp) => (resp.ok ? resp.json() : null))
            .then((view) => {
              if (view) {
                el("prose").innerHTML = sanitizeHtml(view.content_html);
                hydrateIslands(el("prose"), data);
                // The reading line can only be placed once these
                // block-tagged elements exist — the streamed prose this
                // just replaced had no `data-block-id` at all.
                scheduleReadingLine();
              }
            })
            .catch(() => {});
          setReadingToolsEnabled(true);
          armReadToEndWatcher();
          scheduleReadingLine();
        }
      } else if (event === "error") {
        throw new Error(data);
      }
    });
  } catch (err) {
    el("controls").innerHTML =
      '<p class="error">generation error: ' +
      escapeHtml(String(err)) +
      "</p>";
  }
}

// Renders the generated exercise in a SANDBOX iframe (§3.1/§4.4): no
// allow-same-origin, so the generated HTML never sees the token or the
// parent DOM; it returns the answer only via postMessage.
function renderExercise(nodeId) {
  const iframe = buildExerciseIframe(nodeId);
  el("exercise").replaceChildren(iframe);
  exerciseFrame = iframe;
}

// Shows a `plan` move's proposed outline revision (§5 propose→approve,
// §S4). The rationale is sanitized prose (app origin, same contract as
// the node's own prose); the proposed titles are plain text.
function renderPlanProposal(proposal) {
  el("controls").innerHTML = "";
  el("planRationale").innerHTML = sanitizeHtml(proposal.html);
  el("planProposedOutline").innerHTML = proposal.proposed
    .map((t) => "<li>" + escapeHtml(t) + "</li>")
    .join("");
  el("planProposal").hidden = false;
}

el("planApproveBtn").addEventListener("click", () => decidePlanProposal(true));
el("planRejectBtn").addEventListener("click", () => decidePlanProposal(false));

// Either way the proposal is resolved and the interrupted generation
// attempt (nothing was persisted for it) is retried from scratch.
async function decidePlanProposal(approve) {
  el("planProposal").hidden = true;
  try {
    const resp = await postJson(
      `/api/documents/${state.docId}/plan/decide`,
      { approve },
    );
    if (!resp.ok) throw new Error(await resp.text());
    const data = await resp.json();
    state.items = data.items;
    renderOutline();
  } catch (err) {
    el("result").innerHTML =
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
    iframe.title = "Interactive content";
    iframe.setAttribute("sandbox", "allow-scripts");
    const url = new URL(
      `/api/documents/${state.docId}/nodes/${nodeId}/blocks/${el.dataset.blockId}/frame`,
      location.origin,
    );
    url.searchParams.set("token", TOKEN);
    url.searchParams.set("theme", currentTheme());
    iframe.src = url.toString();
    el.replaceChildren(iframe);
  }
}

function buildExerciseIframe(nodeId) {
  const iframe = document.createElement("iframe");
  iframe.className = "sandbox";
  iframe.title = "Exercise";
  iframe.setAttribute("sandbox", "allow-scripts");
  const url = new URL(
    `/api/documents/${state.docId}/nodes/${nodeId}/exercise-frame`,
    location.origin,
  );
  url.searchParams.set("token", TOKEN);
  url.searchParams.set("theme", currentTheme());
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
  // Transient status only — never wipes the interaction log below.
  el("result").innerHTML = '<p class="muted">evaluating…</p>';
  try {
    const resp = await postJson(
      `/api/documents/${state.docId}/nodes/${state.nodeId}/answer`,
      { answer },
    );
    if (!resp.ok) throw new Error(await resp.text());
    const data = await resp.json();
    el("result").innerHTML = "";
    appendGrades(data.grades);
    if (data.advance) {
      await refreshOutline();
      el("controls").innerHTML = "";
      renderNextControl();
    } else if (data.remediation_html) {
      appendRemediation(data.remediation_html);
    }
  } catch (err) {
    el("result").innerHTML =
      '<p class="error">error: ' + escapeHtml(String(err)) + "</p>";
  }
}

// Append-only interaction layer (§4.3): each attempt's grades and each
// remediation stay in the document, never overwritten within a node.
function appendGrades(grades) {
  const div = document.createElement("div");
  div.className = "grades";
  div.innerHTML = (grades || [])
    .map(
      (g) =>
        '<div class="grade ' +
        g.grade +
        '"><strong>' +
        g.grade.replace("_", " ") +
        "</strong> — " +
        escapeHtml(g.feedback || "") +
        "</div>",
    )
    .join("");
  el("interactions").appendChild(div);
}

// Remediation (§8.2): a worked EXPLANATION of the missed problem (sanitized
// prose) followed by a NEW practice problem that runs in its own SANDBOX
// iframe — submittable and gradeable exactly like the node's check, with the
// answer never revealed. Server-side that new problem becomes the node's
// active rubric, so its submission (via postMessage → submitAnswer) grades
// the new problem and either advances or remediates again.
function appendRemediation(explanationHtml) {
  // Freeze earlier remediation problems so only the latest is answerable —
  // the log still reads as the full history (§4.3).
  document
    .querySelectorAll("#interactions .remediation .sandbox")
    .forEach((f) => {
      f.style.pointerEvents = "none";
      f.style.opacity = "0.55";
    });
  const wrap = document.createElement("div");
  wrap.className = "remediation";
  const head = document.createElement("div");
  head.innerHTML = "<h3>Let's review</h3>" + sanitizeHtml(explanationHtml);
  wrap.appendChild(head);
  // The server always regenerates a fresh exercise on a failed answer
  // (§8.2) — same node id, same frame URL, sidecar overwritten in place.
  const label = document.createElement("p");
  label.className = "muted";
  label.textContent = "Now try this one:";
  wrap.appendChild(label);
  wrap.appendChild(buildExerciseIframe(state.nodeId));
  el("interactions").appendChild(wrap);
}

boot();
