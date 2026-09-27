//! §S21 post-generation grounding gate — AGENT-CITED shape (2026-09-27,
//! user decision: "faça as citações serem geradas pelo agente, como eram
//! antes, não mais programaticamente"). The generation prompt carries a cite
//! contract again (`engine::prompt::CITE_CONTRACT`) and the MODEL decides
//! what to cite and where; this gate is what keeps that honest:
//!
//! 1. **Validation — zero token.** Every `<cite>` the model wrote is lifted
//!    out ([`learnive_core::extract_block_citations`]) and kept only if its
//!    `data-source-id` + `data-locator` pair names a page that is actually
//!    in the grounding selection the move was given — the `[id: … | loc: …]`
//!    headers of `ctx.grounding`. Anything else is an invented citation by
//!    definition and is dropped (the text it wrapped, if any, stays). This
//!    is the guarantee the 2026-09-05 server-side citer existed for — a
//!    citation can only point at a page the model actually read — kept
//!    without the server choosing the citations itself.
//! 2. **Support check — only on doubt.** Where the node has a page index
//!    (`ctx.grounding_index`), each kept cite's paragraph is embedded with
//!    the LOCAL offline embedder and compared against its OWN cited page.
//!    Below [`MECHANICAL_FLOOR`] it goes into ONE small adjudication call
//!    ([`prompt::verify_support`]): the paragraph plus that page's text.
//!    Judged unsupported ⇒ the cite stays but is stamped `data-unverified`
//!    (orange + warning glyph, `app.css`); cleared ⇒ normal cite. Zero
//!    suspects ⇒ zero model calls.
//!
//! Kept cites are re-inserted as empty markers at the end of their block
//! ([`learnive_core::insert_block_citations`]), so they render exactly as
//! before regardless of where inside the paragraph the model put them.
//! A paragraph the model did not cite stays uncited — there is no
//! mechanical fallback anymore, by the same decision.
//!
//! Failure posture (§12.2, never-fail-silently): validation cannot fail.
//! If the adjudication call fails (provider error, unparseable verdict even
//! after JSON-repair), every SUSPECT is stamped `data-unverified` —
//! infrastructure trouble degrades to honest doubt, never silent
//! confidence. The move's own text is NEVER dropped or replaced.
//!
//! Scope: the streamed move types with grounded prose — `explain`/
//! `integrate`/`revisit`/`respond` ([`in_scope`]). Any other type is
//! returned unchanged; an in-scope move with NO grounding has every cite
//! stripped (there was nothing it could legitimately point at).

use super::{EngineError, GeneratedMove, MoveContext, MoveType, parse, prompt, repair_messages};
use crate::ai::{Ai, Tier};
use crate::engine::collect_within;
use crate::retrieval::Embedder;

/// Best-similarity floor between a cited paragraph and its own cited page
/// below which the citation is treated as unproven and sent to the
/// adjudication call. Calibrated live 2026-09-05 against the mechanical
/// citer's best-page scores (healthy range 0.58–0.83); the `grounding`
/// stderr diagnostic prints every cite's real score for further tuning.
pub const MECHANICAL_FLOOR: f32 = 0.5;

/// Response ceiling for the adjudication call: the verdict is a tiny JSON
/// array, but the cap must still absorb reasoning burn on the free tier
/// (same precedent as `engine`'s `TOC_PAGE_MAX_TOKENS`/`CHAPTER_SPLIT_MAX_TOKENS`).
/// Bounded so the TPM accounting that sizes `SECTION_TEXT_CHAR_BUDGET` can
/// treat this call as small by construction.
const ADJUDICATION_MAX_TOKENS: u32 = 1000;

/// Paragraphs shorter than this are not support-checked: a one-liner's
/// embedding is dominated by stopwords, so its similarity says nothing.
/// Its citation is still validated like any other.
const MIN_BLOCK_CHARS: usize = 40;

/// What the support check needs to compare a paragraph against its cited
/// page: the same cache dir / content hash the node's grounding text was
/// read from (`api::reading::ground_node` owns both and threads them
/// through `prepare`). `Embedder` is the local offline embedder — cloning
/// this struct is cheap.
#[derive(Clone)]
pub struct GroundingIndex {
    pub embedder: Embedder,
    pub dir: std::path::PathBuf,
    pub content_hash: String,
}

impl std::fmt::Debug for GroundingIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroundingIndex")
            .field("dir", &self.dir)
            .field("content_hash", &self.content_hash)
            .finish_non_exhaustive()
    }
}

/// Whether the gate applies at all — the same test the caller
/// (`api::generation::generate_node`) uses to decide whether emitting a
/// status frame before the check is worthwhile.
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
/// `MoveContext::grounding`'s `[id: … | loc: … | title]` header lines — the
/// set a model citation is validated against, and the text a suspect is
/// adjudicated against.
struct Passage {
    id: String,
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
            let (Some(id_part), Some(loc_part), Some(_title)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            out.push(Passage {
                id: id_part.strip_prefix("id: ").unwrap_or(id_part).to_string(),
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

/// A kept citation after validation: its block, the two attributes, and
/// whether it ends up stamped unverified.
struct Kept {
    block: usize,
    source_id: String,
    locator: String,
    unverified: bool,
}

/// Runs the gate. Always returns a usable [`GeneratedMove`] — the caller
/// never sees an `Err` and never needs a retry loop of its own.
pub async fn verify(
    ai: &Ai,
    move_type: MoveType,
    ctx: &MoveContext,
    generated: GeneratedMove,
) -> GeneratedMove {
    if !in_scope(move_type) {
        return generated;
    }
    let (stripped, model_cites) = learnive_core::extract_block_citations(&generated.html);
    if model_cites.is_empty() {
        return generated;
    }
    let mut generated = generated;
    if ctx.grounding.trim().is_empty() {
        // Nothing was supplied, so nothing can be cited: every cite is
        // invented. The wrapped text (if any) stays.
        eprintln!(
            "grounding: ungrounded move, dropped {} cites",
            model_cites.len()
        );
        generated.html = stripped;
        return generated;
    }

    let passages = parse_passages(&ctx.grounding);
    let blocks = learnive_core::block_texts(&stripped);

    // Layer 1 — validation: a cite survives only if it names a page of the
    // selection the move was actually given. Duplicates collapse.
    let mut kept: Vec<Kept> = Vec::new();
    let mut dropped = 0usize;
    for c in &model_cites {
        let (id, loc) = (c.source_id.trim(), c.locator.trim());
        let valid = passages.iter().any(|p| p.id == id && p.loc == loc);
        if !valid {
            dropped += 1;
            continue;
        }
        if kept
            .iter()
            .any(|k| k.block == c.block && k.source_id == id && k.locator == loc)
        {
            continue;
        }
        kept.push(Kept {
            block: c.block,
            source_id: id.to_string(),
            locator: loc.to_string(),
            unverified: false,
        });
    }

    // Layer 2 — support check, only where there is a page index to measure
    // with and only for cites into that indexed book.
    let mut scores: Vec<String> = Vec::new();
    let mut suspects: Vec<usize> = Vec::new(); // indexes into `kept`
    if let Some(index) = &ctx.grounding_index
        && let Ok(chunks) = crate::source::load_index_cache(&index.dir, &index.content_hash)
    {
        for (i, k) in kept.iter().enumerate() {
            if k.source_id != index.content_hash {
                continue;
            }
            let Some(text) = blocks.get(k.block - 1) else {
                continue;
            };
            if text.chars().count() < MIN_BLOCK_CHARS {
                continue;
            }
            let Some(page) = k
                .locator
                .strip_prefix("p:")
                .and_then(|n| n.parse::<usize>().ok())
            else {
                continue;
            };
            let query = index.embedder.embed(text);
            let score = chunks
                .iter()
                .filter(|c| c.page == page)
                .map(|c| crate::retrieval::cosine(&query, &c.vector))
                .fold(f32::MIN, f32::max);
            scores.push(format!("b{}@{}={score:.2}", k.block, k.locator));
            if score < MECHANICAL_FLOOR {
                suspects.push(i);
            }
        }
    }

    if !suspects.is_empty() {
        let page_text = |k: &Kept| {
            passages
                .iter()
                .find(|p| p.id == k.source_id && p.loc == k.locator)
                .map(|p| p.text.clone())
                .unwrap_or_default()
        };
        let items: Vec<(usize, String, String)> = suspects
            .iter()
            .enumerate()
            .map(|(n, &i)| {
                let k = &kept[i];
                (
                    n + 1,
                    blocks.get(k.block - 1).cloned().unwrap_or_default(),
                    page_text(k),
                )
            })
            .collect();
        let checkable: Vec<(usize, &str, &str)> = items
            .iter()
            .filter(|(_, _, page)| !page.is_empty())
            .map(|(n, t, p)| (*n, t.as_str(), p.as_str()))
            .collect();
        let mut unsupported: std::collections::HashSet<usize> = items
            .iter()
            .filter(|(_, _, page)| page.is_empty())
            .map(|(n, _, _)| *n)
            .collect();
        if !checkable.is_empty() {
            match check(ai, &checkable).await {
                Ok(verdict) => unsupported.extend(verdict.unsupported),
                Err(e) => {
                    // Infrastructure failure, not a verdict — degrade to
                    // honest doubt on exactly the suspects.
                    eprintln!("grounding adjudication failed: {e}");
                    unsupported.extend(checkable.iter().map(|(n, _, _)| *n));
                }
            }
        }
        for (n, &i) in suspects.iter().enumerate() {
            kept[i].unverified = unsupported.contains(&(n + 1));
        }
    }

    eprintln!(
        "grounding: model_cites={} kept={} dropped={dropped} suspects={} unverified={} floor={MECHANICAL_FLOOR} scores=[{}]",
        model_cites.len(),
        kept.len(),
        suspects.len(),
        kept.iter().filter(|k| k.unverified).count(),
        scores.join(", "),
    );

    let refs: Vec<(usize, &str, &str, bool)> = kept
        .iter()
        .map(|k| {
            (
                k.block,
                k.source_id.as_str(),
                k.locator.as_str(),
                k.unverified,
            )
        })
        .collect();
    generated.html = learnive_core::insert_block_citations(&stripped, &refs);
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
    let text = collect_within(ai, Tier::Fast, messages.clone(), ADJUDICATION_MAX_TOKENS).await?;
    if let Ok(verdict) = parse::support_verdict(&text) {
        return Ok(verdict);
    }
    let repair = repair_messages(messages, &text, "expected JSON {\"unsupported\":[...]}");
    let text = collect_within(ai, Tier::Fast, repair, ADJUDICATION_MAX_TOKENS).await?;
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

    const A: &str =
        "Photosynthesis converts light energy into chemical energy inside the chloroplast.";
    const B: &str = "Zorbulons fruminate the quuxly bazzoink under pluxtious conditions.";
    const P2: &str = "The stroma surrounds the grana.";

    fn cite(loc: &str) -> String {
        format!(r#"<cite data-source-id="hash1" data-locator="{loc}"></cite>"#)
    }

    /// Grounding without a page index (the /ask cascade, or no chapter
    /// pointer) still VALIDATES — invented cites go, real ones stay — but
    /// never spends a model call: there is nothing to measure support with.
    #[tokio::test]
    async fn grounding_without_an_index_validates_only() {
        let ai = scripted_ai(|_| panic!("no index means no model call"));
        let ctx = MoveContext {
            grounding: format!("[id: hash1 | loc: p:1 | A]\n{A}"),
            ..Default::default()
        };
        let generated = stub_move(&format!(
            "<p>{A}{}</p>\n<p>{B}{}</p>",
            cite("p:1"),
            cite("p:7")
        ));
        let result = verify(&ai, MoveType::Respond, &ctx, generated).await;
        assert_eq!(result.html.matches("<cite").count(), 1, "{}", result.html);
        assert!(
            result.html.contains(&format!("{A}{}</p>", cite("p:1"))),
            "{}",
            result.html
        );
        assert!(!result.html.contains("p:7"), "{}", result.html);
    }

    /// The happy path costs ZERO model calls: the model cited the page the
    /// paragraph came from, its similarity clears the floor, the cite stays.
    #[tokio::test]
    async fn supported_cites_cost_zero_model_calls() {
        let ai = scripted_ai(|_| panic!("a fully supported move must not call the AI"));
        let (_dir, ctx) = grounded_fixture(&[("1", A)]);
        let generated = stub_move(&format!("<p>{A}{}</p>", cite("p:1")));
        let result = verify(&ai, MoveType::Explain, &ctx, generated).await;
        assert!(
            result.html.contains(&format!("{A}{}</p>", cite("p:1"))),
            "{}",
            result.html
        );
        assert!(!result.html.contains("data-unverified"));
    }

    /// A cite naming a page (or a source) outside the selection the move was
    /// given is invented by definition: dropped, text it wrapped kept.
    #[tokio::test]
    async fn invented_cites_are_dropped_and_wrapped_text_kept() {
        let ai = scripted_ai(|_| panic!("validation is zero-token"));
        let (_dir, ctx) = grounded_fixture(&[("1", A), ("2", P2)]);
        let generated = stub_move(&format!(
            r#"<p>{A}<cite data-source-id="hash1" data-locator="p:300"></cite></p>
<p>See <cite data-source-id="other" data-locator="p:1">this claim</cite> now.</p>"#
        ));
        let result = verify(&ai, MoveType::Explain, &ctx, generated).await;
        assert!(!result.html.contains("<cite"), "{}", result.html);
        assert!(
            result.html.contains("See this claim now."),
            "{}",
            result.html
        );
    }

    /// No mechanical fallback: a paragraph the model did not cite stays
    /// uncited, even when it matches a page perfectly.
    #[tokio::test]
    async fn uncited_paragraphs_stay_uncited() {
        let ai = scripted_ai(|_| panic!("no cites, nothing to check"));
        let (_dir, ctx) = grounded_fixture(&[("1", A)]);
        let generated = stub_move(&format!("<p>{A}</p>"));
        let result = verify(&ai, MoveType::Explain, &ctx, generated).await;
        assert_eq!(result.html, format!("<p>{A}</p>"));
    }

    /// An in-scope move with no grounding at all had nothing to cite: every
    /// cite goes.
    #[tokio::test]
    async fn ungrounded_moves_have_every_cite_stripped() {
        let ai = scripted_ai(|_| panic!("zero-token"));
        let generated = stub_move(&format!("<p>{A}{}</p>", cite("p:1")));
        let result = verify(&ai, MoveType::Respond, &MoveContext::default(), generated).await;
        assert!(!result.html.contains("<cite"), "{}", result.html);
        assert!(result.html.contains(A));
    }

    /// The model may put the cite mid-sentence; it is re-seated at the end
    /// of its own paragraph, the one rendering the reader knows.
    #[tokio::test]
    async fn kept_cites_are_reseated_at_the_end_of_their_block() {
        let ai = scripted_ai(|_| panic!("supported, zero-token"));
        let (_dir, ctx) = grounded_fixture(&[("1", A)]);
        let generated = stub_move(
            r#"<p>Photosynthesis <cite data-source-id="hash1" data-locator="p:1">converts light energy</cite> into chemical energy inside the chloroplast.</p>"#,
        );
        let result = verify(&ai, MoveType::Explain, &ctx, generated).await;
        assert!(
            result.html.contains(&format!("{A}{}</p>", cite("p:1"))),
            "{}",
            result.html
        );
    }

    /// A cite whose paragraph doesn't resemble its own page is adjudicated:
    /// cleared ⇒ clean; judged unsupported ⇒ stamped `data-unverified`.
    #[tokio::test]
    async fn suspect_cites_are_adjudicated_and_marked_per_paragraph() {
        let pages = [("1", A), ("2", P2)];
        let html = format!("<p>{A}{}</p>\n<p>{B}{}</p>", cite("p:1"), cite("p:2"));

        let (_dir, ctx) = grounded_fixture(&pages);
        let result = verify(
            &mock_ai(r#"{"unsupported":[]}"#),
            MoveType::Explain,
            &ctx,
            stub_move(&html),
        )
        .await;
        assert_eq!(result.html.matches("<cite").count(), 2, "{}", result.html);
        assert!(!result.html.contains("data-unverified"), "{}", result.html);

        let (_dir, ctx) = grounded_fixture(&pages);
        let result = verify(
            &mock_ai(r#"{"unsupported":[1]}"#),
            MoveType::Explain,
            &ctx,
            stub_move(&html),
        )
        .await;
        assert!(
            result
                .html
                .contains(r#"data-locator="p:2" data-unverified="true""#),
            "the suspect is stamped: {}",
            result.html
        );
        assert!(
            result.html.contains(&format!("{A}{}</p>", cite("p:1"))),
            "{}",
            result.html
        );
    }

    /// The adjudication prompt pairs the suspect with the text of the page
    /// ITS citation points at — never the whole move or the whole window.
    #[tokio::test]
    async fn adjudication_prompt_pairs_suspect_with_its_page() {
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
        let (_dir, ctx) = grounded_fixture(&[("1", A), ("2", P2)]);
        let html = format!("<p>{A}{}</p>\n<p>{B}{}</p>", cite("p:1"), cite("p:2"));
        let _ = verify(&ai, MoveType::Explain, &ctx, stub_move(&html)).await;
        let body = captured.lock().unwrap().clone();
        assert!(
            body.contains("Zorbulons fruminate"),
            "suspect text in prompt"
        );
        assert!(
            body.contains(P2),
            "the cited page's own text in prompt: {body}"
        );
        assert!(
            !body.contains(A),
            "supported paragraphs must not reach the model"
        );
    }

    /// The adjudication call itself failing degrades to honest doubt on
    /// exactly the suspects; supported cites stay clean; text untouched.
    #[tokio::test]
    async fn check_failure_marks_suspects_unverified_and_nothing_else() {
        let ai = mock_ai("I'm sorry, I can't help with that request.");
        let (_dir, ctx) = grounded_fixture(&[("1", A), ("2", P2)]);
        let html = format!("<p>{A}{}</p>\n<p>{B}{}</p>", cite("p:1"), cite("p:2"));
        let result = verify(&ai, MoveType::Explain, &ctx, stub_move(&html)).await;
        assert!(result.html.contains(A) && result.html.contains(B));
        assert_eq!(result.html.matches("<cite").count(), 2);
        assert!(
            result.html.contains(&format!("{A}{}</p>", cite("p:1"))),
            "{}",
            result.html
        );
        assert_eq!(
            result.html.matches("data-unverified").count(),
            1,
            "{}",
            result.html
        );
    }

    /// The passage parser reads the grounding header format every producer
    /// shares ([`cite_block`]'s `[id | loc | title]` form — the pre-retirement
    /// corpus similarity format used the same shape with `—` separators) and
    /// accumulates non-header lines into the current passage.
    #[test]
    fn parse_passages_reads_both_selection_formats() {
        let grounding = "[id: hash1 | loc: p:41 | Stewart — Cálculo]\npage 41 text\nmore text\n\n\
                         [id: wiki1 | loc: p:3 — chap:2 | Photosynthesis — Overview]\nwiki text";
        let passages = parse_passages(grounding);
        assert_eq!(passages.len(), 2);
        assert_eq!(passages[0].id, "hash1");
        assert_eq!(passages[0].loc, "p:41");
        assert_eq!(passages[0].text, "page 41 text\nmore text");
        assert_eq!(passages[1].loc, "p:3 — chap:2");
        assert_eq!(passages[1].text, "wiki text");
    }
}
