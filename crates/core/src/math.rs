//! LaTeX → MathML rendering for generated prose (§4.2).
//!
//! The model writes math in LaTeX — that is what its training data is made of,
//! and for anything past a subscript there is no honest HTML alternative
//! (`(a+b)/(c+d)` is not a fraction). Left alone it reaches the reader as
//! literal `$$\frac{1}{2}$$`. So the server converts it to **MathML**, which
//! every current browser renders natively: no JS, no web fonts, no external
//! asset, nothing for the §3.1 CSP to allow. The alternative — shipping KaTeX
//! or MathJax — would mean vendoring a few hundred KB of JS plus fonts into the
//! binary and loosening the CSP, for a job the backend can do once at write
//! time.
//!
//! **Runs at freeze time, not at read time** (`engine::assemble_node`). The
//! frozen content layer must be exactly what the reader sees, because §4.3
//! quote anchoring resolves a selection against the stored block text
//! (`node::Content::resolve`, which returns `None` — an HTTP 400 — on a miss).
//! Converting on the way out instead would leave the file holding
//! `$$\frac{1}{2}$$` while the page shows `½`, so selecting a formula and
//! asking about it would fail to anchor. The cost of this choice is that nodes
//! frozen before it existed keep their raw LaTeX: rewriting them is exactly the
//! destructive edit §5 forbids.
//!
//! Recognized delimiters, in precedence order: `$$…$$` and `\[…\]` (display),
//! `\(…\)` and `$…$` (inline). The math rule in
//! `engine::prompt::PROSE_HTML_CONTRACT` tells the model to use these and
//! nothing else — the same "fix the form so the server can find it" bargain
//! `ISLAND_CONTRACT` makes with its literal marker tags.
//! Anything that fails to parse is emitted unchanged rather than dropped: a
//! false positive must degrade to the status quo, never to lost content.

use pulldown_latex::{
    Parser, Storage,
    config::{DisplayMode, RenderConfig},
    push_mathml,
};

/// Elements whose text is source code, not prose — a `\frac` inside `<code>`
/// is being *shown*, not typeset, so delimiters there are left alone.
const RAW_TEXT_TAGS: [&str; 4] = ["code", "pre", "script", "style"];

/// Upper bound on a `$…$` span. Long enough for any real inline formula, short
/// enough that two unrelated dollar signs a paragraph apart can't pair up.
const INLINE_DOLLAR_MAX: usize = 200;

/// Definitions prepended to every fragment, for amsmath/KaTeX commands the
/// renderer doesn't know as primitives but models reach for anyway (`\boxed`
/// showed up in the first real equation this was tested against).
///
/// Each maps to its nearest *supported* meaning, and each one here is purely
/// decorative — dropping the box around an equation loses styling, not
/// mathematics. A command that changed the meaning would not belong here; it
/// belongs in the `merror` fallback, which shows the reader the source instead
/// of quietly rendering something else. Kept deliberately short: this is a
/// compatibility shim, not a LaTeX implementation.
const PREAMBLE: &str = r"\newcommand{\boxed}[1]{#1}";

/// Replaces LaTeX math in `html`'s text with MathML, leaving markup, attributes
/// and raw-text elements untouched.
///
/// Scans the serialized HTML rather than walking a parsed tree: the output must
/// be byte-identical to the input everywhere except the spans actually
/// converted. Re-serializing a `scraper` tree would silently renormalize
/// attribute quoting and entities across the whole document, which is a much
/// wider blast radius than this function is asking for.
pub fn render_math(html: &str) -> String {
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len());
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            // A tag: copy it verbatim, and for a raw-text element copy its
            // whole body too.
            b'<' => {
                let tag_end = match find(bytes, i, b'>') {
                    Some(e) => e + 1,
                    // Unterminated `<` (mid-stream truncation): nothing left to
                    // scan safely, take the rest as-is.
                    None => {
                        out.push_str(&html[i..]);
                        break;
                    }
                };
                out.push_str(&html[i..tag_end]);
                let consumed = tag_name(&html[i..tag_end])
                    .filter(|name| RAW_TEXT_TAGS.contains(&name.as_str()))
                    .and_then(|name| copy_raw_text(html, tag_end, &name));
                i = match consumed {
                    Some((body, end)) => {
                        out.push_str(body);
                        end
                    }
                    None => tag_end,
                };
            }
            // `\[`, `\(`, or an escaped delimiter (`\$`, `\\`) that must not be
            // mistaken for the start of one.
            b'\\' if i + 1 < bytes.len() => {
                let (open, close, mode) = match bytes[i + 1] {
                    b'[' => (2, "\\]", DisplayMode::Block),
                    b'(' => (2, "\\)", DisplayMode::Inline),
                    _ => {
                        out.push_str(&html[i..i + 2]);
                        i += 2;
                        continue;
                    }
                };
                i = emit_span(html, i, i + open, close, mode, &mut out);
            }
            b'$' => {
                let display = bytes.get(i + 1) == Some(&b'$');
                let (open, close, mode) = if display {
                    (2, "$$", DisplayMode::Block)
                } else {
                    (1, "$", DisplayMode::Inline)
                };
                let start = i + open;
                match find_close(html, start, close)
                    .filter(|&end| display || plausible_inline_dollar(html, start, end))
                {
                    Some(_) => i = emit_span(html, i, start, close, mode, &mut out),
                    None => {
                        out.push_str(&html[i..start]);
                        i = start;
                    }
                }
            }
            _ => {
                // Copy one whole char — indexing by byte would split UTF-8.
                let ch_len = char_len(bytes[i]);
                let end = (i + ch_len).min(html.len());
                out.push_str(&html[i..end]);
                i = end;
            }
        }
    }
    out
}

/// Converts the span between `content_start` and the next `close`, appending
/// the MathML (or, on any failure, the original text including its delimiters).
/// Returns the index to continue scanning from.
fn emit_span(
    html: &str,
    delim_start: usize,
    content_start: usize,
    close: &str,
    mode: DisplayMode,
    out: &mut String,
) -> usize {
    let Some(end) = find_close(html, content_start, close) else {
        // No closing delimiter: an opener quoted in prose, or a stream cut
        // short. Emit what we have and move past the opener.
        out.push_str(&html[delim_start..content_start]);
        return content_start;
    };
    let latex = &html[content_start..end];
    match to_mathml(latex, mode) {
        Some(mathml) => out.push_str(&mathml),
        None => out.push_str(&html[delim_start..end + close.len()]),
    }
    end + close.len()
}

/// Renders one LaTeX fragment. `None` when it does not parse — the caller then
/// keeps the original text, so unconvertible math is still readable.
fn to_mathml(latex: &str, display_mode: DisplayMode) -> Option<String> {
    if latex.trim().is_empty() {
        return None;
    }
    let storage = Storage::new();
    let source = format!("{PREAMBLE}{latex}");
    let parser = Parser::new(&source, &storage);
    let config = RenderConfig {
        display_mode,
        // Keeps the source in the output (§5: the model's own words survive the
        // transform) and gives assistive tech a text fallback. The annotation
        // is the author's LaTeX, never the shimmed `source`.
        annotation: Some(latex),
        ..RenderConfig::default()
    };
    let mut mathml = String::new();
    push_mathml(&mut mathml, parser, config).ok()?;
    // The renderer reports a bad command by styling it in `error_color` rather
    // than erroring, so a successful call is not proof the LaTeX was valid.
    // `merror` is the marker for that; fall back to the literal source.
    if mathml.contains("<merror") {
        return None;
    }
    Some(mathml)
}

/// Guards `$…$` against prose that merely contains dollar signs — currency
/// above all ("custa $5 e não $10"). Display `$$` needs no such guard: two
/// dollars in a row are not a price.
fn plausible_inline_dollar(html: &str, start: usize, end: usize) -> bool {
    let inner = &html[start..end];
    if inner.is_empty() || inner.len() > INLINE_DOLLAR_MAX || inner.contains('\n') {
        return false;
    }
    // Real math abuts its delimiters; "$5 e não $" does not.
    if inner.starts_with(char::is_whitespace) || inner.ends_with(char::is_whitespace) {
        return false;
    }
    // A price range pairs the two dollar signs of "$5 … $10" and would swallow
    // the text between them.
    if html[end + 1..].starts_with(|c: char| c.is_ascii_digit()) {
        return false;
    }
    // Digits and separators alone are a number, not a formula.
    !inner
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '.' | ',' | ' '))
}

/// Finds `close` after `from`, skipping an escaped occurrence (`\$`, `\)`).
fn find_close(html: &str, from: usize, close: &str) -> Option<usize> {
    let mut search = from;
    while let Some(rel) = html[search..].find(close) {
        let at = search + rel;
        if html[..at].ends_with('\\') && close != "\\]" && close != "\\)" {
            search = at + close.len();
            continue;
        }
        return Some(at);
    }
    None
}

/// Lowercased tag name of a just-copied opening tag, or `None` for a closing
/// tag, comment, or self-closing tag (which has no body to skip).
fn tag_name(tag: &str) -> Option<String> {
    let inner = tag.strip_prefix('<')?.strip_suffix('>')?;
    if inner.starts_with('/') || inner.starts_with('!') || inner.ends_with('/') {
        return None;
    }
    let name: String = inner
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    (!name.is_empty()).then(|| name.to_ascii_lowercase())
}

/// Body of a raw-text element starting at `from`, plus the index just past its
/// closing tag. `None` when unclosed — the caller then keeps scanning normally
/// rather than swallowing the remainder of the document.
fn copy_raw_text<'a>(html: &'a str, from: usize, name: &str) -> Option<(&'a str, usize)> {
    let needle = format!("</{name}");
    let rel = html[from..].to_ascii_lowercase().find(&needle)?;
    let close_start = from + rel;
    let end = find(html.as_bytes(), close_start, b'>')? + 1;
    Some((&html[from..end], end))
}

fn find(bytes: &[u8], from: usize, target: u8) -> Option<usize> {
    bytes[from..]
        .iter()
        .position(|&b| b == target)
        .map(|p| from + p)
}

/// Byte length of the UTF-8 char starting with `first`.
fn char_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_display_and_inline_delimiters() {
        for (input, mode) in [
            ("$$x^2$$", "block"),
            (r"\[x^2\]", "block"),
            (r"\(x^2\)", "inline"),
            ("$x^2$", "inline"),
        ] {
            let out = render_math(input);
            assert!(out.contains("<math"), "{input} produced {out}");
            // Really typeset, not just wrapped: `x` is an identifier and `2`
            // the superscript. (`x^2` still appears verbatim inside
            // `<annotation>` — that copy is deliberate, see `to_mathml`.)
            assert!(out.contains("<msup><mi>x</mi><mn>2</mn></msup>"), "{out}");
            if mode == "block" {
                assert!(out.contains("display=\"block\""), "{input} -> {out}");
            }
        }
    }

    #[test]
    fn keeps_surrounding_markup_and_prose_intact() {
        let out = render_math("<p data-block-id=\"b1\">A metade é $\\frac{1}{2}$ do todo.</p>");
        assert!(out.starts_with("<p data-block-id=\"b1\">A metade é "));
        assert!(out.ends_with(" do todo.</p>"));
        assert!(out.contains("<math"));
    }

    #[test]
    fn leaves_math_inside_code_and_pre_alone() {
        // Shown as source, not typeset — converting it would destroy the point
        // of the example.
        let input = "<pre><code>$\\frac{1}{2}$</code></pre>";
        assert_eq!(render_math(input), input);
    }

    #[test]
    fn does_not_eat_currency() {
        // The pathological case for `$…$`: two prices in one sentence would
        // otherwise pair up and swallow the words between them.
        let input = "<p>Custa $5 e não $10 por mês.</p>";
        assert_eq!(render_math(input), input);
        let input = "<p>Preço: $ 42 $ reais</p>";
        assert_eq!(render_math(input), input);
    }

    #[test]
    fn unparseable_latex_survives_as_text() {
        // Never silently drop content: a command the renderer rejects has to
        // reach the reader as the source it was.
        let out = render_math(r"$\notacommand{x}$");
        assert!(out.contains("notacommand"), "{out}");
    }

    #[test]
    fn unclosed_delimiter_does_not_swallow_the_document() {
        let out = render_math("<p>o custo é $ e nada mais</p>");
        assert!(out.contains("e nada mais</p>"), "{out}");
    }

    #[test]
    fn escaped_dollar_is_not_a_delimiter() {
        let out = render_math(r"<p>custa \$5 hoje</p>");
        assert!(!out.contains("<math"), "{out}");
    }

    #[test]
    fn preserves_non_ascii_prose() {
        // The scanner walks bytes; a multi-byte char must not be split.
        let input = "<p>Frações, ângulos e π são coisas distintas.</p>";
        assert_eq!(render_math(input), input);
    }

    #[test]
    fn real_generated_equation_from_a_stored_node() {
        // Verbatim from learnive-data (photosynthesis node) — the exact string
        // that reached a reader as literal LaTeX.
        let out = render_math(
            r"$$\boxed{6\,CO_2 + 6\,H_2O + \text{energia luminosa} \longrightarrow C_6H_{12}O_6 + 6\,O_2}$$",
        );
        assert!(out.contains("<math"), "{out}");
        // Typeset, not passed through: subscripted formulas and the arrow.
        assert!(out.contains("<msub><mi>O</mi><mn>2</mn></msub>"), "{out}");
        assert!(out.contains("⟶"), "{out}");
        assert!(out.contains("<mtext>energia luminosa</mtext>"), "{out}");
        // `\boxed` reached the renderer via PREAMBLE rather than erroring out;
        // it survives only inside the source annotation.
        assert!(!out.contains("<merror"), "{out}");
    }
}
