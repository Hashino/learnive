// learnive — shared plumbing, loaded first: the session token and the
// authenticated fetch helpers (§3.1), the app-wide `state`, the theme
// toggle, the SSE-over-POST reader and HTML escaping/sanitizing. Every
// other script assumes these already exist.

const TOKEN = document.querySelector('meta[name="learnive-token"]').content;

// Every request carries the token in the header (§3.1).
function api(path, options = {}) {
  const headers = Object.assign(
    { "X-Learnive-Token": TOKEN, "X-Learnive-Lang": I18N_LANG },
    options.headers || {},
  );
  return fetch(path, Object.assign({}, options, { headers }));
}
function postJson(path, body) {
  return api(path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
}
function putJson(path, body) {
  return api(path, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
}

const el = (id) => document.getElementById(id);

// The page rebuilds its document from the server on every load, so the
// browser's remembered scroll offset means nothing here — and it is
// restored *asynchronously, after* boot has already placed the view
// (`parkAtDocumentTop`), silently undoing it. We position the view
// ourselves; the browser must not second-guess it.
if ("scrollRestoration" in history) {
  history.scrollRestoration = "manual";
}
// items: [{id, title, state}] from the server (§S5) — state is
// "locked" | "available" | "demonstrated", the graph's gate resolved.
// currentId is the outline item id being generated/answered; nodeId
// mirrors it once generation actually completes (kept separate so a
// still-streaming request doesn't let a stray postMessage answer
// reach the wrong node).
const state = {
  docId: null,
  docName: "",
  // Every living document on disk (§S12), newest first — what the
  // sidebar's document list shows and what boot() resumes from.
  docs: [],
  items: [],
  // §S15: the same items, unfiltered (sub-nodes included) — `renderOutline`
  // nests on `parent_id` from this; `items` above stays main-line-only for
  // every consumer that predates the tree (skip eligibility, lazy
  // neighbor-loading, "next available" advance).
  allItems: [],
  currentId: null,
  nodeId: null,
  suggestedRevisit: null,
  // One entry per mounted `.node-section` (nodeId -> {el, prose, exercise,
  // sentinel, interactions, result, controls, exerciseFrame, ...}) — the
  // continuous document is however many of these are currently in
  // `#nodeSections`, not a single swapped view (§9 "a document that
  // emerges as you interact with it").
  sections: new Map(),
};

// --- Theme toggle -----------------------------------------------------
// The head script already applied the theme; here we wire the switch and
// persist the choice. A checkbox styled as a switch (app.css); checked =
// dark, unchecked = light.
const currentTheme = () =>
  document.documentElement.dataset.theme === "light" ? "light" : "dark";
function syncThemeToggle() {
  el("themeToggle").checked = currentTheme() === "dark";
}
el("themeToggle").addEventListener("change", () => {
  const next = el("themeToggle").checked ? "dark" : "light";
  document.documentElement.dataset.theme = next;
  localStorage.setItem("learnive-theme", next);
  syncThemeToggle();
  // Re-theme every sandbox iframe (node check + remediation problems);
  // they're reachable only by message (§4.4).
  for (const f of document.querySelectorAll("iframe.sandbox")) {
    if (f.contentWindow) {
      f.contentWindow.postMessage(
        { type: "learnive-theme", theme: next },
        "*",
      );
    }
  }
});
syncThemeToggle();

// --- Language switcher ------------------------------------------------
// A dropdown (one option per `I18N_SUPPORTED` locale, i18n.js) rather than a
// cycling toggle — scales to more than two locales without changing shape.
// The choice persists in localStorage (i18n.js) and rides every request as
// the `X-Learnive-Lang` header, so server-rendered strings follow too.
if (el("langToggle")) {
  const refreshLangLabel = () => {
    el("langToggle").value = currentLocale();
  };
  refreshLangLabel();
  el("langToggle").addEventListener("change", () => {
    const next = el("langToggle").value;
    i18nSetLang(next);
    refreshLangLabel();
    // Re-fill static chrome; already-rendered dynamic strings adopt the new
    // locale on their next render (e.g. on the next navigation).
    applyI18n(document);
    if (typeof renderDocList === "function") renderDocList();
    if (typeof renderOutline === "function") renderOutline();
  });
}

// --- SSE parser over fetch (POST) -------------------------------------
async function readSse(response, onEvent) {
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buf = "";
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    buf += decoder.decode(value, { stream: true });
    let sep;
    while ((sep = buf.indexOf("\n\n")) !== -1) {
      const block = buf.slice(0, sep);
      buf = buf.slice(sep + 2);
      let event = "message";
      let data = "";
      for (const line of block.split("\n")) {
        if (line.startsWith("event:")) event = line.slice(6).trim();
        else if (line.startsWith("data:")) data += line.slice(5).trim();
      }
      if (data) onEvent(event, JSON.parse(data));
    }
  }
}

function escapeHtml(s) {
  return s.replace(
    /[&<>"]/g,
    (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c],
  );
}

// Sanitizes LLM-generated HTML before inserting it into the app origin
// (§3.1, §4.4). Parsing happens in an inert <template> (images don't load,
// scripts don't run), then we remove dangerous elements, event attributes
// (on*) and non-http href/src URLs. Real interactive blocks don't go
// through here — they run isolated in the sandbox iframe.
//
// The BLOCKED/attributes/CLASSES lists below are the enforcement counterpart
// of PROSE_HTML_CONTRACT in engine.rs, which tells the model the same limit.
// Change one, change the other — otherwise the model generates something
// that disappears here.

// Every class allowed to survive sanitizing. `class` is allowlisted rather
// than passed through because it is the one attribute that can reach the
// app's own stylesheet: an arbitrary class lets generated content wear the
// chrome's clothes (a fake nav, a fake status line), and unknown classes
// would silently render as unstyled divs anyway. `style` stays blocked — the
// vocabulary is the point, free-form CSS is not.
//
// Two families, both passing through here: the layout vocabulary the model
// may use (§4.4, mirrored by PROSE_HTML_CONTRACT), and the wrappers the
// server itself puts in an interaction item's `body_html` (§4.3) — those are
// ours, not the model's, but they arrive by the same route and would be
// stripped just the same.
const ALLOWED_CLASSES = new Set([
  "callout",
  "key",
  "warning",
  "columns",
  // "panel", not "card": the app's own chrome already owns `.card`, and a
  // generated block must never be able to wear the chrome's clothes.
  "panel",
  "term",
  "hl",
  // Server-side (api.rs `question_header` / the answer wrapper).
  "question",
  "about",
  "answer",
  // Server-side (grading.rs `render_attempt`, §8/§4.3): a frozen graded
  // submission — the CSS these classes drive (dimming the dead exercise,
  // color-coding each grade) is what makes the live response and a reload
  // look identical, not just contain the same text.
  "attempt-exercise",
  "attempt-answer",
  "answer-field",
  "answer-label",
  "grades",
  "grade",
  "demonstrated",
  "partial",
  "not_demonstrated",
  // Server-side (grading.rs, §S15 item 4): a remediation-thread hint
  // pointing at the document's existing revisit suggestion.
  "revisit-hint",
]);

function sanitizeHtml(html) {
  const tpl = document.createElement("template");
  tpl.innerHTML = html;
  const BLOCKED = new Set([
    "SCRIPT",
    "IFRAME",
    "OBJECT",
    "EMBED",
    "LINK",
    "META",
    "BASE",
    "STYLE",
    "FORM",
  ]);
  const SAFE_URI = /^(https?:|mailto:|#|\/|data:image\/)/i;
  const ASSET_SRC = /^\/api\/sources\/[^/?#]+\/assets\//;
  const els = [];
  const walker = document.createTreeWalker(
    tpl.content,
    NodeFilter.SHOW_ELEMENT,
  );
  for (let n = walker.nextNode(); n; n = walker.nextNode()) els.push(n);
  for (const elm of els) {
    if (BLOCKED.has(elm.tagName)) {
      elm.remove();
      continue;
    }
    // MathML comes from the server's own LaTeX renderer, which tags <mtable>
    // and friends with `menv-*` classes — filtering those would break the
    // layout of every rendered equation.
    const inMath = !!elm.closest("math");
    for (const attr of [...elm.attributes]) {
      const name = attr.name.toLowerCase();
      if (name.startsWith("on") || name === "srcdoc" || name === "style") {
        elm.removeAttribute(attr.name);
      } else if (name === "class" && !inMath) {
        const kept = attr.value
          .split(/\s+/)
          .filter((c) => ALLOWED_CLASSES.has(c));
        // setAttribute, not `.className`: on SVG elements className is a
        // read-only SVGAnimatedString and the assignment silently no-ops.
        if (kept.length) elm.setAttribute("class", kept.join(" "));
        else elm.removeAttribute("class");
      } else if (
        (name === "href" || name === "src" || name === "xlink:href") &&
        !SAFE_URI.test(attr.value.trim())
      ) {
        elm.removeAttribute(attr.name);
      } else if (name === "src" && ASSET_SRC.test(attr.value)) {
        // The security guard requires the session token on every request,
        // but a plain <img src> can't carry the X-Learnive-Token header —
        // so locally-served source assets need the token in the query,
        // same convention as index.html's <script src="...?token=">.
        const sep = attr.value.includes("?") ? "&" : "?";
        elm.setAttribute("src", `${attr.value}${sep}token=${TOKEN}`);
      }
    }
  }
  return tpl.innerHTML;
}

