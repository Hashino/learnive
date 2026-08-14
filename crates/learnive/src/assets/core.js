// learnive — shared plumbing, loaded first: the session token and the
// authenticated fetch helpers (§3.1), the app-wide `state`, the theme
// toggle, the SSE-over-POST reader and HTML escaping/sanitizing. Every
// other script assumes these already exist.

const TOKEN = document.querySelector('meta[name="learnive-token"]').content;

// Every request carries the token in the header (§3.1).
function api(path, options = {}) {
  const headers = Object.assign(
    { "X-Learnive-Token": TOKEN },
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
  currentId: null,
  nodeId: null,
  suggestedRevisit: null,
};
// The current exercise's sandbox iframe (for height + theme messages).
let exerciseFrame = null;

// --- Theme toggle -----------------------------------------------------
// The head script already applied the theme; here we wire the switch and
// persist the choice. The icon shows the mode you'd switch TO.
const currentTheme = () =>
  document.documentElement.dataset.theme === "light" ? "light" : "dark";
function syncThemeToggle() {
  el("themeToggle").textContent = currentTheme() === "light" ? "☾" : "☀";
}
el("themeToggle").addEventListener("click", () => {
  const next = currentTheme() === "light" ? "dark" : "light";
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
      }
    }
  }
  return tpl.innerHTML;
}

