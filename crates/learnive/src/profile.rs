//! Evidence profile (§7/§7.1, minimum slice — S7 of the agentic-loop build,
//! see PLAN.md's "core evolution: agentic loop" section, "Perfil por
//! evidência").
//!
//! Two halves, deliberately asymmetric in cost:
//! - **Evidence table** — `events::aggregate::tactic_outcomes` folded over
//!   the log, 0 LLM tokens, always fresh (recomputed on every read, same as
//!   every other aggregate in this codebase — no separate cache to desync).
//! - **Distillation** — a rare (§7.1: "destilação rara") fast-tier LLM call
//!   that turns the evidence table + coarse activity counts into a handful
//!   of distilled traits and 1-3 open hypotheses the `profile` move type
//!   tests. Persisted at `<doc>/profile.json` (`ProfileProjection`) so the
//!   rare call isn't repeated on every node — same sidecar convention as
//!   `objective.json`/`outline.json` (`store::write_doc_file`/`read_doc_file`).
//!
//! Deliberately NOT built in this minimum slice: the §5-style timestamped
//! decay of *individual* traits (here the whole trait/hypothesis set is
//! replaced wholesale at each distillation, not versioned per-belief) and
//! learning-style diagnosis (explicitly rejected by the spec — weak evidence
//! base, Pashler et al. 2008). Both are `§7.1` depth for a later slice, not
//! this one (PLAN.md's phasing principle: loop complete first, depth later).
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

use crate::ai::{Ai, Tier};
use crate::engine::{self, EngineError};
use crate::events::aggregate::{ActivityCounts, TacticOutcome};
use std::collections::HashMap;

/// How many new events since the last distillation force a re-run even
/// without a node closing (§7.1: "destilação rara ... ~30 eventos").
const DISTILL_EVERY: u32 = 30;

/// Soft char budget for the rendered prompt text (§14: ≤~800 tokens ≈ 3200
/// chars at a rough 4-chars/token estimate) — a backstop, not the primary
/// control (the distillation prompt already asks for short traits).
const PROMPT_CHAR_BUDGET: usize = 3200;

/// The learner's distilled profile (§7.1) — human-readable, user-inspectable
/// and editable JSON (`GET`/`POST .../profile`), the same "files are the
/// source of truth" convention as `objective.json`. Traits are NOT
/// individually versioned/decayed in this minimum slice (see module docs) —
/// the whole set is replaced at each distillation, which is itself rare.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProfileProjection {
    /// Short, evidence-citing statements (e.g. "worked-example lands better
    /// than analogy on procedural checks, 3/3 vs 1/3 demonstrated") —
    /// adversarially prompted against empty flattery.
    #[serde(default)]
    pub traits: Vec<String>,
    /// 1-3 open questions about the learner the NEXT `profile` move should
    /// test — a hypothesis, not settled fact.
    #[serde(default)]
    pub hypotheses: Vec<String>,
    /// Total event-log length at the last distillation — `should_distill`'s
    /// ~30-event threshold input. Never decremented.
    #[serde(default)]
    pub distilled_through: u32,
}

/// Rare-distillation trigger (§7.1): a node just closed (its graded check
/// demonstrated), OR enough raw events have piled up since the last
/// distillation that the cached traits/hypotheses are stale. `false` on an
/// empty log — nothing yet to distill.
pub fn should_distill(node_closed: bool, total_events: u32, distilled_through: u32) -> bool {
    total_events > 0
        && (node_closed || total_events.saturating_sub(distilled_through) >= DISTILL_EVERY)
}

/// Renders the evidence table (0 LLM tokens — plain Rust numbers folded over
/// the log) for both the distillation prompt and `MoveContext::profile`.
/// Sorted by tactic name for a deterministic, diffable render.
pub fn evidence_table_text(table: &HashMap<String, TacticOutcome>) -> String {
    if table.is_empty() {
        return String::new();
    }
    let mut rows: Vec<(&String, &TacticOutcome)> = table.iter().collect();
    rows.sort_by_key(|(tactic, _)| tactic.as_str());
    rows.into_iter()
        .map(|(tactic, o)| {
            format!(
                "{tactic}: {} demonstrated, {} partial, {} not-demonstrated",
                o.demonstrated, o.partial, o.not_demonstrated
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn activity_text(counts: &ActivityCounts) -> String {
    format!(
        "nodes skipped: {}, questions asked: {}, annotations added: {}",
        counts.nodes_skipped, counts.questions_asked, counts.annotations_added
    )
}

/// The rare fast-tier distillation call (§7.1). Adversarial prompt: record
/// what actually changed behavior, not generic praise — the same
/// not-flattering instruction §7 already gives `confront`. `total_events` is
/// stamped onto the result as `distilled_through`, so the caller doesn't have
/// to remember to set it.
///
/// `existing` (the current `profile.json`, if any) is fed back in as context
/// to revise rather than a blank slate to overwrite — without this, a node
/// closing (the common trigger) would silently clobber a learner's own
/// edit (§7.1 "editável a qualquer momento") the moment the next node closes.
/// The model is asked to keep what still holds and only change what the new
/// evidence actually contradicts; this is advisory (the model can still drop
/// a trait), not a hard non-destructive guarantee like `objective.json`'s
/// version chain.
pub async fn distill(
    ai: &Ai,
    evidence_text: &str,
    activity: &ActivityCounts,
    total_events: u32,
    existing: Option<&ProfileProjection>,
) -> Result<ProfileProjection, EngineError> {
    let messages = prompt::distill(evidence_text, &activity_text(activity), existing);
    let text = engine::collect(ai, Tier::Fast, messages).await?;
    let mut projection = parse::projection(&text)?;
    projection.distilled_through = total_events;
    Ok(projection)
}

/// Header the open-hypothesis list is rendered under. Public because
/// `movement::prompt` reads it back out of the rendered profile text to know
/// whether a `profile` move has anything to investigate at all — the two are
/// the same fact seen from opposite ends, so they must be one constant.
pub const HYPOTHESES_HEADER: &str = "Open hypotheses";

/// Combines the (always-fresh) evidence table with the (rarely-updated)
/// distilled traits/hypotheses into `MoveContext::profile`'s text. A soft
/// char budget backstops the §14 ≤~800-token target. Degrades gracefully —
/// empty table and `None` projection (a fresh document, or one predating
/// `profile.json`) yield an empty string, same convention as `grounding_for`.
pub fn render_for_prompt(evidence_text: &str, projection: Option<&ProfileProjection>) -> String {
    let mut out = String::new();
    if !evidence_text.is_empty() {
        out.push_str("Evidence (tactic -> outcome, from the event log):\n");
        out.push_str(evidence_text);
    }
    if let Some(p) = projection {
        if !p.traits.is_empty() {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str("Distilled traits:\n");
            out.push_str(
                &p.traits
                    .iter()
                    .map(|t| format!("- {t}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
        if !p.hypotheses.is_empty() {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(HYPOTHESES_HEADER);
            out.push_str(" (a \"profile\" move should test one of these):\n");
            out.push_str(
                &p.hypotheses
                    .iter()
                    .map(|h| format!("- {h}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
    }
    truncate_chars(&out, PROMPT_CHAR_BUDGET).to_string()
}

fn truncate_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

mod prompt {
    use super::ProfileProjection;
    use crate::ai::ChatMessage;

    fn existing_text(existing: Option<&ProfileProjection>) -> String {
        let Some(p) = existing else {
            return "(none yet — this is the first distillation)".to_string();
        };
        if p.traits.is_empty() && p.hypotheses.is_empty() {
            return "(none yet — this is the first distillation)".to_string();
        }
        format!(
            "Traits:\n{}\nHypotheses:\n{}",
            p.traits
                .iter()
                .map(|t| format!("- {t}"))
                .collect::<Vec<_>>()
                .join("\n"),
            p.hypotheses
                .iter()
                .map(|h| format!("- {h}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }

    pub fn distill(
        evidence_text: &str,
        activity_text: &str,
        existing: Option<&ProfileProjection>,
    ) -> Vec<ChatMessage> {
        vec![
            ChatMessage::system(
                "You distill a learner's evidence profile from raw \
                 intervention-outcome data. Be adversarial against empty \
                 flattery: record ONLY what actually changed behavior (an \
                 outcome difference between tactics), never generic praise \
                 or a restated fact with no evidence behind it. If the \
                 evidence is too thin to support a trait, report fewer \
                 things rather than padding. Emit at most a handful of \
                 traits and 1-3 open hypotheses about the learner that a \
                 future \"profile\" move should test — a hypothesis, not a \
                 settled fact.\nYou are REVISING an existing profile, not \
                 starting blank: keep what still holds (including anything \
                 the learner hand-edited), drop or change only what the new \
                 evidence actually contradicts.\nRespond ONLY with JSON: \
                 {\"traits\":[\"...\"],\"hypotheses\":[\"...\"]}."
                    .to_string(),
            ),
            ChatMessage::user(format!(
                "Existing profile (revise, don't discard):\n{}\n\n\
                 Evidence table (tactic -> outcome counts):\n{}\n\nOther activity: {}",
                existing_text(existing),
                if evidence_text.is_empty() {
                    "(none yet)"
                } else {
                    evidence_text
                },
                activity_text,
            )),
        ]
    }
}

mod parse {
    use super::{EngineError, ProfileProjection};
    use crate::engine::parse::extract_json;
    use serde::Deserialize;

    #[derive(Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        traits: Vec<String>,
        #[serde(default)]
        hypotheses: Vec<String>,
    }

    /// `distilled_through` is left at 0 here — the caller (`distill`) stamps
    /// the real event count, since this function only sees model output.
    pub fn projection(text: &str) -> Result<ProfileProjection, EngineError> {
        let json = extract_json(text).ok_or_else(|| EngineError::Parse("no JSON".to_string()))?;
        let raw: Raw = serde_json::from_str(json).map_err(|e| EngineError::Parse(e.to_string()))?;
        Ok(ProfileProjection {
            traits: raw.traits,
            hypotheses: raw.hypotheses,
            distilled_through: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{Ai, MockProvider, Models, Provider};

    fn mock_ai(reply: &str) -> Ai {
        Ai::new(
            Provider::Mock(MockProvider::new(reply)),
            Models::single("mock"),
        )
    }

    fn scripted_ai<F>(f: F) -> Ai
    where
        F: Fn(&crate::ai::ChatRequest) -> String + Send + Sync + 'static,
    {
        Ai::new(
            Provider::Mock(MockProvider::scripted(f)),
            Models::single("mock"),
        )
    }

    #[test]
    fn should_distill_on_node_close_regardless_of_event_count() {
        assert!(should_distill(true, 3, 0));
    }

    #[test]
    fn should_distill_on_threshold_without_a_close() {
        assert!(!should_distill(false, 29, 0));
        assert!(should_distill(false, 30, 0));
        assert!(!should_distill(false, 60, 31));
        assert!(should_distill(false, 61, 31));
    }

    #[test]
    fn should_distill_is_false_on_an_empty_log() {
        assert!(!should_distill(true, 0, 0));
    }

    #[test]
    fn evidence_table_text_is_sorted_and_empty_when_no_evidence() {
        assert_eq!(evidence_table_text(&HashMap::new()), "");

        let mut table = HashMap::new();
        table.insert(
            "worked-example".to_string(),
            TacticOutcome {
                demonstrated: 2,
                partial: 0,
                not_demonstrated: 0,
            },
        );
        table.insert(
            "analogy".to_string(),
            TacticOutcome {
                demonstrated: 1,
                partial: 1,
                not_demonstrated: 0,
            },
        );
        let text = evidence_table_text(&table);
        let analogy_pos = text.find("analogy").unwrap();
        let worked_pos = text.find("worked-example").unwrap();
        assert!(analogy_pos < worked_pos, "sorted alphabetically");
        assert!(text.contains("1 demonstrated, 1 partial, 0 not-demonstrated"));
    }

    #[tokio::test]
    async fn distill_parses_traits_and_hypotheses_and_stamps_event_count() {
        let ai = mock_ai(
            r#"{"traits":["worked-example lands better than analogy (3/3 vs 1/3)"],"hypotheses":["struggles with abstract analogy explanations"]}"#,
        );
        let activity = ActivityCounts::default();
        let projection = distill(&ai, "analogy: 1 demonstrated", &activity, 42, None)
            .await
            .unwrap();
        assert_eq!(projection.traits.len(), 1);
        assert_eq!(projection.hypotheses.len(), 1);
        assert_eq!(projection.distilled_through, 42);
    }

    #[tokio::test]
    async fn distill_errors_on_malformed_output() {
        let ai = mock_ai("not json at all");
        let err = distill(&ai, "", &ActivityCounts::default(), 1, None)
            .await
            .unwrap_err();
        assert!(matches!(err, EngineError::Parse(_)));
    }

    #[tokio::test]
    async fn distill_prompt_carries_the_existing_profile_for_revision() {
        let ai = scripted_ai(|req| {
            let full = req
                .messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                full.contains("user-authored trait"),
                "existing traits must reach the distillation prompt: {full}"
            );
            r#"{"traits":["user-authored trait"],"hypotheses":[]}"#.to_string()
        });
        let existing = ProfileProjection {
            traits: vec!["user-authored trait".to_string()],
            hypotheses: vec![],
            distilled_through: 5,
        };
        distill(&ai, "", &ActivityCounts::default(), 6, Some(&existing))
            .await
            .unwrap();
    }

    #[test]
    fn render_for_prompt_degrades_to_empty_on_nothing_yet() {
        assert_eq!(render_for_prompt("", None), "");
    }

    #[test]
    fn render_for_prompt_includes_evidence_traits_and_hypotheses() {
        let projection = ProfileProjection {
            traits: vec!["trait one".to_string()],
            hypotheses: vec!["hypothesis one".to_string()],
            distilled_through: 10,
        };
        let text = render_for_prompt("analogy: 1 demonstrated", Some(&projection));
        assert!(text.contains("analogy: 1 demonstrated"));
        assert!(text.contains("trait one"));
        assert!(text.contains("hypothesis one"));
    }

    #[test]
    fn render_for_prompt_respects_the_char_budget() {
        let projection = ProfileProjection {
            traits: vec!["x".repeat(10_000)],
            hypotheses: vec![],
            distilled_through: 0,
        };
        let text = render_for_prompt("", Some(&projection));
        assert!(text.chars().count() <= PROMPT_CHAR_BUDGET);
    }
}
