//! Shared lenient bibliographic-identity text matching (§11.1).
//!
//! Both the acervo gate (`acervo.rs`, S27c — strict: runs against a real
//! file, so a wrong file with a right-sounding filename must not sail
//! through) and the bibliographic-existence check (`bibliography.rs`, S27d —
//! deliberately lenient: "does something like this exist?", not "is this
//! exact edition correct") compare a proposed title/author against candidate
//! text using the same normalized-comparison rule (SPEC's own wording,
//! shared verbatim: lowercase, punctuation-insensitive, optional subtitle,
//! surname-only author matching). Extracted here so the two checks can't
//! silently drift apart into two slightly different notions of "matches" —
//! the *bar* each applies differs (contains-whole-string vs. either-way
//! containment, title-alone vs. title-and-author), but the primitives are one
//! shared implementation.

/// Lowercases, drops punctuation (keeping alphanumerics), and collapses
/// whitespace — the "comparação normalizada" SPEC asks for, for both title
/// and author matching.
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = true; // suppress a leading space
    for c in s.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim_end().to_string()
}

/// Drops an optional subtitle (SPEC: "subtítulo opcional") — the part after
/// the first colon, the common English/Portuguese convention for
/// "Title: Subtitle".
pub fn primary_title(title: &str) -> &str {
    title.split(':').next().unwrap_or(title).trim()
}

/// Last whitespace-separated token of an author string — "Michael Sipser" →
/// "Sipser". Assumes "First Last" order; good enough for matching, not a
/// bibliography formatter.
pub fn surname_of(author: &str) -> &str {
    author.split_whitespace().next_back().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_lowercases_and_strips_punctuation() {
        assert_eq!(
            normalize("Introduction, to THE Theory!!"),
            "introduction to the theory"
        );
    }

    #[test]
    fn primary_title_drops_an_optional_subtitle() {
        assert_eq!(primary_title("Calculus: An Intuitive Approach"), "Calculus");
        assert_eq!(primary_title("No Subtitle Here"), "No Subtitle Here");
    }

    #[test]
    fn surname_of_takes_the_last_token() {
        assert_eq!(surname_of("Michael Sipser"), "Sipser");
        assert_eq!(surname_of("Sipser"), "Sipser");
    }
}
