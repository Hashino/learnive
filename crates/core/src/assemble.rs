//! Dialect assembly from generated content (§4.2/§4.3).
//!
//! The model generates semantic prose blocks **without** IDs; the server assigns
//! a stable `data-block-id` to each top-level block, guaranteeing control and
//! uniqueness of the IDs (the §4.3 anchoring depends on them). It is dialect
//! logic, I/O-free, so it lives in the wasm-safe core.

use scraper::{ElementRef, Html, Node, Selector};

/// Assigns a sequential `data-block-id` (`{prefix}{n}`) to each top-level element
/// of the HTML that does not already have one. Reserializes via `scraper`.
///
/// Walks every direct child of the wrapper, not just `> *` elements: a model
/// can emit a top-level run of bare text with no wrapping tag at all (no
/// stray `<p>`, nothing), and html5ever's fragment parser does not imply one
/// under a `<div>` context — that text stays a bare text node, invisible to
/// an element selector, and silently vanished from the output. Wrap it in a
/// synthetic `<p>` of its own so it gets an id and survives.
pub fn ensure_block_ids(inner_html: &str, prefix: &str) -> String {
    let wrapped = format!(r#"<div id="__lv_root">{inner_html}</div>"#);
    let frag = Html::parse_fragment(&wrapped);
    let root_sel = Selector::parse("#__lv_root").expect("static selector");
    let root = frag
        .select(&root_sel)
        .next()
        .expect("wrapper div always present");

    let mut out = String::new();
    let mut n = 1;
    for child in root.children() {
        match child.value() {
            Node::Element(_) => {
                let el = ElementRef::wrap(child).expect("element node wraps");
                let html = el.html();
                if el.value().attr("data-block-id").is_some() {
                    out.push_str(&html);
                } else {
                    out.push_str(&inject_attr(&html, &format!("{prefix}{n}")));
                    n += 1;
                }
                out.push('\n');
            }
            Node::Text(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                out.push_str(&format!(
                    r#"<p data-block-id="{prefix}{n}">{}</p>"#,
                    escape_text(trimmed)
                ));
                out.push('\n');
                n += 1;
            }
            _ => {}
        }
    }
    out.trim_end().to_string()
}

fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;")
}

/// The top-level element children of `inner_html`, as `(tag_name, html)` in
/// document order — the same walk [`ensure_block_ids`] uses for its block
/// numbering, so a block index minted by one of these helpers means the same
/// element in the other. Bare text runs are NOT elements and don't consume an
/// index (they have no tag for a citation to attach to).
fn top_level_elements(inner_html: &str) -> Vec<(String, String)> {
    let wrapped = format!(r#"<div id="__lv_root">{inner_html}</div>"#);
    let frag = Html::parse_fragment(&wrapped);
    let root_sel = Selector::parse("#__lv_root").expect("static selector");
    let root = frag
        .select(&root_sel)
        .next()
        .expect("wrapper div always present");

    root.children()
        .filter_map(|child| {
            ElementRef::wrap(child).map(|el| (el.value().name().to_string(), el.html()))
        })
        .collect()
}

/// Numbers a move's top-level element blocks (`[1] …`, `[2] …`, one per
/// line) — the form `movement::grounding`'s post-generation verification
/// prompt presents GENERATED in, so the model's citation mapping can
/// reference block numbers instead of quoting HTML back. The numbering is
/// the same walk [`ensure_block_ids`] uses, and matches
/// [`insert_block_citations`]'s block numbers exactly: `[N]` here is the
/// Nth top-level element there.
pub fn numbered_blocks(inner_html: &str) -> String {
    top_level_elements(inner_html)
        .iter()
        .enumerate()
        .map(|(i, (_, html))| format!("[{}] {}", i + 1, html))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Appends empty `<cite data-source-id data-locator></cite>` markers at the
/// end of the named top-level blocks (`citations` entries are
/// `(block_number, source_id, locator)`, 1-based against
/// [`numbered_blocks`]' numbering). The markers are EMPTY on purpose — the
/// locator renders from the attribute (`app.css`'s `.prose cite::before`),
/// the same shape the model used to be asked to emit; now the server emits
/// them itself from the verification call's mapping, so a citation can only
/// ever point at a source the server itself selected. Block numbers outside
/// the document are silently dropped (the caller validates source/locator;
/// this validates placement). Bare text runs are wrapped in a synthetic
/// `<p>` exactly like [`ensure_block_ids`] does, so nothing is lost between
/// this pass and the block-id pass that follows it. Reserializes via
/// `scraper`, like every other helper here.
pub fn insert_block_citations(inner_html: &str, citations: &[(usize, &str, &str)]) -> String {
    let elements = top_level_elements(inner_html);
    let mut markers: Vec<Vec<String>> = vec![Vec::new(); elements.len()];
    for (b, id, loc) in citations {
        if let Some(slot) = markers.get_mut(b.checked_sub(1).unwrap_or(usize::MAX)) {
            slot.push(format!(
                r#"<cite data-source-id="{id}" data-locator="{loc}"></cite>"#
            ));
        }
    }

    let wrapped = format!(r#"<div id="__lv_root">{inner_html}</div>"#);
    let frag = Html::parse_fragment(&wrapped);
    let root_sel = Selector::parse("#__lv_root").expect("static selector");
    let root = frag
        .select(&root_sel)
        .next()
        .expect("wrapper div always present");

    let mut out = String::new();
    let mut n = 0;
    for child in root.children() {
        match child.value() {
            Node::Element(_) => {
                let cited = &markers[n];
                n += 1;
                let el = ElementRef::wrap(child).expect("element node wraps");
                if cited.is_empty() {
                    out.push_str(&el.html());
                } else {
                    let name = el.value().name();
                    let html = el.html();
                    let close = format!("</{name}");
                    // rfind, not find: nested children close before their
                    // parent, so the LAST `</name` in the serialized element
                    // is this element's own closing tag.
                    match html.rfind(&close) {
                        Some(pos) => {
                            out.push_str(&html[..pos]);
                            for m in cited {
                                out.push_str(m);
                            }
                            out.push_str(&html[pos..]);
                        }
                        // Void element (no closing tag): marker after it.
                        None => {
                            out.push_str(&html);
                            for m in cited {
                                out.push_str(m);
                            }
                        }
                    }
                }
                out.push('\n');
            }
            Node::Text(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                out.push_str(&format!(r#"<p>{}</p>"#, escape_text(trimmed)));
                out.push('\n');
            }
            _ => {}
        }
    }
    out.trim_end().to_string()
}

/// Reconstructs just the frozen prose blocks from a node's stored
/// `content.html` — every top-level element that carries `data-block-id`
/// but not `data-exercise-id`, in document order, re-serialized. Used to
/// split the exercise back out on read (`api::get_node`, §S5/§4.3): the
/// exercise wrapper `ensure_form_ids` (`engine.rs`) produces is not always a
/// bare `<form>` (the model may wrap it, e.g. `<div><p>question</p><form>…`),
/// so finding it by searching for the `<form` substring leaves that
/// wrapper's non-block prefix dangling in the split-off prose. Selecting on
/// the id attribute instead of a tag-name substring is exact regardless of
/// what the model wrapped the form in.
///
/// The exercise form *does* carry its own `data-block-id` (§4.3, reused from
/// its `exercise_id` — `ensure_form_ids`) so it is addressable for anchoring
/// and the reading line like any other block. It must still never end up in
/// `content_html`, which is inserted straight into the app origin unsandboxed
/// (§4.4) — the exercise only ever renders sandboxed, via its own
/// `exercise-frame` endpoint — so the `data-exercise-id` marker excludes it
/// here explicitly, rather than relying on the absence of a block id.
pub fn prose_blocks_only(content_html: &str) -> String {
    let wrapped = format!(r#"<div id="__lv_root">{content_html}</div>"#);
    let frag = Html::parse_fragment(&wrapped);
    let sel = Selector::parse("#__lv_root > *").expect("static selector");

    let mut out = String::new();
    for el in frag.select(&sel) {
        let is_block = el.value().attr("data-block-id").is_some();
        let is_exercise = el.value().attr("data-exercise-id").is_some();
        if is_block && !is_exercise {
            out.push_str(&el.html());
            out.push('\n');
        }
    }
    out.trim_end().to_string()
}

/// Extracts a single top-level content-layer element's raw HTML by its
/// `data-block-id` (§4.3) — the interactive-island counterpart of the
/// exercise-by-sidecar lookup: `api::block_frame` calls this to serve one
/// `data-interactive` block (§4.4) through its own sandboxed frame, the same
/// pattern `api::exercise_frame` uses for the graded exercise.
pub fn extract_block_by_id(content_html: &str, block_id: &str) -> Option<String> {
    let wrapped = format!(r#"<div id="__lv_root">{content_html}</div>"#);
    let frag = Html::parse_fragment(&wrapped);
    let sel = Selector::parse("#__lv_root > *").expect("static selector");

    frag.select(&sel)
        .find(|el| el.value().attr("data-block-id") == Some(block_id))
        .map(|el| el.html())
}

/// Finds an element by `data-block-id` **anywhere** in the fragment, not only
/// at the top level, and returns its raw HTML.
///
/// The top-level-only [`extract_block_by_id`] is right for the content layer,
/// where blocks are the frozen top-level elements by construction. An
/// interaction item is not shaped that way: its `body_html` is a header plus
/// a wrapper around the answer, and the anchorable paragraphs sit one level
/// down inside that wrapper (§4.3 — an answer is several paragraphs, each one
/// a place the learner can ask about).
pub fn find_block_html(html: &str, block_id: &str) -> Option<String> {
    let frag = Html::parse_fragment(html);
    let sel = Selector::parse("[data-block-id]").expect("static selector");
    frag.select(&sel)
        .find(|el| el.value().attr("data-block-id") == Some(block_id))
        .map(|el| el.html())
}

/// Empties every top-level `data-interactive` block (§4.4) in place: keeps
/// its opening tag and attributes (so the client can still find it by
/// `data-block-id` and hydrate it) but drops its raw HTML/JS content. The
/// frozen original is only ever read back through `extract_block_by_id`, for
/// the sandboxed frame route — it must never reach `content_html`, which is
/// inserted into the app origin and only lightly re-sanitized there (§3.1).
pub fn redact_interactive_blocks(content_html: &str) -> String {
    let wrapped = format!(r#"<div id="__lv_root">{content_html}</div>"#);
    let frag = Html::parse_fragment(&wrapped);
    let sel = Selector::parse("#__lv_root > *").expect("static selector");

    let mut out = String::new();
    for el in frag.select(&sel) {
        if el.value().attr("data-interactive").is_some() {
            out.push_str(&opening_tag(el.value()));
            out.push_str("</");
            out.push_str(el.value().name());
            out.push('>');
        } else {
            out.push_str(&el.html());
        }
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// Freezes a generated exercise (§8) for read-only display in the
/// interaction layer (§4.3): unwraps a top-level `<form>` so the client's
/// blanket `sanitizeHtml` FORM removal (it strips the WHOLE subtree, §3.1)
/// doesn't take the exercise's own question text down with it — an
/// `EXERCISE_HTML_CONTRACT`-shaped exercise wraps its prose and fields
/// together in one `<form>`. The fields inside stay (inert once frozen,
/// already answered) but the tag that would let the block behave like a
/// live, resubmittable form is gone. A `<form>`-less exercise (the
/// interactive/bespoke-JS case) passes through untouched — its own
/// `<script>` still gets stripped downstream, same ceiling as any other
/// frozen interactive content (§4.4).
pub fn freeze_exercise_html(exercise_html: &str) -> String {
    let wrapped = format!(r#"<div id="__lv_root">{exercise_html}</div>"#);
    let frag = Html::parse_fragment(&wrapped);
    let sel = Selector::parse("#__lv_root > *").expect("static selector");

    let mut out = String::new();
    for el in frag.select(&sel) {
        if el.value().name() == "form" {
            out.push_str(&el.inner_html());
        } else {
            out.push_str(&el.html());
        }
        out.push('\n');
    }
    out.trim_end().to_string()
}

fn opening_tag(el: &scraper::node::Element) -> String {
    let mut s = format!("<{}", el.name());
    for (name, value) in el.attrs() {
        s.push(' ');
        s.push_str(name);
        s.push_str("=\"");
        s.push_str(&value.replace('&', "&amp;").replace('"', "&quot;"));
        s.push('"');
    }
    s.push('>');
    s
}

/// Inserts ` data-block-id="id"` right after the opening tag name.
fn inject_attr(html: &str, id: &str) -> String {
    let Some(rest) = html.strip_prefix('<') else {
        return html.to_string();
    };
    let tag_end = rest
        .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .unwrap_or(rest.len());
    let insert_at = 1 + tag_end;
    let mut s = String::with_capacity(html.len() + 32);
    s.push_str(&html[..insert_at]);
    s.push_str(&format!(r#" data-block-id="{id}""#));
    s.push_str(&html[insert_at..]);
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Node;

    #[test]
    fn assigns_ids_to_idless_blocks() {
        let inner = "<h2>Title</h2>\n<p>Paragraph one.</p>\n<p>Paragraph two.</p>";
        let out = ensure_block_ids(inner, "b");
        assert!(out.contains(r#"data-block-id="b1""#));
        assert!(out.contains(r#"data-block-id="b2""#));
        assert!(out.contains(r#"data-block-id="b3""#));
    }

    #[test]
    fn produces_parseable_node_with_three_blocks() {
        let inner = ensure_block_ids("<p>a</p><p>b</p>", "b");
        let html = format!(
            "<article data-node-id=\"n1\" data-doc-id=\"d1\">\
             <section data-layer=\"content\">{inner}</section>\
             <section data-layer=\"interaction\"></section></article>"
        );
        let node = Node::parse(&html).unwrap();
        assert_eq!(node.content.blocks.len(), 2);
        assert_eq!(node.content.blocks[0].id, "b1");
        assert_eq!(node.content.blocks[1].id, "b2");
    }

    #[test]
    fn bare_text_with_no_wrapping_element_is_not_silently_dropped() {
        // A model can emit a top-level run of prose with no block-level
        // wrapper at all — not even a stray <p>. html5ever's fragment
        // parser does not imply one under a <div> context, so an
        // element-only selector (`> *`) would leave it a bare text node
        // and drop it on the floor with no error. Confirmed by this test
        // failing against the old element-only-selector implementation.
        let inner = "Just some bare text, no tags at all.";
        let out = ensure_block_ids(inner, "b");
        assert!(
            out.contains("Just some bare text"),
            "bare text was dropped: {out:?}"
        );
        assert!(out.contains(r#"data-block-id="b1""#));
    }

    #[test]
    fn bare_text_mixed_with_element_blocks_keeps_both_and_orders_ids() {
        let inner = "Lead-in text.<p>Then a real paragraph.</p>";
        let out = ensure_block_ids(inner, "b");
        assert!(out.contains(r#"<p data-block-id="b1">Lead-in text.</p>"#));
        assert!(out.contains(r#"<p data-block-id="b2">Then a real paragraph.</p>"#));
    }

    #[test]
    fn numbered_blocks_numbers_elements_in_order_and_skips_bare_text() {
        let inner = "Lead-in text.<p>First.</p><div>Nested <em>markup</em>.</div>";
        let numbered = numbered_blocks(inner);
        assert!(numbered.starts_with("[1] <p>First.</p>"));
        assert!(numbered.contains("[2] <div>Nested <em>markup</em>.</div>"));
        // The bare text run consumed no number: the citation mapping counts
        // elements only, and there is no element for a cite to attach to.
        assert!(!numbered.contains("Lead-in"));
    }

    #[test]
    fn numbered_and_insert_block_numbers_agree() {
        let inner = "<p>One.</p><h3>Two.</h3><p>Three.</p>";
        let count = numbered_blocks(inner).matches("[").count();
        let out = insert_block_citations(inner, &[(2, "hash1", "p:7")]);
        // Block 2 is the <h3>: its citation lands inside it, blocks 1/3
        // untouched.
        assert!(out.contains("<h3>Two.<cite"));
        assert!(out.contains(r#"data-locator="p:7""#));
        assert_eq!(count, 3);
    }

    #[test]
    fn insert_block_citations_appends_empty_cite_before_the_blocks_close_tag() {
        let inner = r#"<p>Claim one.</p><p>Nested <strong>text</strong> here.</p>"#;
        let out = insert_block_citations(inner, &[(2, "abc123", "p:42")]);
        assert!(
            out.contains(r#"<p>Nested <strong>text</strong> here.<cite data-source-id="abc123" data-locator="p:42"></cite></p>"#),
            "cite must sit before the block's own closing tag: {out:?}"
        );
        // Empty inner text: the locator renders from the attribute
        // (`app.css`'s `.prose cite::before`).
        assert!(out.contains(r#"></cite>"#));
        assert!(out.contains("<p>Claim one.</p>"));
    }

    #[test]
    fn insert_block_citations_drops_out_of_range_blocks_and_keeps_the_rest() {
        let inner = "<p>Only.</p>";
        let out =
            insert_block_citations(inner, &[(5, "h", "p:1"), (0, "h", "p:2"), (1, "h", "p:3")]);
        assert!(out.contains(r#"data-locator="p:3""#));
        assert!(!out.contains(r#"p:1"#));
        assert!(!out.contains(r#"p:2"#));
    }

    #[test]
    fn insert_block_citations_keeps_bare_text_and_orders_multiple_cites() {
        let inner = "Loose text.<p>Both pages.</p>";
        let out = insert_block_citations(inner, &[(1, "h1", "p:10"), (1, "h1", "p:11")]);
        assert!(
            out.contains("<p>Loose text.</p>"),
            "bare text preserved: {out:?}"
        );
        let one = out.find(r#"p:10"#).unwrap();
        let two = out.find(r#"p:11"#).unwrap();
        assert!(one < two, "same-block citations keep their given order");
    }

    #[test]
    fn keeps_existing_ids() {
        let inner = r#"<p data-block-id="keep">x</p><p>y</p>"#;
        let out = ensure_block_ids(inner, "b");
        assert!(out.contains(r#"data-block-id="keep""#));
        assert!(out.contains(r#"data-block-id="b1""#));
    }

    #[test]
    fn prose_blocks_only_drops_a_wrapped_exercise() {
        // The model doesn't always wrap the exercise in a bare <form> — a
        // surrounding <div>/<p> is common. A substring split on "<form"
        // would leave that wrapper's opening tags dangling in the prose.
        // The wrapper itself carries no data-block-id (only the exercise
        // deep inside it does, in real assembly), so it's excluded on that
        // basis alone here — the `data-exercise-id` exclusion is covered by
        // `prose_blocks_only_drops_an_id_tagged_exercise` below.
        let content = r#"<p data-block-id="b1">Hello</p>
<div><p>Question text</p><form data-exercise-id="ex1"><input></form></div>"#;
        let prose = prose_blocks_only(content);
        assert_eq!(prose, r#"<p data-block-id="b1">Hello</p>"#);
        assert!(!prose.contains("Question text"));
        assert!(!prose.contains("<form"));
    }

    #[test]
    fn prose_blocks_only_drops_an_id_tagged_exercise() {
        // The exercise form carries its own real data-block-id (§4.3,
        // `ensure_form_ids` reuses its exercise_id) so it's addressable for
        // anchoring/the reading line — but it must still never reach
        // content_html, which renders unsandboxed in the app origin (§4.4).
        let content = r#"<p data-block-id="b1">Hello</p>
<form data-block-id="ex1" data-exercise-id="ex1"><input></form>"#;
        let prose = prose_blocks_only(content);
        assert_eq!(prose, r#"<p data-block-id="b1">Hello</p>"#);
        assert!(!prose.contains("<form"));
    }

    #[test]
    fn extract_block_by_id_returns_the_raw_element() {
        let content = r#"<p data-block-id="b1">Hello</p>
<figure data-block-id="b2" data-interactive><script>alert(1)</script></figure>
<p data-block-id="b3">World</p>"#;
        let block = extract_block_by_id(content, "b2").unwrap();
        assert!(block.contains("<script>alert(1)</script>"));
        assert!(block.contains("data-interactive"));
        assert!(extract_block_by_id(content, "nope").is_none());
    }

    #[test]
    fn redact_interactive_blocks_empties_but_keeps_the_slot() {
        let content = r#"<p data-block-id="b1">Hello</p>
<figure data-block-id="b2" data-interactive><script>alert(document.cookie)</script></figure>
<p data-block-id="b3">World</p>"#;
        let redacted = redact_interactive_blocks(content);
        assert!(!redacted.contains("<script"));
        assert!(!redacted.contains("alert"));
        assert!(redacted.contains(r#"data-block-id="b2""#));
        assert!(redacted.contains("data-interactive"));
        assert!(redacted.contains("Hello"));
        assert!(redacted.contains("World"));
    }

    #[test]
    fn prose_blocks_only_keeps_a_bare_form_out_too() {
        let content = r#"<p data-block-id="b1">Hello</p>
<p data-block-id="b2">World</p>
<form data-exercise-id="ex1"><input></form>"#;
        let prose = prose_blocks_only(content);
        assert_eq!(
            prose,
            "<p data-block-id=\"b1\">Hello</p>\n<p data-block-id=\"b2\">World</p>"
        );
    }
}
