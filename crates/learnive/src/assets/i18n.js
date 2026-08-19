// learnive — localization (§3, §15). The default locale is English; pt-BR
// is an added, selectable option. The dictionary below is the single source
// of truth for user-facing chrome strings: static HTML text is marked with
// `data-i18n*` attributes and filled by `applyI18n`; JS-built strings use
// `t(key, ...args)`. Adding a third locale means adding a key to I18N and a
// switcher entry — nothing else changes.

const I18N_STORAGE_KEY = "learnive-lang";
const I18N_SUPPORTED = ["en", "pt-BR"];

// English is the baseline; every other locale is a translation of it. Keys
// are stable identifiers, never the English text itself.
const I18N = {
  en: {
    // Sidebar / navigation
    "nav.allDocuments": "All documents",
    "doc.rename": "Click to rename",
    "docs.section": "Documents",
    "doc.new": "New document",
    "theme.toggle": "Toggle light/dark theme",
    "source.title": "Source",
    "source.close": "Close",
    "docs.backOutline": "Back to outline",
    // Cold start / objective
    "coldstart.title": "What are we learning?",
    "start.button": "Start",
    "coldstart.back": "Back",
    // Main-line outline preview (§S16), shown above the prerequisite tree
    "outline.mainlineTitle": "What you'll learn",
    // Prerequisite tree confirmation (§S15)
    "prereq.title": "Before we start",
    "prereq.hint":
      "These look like prerequisites for what you asked. Mark what you already know as review or skip — everything else is taught in full.",
    "prereq.confirm": "Confirm",
    "prereq.action.learn": "Learn",
    "prereq.action.review": "Review",
    "prereq.action.skip": "Skip",
    "prereq.knownIn": "known in {0}",
    "prereq.reviewBadge": "(review)",
    // Plan proposal
    "plan.title": "Proposed outline change",
    "plan.approve": "Approve",
    "plan.reject": "Reject",
    // Ask bar
    "ask.placeholder": "Ask about what you're reading…",
    "ask.aria": "Ask about what you're reading",
    "ask.send": "Send",
    "ask.aboutSelected": "Asking about:",
    "ask.aboutLine": "Asking about the line you're on: {0}",
    "ask.aboutPage": "Asking about this page",
    "ask.thinking": "thinking…",
    // Document list
    "doc.count": "{0} / {1} demonstrated",
    "delete.title": "Delete this document",
    "delete.confirm": "Delete \"{0}\" and everything in it?",
    "delete.button": "Delete",
    "delete.cancel": "Cancel",
    "delete.failed": "delete failed: ",
    "delete.close": "Close",
    // Outline / navigation
    "revisit.hint": "↺ Consider revisiting: {0}",
    "open.failed": "could not open node: ",
    "skip.button": "Skip for now →",
    "skip.none": "Nothing else available yet.",
    "skip.failed": "skip failed: ",
    // Node / generation
    "status.generating": "generating…",
    "gen.error": "generation error: ",
    "asked.prefix": "You asked:",
    "iframe.interactive": "Interactive content",
    "iframe.exercise": "Exercise",
    "status.evaluating": "evaluating…",
    "answer.error": "error: ",
    "completed": "You have completed the current outline.",
    "review.title": "Let's review",
    "review.try": "Now try this one:",
    "source.loading": "Loading…",
    "source.unavailable": "Source unavailable",
    // Status
    "status.objective": "thinking about the objective…",
    "error.failed": "failed: ",
    "status.curriculum": "planning the curriculum…",
    "note.add": "Add a note",
    // Setup page
    "setup.nav": "setup",
    "setup.title": "Provider setup",
    "setup.intro":
      "Bring your own key (§12). Your key is stored locally in a <code>0600</code> file (never a database, never committed) and never leaves this machine except to the provider you choose. Pick free or paid — the app derives both model tiers for you.",
    "provider.legend": "Provider",
    "provider.label": "Which provider?",
    "provider.demo": "Demo (no key — canned content)",
    "provider.openrouter": "OpenRouter (recommended)",
    "provider.compat": "OpenAI-compatible endpoint (Mercury, local, …)",
    "baseurl.label": "Base URL",
    "apikey.label": "API key",
    "apikey.hint": "(a key is already saved — leave blank to keep it)",
    "budget.legend": "Budget",
    "budget.free": "Free models",
    "budget.paid": "Paid models (better quality)",
    "save.button": "Save",
    "status.saving": "Saving…",
    "status.demoSaved": "Saved. Running in demo mode.",
    "status.saved": "Saved and applied — no restart needs.",
    "error.prefix": "Error: ",
    "derived.demo": "Demo mode: canned content, no models.",
    "derived.models": "Models — fast: {0} · robust: {1}",
  },
  "pt-BR": {
    "nav.allDocuments": "Todos os documentos",
    "doc.rename": "Clique para renomear",
    "docs.section": "Documentos",
    "doc.new": "Novo documento",
    "theme.toggle": "Alternar tema claro/escuro",
    "source.title": "Fonte",
    "source.close": "Fechar",
    "docs.backOutline": "Voltar ao sumário",
    "coldstart.title": "O que vamos aprender?",
    "start.button": "Começar",
    "coldstart.back": "Voltar",
    "outline.mainlineTitle": "O que você vai aprender",
    "prereq.title": "Antes de começar",
    "prereq.hint":
      "Estes parecem ser pré-requisitos para o que você pediu. Marque o que já sabe como revisar ou pular — o resto é ensinado por completo.",
    "prereq.confirm": "Confirmar",
    "prereq.action.learn": "Aprender",
    "prereq.action.review": "Revisar",
    "prereq.action.skip": "Pular",
    "prereq.knownIn": "já aprendido em {0}",
    "prereq.reviewBadge": "(revisão)",
    "plan.title": "Mudança de estrutura proposta",
    "plan.approve": "Aprovar",
    "plan.reject": "Rejeitar",
    "ask.placeholder": "Pergunte sobre o que está lendo…",
    "ask.aria": "Pergunte sobre o que está lendo",
    "ask.send": "Enviar",
    "ask.aboutSelected": "Perguntando sobre:",
    "ask.aboutLine": "Perguntando sobre a linha em que você está: {0}",
    "ask.aboutPage": "Perguntando sobre esta página",
    "ask.thinking": "pensando…",
    "doc.count": "{0} / {1} demonstrados",
    "delete.title": "Excluir este documento",
    "delete.confirm": 'Excluir "{0}" e tudo o que contém?',
    "delete.button": "Excluir",
    "delete.cancel": "Cancelar",
    "delete.failed": "falha ao excluir: ",
    "delete.close": "Fechar",
    "revisit.hint": "↺ Considere revisitar: {0}",
    "open.failed": "não foi possível abrir o nó: ",
    "skip.button": "Pular por ora →",
    "skip.none": "Nada mais disponível por enquanto.",
    "skip.failed": "falha ao pular: ",
    "status.generating": "gerando…",
    "gen.error": "erro de geração: ",
    "asked.prefix": "Você perguntou:",
    "iframe.interactive": "Conteúdo interativo",
    "iframe.exercise": "Exercício",
    "status.evaluating": "avaliando…",
    "answer.error": "erro: ",
    "completed": "Você concluiu o esboço atual.",
    "review.title": "Vamos revisar",
    "review.try": "Agora tente este:",
    "source.loading": "Carregando…",
    "source.unavailable": "Fonte indisponível",
    "status.objective": "pensando no objetivo…",
    "error.failed": "falhou: ",
    "status.curriculum": "planejando o currículo…",
    "note.add": "Adicionar nota",
    "setup.nav": "configuração",
    "setup.title": "Configuração do provedor",
    "setup.intro":
      "Traga sua própria chave (§12). Sua chave é armazenada localmente em um arquivo <code>0600</code> (nunca um banco de dados, nunca é commitada) e nunca sai desta máquina exceto para o provedor que você escolher. Escolha gratuito ou pago — o app deriva ambas as faixas de modelo para você.",
    "provider.legend": "Provedor",
    "provider.label": "Qual provedor?",
    "provider.demo": "Demonstração (sem chave — conteúdo predefinido)",
    "provider.openrouter": "OpenRouter (recomendado)",
    "provider.compat": "Endpoint compatível com OpenAI (Mercury, local, …)",
    "baseurl.label": "URL base",
    "apikey.label": "Chave de API",
    "apikey.hint": "(uma chave já está salva — deixe em branco para mantê-la)",
    "budget.legend": "Orçamento",
    "budget.free": "Modelos gratuitos",
    "budget.paid": "Modelos pagos (melhor qualidade)",
    "save.button": "Salvar",
    "status.saving": "Salvando…",
    "status.demoSaved": "Salvo. Rodando em modo demonstração.",
    "status.saved": "Salvo e aplicado — nenhum reinício necessário.",
    "error.prefix": "Erro: ",
    "derived.demo": "Modo demonstração: conteúdo predefinido, sem modelos.",
    "derived.models": "Modelos — rápido: {0} · robusto: {1}",
  },
};

function i18nResolveLang() {
  let lang = null;
  try {
    lang = localStorage.getItem(I18N_STORAGE_KEY);
  } catch (e) {}
  if (lang && I18N_SUPPORTED.includes(lang)) return lang;
  return "en";
}

// Mutable: the switcher can change it at runtime.
let I18N_LANG = i18nResolveLang();

function currentLocale() {
  return I18N_LANG;
}

function i18nSetLang(lang) {
  if (!I18N_SUPPORTED.includes(lang)) lang = "en";
  I18N_LANG = lang;
  try {
    localStorage.setItem(I18N_STORAGE_KEY, lang);
  } catch (e) {}
  document.documentElement.lang = lang;
}
i18nSetLang(I18N_LANG);

// Translate a key for the current locale, falling back to English, then to
// the key itself (so an untranslated key is visible rather than blank).
// `t("key", a, b, …)` substitutes positional `{0}`, `{1}`, … placeholders.
function t(key, ...args) {
  const table = I18N[I18N_LANG] || I18N.en;
  let s =
    table[key] !== undefined
      ? table[key]
      : I18N.en[key] !== undefined
        ? I18N.en[key]
        : key;
  args.forEach((a, i) => {
    s = s.split("{" + i + "}").join(String(a));
  });
  return s;
}

// Fill every static, translatable element under `root` from the dictionary.
// `data-i18n` sets textContent (only for leaf elements, so markup survives);
// `data-i18n-html` sets innerHTML (trusted, static locale strings only);
// the `-placeholder`/`-title`/`-aria` variants set the matching attribute.
function applyI18n(root) {
  root = root || document;
  root.querySelectorAll("[data-i18n]").forEach((el) => {
    if (el.children.length) return;
    el.textContent = t(el.getAttribute("data-i18n"));
  });
  root.querySelectorAll("[data-i18n-html]").forEach((el) => {
    el.innerHTML = t(el.getAttribute("data-i18n-html"));
  });
  root.querySelectorAll("[data-i18n-placeholder]").forEach((el) => {
    el_set_attr(el, "placeholder", t(el.getAttribute("data-i18n-placeholder")));
  });
  root.querySelectorAll("[data-i18n-title]").forEach((el) => {
    el_set_attr(el, "title", t(el.getAttribute("data-i18n-title")));
  });
  root.querySelectorAll("[data-i18n-aria]").forEach((el) => {
    el_set_attr(el, "aria-label", t(el.getAttribute("data-i18n-aria")));
  });
}

function el_set_attr(el, name, value) {
  if (value) el.setAttribute(name, value);
  else el.removeAttribute(name);
}
