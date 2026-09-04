// learnive — chapter-match-failure remediation (S27g's designed-but-unbuilt
// cascade step 3, PLAN.md ~line 823): when `source::match_chapter` cannot
// place a Chapter anywhere in its book's confirmed table of contents, the
// sidebar (outline.js's badge) opens this modal instead of leaving the
// learner stuck on a permanently-locked item. Three terminal options, all
// user-initiated (never automatic — the project has already rejected an
// automatic "just regenerate the outline" fallback because it silently
// drops the failed chapter's coherence role in the reading list):
//   1. pick the page by hand — reuses the acervo TOC-candidate read
//      (GET .../acervo/toc/{bookId}) as a source of suggestions, plus a
//      manual page-number fallback for when nothing useful comes back;
//   2. skip the whole book (POST .../outline/{bookId}/skip_book);
//   3. restart cold start with the same topic (reuses documents.js's
//      newDocBtn reset + startForm submit).
// Depends on core.js (api/postJson/putJson/el/escapeHtml/state), i18n.js
// (t), outline.js (refreshOutline, state.allItems), documents.js (newDocBtn
// reset, startForm submit) — all loaded earlier.

(function () {
  const modal = el("chapterRemediateModal");
  let chapterId = null;
  let bookId = null;
  let bookTitle = "";
  // Inline-confirm state for the two irreversible-feeling actions (skip,
  // restart) — reset whenever the modal opens for a (possibly different)
  // chapter, so a half-confirmed click from a previous open never carries
  // over onto a different chapter's card.
  let skipConfirming = false;
  let restartConfirming = false;

  function openRemediate() {
    if (!modal.open) modal.showModal();
  }
  function closeRemediate() {
    if (modal.open) modal.close();
  }
  el("remediateCloseBtn").addEventListener("click", closeRemediate);
  modal.addEventListener("click", (e) => {
    if (e.target === modal) closeRemediate();
  });

  function setStatus(msg, cls) {
    el("remediateStatus").textContent = msg || "";
    el("remediateStatus").className = cls || "muted";
  }

  // Called from outline.js when a `chapter_match_failed` row is clicked.
  window.openChapterRemediate = function openChapterRemediate(id) {
    const chapter = (state.allItems || []).find((it) => it.id === id);
    if (!chapter) return;
    const book = (state.allItems || []).find((it) => it.id === chapter.parent_id);
    chapterId = id;
    bookId = book ? book.id : null;
    bookTitle = book ? book.title : "";
    el("remediateIntro").textContent = t(
      "remediate.intro",
      chapter.title,
      bookTitle,
    );
    el("remediateManualPage").value = "";
    setStatus("");
    skipConfirming = false;
    restartConfirming = false;
    el("remediateSkipBookBtn").textContent = t("remediate.skipBook.button");
    el("remediateRestartBtn").textContent = t("remediate.restart.button");
    openRemediate();
    loadRemediateToc();
  };

  // App-deduced candidates (S27k) → a dropdown of chapters to pick from;
  // nothing extracted → the manual page-number row is the only way in.
  // Never both visible at once.
  async function loadRemediateToc() {
    const picker = el("remediateTocPicker");
    const manualRow = el("remediateManualRow");
    const select = el("remediateTocSelect");
    select.innerHTML = "";
    picker.hidden = true;
    manualRow.hidden = true;
    if (!bookId) {
      el("remediateTocStatus").textContent = "";
      manualRow.hidden = false;
      return;
    }
    el("remediateTocStatus").textContent = t("remediate.pickPage.loading");
    try {
      const resp = await api(`/api/documents/${state.docId}/acervo/toc/${bookId}`);
      if (!resp.ok) throw new Error(await resp.text());
      const data = await resp.json();
      const entries = (data.entries || []).filter((e) => e.page != null);
      if (entries.length) {
        el("remediateTocStatus").textContent = "";
        select.replaceChildren(
          ...entries.map((entry) => {
            const opt = document.createElement("option");
            opt.value = String(entry.page);
            opt.textContent = entry.title + " — " + t("remediate.pickPage.page", entry.page);
            return opt;
          }),
        );
        picker.hidden = false;
      } else {
        el("remediateTocStatus").textContent = t("remediate.pickPage.empty");
        manualRow.hidden = false;
      }
    } catch (err) {
      el("remediateTocStatus").innerHTML =
        '<span class="error">' + t("error.prefix") + escapeHtml(String(err)) + "</span>";
      manualRow.hidden = false;
    }
  }

  el("remediateTocConfirmBtn").addEventListener("click", () => {
    const page = parseInt(el("remediateTocSelect").value, 10);
    submitResolvedPage(page);
  });

  async function submitResolvedPage(page) {
    if (!chapterId || !Number.isFinite(page) || page < 1) return;
    setStatus(t("status.saving"));
    try {
      const resp = await putJson(
        `/api/documents/${state.docId}/outline/${chapterId}/resolved_page`,
        { page },
      );
      if (!resp.ok) throw new Error(await resp.text());
      await refreshOutline();
      closeRemediate();
    } catch (err) {
      setStatus(t("error.prefix") + String(err), "error");
    }
  }

  el("remediateManualBtn").addEventListener("click", () => {
    const page = parseInt(el("remediateManualPage").value, 10);
    submitResolvedPage(page);
  });

  // Skipping is destructive-ish (marks every chapter in the book skipped),
  // so it asks first — same inline-confirm idiom as `confirmDeleteDocument`
  // (documents.js), rendered right next to the button rather than a native
  // confirm() that would take the surrounding context off screen.
  el("remediateSkipBookBtn").addEventListener("click", async () => {
    if (!bookId) return;
    if (!skipConfirming) {
      skipConfirming = true;
      el("remediateSkipBookBtn").textContent = t("remediate.skipBook.confirm");
      return;
    }
    skipConfirming = false;
    el("remediateSkipBookBtn").textContent = t("remediate.skipBook.button");
    setStatus(t("status.saving"));
    try {
      const resp = await postJson(
        `/api/documents/${state.docId}/outline/${bookId}/skip_book`,
        {},
      );
      if (!resp.ok) throw new Error(await resp.text());
      const data = await resp.json();
      setOutlineItems(data.items);
      state.dueReview = data.due_review || null;
      renderOutline();
      renderRevisitHint();
      closeRemediate();
    } catch (err) {
      setStatus(t("error.prefix") + String(err), "error");
    }
  });

  // Restart is a brand-new document (no destructive edit of the stuck one,
  // §5's non-destructive principle) — the same topic re-submitted through
  // the ordinary cold-start form, "as if the user typed the same prompt and
  // pressed submit" (2026-08-30 feature request). `newDocBtn`'s own click
  // handler already does the full reset (clears state.docId, shows the
  // cold-start screen, focuses #topic); this just adds the topic text and
  // fires the submit.
  el("remediateRestartBtn").addEventListener("click", () => {
    if (!restartConfirming) {
      restartConfirming = true;
      el("remediateRestartBtn").textContent = t("remediate.restart.confirm");
      return;
    }
    restartConfirming = false;
    el("remediateRestartBtn").textContent = t("remediate.restart.button");
    const topic = (state.docs.find((d) => d.doc_id === state.docId) || {}).topic || "";
    closeRemediate();
    el("newDocBtn").click();
    el("topic").value = topic;
    el("startForm").requestSubmit();
  });
})();
