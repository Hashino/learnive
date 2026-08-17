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
