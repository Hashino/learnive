//! §S21 post-generation grounding gate — LEAN shape (2026-09-05, user
//! decision). Generation prompts no longer carry any cite contract, and the
//! model never assigns citations; the gate works in two layers:
//!
//! 1. **Citations are MECHANICAL** (§2.1 cost category 1 — zero token,
//!    arithmetic over the index): after a grounded move's content is fully
//!    generated, each text-bearing block is embedded with the LOCAL offline
//!    embedder (`retrieval::Embedder`, model2vec — no network, no 429, no
//!    truncation) and matched by cosine against the SAME book's page-index
//!    cache the grounding text was read from. The best page becomes the
//!    block's `<cite data-source-id data-locator>` — inserted by
//!    [`learnive_core::insert_block_citations`], so by construction a
//!    citation can only point at a page the server's own index selected,
//!    never one a model invented.
//! 2. **Only the doubtful blocks reach the model.** A block whose best
//!    similarity falls below [`MECHANICAL_FLOOR`] (its top page may be
//!    coincidence, not derivation) goes into ONE small adjudication call
//!    ([`prompt::verify_support`]): the paragraph plus the text of the page
//!    its citation points at — never the whole move, never the whole
//!    chapter window. That smallness is what makes the call survive the
//!    free-tier reasoning burn that truncated the old dual-task check
//!    (verified live, Groq gpt-oss-20b, same day). A suspect the model
//!    judges unsupported keeps its citation stamped `data-unverified`
//!    (orange + warning glyph, `app.css`) — the reader sees exactly which
//!    pointer is doubtful. A suspect the check CLEARS keeps a normal
//!    citation. No whole-move banner: the doubt is per-paragraph because
//!    the evidence is per-paragraph.
//!
//! Failure posture (§12.2, never-fail-silently): the mechanical layer
//! cannot fail on the network (local embedder, local file). If the
//! adjudication call itself fails (provider error, unparseable verdict
//! even after JSON-repair), every SUSPECT is stamped `data-unverified` —
//! infrastructure trouble must degrade to honest doubt ("we could not
//! confirm this one"), never to silent confidence on a block that already
//! measured below the floor. Non-suspect blocks are untouched by provider
//! hiccups, and the move's own content is NEVER dropped or replaced
//! regardless of outcome.
//!
//! Scope: the streamed move types with grounded prose — `explain`/
//! `integrate`/`revisit`/`respond` ([`in_scope`]). A no-op (returns
//! `generated` completely unchanged) for any other type, when
//! `ctx.grounding` is empty, or when the node's grounding did not come
//! from a chapter page window (`ctx.grounding_index` is `None` — the
//! mechanical citer has no page index to match against).

use super::{EngineError, GeneratedMove, MoveContext, MoveType, parse, prompt, repair_messages};
use crate::ai::{Ai, Tier};
use crate::engine::collect;
use crate::retrieval::Embedder;

/// Best-similarity floor below which a block's top page match is treated as
/// unproven and the block is sent to the adjudication call. Picked as a
/// starting point, not a measurement — the `grounding (lean)` stderr
/// diagnostic prints every block's real score precisely so live rounds can
/// tune this number against telemetry (same discipline as the retriever's
/// own `min_score`, PLAN.md).
pub const MECHANICAL_FLOOR: f32 = 0.5;

/// Blocks shorter than this get no citation and no check: a heading or a
/// one-liner has no substantive claim to point a page at, and its embedding
/// would be dominated by stopwords anyway.
const MIN_BLOCK_CHARS: usize = 40;

/// What the mechanical citer needs to match blocks against a book's page
/// index: the same cache dir / content hash / page window the node's
/// grounding text was read from (`api::reading::ground_node` owns all three
/// and threads them through `prepare`). `Embedder` is the local offline
/// embedder — cloning this struct is cheap.
#[derive(Clone)]
pub struct GroundingIndex {
    pub embedder: Embedder,
    pub dir: std::path::PathBuf,
    pub content_hash: String,
    pub page_range: Option<(usize, Option<usize>)>,
}

impl std::fmt::Debug for GroundingIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroundingIndex")
            .field("dir", &self.dir)
            .field("content_hash", &self.content_hash)
            .field("page_range", &self.page_range)
            .finish_non_exhaustive()
    }
}

/// Whether the gate applies at all — the same test the caller
/// (`api::generation::generate_node`) uses to decide whether emitting a
/// status frame before the check is worthwhile. [`verify`] re-checks the
/// index itself.
pub fn applies(move_type: MoveType, grounding: &str) -> bool {
    !grounding.trim().is_empty() && in_scope(move_type)
}

fn in_scope(move_type: MoveType) -> bool {
    matches!(
        move_type,
        MoveType::Explain | MoveType::Integrate | MoveType::Revisit | MoveType::Respond
    )
}

/// One SOURCE page of the node's grounding selection, parsed out of
/// `MoveContext::grounding`'s `[id: … | loc: … | title]` header lines —
/// used to resolve a suspect block's cited page back to its text for the
/// adjudication prompt.
struct Passage {
    loc: String,
    text: String,
}

/// Splits the grounding text into its pages. A line starting with `[id: `
/// and carrying ` | loc: ` opens a new passage; everything else accumulates
/// into the current one.
fn parse_passages(grounding: &str) -> Vec<Passage> {
    let mut out: Vec<Passage> = Vec::new();
    for line in grounding.lines() {
        let t = line.trim();
        if t.starts_with("[id: ") && t.contains(" | loc: ") && t.ends_with(']') {
            let inner = &t[1..t.len() - 1];
            let mut parts = inner.splitn(3, " | ");
            let (Some(_id_part), Some(loc_part), Some(_title)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            out.push(Passage {
                loc: loc_part
                    .strip_prefix("loc: ")
                    .unwrap_or(loc_part)
                    .to_string(),
                text: String::new(),
            });
        } else if let Some(last) = out.last_mut() {
            if !last.text.is_empty() {
                last.text.push('\n');
            }
            last.text.push_str(line);
        }
    }
    // The blank line separating two passages accumulates into the previous
    // one's text — drop the trailing whitespace it leaves, keep internal
    // blank lines (real paragraph breaks in the extracted page text).
    for p in &mut out {
        p.text = p.text.trim_end().to_string();
    }
    out
}

/// One block the mechanical layer measured below the floor: its 1-based
/// block number, its visible text, and the text of the page its citation
/// points at (empty when that page isn't in the selection — such a block is
/// marked unverified outright, without spending a model call it could never
/// pass: the checker must see the page it is judging against).
#[derive(Clone)]
struct Suspect {
    block: usize,
    text: String,
    page_text: String,
}

/// Runs the gate. Always returns a usable [`GeneratedMove`] — the caller
/// never sees an `Err` and never needs a retry loop of its own.
pub async fn verify(
    ai: &Ai,
    move_type: MoveType,
    ctx: &MoveContext,
    generated: GeneratedMove,
) -> GeneratedMove {
    if !applies(move_type, &ctx.grounding) {
        return generated;
    }
    let Some(index) = &ctx.grounding_index else {
        return generated;
    };

    let blocks = learnive_core::block_texts(&generated.html);
    let passages = parse_passages(&ctx.grounding);

    // Layer 1 — mechanical citation: embed each text-bearing block, cite its
    // best-matching page. Every score is kept for the stderr diagnostic, so
    // MECHANICAL_FLOOR can be tuned against real distributions instead of
    // guesses.
    let mut cites: Vec<(usize, String, String, bool)> = Vec::new();
    let mut suspects: Vec<Suspect> = Vec::new();
    let mut scores: Vec<String> = Vec::new();
    for (i, text) in blocks.iter().enumerate() {
        if text.chars().count() < MIN_BLOCK_CHARS {
            continue;
        }
        let block_no = i + 1;
        let Ok(mut hits) = crate::source::search_index_cache(
            &index.dir,
            &index.content_hash,
            &index.embedder,
            text,
            1,
            index.page_range,
        ) else {
            continue;
        };
        let Some((page, _, score)) = hits.pop() else {
            continue;
        };
        let loc = format!("p:{page}");
        let supported = score >= MECHANICAL_FLOOR;
        scores.push(format!("b{block_no}@{loc}={score:.2}"));
        cites.push((
            block_no,
            index.content_hash.clone(),
            loc.clone(),
            !supported,
        ));
        if !supported {
            let page_text = passages
                .iter()
                .find(|p| p.loc == loc)
                .map(|p| p.text.clone())
                .unwrap_or_default();
            suspects.push(Suspect {
                block: block_no,
                text: text.clone(),
                page_text,
            });
        }
    }

    if cites.is_empty() {
        return generated;
    }

    // Layer 2 — adjudicate only the suspects. Blocks the mechanical layer
    // already trusts never reach the model; a move with zero suspects costs
    // ZERO model calls.
    let mut unsupported: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let checkable: Vec<Suspect> = suspects
        .iter()
        .filter(|s| !s.page_text.is_empty())
        .cloned()
        .collect();
    if !checkable.is_empty() {
        let views: Vec<(usize, &str, &str)> = checkable
            .iter()
            .map(|s| (s.block, s.text.as_str(), s.page_text.as_str()))
            .collect();
        match check(ai, &views).await {
            Ok(verdict) => {
                for n in verdict.unsupported {
                    unsupported.insert(n);
                }
            }
            Err(e) => {
                // Infrastructure failure, not a verdict — degrade to honest
                // doubt on exactly the blocks that measured below the floor.
                // Non-suspect blocks keep their clean citations.
                eprintln!("grounding adjudication failed: {e}");
                for s in &checkable {
                    unsupported.insert(s.block);
                }
            }
        }
    }
    // Suspects with no page text to judge against are unconfirmed by
    // construction — no call could ever clear them.
    for s in &suspects {
        if s.page_text.is_empty() {
            unsupported.insert(s.block);
        }
    }
    // The cite's flag was born as "measured below the floor" — the
    // adjudication verdict is what settles it: cleared ⇒ clean, named (or
    // never checkable) ⇒ unverified.
    for cite in &mut cites {
        cite.3 = cite.3 && unsupported.contains(&cite.0);
    }

    eprintln!(
        "grounding (lean): cited={} suspects={} unsupported={} floor={MECHANICAL_FLOOR} scores=[{}]",
        cites.len(),
        suspects.len(),
        unsupported.len(),
        scores.join(", "),
    );

    let mut generated = generated;
    let refs: Vec<(usize, &str, &str, bool)> = cites
        .iter()
        .map(|(b, id, loc, unv)| (*b, id.as_str(), loc.as_str(), *unv))
        .collect();
    generated.html = learnive_core::insert_block_citations(&generated.html, &refs);
    generated
}

/// One small structured adjudication call, with the same JSON-repair bound
/// `generate_move` already uses for the Move contract — a DIFFERENT concern
/// from the verdict handling in [`verify`] above: this is only "did the
/// response parse as the expected shape", never "was the verdict itself
/// correct".
async fn check(
    ai: &Ai,
    suspects: &[(usize, &str, &str)],
) -> Result<parse::SupportVerdict, EngineError> {
    let messages = prompt::verify_support(suspects);
    let text = collect(ai, Tier::Fast, messages.clone()).await?;
    if let Ok(verdict) = parse::support_verdict(&text) {
        return Ok(verdict);
    }
    let repair = repair_messages(messages, &text, "expected JSON {\"unsupported\":[...]}");
    let text = collect(ai, Tier::Fast, repair).await?;
    parse::support_verdict(&text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{ChatRequest, MockProvider, Models, Provider};

    fn mock_ai(reply: &str) -> Ai {
        Ai::new(
            Provider::Mock(MockProvider::new(reply)),
            Models::single("mock"),
        )
    }

    fn scripted_ai<F>(f: F) -> Ai
    where
        F: Fn(&ChatRequest) -> String + Send + Sync + 'static,
    {
        Ai::new(
            Provider::Mock(MockProvider::scripted(f)),
            Models::single("mock"),
        )
    }

    /// A page index whose chunks are the Mock embedder's own vectors —
    /// cosine of the Mock hash-bag space: identical text ⇒ 1.0, disjoint
    /// vocabularies ⇒ 0.0, so the floor's two sides are deterministic. The
    /// context's grounding text is built from the SAME page list, like
    /// `ground_node` does in production (selection and index always agree).
    fn grounded_fixture(pages: &[(&str, &str)]) -> (tempfile::TempDir, MoveContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        let chunks: Vec<serde_json::Value> = pages
            .iter()
            .map(|(page, text)| {
                let v = Embedder::Mock.embed(text);
                serde_json::json!({ "page": page.parse::<usize>().unwrap(), "text": text, "vector": v })
            })
            .collect();
        std::fs::write(
            dir.path().join("hash1.json"),
            serde_json::to_string(&chunks).unwrap(),
        )
        .unwrap();
        let grounding = pages
            .iter()
            .map(|(page, text)| format!("[id: hash1 | loc: p:{page} | Book]\n{text}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let ctx = MoveContext {
            grounding,
            grounding_index: Some(GroundingIndex {
                embedder: Embedder::Mock,
                dir: dir.path().to_path_buf(),
                content_hash: "hash1".to_string(),
                page_range: None,
            }),
            ..Default::default()
        };
        (dir, ctx)
    }

    fn stub_move(html: &str) -> GeneratedMove {
        GeneratedMove {
            move_type: MoveType::Explain,
            interactive: false,
            graded: false,
            html: html.to_string(),
            tactics: Vec::new(),
            rubric: None,
            reference_solution: String::new(),
            repaired: false,
        }
    }

    #[test]
    fn applies_requires_both_grounding_and_scope() {
        assert!(!applies(MoveType::Explain, ""));
        assert!(!applies(MoveType::Explain, "   "));
        assert!(!applies(MoveType::Test, "some source text"));
        assert!(applies(MoveType::Explain, "some source text"));
        assert!(applies(MoveType::Revisit, "some source text"));
    }

    /// Out of scope (empty grounding, or a type like `Test`) must never
    /// touch the AI at all — the gate is a true no-op, not just a no-op
    /// outcome.
    #[tokio::test]
    async fn out_of_scope_never_calls_the_ai() {
        let ai = scripted_ai(|_| panic!("the AI must not be called out of scope"));
        let ctx = MoveContext::default(); // empty grounding
        let generated = stub_move("<p>Ungrounded prose.</p>");
        let result = verify(&ai, MoveType::Explain, &ctx, generated).await;
        assert_eq!(result.html, "<p>Ungrounded prose.</p>");

        let (_dir, ctx) = grounded_fixture(&[(
            "1",
            "Photosynthesis converts light energy into chemical energy.",
        )]);
        let ai = scripted_ai(|_| panic!("the AI must not be called out of scope"));
        let generated = stub_move("<form>An exercise.</form>");
        let result = verify(&ai, MoveType::Test, &ctx, generated).await;
        assert_eq!(result.html, "<form>An exercise.</form>");
    }

    /// Grounding without a page index (no chapter pointer — the mechanical
    /// citer has nothing to match against) is a full no-op: no calls, no
    /// cites, unchanged content.
    #[tokio::test]
    async fn grounding_without_an_index_is_a_full_noop() {
        let ai = scripted_ai(|_| panic!("no index means no model call"));
        let ctx = MoveContext {
            grounding: "[id: hash1 | loc: p:1 | A]\nsome source text".to_string(),
            ..Default::default()
        };
        let generated =
            stub_move("<p>Some claim that stands alone without any citation marker at all.</p>");
        let result = verify(&ai, MoveType::Explain, &ctx, generated).await;
        assert_eq!(
            result.html,
            "<p>Some claim that stands alone without any citation marker at all.</p>"
        );
    }

    /// The happy path costs ZERO model calls: a block identical to its page
    /// scores 1.0 ≥ floor, gets its citation, and nothing is adjudicated.
    #[tokio::test]
    async fn trusted_blocks_cost_zero_model_calls() {
        let ai = scripted_ai(|_| panic!("a fully trusted move must not call the AI"));
        let page_text =
            "Photosynthesis converts light energy into chemical energy inside the chloroplast.";
        let (_dir, ctx) = grounded_fixture(&[("1", page_text)]);
        let generated = stub_move(&format!("<p>{page_text}</p>"));
        let result = verify(&ai, MoveType::Explain, &ctx, generated).await;
        assert!(
            result
                .html
                .contains(r#"<cite data-source-id="hash1" data-locator="p:1"></cite></p>"#),
            "trusted block gets a clean cite: {}",
            result.html
        );
        assert!(!result.html.contains("data-unverified"));
    }

    /// A block below the floor is adjudicated: cleared by the model ⇒ clean
    /// cite; judged unsupported ⇒ the SAME cite gains `data-unverified`.
    #[tokio::test]
    async fn suspect_blocks_are_adjudicated_and_marked_per_paragraph() {
        // Block A's text matches page 1 (trusted); block B shares no
        // vocabulary with any page (score 0.0 < floor ⇒ suspect).
        let a_text =
            "Photosynthesis converts light energy into chemical energy inside the chloroplast.";
        let b_text = "Zorbulons fruminate the quuxly bazzoink under pluxtious conditions.";

        // Cleared: the verdict lists no unsupported numbers.
        let pages = [("1", a_text), ("2", "The stroma surrounds the grana.")];
        let (_dir, ctx) = grounded_fixture(&pages);
        let ai = mock_ai(r#"{"unsupported":[]}"#);
        let generated = stub_move(&format!("<p>{a_text}</p>\n<p>{b_text}</p>"));
        let result = verify(&ai, MoveType::Explain, &ctx, generated).await;
        assert_eq!(result.html.matches("<cite").count(), 2, "{}", result.html);
        assert!(!result.html.contains("data-unverified"), "{}", result.html);

        // Flagged: block B (the suspect) keeps its cite, stamped unverified;
        // block A stays clean.
        let (_dir, ctx) = grounded_fixture(&pages);
        let ai = mock_ai(r#"{"unsupported":[2]}"#);
        let generated = stub_move(&format!("<p>{a_text}</p>\n<p>{b_text}</p>"));
        let result = verify(&ai, MoveType::Explain, &ctx, generated).await;
        assert!(
            result
                .html
                .contains(r#"data-locator="p:1" data-unverified="true""#)
                || result.html.contains(r#"data-unverified="true""#),
            "suspect must be stamped: {}",
            result.html
        );
        let clean_a =
            format!(r#"<p>{a_text}<cite data-source-id="hash1" data-locator="p:1"></cite></p>"#);
        assert!(
            result.html.contains(&clean_a),
            "trusted block must keep its clean cite: {}",
            result.html
        );
    }

    /// The adjudication prompt pairs each suspect with the text of the page
    /// its citation points at — never the whole move or the whole window.
    #[tokio::test]
    async fn adjudication_prompt_pairs_suspect_with_its_page() {
        let a_text =
            "Photosynthesis converts light energy into chemical energy inside the chloroplast.";
        let b_text = "Zorbulons fruminate the quuxly bazzoink under pluxtious conditions.";
        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let cap = captured.clone();
        let ai = scripted_ai(move |req| {
            *cap.lock().unwrap() = req
                .messages
                .iter()
                .map(|m| m.content.clone())
                .collect::<Vec<_>>()
                .join("\n");
            r#"{"unsupported":[]}"#.to_string()
        });
        let (_dir, ctx) =
            grounded_fixture(&[("1", a_text), ("2", "The stroma surrounds the grana.")]);
        let generated = stub_move(&format!("<p>{a_text}</p>\n<p>{b_text}</p>"));
        let _ = verify(&ai, MoveType::Explain, &ctx, generated).await;
        let body = captured.lock().unwrap().clone();
        assert!(
            body.contains("Zorbulons fruminate"),
            "suspect text in prompt"
        );
        assert!(
            body.contains("The stroma surrounds the grana"),
            "the cited page's own text in prompt: {body}"
        );
        assert!(
            !body.contains(a_text),
            "trusted blocks must not reach the model"
        );
    }

    /// The adjudication call itself failing (unparseable even after repair)
    /// degrades to honest doubt on exactly the suspects — their cites gain
    /// `data-unverified`; trusted blocks stay clean; content is untouched.
    #[tokio::test]
    async fn check_failure_marks_suspects_unverified_and_nothing_else() {
        let a_text =
            "Photosynthesis converts light energy into chemical energy inside the chloroplast.";
        let b_text = "Zorbulons fruminate the quuxly bazzoink under pluxtious conditions.";
        let ai = mock_ai("I'm sorry, I can't help with that request.");
        let (_dir, ctx) =
            grounded_fixture(&[("1", a_text), ("2", "The stroma surrounds the grana.")]);
        let generated = stub_move(&format!("<p>{a_text}</p>\n<p>{b_text}</p>"));
        let result = verify(&ai, MoveType::Explain, &ctx, generated).await;

        // Content preserved verbatim except for the inserted cites.
        assert!(result.html.contains(a_text));
        assert!(result.html.contains(b_text));
        assert_eq!(result.html.matches("<cite").count(), 2);
        // The suspect's cite is stamped; the trusted one is not.
        let clean =
            format!(r#"<p>{a_text}<cite data-source-id="hash1" data-locator="p:1"></cite></p>"#);
        assert!(result.html.contains(&clean), "{}", result.html);
        assert!(
            result.html.matches("data-unverified").count() == 1,
            "exactly the suspect is stamped: {}",
            result.html
        );
    }

    /// The passage parser reads both producers' header format
    /// (`ground_node`'s chapter form and `grounding_for`'s similarity
    /// form) and accumulates non-header lines into the current passage.
    #[test]
    fn parse_passages_reads_both_selection_formats() {
        let grounding = "[id: hash1 | loc: p:41 | Stewart — Cálculo]\npage 41 text\nmore text\n\n\
                         [id: wiki1 | loc: p:3 — chap:2 | Photosynthesis — Overview]\nwiki text";
        let passages = parse_passages(grounding);
        assert_eq!(passages.len(), 2);
        assert_eq!(passages[0].loc, "p:41");
        assert_eq!(passages[0].text, "page 41 text\nmore text");
        assert_eq!(passages[1].loc, "p:3 — chap:2");
        assert_eq!(passages[1].text, "wiki text");
    }
}
