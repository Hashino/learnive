//! §S21 post-generation grounding-verification gate — PLAN.md's "Ajuste
//! pós-S27 (2026-08-26)" point 2 (search "checagem de fundamentação
//! pós-geração"). [`CITE_CONTRACT`](crate::engine::prompt::CITE_CONTRACT)
//! only ever asked the model nicely to cite what it draws from SOURCES;
//! nothing checked that it actually did, or that what it wrote is even true
//! of the source it was given. Live QA (2026-08-27, Groq `gpt-oss-120b`)
//! confirmed the gap empirically, not hypothetically: fed a real Wikipedia
//! excerpt on photosynthesis, the model fabricated a detailed photosystem
//! I/II mechanism absent from the source, with zero `<cite>` tags on
//! anything, fabricated or genuine — exactly the §16 "grounding alucinado"
//! risk.
//!
//! This module is the mechanical safety net: after a grounded move's
//! content is fully generated (streaming itself unchanged — §14, TTFT stays
//! the metric; this runs AFTER the stream already closed), a SEPARATE
//! fast-tier structured call ([`prompt::verify_grounding`]) compares the
//! finished HTML against the exact source text the prompt saw
//! (`MoveContext.grounding`, unchanged — not a fresh retrieval) and lists
//! any unsupported claims. Unsupported → the move lands anyway, NEVER
//! dropped, but with a visible "grounding unconfirmed" banner prepended to
//! its HTML — the same never-fail-silently principle already used for S27's
//! existence verification and citation resolution.
//!
//! **Cut down to a single check call, 2026-09-01 (live QA).** This used to
//! also attempt one corrective regeneration plus a re-check on top of the
//! check itself (up to 3 extra model calls total) — removed after two
//! pieces of live evidence in the same QA pass: (1) §12.2's "recovery must
//! cost zero tokens" corollary is a hard constraint, and a corrective
//! regeneration is exactly the kind of extra-call recovery path it forbids;
//! (2) it was directly caught failing live, on the very models configured
//! in this project's own `.env` (`grounding corrective regeneration failed:
//! provider: the model hit its token budget after 1070 characters without
//! finishing`) — the free-tier reality this project targets (§15) makes the
//! "recovery" call itself an extra point of failure, not a safety net. The
//! architecture also makes this the right place to draw the line, not just
//! the cheap one: a node is already born from one specific chapter/section
//! (§11's structural per-node source selection), so an unsupported claim
//! means the model drifted from prose *it was handed*, not a retrieval
//! miss — a rare defect worth surfacing immediately, not one that earns an
//! expensive self-healing pipeline. The corrective-regeneration call site
//! itself is gone; `MoveContext::grounding_correction`/
//! `prompt::grounding_correction_addendum` are left in place (still
//! exercised by their own unit tests in `movement/prompt.rs`) rather than
//! torn out, since a future async-after-the-fact redesign of this gate
//! (patch the banner in without blocking the move at all — not yet built,
//! floated live 2026-09-01) could still want the addendum shape.
//!
//! Scope: the six move types with locked sections that receive
//! `CITE_CONTRACT` today — `explain`/`ask`/`confront`/`integrate`/
//! `revisit`/`plan` ([`in_scope`]). A no-op (returns `generated` completely
//! unchanged) for any other type or when `ctx.grounding` is empty, so no
//! other call site's behavior changes because this gate exists.

use super::{EngineError, GeneratedMove, MoveContext, MoveType, parse, prompt, repair_messages};
use crate::ai::{Ai, Tier};
use crate::engine::collect;
use crate::locale::{Locale, pick};

/// Whether the gate applies at all — the same test the caller
/// (`api::generation::generate_node`) uses to decide whether emitting a
/// status frame before the check is worthwhile.
pub fn applies(move_type: MoveType, grounding: &str) -> bool {
    !grounding.trim().is_empty() && in_scope(move_type)
}

fn in_scope(move_type: MoveType) -> bool {
    matches!(
        move_type,
        MoveType::Explain
            | MoveType::Ask
            | MoveType::Confront
            | MoveType::Integrate
            | MoveType::Revisit
            | MoveType::Plan
    )
}

/// Runs the gate. Always returns a usable [`GeneratedMove`] — the caller
/// never sees an `Err` and never needs a retry loop of its own. Two
/// DIFFERENT failure shapes are deliberately kept apart, not conflated into
/// one "flag it" branch: a genuine unsupported-claims VERDICT (the check ran
/// fine and found a problem) earns the visible "unconfirmed" banner, but the
/// CHECK ITSELF failing to run (provider error, timeout, an unparseable
/// response even after JSON-repair) is infrastructure trouble, not a verdict
/// about the content — it degrades to passing `generated` through
/// unflagged, logged the same fire-and-forget way this module's caller
/// already logs event-log/progressive-write failures. The banner is baked
/// into the FROZEN content layer (§4.3) the moment it's returned, so there
/// is no un-flagging it later — a transient hiccup must never leave a
/// permanent "this may be fabricated" mark on correctly-grounded content.
pub async fn verify(
    ai: &Ai,
    move_type: MoveType,
    ctx: &MoveContext,
    generated: GeneratedMove,
) -> GeneratedMove {
    if !applies(move_type, &ctx.grounding) {
        return generated;
    }

    let unsupported = match check(ai, &ctx.grounding, &generated.html).await {
        Ok(claims) => claims,
        Err(e) => {
            eprintln!("grounding check failed: {e}");
            return generated;
        }
    };
    if unsupported.is_empty() {
        return generated;
    }

    // No corrective regeneration (removed 2026-09-01, see module doc) — a
    // real verdict on THIS content, flag it immediately rather than paying
    // for a recovery call §12.2 forbids.
    flag_unconfirmed(generated, ctx.locale)
}

/// One structured verification call, with the same JSON-repair bound
/// `generate_move` already uses for the Move contract — a DIFFERENT concern
/// from the verdict handling in [`verify`] above: this is only "did the
/// response parse as the expected shape", never "was the verdict itself
/// correct".
async fn check(
    ai: &Ai,
    source_text: &str,
    generated_html: &str,
) -> Result<Vec<String>, EngineError> {
    let messages = prompt::verify_grounding(source_text, generated_html);
    let text = collect(ai, Tier::Fast, messages.clone()).await?;
    if let Ok(claims) = parse::grounding_verdict(&text) {
        return Ok(claims);
    }
    let repair = repair_messages(
        messages,
        &text,
        "expected JSON {\"unsupported_claims\":[...]}",
    );
    let text = collect(ai, Tier::Fast, repair).await?;
    parse::grounding_verdict(&text)
}

const UNCONFIRMED_EN: &str = "This section's grounding could not be fully confirmed against \
     its cited source — some claims may not be accurately supported.";
const UNCONFIRMED_PT: &str = "A fundamentação desta seção não pôde ser totalmente confirmada \
     contra a fonte citada — algumas afirmações podem não estar corretamente apoiadas.";

/// Prepends a visible warning banner — never a silent drop, PLAN.md's
/// standing principle, same one S27's existence verification and citation
/// resolution already follow — using the SAME `callout warning` visual
/// vocabulary the model already produces spontaneously in prose
/// (`assets/app.css`'s `.prose .callout.warning` — no new CSS needed) plus a
/// `data-*` marker for anything that later wants to find it
/// programmatically. The move's own content is NEVER dropped or replaced —
/// only prepended to — regardless of which caller (first-pass or
/// post-retry) hands it in.
fn flag_unconfirmed(mut generated: GeneratedMove, locale: Locale) -> GeneratedMove {
    let msg = pick(locale, UNCONFIRMED_EN, UNCONFIRMED_PT);
    generated.html = format!(
        "<div class=\"callout warning\" data-grounding-unconfirmed=\"true\"><p>{msg}</p></div>\n{}",
        generated.html
    );
    generated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{ChatRequest, MockProvider, Models, Provider};
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    fn grounded_ctx() -> MoveContext {
        MoveContext {
            grounding: "[id: s1 | loc: p:1 | Photosynthesis — Overview]\n\
                        Photosynthesis converts light energy into chemical energy."
                .to_string(),
            ..Default::default()
        }
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
            proposed_outline: Vec::new(),
            repaired: false,
        }
    }

    #[test]
    fn applies_requires_both_grounding_and_scope() {
        assert!(!applies(MoveType::Explain, ""));
        assert!(!applies(MoveType::Explain, "   "));
        assert!(!applies(MoveType::Test, "some source text"));
        assert!(applies(MoveType::Explain, "some source text"));
        assert!(applies(MoveType::Plan, "some source text"));
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

        let ai = scripted_ai(|_| panic!("the AI must not be called out of scope"));
        let ctx = grounded_ctx();
        let generated = stub_move("<form>An exercise.</form>");
        let result = verify(&ai, MoveType::Test, &ctx, generated).await;
        assert_eq!(result.html, "<form>An exercise.</form>");
    }

    /// All-supported passes through completely unchanged — no banner, no
    /// second call.
    #[tokio::test]
    async fn fully_supported_move_passes_through_unchanged() {
        let ai = mock_ai(r#"{"unsupported_claims":[]}"#);
        let ctx = grounded_ctx();
        let generated = stub_move("<p>Photosynthesis converts light into chemical energy.</p>");
        let result = verify(&ai, MoveType::Explain, &ctx, generated).await;
        assert_eq!(
            result.html,
            "<p>Photosynthesis converts light into chemical energy.</p>"
        );
    }

    /// An unsupported claim flags the ORIGINAL content immediately —
    /// exactly one AI call total (2026-09-01: no more corrective
    /// regeneration, see the module doc for why), never a second one.
    #[tokio::test]
    async fn unsupported_claim_flags_immediately_with_a_single_call() {
        let call = AtomicUsize::new(0);
        let ai = scripted_ai(move |_req| {
            let n = call.fetch_add(1, Ordering::SeqCst);
            match n {
                0 => r#"{"unsupported_claims":["a fabricated mechanism"]}"#.to_string(),
                other => {
                    panic!("unexpected extra AI call #{other} — no more corrective regeneration")
                }
            }
        });

        let ctx = grounded_ctx();
        let generated = stub_move("<p>A fabricated mechanism, stated as fact.</p>");
        let result = verify(&ai, MoveType::Explain, &ctx, generated).await;
        assert!(result.html.contains("data-grounding-unconfirmed"));
        assert!(result.html.contains("callout warning"));
        assert!(
            result
                .html
                .contains("A fabricated mechanism, stated as fact.")
        );
    }

    /// The check call itself failing to produce a parseable verdict (even
    /// after the one JSON-repair round `check` allows) is infrastructure
    /// trouble, not a verdict — `generated` must pass through completely
    /// unflagged, not get the "unconfirmed" banner baked into its frozen
    /// content permanently over what could be a transient hiccup.
    #[tokio::test]
    async fn check_failure_never_flags_the_original_content() {
        let call = AtomicUsize::new(0);
        let ai = scripted_ai(move |_req| {
            call.fetch_add(1, Ordering::SeqCst);
            "I'm sorry, I can't help with that request.".to_string()
        });

        let ctx = grounded_ctx();
        let generated = stub_move("<p>Photosynthesis converts light into chemical energy.</p>");
        let result = verify(&ai, MoveType::Explain, &ctx, generated).await;
        assert_eq!(
            result.html,
            "<p>Photosynthesis converts light into chemical energy.</p>"
        );
        assert!(!result.html.contains("data-grounding-unconfirmed"));
    }

    /// A `plan` move (the one STRUCTURED type in the gate's scope) flags
    /// through the same single-call path as the streamed types — no
    /// render-path branching left to get wrong.
    #[tokio::test]
    async fn plan_moves_flag_through_the_same_single_call_path() {
        let ai = mock_ai(r#"{"unsupported_claims":["a fabricated outline rationale"]}"#);
        let ctx = grounded_ctx();
        let mut generated = stub_move("<p>A fabricated outline rationale.</p>");
        generated.move_type = MoveType::Plan;
        let result = verify(&ai, MoveType::Plan, &ctx, generated).await;
        assert!(result.html.contains("data-grounding-unconfirmed"));
        assert!(result.html.contains("A fabricated outline rationale."));
    }
}
