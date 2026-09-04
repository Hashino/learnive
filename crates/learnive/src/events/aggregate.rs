use super::{Event, EventKind, Grade};
use std::collections::{HashMap, HashSet};

/// §S23 scaffolding parameter (SPEC §6.2, §8) — a few-rung, zero-cost
/// signal derived by folding over the same log `node_states` reads, no new
/// event and no persisted state to desync. The measured signal is the
/// **cost of reaching `demonstrated`**: how many `MoveGraded` a node took
/// before its first `Demonstrated` grade, averaged over the most recently
/// demonstrated nodes. Many attempts recently ⇒ more support (a worked
/// example before the problem); first-try success ⇒ less (the problem
/// direct). This calibrates SUPPORT, never difficulty —
/// `movement/prompt.rs`'s fade addendum must never read a low level as
/// license to make the exercise itself harder.
///
/// **Signal correction this slice carries (SPEC §6.2):** the original
/// spec counted *questions* toward "material is too easy". That's
/// inverted — asking is elaborative generation, associated with BETTER
/// learning, and §7 already calls the question the single most valuable
/// signal in the whole system, so counting it as a difficulty signal here
/// would contradict that outright. Only grading attempts feed this fold;
/// `QuestionAsked` is deliberately absent from the match below.
///
/// Since S33 this is the ONLY behavior-telemetry consumer left: move
/// choice is a deterministic template, so nothing else folds the log to
/// steer generation — telemetry answers "what comes next" (review
/// scheduling, gating, support level), never "what to say".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScaffoldingLevel {
    /// Several attempts needed on recently demonstrated nodes: more support.
    High,
    /// Not enough sample yet, or a mixed recent history — the neutral prior.
    #[default]
    Medium,
    /// Recently demonstrating on (close to) the first attempt: less support.
    Low,
}

/// Minimum demonstrated nodes before this fold says anything but the
/// neutral `Medium` — same "don't snap-judge on noise" principle §9
/// applied to ladder calibration, applied to a much smaller, per-node
/// sample here (one point per NODE, not per move).
const MIN_NODES_FOR_SCAFFOLDING: usize = 3;

/// How many of the most recently demonstrated nodes the average is taken
/// over — deliberately small: SPEC §6.2 asks for calibration that's
/// "contínua e local", and older nodes say less about the learner's
/// CURRENT footing than what just happened.
const RECENT_NODES_FOR_SCAFFOLDING: usize = 8;

pub fn scaffolding_level(events: impl Iterator<Item = Event>) -> ScaffoldingLevel {
    let mut attempts: HashMap<String, u32> = HashMap::new();
    let mut done: HashSet<String> = HashSet::new();
    // (ts of the demonstrating grade, attempts taken to reach it).
    let mut demonstrated: Vec<(u64, u32)> = Vec::new();
    for event in events {
        let Some(node_id) = event.node_id else {
            continue;
        };
        if done.contains(&node_id) {
            continue;
        }
        if let EventKind::MoveGraded { grade, .. } = event.kind {
            let count = attempts.entry(node_id.clone()).or_insert(0);
            *count += 1;
            if grade == Grade::Demonstrated {
                demonstrated.push((event.ts, *count));
                done.insert(node_id);
            }
        }
    }
    if demonstrated.len() < MIN_NODES_FOR_SCAFFOLDING {
        return ScaffoldingLevel::Medium;
    }
    demonstrated.sort_by_key(|(ts, _)| *ts);
    let recent: Vec<u32> = demonstrated
        .into_iter()
        .rev()
        .take(RECENT_NODES_FOR_SCAFFOLDING)
        .map(|(_, a)| a)
        .collect();
    let avg = f64::from(recent.iter().sum::<u32>()) / recent.len() as f64;
    // Arbitrary, few-rung thresholds — noted honestly in PLAN.md S23 as
    // having no basis in this application yet: this is a bias nudge on a
    // prompt addendum, never the sole thing deciding a move's shape, so
    // getting these exact cutoffs slightly wrong costs little.
    if avg >= 2.0 {
        ScaffoldingLevel::High
    } else if avg <= 1.2 {
        ScaffoldingLevel::Low
    } else {
        ScaffoldingLevel::Medium
    }
}

/// A node's status derived from the log (§S5) — the availability gate's
/// only input, folded in one pass rather than tracked in a second
/// `progress.json` that could desync from it (the config/state desync
/// class of bug, S3). Absence from the returned map means never attempted
/// or skipped: "locked" vs. "available" for that case is a prerequisites
/// check the caller makes, not state this fold owns.
///
/// Monotonic: once a node reaches `Demonstrated` no later event can move
/// it back down (a node is not re-attempted after demonstrating it —
/// §S5's revisit scheduler reopens already-`Skipped` nodes, not
/// demonstrated ones). `Skipped` and `Attempted` rank equally — whichever
/// happened most recently wins, since both just mean "touched, open".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeState {
    Skipped,
    Attempted,
    Demonstrated,
}

fn node_state_rank(s: NodeState) -> u8 {
    match s {
        NodeState::Skipped | NodeState::Attempted => 1,
        NodeState::Demonstrated => 2,
    }
}

/// Folds `other` into `into`, keeping the higher-ranked state per node id
/// (§S15b step 3: a visiting document's own log and a reference's owner log
/// are folded together, and neither is allowed to downgrade the other — a
/// plain `HashMap::extend` would let iteration order decide the winner).
pub fn merge_node_states(into: &mut HashMap<String, NodeState>, other: HashMap<String, NodeState>) {
    for (node_id, candidate) in other {
        match into.get(&node_id) {
            Some(existing) if node_state_rank(*existing) >= node_state_rank(candidate) => {}
            _ => {
                into.insert(node_id, candidate);
            }
        }
    }
}

pub fn node_states(events: impl Iterator<Item = Event>) -> HashMap<String, NodeState> {
    let mut out: HashMap<String, NodeState> = HashMap::new();
    for event in events {
        let Some(node_id) = event.node_id else {
            continue;
        };
        let candidate = match event.kind {
            EventKind::MoveGraded {
                grade: Grade::Demonstrated,
                ..
            } => NodeState::Demonstrated,
            EventKind::MoveGraded { .. } => NodeState::Attempted,
            EventKind::NodeSkipped => NodeState::Skipped,
            _ => continue,
        };
        match out.get(&node_id) {
            Some(existing) if node_state_rank(*existing) > node_state_rank(candidate) => {}
            _ => {
                out.insert(node_id, candidate);
            }
        }
    }
    out
}

/// Whether `finalize` has ever completed for this node id — `prepare`'s
/// regen guard (§S6 follow-up), replacing "a node file exists" now that
/// content persists progressively per move. A node file can exist and still
/// be mid-generation (or abandoned after a dropped connection); only a
/// `NodeGenerated` event means the content layer is the real, complete
/// thing `finalize` produces (prose through the graded move + its rubric
/// sidecar), so retrying generation for a node that has one is refused, and
/// retrying one that doesn't is allowed to overwrite the partial file
/// cleanly — the log is the source of truth, not the file's mere presence.
pub fn node_generated(mut events: impl Iterator<Item = Event>, node_id: &str) -> bool {
    events.any(|e| {
        e.node_id.as_deref() == Some(node_id) && matches!(e.kind, EventKind::NodeGenerated { .. })
    })
}

/// Whether a `research` move has EVER been logged for `node_id` — §S18's
/// cross-request counterpart to `MoveContext::research_attempted`. Before
/// the per-move-request split, that cap lived entirely in one request's
/// in-memory `ctx` (never reset across the move loop's iterations), which
/// was enough because the whole node generated in one request. Now each
/// `/generate` call gets a fresh `ctx`, so `prepare` reconstructs the cap
/// from the log the same way it reconstructs `resumed_ungraded_moves` —
/// otherwise the Rust-forced research interception (S33) could fire again
/// on every request it's still eligible for, burning through
/// `MAX_MOVES_PER_NODE`'s whole budget on research alone and leaving no
/// slot for the template's teaching or graded moves.
pub fn research_attempted(mut events: impl Iterator<Item = Event>, node_id: &str) -> bool {
    events.any(|e| {
        e.node_id.as_deref() == Some(node_id)
            && matches!(&e.kind, EventKind::MoveGenerated { move_type, .. } if move_type == "research")
    })
}

/// §S5's revisit scheduler: which currently-`Skipped` node has been
/// deferred longest. A pure spacing heuristic (least-recently-touched
/// wins), not a full spaced-repetition algorithm — PLAN.md's S5 bullet
/// asks for "scheduler de revisita", this is its whole implementation.
/// `None` when nothing is currently skipped. A separate one-pass fold
/// rather than folded into `node_states` (which only keeps the *final*
/// state, discarding the timestamp that made it the most-overdue skip) —
/// same "several small aggregates, each its own pass" shape as the rest
/// of this module.
pub fn revisit_suggestion(events: impl Iterator<Item = Event>) -> Option<String> {
    let mut last_skip_ts: HashMap<String, u64> = HashMap::new();
    for event in events {
        let ts = event.ts;
        let Some(node_id) = event.node_id else {
            continue;
        };
        match event.kind {
            EventKind::NodeSkipped => {
                last_skip_ts.insert(node_id, ts);
            }
            // Graded (attempted or demonstrated) since the skip: no
            // longer merely deferred, drop it as a revisit candidate.
            EventKind::MoveGraded { .. } => {
                last_skip_ts.remove(&node_id);
            }
            _ => {}
        }
    }
    last_skip_ts
        .into_iter()
        .min_by_key(|(_, ts)| *ts)
        .map(|(id, _)| id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Event;

    fn graded(node_id: &str, ts: u64, grade: Grade) -> Event {
        Event {
            id: "e".to_string(),
            ts,
            node_id: Some(node_id.to_string()),
            kind: EventKind::MoveGraded {
                move_id: "m".to_string(),
                grade,
            },
        }
    }

    /// A thin sample (fewer than [`MIN_NODES_FOR_SCAFFOLDING`] demonstrated
    /// nodes) must not tip the level away from the neutral prior — same
    /// "don't snap-judge on noise" principle the old ladder calibration
    /// applied.
    #[test]
    fn scaffolding_level_stays_medium_on_a_thin_sample() {
        let events = vec![
            graded("n1", 1, Grade::NotDemonstrated),
            graded("n1", 2, Grade::Demonstrated),
        ];
        assert_eq!(
            scaffolding_level(events.into_iter()),
            ScaffoldingLevel::Medium
        );
    }

    /// Several recent nodes each needing multiple attempts before
    /// `demonstrated` ⇒ High (more support), not read as "make it harder".
    #[test]
    fn scaffolding_level_is_high_when_recent_nodes_took_several_attempts() {
        let mut events = Vec::new();
        for (i, node) in ["n1", "n2", "n3"].into_iter().enumerate() {
            let base = (i as u64) * 10;
            events.push(graded(node, base, Grade::NotDemonstrated));
            events.push(graded(node, base + 1, Grade::Partial));
            events.push(graded(node, base + 2, Grade::Demonstrated));
        }
        assert_eq!(
            scaffolding_level(events.into_iter()),
            ScaffoldingLevel::High
        );
    }

    /// Several recent nodes each demonstrated on the first attempt ⇒ Low
    /// (less support, the problem direct).
    #[test]
    fn scaffolding_level_is_low_when_recent_nodes_demonstrate_on_the_first_try() {
        let events = ["n1", "n2", "n3"]
            .into_iter()
            .enumerate()
            .map(|(i, node)| graded(node, i as u64, Grade::Demonstrated))
            .collect::<Vec<_>>();
        assert_eq!(scaffolding_level(events.into_iter()), ScaffoldingLevel::Low);
    }

    /// A `MoveGraded` for a node that never reaches `Demonstrated` must
    /// never contribute a data point — an abandoned or still-struggling
    /// node shouldn't silently count toward "recently easy" or "recently
    /// hard" until it actually closes.
    #[test]
    fn scaffolding_level_ignores_nodes_never_demonstrated() {
        let events = vec![
            graded("stuck", 1, Grade::NotDemonstrated),
            graded("stuck", 2, Grade::Partial),
            graded("n1", 3, Grade::Demonstrated),
            graded("n2", 4, Grade::Demonstrated),
            graded("n3", 5, Grade::Demonstrated),
        ];
        assert_eq!(scaffolding_level(events.into_iter()), ScaffoldingLevel::Low);
    }

    /// Asking a question must never itself count as a grading attempt — the
    /// §6.2 signal correction this slice carries: elaboration is not
    /// difficulty.
    #[test]
    fn scaffolding_level_does_not_count_questions_as_attempts() {
        let events = vec![
            Event {
                id: "e".to_string(),
                ts: 1,
                node_id: Some("n1".to_string()),
                kind: EventKind::QuestionAsked {
                    move_id: "m".to_string(),
                    anchor_block: "b1".to_string(),
                    question: "why?".to_string(),
                },
            },
            graded("n1", 2, Grade::Demonstrated),
            graded("n2", 3, Grade::Demonstrated),
            graded("n3", 4, Grade::Demonstrated),
        ];
        assert_eq!(scaffolding_level(events.into_iter()), ScaffoldingLevel::Low);
    }

    fn generated_event(node_id: &str, kind: EventKind) -> Event {
        Event {
            id: "e".to_string(),
            ts: 0,
            node_id: Some(node_id.to_string()),
            kind,
        }
    }

    /// §S6 follow-up: `node_generated` is the explicit completion signal
    /// `prepare`'s regen guard reads — a node with moves but no
    /// `NodeGenerated` event is still mid-generation (or abandoned
    /// mid-stream), not done.
    #[test]
    fn node_generated_is_true_only_after_the_completion_event() {
        let mid_generation = vec![generated_event(
            "n1",
            EventKind::MoveGenerated {
                move_id: "m1".to_string(),
                move_type: "explain".to_string(),
                tactics: Vec::new(),
                rung: "deterministic".to_string(),
            },
        )];
        assert!(!node_generated(mid_generation.into_iter(), "n1"));

        let finished = vec![generated_event(
            "n1",
            EventKind::NodeGenerated {
                move_id: "m2".to_string(),
            },
        )];
        assert!(node_generated(finished.into_iter(), "n1"));
    }

    /// A `NodeGenerated` event for a different node must never mark this
    /// one finalized — the guard is keyed on node id, not "any completion
    /// event exists in this document's log at all".
    #[test]
    fn node_generated_does_not_leak_across_node_ids() {
        let events = vec![generated_event(
            "other",
            EventKind::NodeGenerated {
                move_id: "m1".to_string(),
            },
        )];
        assert!(!node_generated(events.into_iter(), "n1"));
    }
}
