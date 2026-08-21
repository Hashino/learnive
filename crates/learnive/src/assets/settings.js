// learnive — settings dialog (§12): theme/language (moved out of the sidebar
// footer) and the BYOK provider form, both now living in one centered
// floating dialog (`#settingsModal`, a native <dialog>) instead of the old
// standalone /setup page. Depends on core.js (`api`, `el`) and i18n.js
// (`t`, `applyI18n`), both loaded first.

(function () {
  const modal = el("settingsModal");

  // --- Open/close --------------------------------------------------------
  // A native <dialog> centers itself and paints a backdrop for free
  // (`::backdrop` in app.css); it does not close on a backdrop click or
  // close by default, and the browser's own Escape handling is enough —
  // we only need to react to it. Cancel fires on Escape too.
  function openSettings() {
    if (!modal.open) modal.showModal();
  }
  function closeSettings() {
    if (modal.open) modal.close();
  }
  el("settingsBtn").addEventListener("click", openSettings);
  el("settingsCloseBtn").addEventListener("click", closeSettings);
  modal.addEventListener("click", (e) => {
    // A click that lands on the <dialog> element itself (not any of its
    // children) is a click on the backdrop area — dialogs have no padding,
    // so the element's own box is exactly the content area.
    if (e.target === modal) closeSettings();
  });

  // --- Section nav (left rail + right panel, common settings-window shape) -
  const navItems = [...document.querySelectorAll(".settings-nav-item")];
  const sections = [...document.querySelectorAll(".settings-section")];
  function showSection(id) {
    sections.forEach((s) => (s.hidden = s.id !== id));
    navItems.forEach((b) => b.classList.toggle("active", b.dataset.target === id));
  }
  navItems.forEach((b) =>
    b.addEventListener("click", () => showSection(b.dataset.target)),
  );
  showSection("appearanceSection");

  // --- BYOK form -----------------------------------------------------
  // Presets map a friendly choice to the (provider kind, base_url) pair the
  // backend actually stores — no server-side notion of "Groq" or "OpenAI"
  // exists, they're all just `openai_compatible` with a different base_url.
  // "openrouter" is the one exception: it's its own provider kind, not an
  // OpenAI-compatible base_url. "custom" is the same shape as any other
  // openai_compatible preset with an empty, user-editable base_url.
  // `freeTier`: does this provider have any model usable at zero cost? OpenAI's
  // API has no free models (paid-only, aside from limited new-account trial
  // credits that aren't a standing "free model") — the Budget choice is moot
  // there, so it's hidden rather than offering a "Free" option that can't
  // work. "custom" is left `true`: an arbitrary self-hosted/BYO endpoint might
  // well be free, and there's no way to know from a base_url alone.
  const PROVIDER_PRESETS = {
    openrouter: { kind: "openrouter", base_url: null, freeTier: true },
    openai: {
      kind: "openai_compatible",
      base_url: "https://api.openai.com/v1",
      freeTier: false,
    },
    groq: {
      kind: "openai_compatible",
      base_url: "https://api.groq.com/openai/v1",
      freeTier: true,
    },
    opencode_zen: {
      kind: "openai_compatible",
      base_url: "https://opencode.ai/zen/v1",
      freeTier: true,
    },
    cerebras: {
      kind: "openai_compatible",
      base_url: "https://api.cerebras.ai/v1",
      freeTier: true,
    },
    custom: { kind: "openai_compatible", base_url: null, freeTier: true },
  };

  // Reverse lookup for prefilling the select from the saved config: a known
  // base_url maps back to its preset id, anything else falls to "custom".
  function presetIdFor(providerKind, baseUrl) {
    if (providerKind === "openrouter") return "openrouter";
    const known = Object.entries(PROVIDER_PRESETS).find(
      ([id, p]) => id !== "custom" && p.kind === "openai_compatible" && p.base_url === baseUrl,
    );
    return known ? known[0] : "custom";
  }

  const form = el("setupForm");

  function syncFields() {
    const presetId = el("provider").value;
    // Only "Custom" needs a typed base_url — every other preset already
    // knows its own (PROVIDER_PRESETS), so the field only appears, and is
    // only ever editable, for Custom.
    el("baseUrlRow").hidden = presetId !== "custom";

    // The Free/Paid choice only makes sense where a free option actually
    // exists; a provider with no free tier is pinned to "paid" and the
    // choice itself disappears rather than offering a radio that can't work.
    const hasFreeTier = PROVIDER_PRESETS[presetId].freeTier;
    el("budgetChoice").hidden = !hasFreeTier;
    if (!hasFreeTier) {
      const paid = [...document.getElementsByName("intent")].find(
        (r) => r.value === "paid",
      );
      if (paid) paid.checked = true;
    }
  }
  el("provider").addEventListener("change", syncFields);

  el("advancedToggle").addEventListener("click", () => {
    const box = el("advancedBox");
    box.hidden = !box.hidden;
    // Keep `data-i18n` in sync too, not just the visible text: a later
    // language switch calls `applyI18n(document)` globally, which would
    // otherwise stomp this back to the closed-state label.
    const key = box.hidden ? "settings.advancedShow" : "settings.advancedHide";
    el("advancedToggle").dataset.i18n = key;
    el("advancedToggle").textContent = t(key);
  });

  function showStatus(s) {
    el("provider").value = presetIdFor(s.provider, s.base_url || "");
    el("baseUrl").value = s.base_url || "";
    for (const r of document.getElementsByName("intent"))
      r.checked = r.value === s.intent;
    el("keyHint").textContent = s.has_key ? t("apikey.hint") : "";
    el("derived").textContent = s.unconfigured
      ? t("derived.unconfigured")
      : t("derived.models", s.model_fast, s.model_robust);
    syncFields();
  }

  // Prefill from current config, and auto-open straight to the Provider
  // section when there's no working provider yet — the settings dialog is
  // the in-app replacement for a first-run setup gate.
  api("/api/setup")
    .then((r) => r.json())
    .then((s) => {
      showStatus(s);
      if (s.needs_setup) {
        openSettings();
        showSection("byokSection");
        el("provider").focus();
      }
    })
    .catch(() => {});

  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    const saveBtn = el("setupSaveBtn");
    saveBtn.disabled = true;
    el("setupMsg").textContent = t("status.validating");
    el("setupMsg").className = "muted";
    const intent = [...document.getElementsByName("intent")].find(
      (r) => r.checked,
    ).value;
    const presetId = el("provider").value;
    const preset = PROVIDER_PRESETS[presetId];
    const body = {
      provider: preset.kind,
      intent,
      base_url:
        preset.kind === "openai_compatible"
          ? (presetId === "custom" ? el("baseUrl").value : preset.base_url) || null
          : null,
      api_key: el("apiKey").value || null,
      model_fast: el("modelFast").value || null,
      model_robust: el("modelRobust").value || null,
    };
    try {
      const resp = await api("/api/setup", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
      if (!resp.ok) throw new Error(await resp.text());
      const s = await resp.json();
      el("apiKey").value = "";
      showStatus(s);
      el("setupMsg").textContent = s.unconfigured
        ? t("status.unconfiguredSaved")
        : t("status.saved");
      el("setupMsg").className = "ok";
    } catch (err) {
      el("setupMsg").textContent = t("error.prefix") + err;
      el("setupMsg").className = "err";
    } finally {
      saveBtn.disabled = false;
    }
  });
})();
