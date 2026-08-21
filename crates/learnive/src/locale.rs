//! Localization (§3, §15): the UI default is English, with pt-BR offered as
//! a selectable option. The client sends its current choice as the
//! `X-Learnive-Lang` request header (and `lang` query param for the
//! sandboxed frame endpoints, which carry no header); handlers that render
//! user-facing strings read it through `Locale::from_*`. `pick` returns the
//! right string for a locale without a big per-callsite match.

use axum::http::HeaderMap;

/// The two locales the UI ships with. `En` is the baseline and the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Locale {
    #[default]
    En,
    PtBr,
}

impl Locale {
    /// Resolve a locale from the `X-Learnive-Lang` request header.
    pub fn from_header(headers: &HeaderMap) -> Locale {
        Locale::from_str_opt(headers.get("x-learnive-lang").and_then(|v| v.to_str().ok()))
    }

    /// Resolve a locale from an optional query/string value (used by the
    /// sandboxed frame endpoints, which receive `lang` as a query param).
    pub fn from_str_opt(value: Option<&str>) -> Locale {
        value.map(Locale::parse).unwrap_or_default()
    }

    fn parse(s: &str) -> Locale {
        match s.trim().to_ascii_lowercase().as_str() {
            "pt-br" | "pt_br" | "pt" => Locale::PtBr,
            _ => Locale::En,
        }
    }

    /// BCP-47 code for this locale (also what the client sends back).
    pub fn as_code(self) -> &'static str {
        match self {
            Locale::En => "en",
            Locale::PtBr => "pt-BR",
        }
    }

    /// Human-readable target-language name for content directives sent to
    /// the model — distinct from [`Locale::as_code`] (machine-facing
    /// BCP-47), this is prose meant to be read by the model in a sentence
    /// like "write everything in {name}".
    fn language_name(self) -> &'static str {
        match self {
            Locale::En => "English",
            Locale::PtBr => "Brazilian Portuguese (pt-BR)",
        }
    }
}

impl std::fmt::Display for Locale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_code())
    }
}

/// Choose the localized form for `locale`, falling back to `en`. The two
/// strings are the same literal in different languages, so this stays a
/// trivial, legible two-branch pick rather than a hidden dictionary lookup.
pub fn pick<'a>(locale: Locale, en: &'a str, pt: &'a str) -> &'a str {
    match locale {
        Locale::PtBr => pt,
        Locale::En => en,
    }
}

/// Deterministic content-language directive for agent prompts, threaded
/// from the UI's selected locale rather than inferred from the request
/// text. Before this existed, every move-generation prompt
/// (`movement::prompt`) carried NO language instruction at all — only the
/// cold-start prompts (`propose_objective`/`propose_outline`) told the
/// model to match the request's language — so a document's outline could
/// come out correctly in pt-BR while its nodes drifted into English mid-
/// document (live report, 2026-08-20). Deliberately silent about anything
/// that is a machine contract rather than learner-facing prose: move-type
/// names, JSON keys/enum values, and literal sentinel markers a prompt
/// specifies verbatim must never be translated, or the server-side parser
/// for that prompt breaks.
pub fn language_directive(locale: Locale) -> String {
    format!(
        "Write everything the learner will read — prose, headings, exercise \
         text and options, feedback, citations' surrounding text — in {}. \
         This does NOT apply to anything that is a literal marker, JSON key, \
         enum value, CSS class name, or move-type name this prompt specifies \
         verbatim (e.g. <!--move: ...-->, \"html\", \"graded\", \"objectives\", \
         \"not_demonstrated\", \"callout\"): keep every one of those exactly \
         as written, unchanged, regardless of this instruction.",
        locale.language_name()
    )
}
