//! Anchoring (§4.3).
//!
//! Primary: by **stable block ID** — resolves deterministically because the
//! content layer is frozen. Sub-block (a span inside a block): **fuzzy quote**
//! anchor — exact quote + prefix/suffix context (W3C Web Annotation /
//! hypothes.is style). Since the block text is immutable, the fuzzy path is only
//! robustness against minimal whitespace normalization.
//!
//! All resolution is pure and I/O-free — exactly what gets compiled to wasm and
//! reused on the client.

use serde::{Deserialize, Serialize};

/// Quote selector for anchoring a span inside a block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteSelector {
    /// Exact selected span.
    pub exact: String,
    /// Context immediately before, to disambiguate repeated occurrences.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// Context immediately after, to disambiguate repeated occurrences.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suffix: Option<String>,
}

/// Anchor: a block, optionally refined to a span by quote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    pub block_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<QuoteSelector>,
}

impl Anchor {
    /// Whole-block anchor (no span).
    pub fn block(block_id: impl Into<String>) -> Self {
        Self {
            block_id: block_id.into(),
            quote: None,
        }
    }
}

/// Resolution result: the block and, when a quote is present, the byte range
/// `[start, end)` within the block text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedAnchor {
    pub block_id: String,
    pub range: Option<(usize, usize)>,
}

/// Resolves a quote against the (frozen) text of a block, returning the byte
/// range in the original text.
///
/// Strategy: (1) exact match — if unique, done; (2) if multiple exact matches,
/// disambiguate by prefix/suffix; (3) if no exact match, whitespace-flexible
/// search (§4.3 robustness).
pub fn resolve_quote(text: &str, quote: &QuoteSelector) -> Option<(usize, usize)> {
    if quote.exact.is_empty() {
        return None;
    }

    let exacts = find_all(text, &quote.exact);
    match exacts.len() {
        1 => {
            let start = exacts[0];
            Some((start, start + quote.exact.len()))
        }
        0 => find_flexible(text, &quote.exact),
        _ => {
            let start = disambiguate(text, &exacts, quote)?;
            Some((start, start + quote.exact.len()))
        }
    }
}

/// Byte indices of every (non-overlapping) occurrence of `needle`.
fn find_all(hay: &str, needle: &str) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut start = 0;
    while let Some(rel) = hay[start..].find(needle) {
        let idx = start + rel;
        out.push(idx);
        start = idx + needle.len();
    }
    out
}

/// Picks among multiple exact occurrences using prefix/suffix.
fn disambiguate(text: &str, matches: &[usize], quote: &QuoteSelector) -> Option<usize> {
    let end_of = |start: usize| start + quote.exact.len();
    let mut candidates = matches.iter().copied().filter(|&start| {
        let prefix_ok = quote
            .prefix
            .as_deref()
            .map(|p| text[..start].ends_with(p))
            .unwrap_or(true);
        let suffix_ok = quote
            .suffix
            .as_deref()
            .map(|s| text[end_of(start)..].starts_with(s))
            .unwrap_or(true);
        prefix_ok && suffix_ok
    });
    candidates.next()
}

/// Search where each run of whitespace in the text matches a single space in the
/// normalized `needle`. Returns the byte range in the original text. Used when
/// the exact match fails (e.g. whitespace collapsed on reflow).
fn find_flexible(hay: &str, needle: &str) -> Option<(usize, usize)> {
    let normalized = collapse_ws(needle.trim());
    if normalized.is_empty() {
        return None;
    }
    let needle_chars: Vec<char> = normalized.chars().collect();
    let hay_chars: Vec<(usize, char)> = hay.char_indices().collect();

    for start in 0..hay_chars.len() {
        if let Some(end) = try_match_at(&hay_chars, start, &needle_chars, hay.len()) {
            return Some((hay_chars[start].0, end));
        }
    }
    None
}

/// Tries to match `needle` (normalized) starting at `start` in `hay`. A space in
/// the needle matches one or more whitespace positions in the hay.
fn try_match_at(
    hay: &[(usize, char)],
    start: usize,
    needle: &[char],
    hay_len: usize,
) -> Option<usize> {
    let mut hi = start;
    let mut ni = 0;
    while ni < needle.len() {
        if needle[ni] == ' ' {
            if hi >= hay.len() || !hay[hi].1.is_whitespace() {
                return None;
            }
            while hi < hay.len() && hay[hi].1.is_whitespace() {
                hi += 1;
            }
            ni += 1;
        } else {
            if hi >= hay.len() || hay[hi].1 != needle[ni] {
                return None;
            }
            hi += 1;
            ni += 1;
        }
    }
    Some(if hi < hay.len() { hay[hi].0 } else { hay_len })
}

/// Collapses runs of whitespace into a single space. Does not trim the ends (the
/// caller trims the `needle` first).
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(exact: &str) -> QuoteSelector {
        QuoteSelector {
            exact: exact.to_string(),
            prefix: None,
            suffix: None,
        }
    }

    #[test]
    fn exact_single_match() {
        let text = "First paragraph about limits.";
        let (s, e) = resolve_quote(text, &q("limits")).unwrap();
        assert_eq!(&text[s..e], "limits");
    }

    #[test]
    fn missing_quote_is_none() {
        let text = "nothing here";
        assert!(resolve_quote(text, &q("absent")).is_none());
    }

    #[test]
    fn whitespace_flexible_fallback() {
        let text = "First paragraph about limits.";
        // The quote asks for two spaces; the text has only one — flexible match.
        let (s, e) = resolve_quote(text, &q("paragraph  about")).unwrap();
        assert_eq!(collapse_ws(&text[s..e]), "paragraph about");
    }

    #[test]
    fn disambiguate_by_prefix() {
        let text = "alfa X beta X gama";
        let quote = QuoteSelector {
            exact: "X".to_string(),
            prefix: Some("alfa ".to_string()),
            suffix: None,
        };
        let (s, e) = resolve_quote(text, &quote).unwrap();
        assert_eq!(s, 5); // second character after "alfa "
        assert_eq!(&text[s..e], "X");
    }

    #[test]
    fn disambiguate_by_suffix() {
        let text = "alfa X beta X gama";
        let quote = QuoteSelector {
            exact: "X".to_string(),
            prefix: None,
            suffix: Some(" gama".to_string()),
        };
        let (s, e) = resolve_quote(text, &quote).unwrap();
        assert_eq!(&text[s..e], "X");
        // Must be the second occurrence (the one followed by " gama").
        assert_eq!(&text[..s], "alfa X beta ");
    }
}
