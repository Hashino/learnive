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
//! any unsupported claims. Unsupported → one corrective regeneration with
//! those claims named (`prompt::grounding_correction_addendum`, same §8.2
//! escalation shape as remediation: name the specific problem, don't just
//! say "try again"). Still unsupported after the retry → the move lands
//! anyway, NEVER dropped, but with a visible "grounding unconfirmed" banner
//! prepended to its HTML — the same never-fail-silently principle already
//! used for S27's existence verification and citation resolution.
//!
//! Scope: the six move types with locked sections that receive
//! `CITE_CONTRACT` today — `explain`/`ask`/`confront`/`integrate`/
//! `revisit`/`plan` ([`in_scope`]). A no-op (returns `generated` completely
//! unchanged) for any other type or when `ctx.grounding` is empty, so no
//! other call site's behavior changes because this gate exists.

use super::{
    AgentPolicy, EngineError, GeneratedMove, MoveContext, MoveRender, MoveType, generate_move,
    generate_move_complete, parse, prompt, repair_messages,
};
use crate::ai::{Ai, Tier};
use crate::engine::collect;
use crate::locale::{Locale, pick};

/// Whether the gate applies at all — the same test the caller
/// (`api::generation::generate_node`) uses to decide whether emitting a
/// status frame before the (possibly slow — up to three extra calls)
/// check is worthwhile.
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
pub async fn verify_and_correct(
    ai: &Ai,
    policy: AgentPolicy,
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

    // One corrective regeneration (§8.2 escalation shape): re-run the same
    // move's own generation path with the flagged claims named.
    let mut retry_ctx = ctx.clone();
    retry_ctx.grounding_correction = Some(unsupported);
    let regenerated = match regenerate(ai, policy, move_type, &retry_ctx).await {
        Ok(mv) => mv,
        Err(e) => {
            // The check itself found a real problem with `generated` above —
            // that verdict stands even though the corrective regeneration
            // couldn't run, so (unlike the two infrastructure-error arms)
            // this one still flags, on the original content.
            eprintln!("grounding corrective regeneration failed: {e}");
            return flag_unconfirmed(generated, ctx.locale);
        }
    };

    match check(ai, &ctx.grounding, &regenerated.html).await {
        Ok(claims) if claims.is_empty() => regenerated,
        Ok(_) => flag_unconfirmed(regenerated, ctx.locale),
        Err(e) => {
            // Re-verification itself failed to run — infrastructure, not a
            // verdict on the regenerated content. Same principle as the
            // first check above: don't bake a permanent mark on content that
            // was never actually found unsupported.
            eprintln!("grounding re-check failed: {e}");
            regenerated
        }
    }
}

/// One structured verification call, with the same JSON-repair bound
/// `generate_move` already uses for the Move contract — a DIFFERENT concern
/// from the semantic escalation in [`verify_and_correct`] above: this is
/// only "did the response parse as the expected shape", never "was the
/// verdict itself correct".
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

/// The single corrective regeneration: re-runs the SAME move type's own
/// generation path — streamed types (`explain`/`ask`/`confront`/
/// `integrate`/`revisit`) via [`generate_move_complete`] (still the
/// streamed prose contract, just collected instead of pumped through SSE —
/// this corrective attempt is never shown live token-by-token, so there is
/// no TTFT to protect here, same reasoning `generate_move_complete`'s own
/// doc comment already gives for `/ask`/remediation), the one structured
/// type in scope (`plan`) via [`generate_move`] — with
/// `ctx.grounding_correction` set so `prompt::grounding_correction_addendum`
/// fires in `purpose()`.
///
/// This is a FULL regeneration, not a patch: it replaces `tactics` wholesale
/// (empty if the model drops the `<!--tactics:-->` sentinel on the retry,
/// which weakens that move's §7 evidence row) and, for `plan`, replaces
/// `proposed_outline` too (a first attempt that proposed an outline change
/// can come back from the retry with none, silently dropping the proposal
/// before `plan_proposal` ever sees it). Both are accepted trade-offs of
/// reusing the move's own generation path rather than a bespoke
/// claim-by-claim patcher, not oversights.
async fn regenerate(
    ai: &Ai,
    policy: AgentPolicy,
    move_type: MoveType,
    ctx: &MoveContext,
) -> Result<GeneratedMove, EngineError> {
    match move_type.render() {
        MoveRender::Streamed => generate_move_complete(ai, move_type, ctx).await,
        MoveRender::Structured => generate_move(ai, policy, move_type, ctx).await,
    }
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

    fn full_text(req: &ChatRequest) -> String {
        req.messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n")
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
        let result =
            verify_and_correct(&ai, AgentPolicy::L1, MoveType::Explain, &ctx, generated).await;
        assert_eq!(result.html, "<p>Ungrounded prose.</p>");

        let ai = scripted_ai(|_| panic!("the AI must not be called out of scope"));
        let ctx = grounded_ctx();
        let generated = stub_move("<form>An exercise.</form>");
        let result =
            verify_and_correct(&ai, AgentPolicy::L1, MoveType::Test, &ctx, generated).await;
        assert_eq!(result.html, "<form>An exercise.</form>");
    }

    /// All-supported passes through completely unchanged — no banner, no
    /// second call.
    #[tokio::test]
    async fn fully_supported_move_passes_through_unchanged() {
        let ai = mock_ai(r#"{"unsupported_claims":[]}"#);
        let ctx = grounded_ctx();
        let generated = stub_move("<p>Photosynthesis converts light into chemical energy.</p>");
        let result =
            verify_and_correct(&ai, AgentPolicy::L1, MoveType::Explain, &ctx, generated).await;
        assert_eq!(
            result.html,
            "<p>Photosynthesis converts light into chemical energy.</p>"
        );
    }

    /// One unsupported claim triggers exactly one corrective regeneration,
    /// with the addendum (and the specific flagged claim) present in that
    /// regeneration's prompt — then a clean re-verification lands the
    /// corrected content with no warning.
    #[tokio::test]
    async fn one_unsupported_claim_triggers_exactly_one_corrective_regeneration() {
        let call = AtomicUsize::new(0);
        let ai = scripted_ai(move |req| {
            let n = call.fetch_add(1, Ordering::SeqCst);
            let text = full_text(req);
            match n {
                0 => {
                    // First call: verifying the ORIGINAL html.
                    assert!(!text.contains("GROUNDING CORRECTION"));
                    r#"{"unsupported_claims":["photosystem II makes plastocyanin"]}"#.to_string()
                }
                1 => {
                    // Second call: the corrective regeneration (streamed
                    // prose contract, not JSON — `Explain` renders streamed).
                    assert!(text.contains("GROUNDING CORRECTION"));
                    assert!(text.contains("photosystem II makes plastocyanin"));
                    "<p>Corrected, source-grounded prose.</p>".to_string()
                }
                2 => {
                    // Third call: re-verifying the CORRECTED html — clean.
                    assert!(text.contains("Corrected, source-grounded prose."));
                    r#"{"unsupported_claims":[]}"#.to_string()
                }
                other => panic!("unexpected extra AI call #{other}"),
            }
        });

        let ctx = grounded_ctx();
        let generated = stub_move("<p>Photosystem II makes plastocyanin, allegedly.</p>");
        let result =
            verify_and_correct(&ai, AgentPolicy::L1, MoveType::Explain, &ctx, generated).await;
        assert_eq!(result.html, "<p>Corrected, source-grounded prose.</p>");
        assert!(!result.html.contains("data-grounding-unconfirmed"));
    }

    /// Still unsupported after the one retry: the move lands anyway (the
    /// REGENERATED content, never the original AND never dropped) flagged
    /// with the visible warning banner.
    #[tokio::test]
    async fn still_unsupported_after_retry_lands_flagged_and_is_never_dropped() {
        let call = AtomicUsize::new(0);
        let ai = scripted_ai(move |_req| {
            let n = call.fetch_add(1, Ordering::SeqCst);
            match n {
                0 => r#"{"unsupported_claims":["a fabricated mechanism"]}"#.to_string(),
                1 => "<p>Still not quite grounded prose.</p>".to_string(),
                2 => r#"{"unsupported_claims":["still a fabricated mechanism"]}"#.to_string(),
                other => panic!("unexpected extra AI call #{other}"),
            }
        });

        let ctx = grounded_ctx();
        let generated = stub_move("<p>A fabricated mechanism, stated as fact.</p>");
        let result =
            verify_and_correct(&ai, AgentPolicy::L1, MoveType::Explain, &ctx, generated).await;
        assert!(result.html.contains("data-grounding-unconfirmed"));
        assert!(result.html.contains("callout warning"));
        assert!(result.html.contains("Still not quite grounded prose."));
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
        let result =
            verify_and_correct(&ai, AgentPolicy::L1, MoveType::Explain, &ctx, generated).await;
        assert_eq!(
            result.html,
            "<p>Photosynthesis converts light into chemical energy.</p>"
        );
        assert!(!result.html.contains("data-grounding-unconfirmed"));
    }

    /// The corrective regeneration itself failing (both its attempt and its
    /// own repair round come back unparseable) still lands the ORIGINAL
    /// content, flagged — the first check's verdict (a real unsupported
    /// claim) stands even though the retry couldn't run, unlike the two
    /// purely-infrastructure arms above/below which never flag at all.
    #[tokio::test]
    async fn failed_corrective_regeneration_flags_the_original_verdict() {
        let call = AtomicUsize::new(0);
        let ai = scripted_ai(move |_req| {
            let n = call.fetch_add(1, Ordering::SeqCst);
            match n {
                0 => r#"{"unsupported_claims":["a fabricated outline rationale"]}"#.to_string(),
                // Both the regeneration attempt and its own repair round
                // come back unparseable, so `generate_move` itself errors.
                1 | 2 => "not JSON at all".to_string(),
                other => panic!("unexpected extra AI call #{other}"),
            }
        });

        let ctx = grounded_ctx();
        let mut generated = stub_move("<p>A fabricated outline rationale.</p>");
        generated.move_type = MoveType::Plan;
        let result =
            verify_and_correct(&ai, AgentPolicy::L1, MoveType::Plan, &ctx, generated).await;
        assert!(result.html.contains("data-grounding-unconfirmed"));
        assert!(result.html.contains("A fabricated outline rationale."));
    }

    /// The RE-check after a successful corrective regeneration failing to
    /// parse (infrastructure again, not a verdict) must land the
    /// regenerated content unflagged — the regeneration itself succeeded and
    /// was never actually found unsupported, so no banner belongs on it.
    #[tokio::test]
    async fn failed_recheck_lands_regenerated_content_unflagged() {
        let call = AtomicUsize::new(0);
        let ai = scripted_ai(move |_req| {
            let n = call.fetch_add(1, Ordering::SeqCst);
            match n {
                0 => r#"{"unsupported_claims":["photosystem II makes plastocyanin"]}"#.to_string(),
                1 => "<p>Corrected, source-grounded prose.</p>".to_string(),
                // Both the re-check attempt and its repair round come back
                // unparseable.
                2 | 3 => "not JSON at all".to_string(),
                other => panic!("unexpected extra AI call #{other}"),
            }
        });

        let ctx = grounded_ctx();
        let generated = stub_move("<p>Photosystem II makes plastocyanin, allegedly.</p>");
        let result =
            verify_and_correct(&ai, AgentPolicy::L1, MoveType::Explain, &ctx, generated).await;
        assert_eq!(result.html, "<p>Corrected, source-grounded prose.</p>");
        assert!(!result.html.contains("data-grounding-unconfirmed"));
    }

    /// A `plan` move (the one STRUCTURED type in the gate's scope) must
    /// route its corrective regeneration through the structured path, not
    /// the streamed one — a bare mismatch would trip `generate_move`'s own
    /// debug assertion in a debug build.
    #[tokio::test]
    async fn plan_moves_regenerate_through_the_structured_path() {
        let call = AtomicUsize::new(0);
        let ai = scripted_ai(move |req| {
            let n = call.fetch_add(1, Ordering::SeqCst);
            let text = full_text(req);
            match n {
                0 => r#"{"unsupported_claims":["a fabricated outline rationale"]}"#.to_string(),
                1 => {
                    assert!(text.contains("GROUNDING CORRECTION"));
                    r#"{"html":"<p>Corrected rationale.</p>","interactive":false,"graded":false,"tactics":[]}"#
                        .to_string()
                }
                2 => r#"{"unsupported_claims":[]}"#.to_string(),
                other => panic!("unexpected extra AI call #{other}"),
            }
        });

        let ctx = grounded_ctx();
        let mut generated = stub_move("<p>A fabricated outline rationale.</p>");
        generated.move_type = MoveType::Plan;
        let result =
            verify_and_correct(&ai, AgentPolicy::L1, MoveType::Plan, &ctx, generated).await;
        assert_eq!(result.html, "<p>Corrected rationale.</p>");
    }
}
