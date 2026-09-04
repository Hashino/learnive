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
    // Unified outline confirmation (§S15/§S16): one tree, prerequisites
    // first, ending in the requested topic's own breakdown (locked).
    "outline.mainlineTitle": "What you'll learn",
    "prereq.hint":
      "Mark what you already know as review or skip. What you asked for is always taught in full and can't be skipped.",
    "prereq.confirm": "Confirm",
    "prereq.action.learn": "Learn",
    "prereq.action.review": "Review",
    "prereq.action.skip": "Skip",
    "prereq.knownIn": "known in {0}",
    "prereq.reviewBadge": "(review)",
    // "What are we learning next?" (§S15c) — transient, appended-to-document
    // continuation once the main line is fully demonstrated. Never persisted
    // (TODO futuros, 2026-08-18 decision): discarded the moment generation
    // of the new topic's first node begins.
    "nexttopic.title": "What are we learning next?",
    "nexttopic.button": "Continue",
    // §S15b step 4: provenance marker on an interaction item read through a
    // reference — only shown when it differs from the document being read.
    "interaction.askedIn": "asked while reading {0}",
    // Plan proposal
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
    "continue.button": "Continue →",
    "gen.error": "generation error: ",
    "asked.prefix": "You asked:",
    "iframe.interactive": "Interactive content",
    "iframe.exercise": "Exercise",
    "status.evaluating": "evaluating…",
    "answer.error": "error: ",
    "completed": "You have completed the current outline.",
    "review.title": "Let's review",
    "review.try": "Now try this one:",
    "practice.button": "Practice again",
    "practice.error": "could not start practice: ",
    "source.loading": "Loading…",
    "source.unavailable": "Source unavailable",
    "source.untitled": "Untitled source",
    // Acervo gate / PDF matching / TOC confirmation (§11.1, S27f) — an
    // on-demand check, never a blocking gate in this slice.
    "acervo.openBtn": "Library check",
    "acervo.title": "Library check",
    "acervo.hint": "What your reading list needs, matched against your local library.",
    "acervo.close": "Close",
    "acervo.recheck": "Re-check",
    "acervo.loading": "Checking your library…",
    "acervo.error": "could not check the library: ",
    "acervo.empty": "This document has no book or article sources yet.",
    "acervo.allGood": "Everything is in your library.",
    "acervo.status.found": "In library",
    "acervo.status.missing": "Missing",
    "acervo.checking": "Checking your reading list against your library…",
    "acervo.phase.queued": "Queued…",
    "acervo.phase.scanning": "Reading the library's PDFs…",
    "acervo.phase.presence": "Looking for a matching file…",
    "acervo.phase.identity": "Confirming it's really this work…",
    "acervo.phase.text_layer": "Checking for extractable text…",
    "acervo.phase.toc": "Reading the table of contents…",
    "acervo.phase.page_map": "Checking page numbering…",
    "acervo.phase.index": "Checking the retrieval index…",
    "acervo.libraryPath.label": "Put missing PDFs here:",
    "acervo.libraryPath.copy": "Copy path",
    "acervo.libraryPath.copied": "Copied",
    "acervo.filename": "File: {0}",
    "acervo.identityMismatch": "Possible wrong file: {0}",
    "acervo.noTextLayer": "No extractable text (scanned image?)",
    "acervo.extractorFailed":
      "This file has a text layer we couldn't read \u2014 the book is fine, our extractor isn't. Re-downloading won't help.",
    "acervo.reviewMatch": "Resolve match",
    "acervo.reviewToc": "Review chapters",
    "acervo.matches.title": "Match PDFs to your reading list",
    "acervo.matches.hint":
      "Some items match more than one file in your library, or a library file matches none of them yet.",
    "acervo.matches.back": "Back",
    "acervo.matches.none": "Nothing needs manual matching right now.",
    "acervo.matches.unmatchedTitle": "Unmatched library files",
    "acervo.matches.confidenceStrong": "likely match",
    "acervo.matches.confidenceWeak": "weak match",
    "acervo.matches.choose": "Use this file",
    "acervo.matches.current": "Currently matched: {0}",
    "acervo.matches.saved": "Saved.",
    "acervo.matches.error": "could not save the match: ",
    "acervo.toc.title": "Confirm table of contents",
    "acervo.toc.hint":
      "No bookmarks were found in this PDF — check the deduced chapters below before they're used.",
    "acervo.toc.back": "Back",
    "acervo.toc.sourceEmbedded": "From the PDF's own bookmarks",
    "acervo.toc.sourceDeduced": "Read from the printed contents page",
    "acervo.toc.sourceHeuristic": "Guessed from the text — please check it",
    "acervo.toc.sourceConfirmed": "Confirmed by you",
    "acervo.toc.sourceUnavailable": "Nothing detected yet — add chapters manually",
    "acervo.toc.add": "Add chapter",
    "acervo.toc.remove": "Remove",
    "acervo.toc.titlePlaceholder": "Chapter title",
    "acervo.toc.pagePlaceholder": "Page",
    "acervo.toc.moveUp": "Move up",
    "acervo.toc.moveDown": "Move down",
    "acervo.toc.save": "Save",
    "acervo.toc.saved": "Saved.",
    "acervo.toc.error": "could not save: ",
    "acervo.toc.needsAtLeastOne": "Add at least one chapter before saving.",
    "acervo.toc.unresolved":
      "This item's file isn't resolved yet — match it on the previous screen first.",
    // Status
    "status.objective": "thinking about the objective…",
    "error.failed": "failed: ",
    "status.curriculum": "planning the curriculum…",
    "note.add": "Add a note",
    // Settings dialog (§12)
    "settings.open": "Settings",
    "settings.title": "Settings",
    "settings.close": "Close",
    "settings.appearance": "Appearance",
    "settings.intro":
      "Bring your own key. Your key is stored locally in a <code>0600</code> file (never a database, never committed) and never leaves this machine except to the provider you choose. Confirming validates it against the real endpoint before it's saved.",
    "settings.advancedShow": "Show advanced options",
    "settings.advancedHide": "Hide advanced options",
    "settings.modelFast": "Fast-tier model",
    "settings.modelRobust": "Robust-tier model",
    "status.validating": "Validating provider…",
    "provider.legend": "Provider",
    "provider.label": "Which provider?",
    "provider.openrouter": "OpenRouter (recommended)",
    "provider.openai": "OpenAI",
    "provider.groq": "Groq",
    "provider.opencodeZen": "OpenCode Zen",
    "provider.cerebras": "Cerebras",
    "provider.custom": "Custom",
    "baseurl.label": "Base URL",
    "apikey.label": "API key",
    "apikey.hint": "(a key is already saved — leave blank to keep it)",
    "budget.legend": "Budget",
    "budget.free": "Free models",
    "budget.paid": "Paid models (better quality)",
    "save.button": "Save",
    "status.saving": "Saving…",
    "status.unconfiguredSaved": "Saved, but not connected yet — add a key to generate content.",
    "status.saved": "Saved and applied — no restart needs.",
    "error.prefix": "Error: ",
    "derived.unconfigured": "Not connected yet — no model configured.",
    "derived.models": "Models — fast: {0} · robust: {1}",
    // Chapter-match-failure remediation (S27g, PLAN.md ~line 823)
    "remediate.title": "Chapter not found",
    "remediate.intro":
      "“{0}” in “{1}” couldn't be matched to a page automatically.",
    "remediate.pickPage.heading": "Confirm the chapter/section page in the book",
    "remediate.pickPage.loading": "Loading candidate chapters…",
    "remediate.pickPage.empty": "No candidates with a page number were found — enter the page directly below.",
    "remediate.pickPage.page": "page {0}",
    "remediate.pickPage.manualPlaceholder": "Page",
    "remediate.pickPage.manualButton": "Confirm",
    "remediate.skipBook.heading": "Skip this book",
    "remediate.skipBook.desc":
      "Marks every chapter in this book as skipped and moves on. You can revisit it later.",
    "remediate.skipBook.button": "Skip the whole book",
    "remediate.skipBook.confirm": "Click again to confirm",
    "remediate.restart.heading": "Restart",
    "remediate.restart.desc": "Starts a fresh reading list for the same topic from scratch.",
    "remediate.restart.button": "Restart the process",
    "remediate.restart.confirm": "Click again to confirm",
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
    "prereq.hint":
      "Marque o que já sabe como revisar ou pular. O que você pediu é sempre ensinado por completo e não pode ser pulado.",
    "prereq.confirm": "Confirmar",
    "prereq.action.learn": "Aprender",
    "prereq.action.review": "Revisar",
    "prereq.action.skip": "Pular",
    "prereq.knownIn": "já aprendido em {0}",
    "prereq.reviewBadge": "(revisão)",
    "nexttopic.title": "O que vamos aprender agora?",
    "nexttopic.button": "Continuar",
    "interaction.askedIn": "perguntado lendo {0}",
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
    "continue.button": "Continuar →",
    "gen.error": "erro de geração: ",
    "asked.prefix": "Você perguntou:",
    "iframe.interactive": "Conteúdo interativo",
    "iframe.exercise": "Exercício",
    "status.evaluating": "avaliando…",
    "answer.error": "erro: ",
    "completed": "Você concluiu o esboço atual.",
    "review.title": "Vamos revisar",
    "review.try": "Agora tente este:",
    "practice.button": "Praticar novamente",
    "practice.error": "não foi possível iniciar a prática: ",
    "source.loading": "Carregando…",
    "source.unavailable": "Fonte indisponível",
    "source.untitled": "Fonte sem título",
    // Portão do acervo / casamento de PDFs / confirmação de sumário
    // (§11.1, S27f) — verificação sob demanda, nunca um portão bloqueante
    // nesta fatia.
    "acervo.openBtn": "Checar acervo",
    "acervo.title": "Checar acervo",
    "acervo.hint":
      "O que sua lista de leitura precisa, comparado com sua biblioteca local.",
    "acervo.close": "Fechar",
    "acervo.recheck": "Checar novamente",
    "acervo.loading": "Checando seu acervo…",
    "acervo.error": "não foi possível checar o acervo: ",
    "acervo.empty": "Este documento ainda não tem fontes de livro ou artigo.",
    "acervo.allGood": "Tudo está no seu acervo.",
    "acervo.status.found": "No acervo",
    "acervo.status.missing": "Faltando",
    "acervo.checking": "Checando sua lista de leitura contra seu acervo…",
    "acervo.phase.queued": "Na fila…",
    "acervo.phase.scanning": "Lendo os PDFs do acervo…",
    "acervo.phase.presence": "Procurando um arquivo correspondente…",
    "acervo.phase.identity": "Confirmando que é mesmo essa obra…",
    "acervo.phase.text_layer": "Checando se há texto extraível…",
    "acervo.phase.toc": "Lendo o sumário…",
    "acervo.phase.page_map": "Checando a numeração de páginas…",
    "acervo.phase.index": "Checando o índice de busca…",
    "acervo.libraryPath.label": "Coloque os PDFs que faltam aqui:",
    "acervo.libraryPath.copy": "Copiar caminho",
    "acervo.libraryPath.copied": "Copiado",
    "acervo.filename": "Arquivo: {0}",
    "acervo.identityMismatch": "Possível arquivo errado: {0}",
    "acervo.noTextLayer": "Sem texto extraível (digitalização de imagem?)",
    "acervo.extractorFailed":
      "Este arquivo tem camada de texto que não conseguimos ler \u2014 o livro está certo, o extrator é que falhou. Baixar de novo não resolve.",
    "acervo.reviewMatch": "Resolver correspondência",
    "acervo.reviewToc": "Revisar capítulos",
    "acervo.matches.title": "Casar PDFs com sua lista de leitura",
    "acervo.matches.hint":
      "Alguns itens correspondem a mais de um arquivo do acervo, ou um arquivo do acervo ainda não corresponde a nenhum deles.",
    "acervo.matches.back": "Voltar",
    "acervo.matches.none": "Nada precisa de correspondência manual agora.",
    "acervo.matches.unmatchedTitle": "Arquivos do acervo sem correspondência",
    "acervo.matches.confidenceStrong": "correspondência provável",
    "acervo.matches.confidenceWeak": "correspondência fraca",
    "acervo.matches.choose": "Usar este arquivo",
    "acervo.matches.current": "Atualmente correspondido: {0}",
    "acervo.matches.saved": "Salvo.",
    "acervo.matches.error": "não foi possível salvar a correspondência: ",
    "acervo.toc.title": "Confirmar sumário",
    "acervo.toc.hint":
      "Nenhum marcador foi encontrado neste PDF — confira os capítulos deduzidos abaixo antes que sejam usados.",
    "acervo.toc.back": "Voltar",
    "acervo.toc.sourceEmbedded": "Dos próprios marcadores do PDF",
    "acervo.toc.sourceDeduced": "Lido do sumário impresso",
    "acervo.toc.sourceHeuristic": "Deduzido do texto — confira, por favor",
    "acervo.toc.sourceConfirmed": "Confirmado por você",
    "acervo.toc.sourceUnavailable": "Nada detectado ainda — adicione capítulos manualmente",
    "acervo.toc.add": "Adicionar capítulo",
    "acervo.toc.remove": "Remover",
    "acervo.toc.titlePlaceholder": "Título do capítulo",
    "acervo.toc.pagePlaceholder": "Página",
    "acervo.toc.moveUp": "Mover para cima",
    "acervo.toc.moveDown": "Mover para baixo",
    "acervo.toc.save": "Salvar",
    "acervo.toc.saved": "Salvo.",
    "acervo.toc.error": "não foi possível salvar: ",
    "acervo.toc.needsAtLeastOne": "Adicione ao menos um capítulo antes de salvar.",
    "acervo.toc.unresolved":
      "O arquivo deste item ainda não foi resolvido — faça a correspondência na tela anterior primeiro.",
    "status.objective": "pensando no objetivo…",
    "error.failed": "falhou: ",
    "status.curriculum": "planejando o currículo…",
    "note.add": "Adicionar nota",
    "settings.open": "Configurações",
    "settings.title": "Configurações",
    "settings.close": "Fechar",
    "settings.appearance": "Aparência",
    "settings.intro":
      "Traga sua própria chave. Sua chave é armazenada localmente em um arquivo <code>0600</code> (nunca um banco de dados, nunca é commitada) e nunca sai desta máquina exceto para o provedor que você escolher. Ao confirmar, ela é validada contra o endpoint real antes de ser salva.",
    "settings.advancedShow": "Mostrar opções avançadas",
    "settings.advancedHide": "Ocultar opções avançadas",
    "settings.modelFast": "Modelo do tier rápido",
    "settings.modelRobust": "Modelo do tier robusto",
    "status.validating": "Validando provedor…",
    "provider.legend": "Provedor",
    "provider.label": "Qual provedor?",
    "provider.openrouter": "OpenRouter (recomendado)",
    "provider.openai": "OpenAI",
    "provider.groq": "Groq",
    "provider.opencodeZen": "OpenCode Zen",
    "provider.cerebras": "Cerebras",
    "provider.custom": "Personalizado",
    "baseurl.label": "URL base",
    "apikey.label": "Chave de API",
    "apikey.hint": "(uma chave já está salva — deixe em branco para mantê-la)",
    "budget.legend": "Orçamento",
    "budget.free": "Modelos gratuitos",
    "budget.paid": "Modelos pagos (melhor qualidade)",
    "save.button": "Salvar",
    "status.saving": "Salvando…",
    "status.unconfiguredSaved": "Salvo, mas ainda não conectado — adicione uma chave para gerar conteúdo.",
    "status.saved": "Salvo e aplicado — nenhum reinício necessário.",
    "error.prefix": "Erro: ",
    "derived.unconfigured": "Ainda não conectado — nenhum modelo configurado.",
    "derived.models": "Modelos — rápido: {0} · robusto: {1}",
    // Correção de capítulo sem correspondência (S27g, PLAN.md ~linha 823)
    "remediate.title": "Capítulo não encontrado",
    "remediate.intro":
      "“{0}” em “{1}” não pôde ser associado automaticamente a uma página.",
    "remediate.pickPage.heading": "Confirme a página do capítulo/seção no livro",
    "remediate.pickPage.loading": "Carregando capítulos candidatos…",
    "remediate.pickPage.empty": "Nenhum candidato com número de página foi encontrado — digite a página abaixo.",
    "remediate.pickPage.page": "página {0}",
    "remediate.pickPage.manualPlaceholder": "Página",
    "remediate.pickPage.manualButton": "Confirmar",
    "remediate.skipBook.heading": "Pular este livro",
    "remediate.skipBook.desc":
      "Marca todos os capítulos deste livro como pulados e segue em frente. Você pode revisitá-lo depois.",
    "remediate.skipBook.button": "Pular o livro inteiro",
    "remediate.skipBook.confirm": "Clique novamente para confirmar",
    "remediate.restart.heading": "Reiniciar",
    "remediate.restart.desc": "Começa uma nova lista de leitura do mesmo tema, do zero.",
    "remediate.restart.button": "Reiniciar o processo",
    "remediate.restart.confirm": "Clique novamente para confirmar",
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
