use super::{Event, EventKind, Grade};
use crate::movement::AgentPolicy;
use std::collections::{HashMap, HashSet};

/// Outcome counts for one tactic (§7): the profile's 0-token evidence
/// table. Keyed on tactic alone for S1 — the concept/domain axis (§6.2:
/// local-to-concept calibration) joins in once S3 gives a node→concept
/// mapping to key on; keying on `move_type` now would silently build the
/// wrong table (move type is a shape, not a subject).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TacticOutcome {
    pub demonstrated: u32,
    pub partial: u32,
    pub not_demonstrated: u32,
}

/// Joins each `MoveGenerated`'s tactics with the outcome of its **node**
/// (S16, 2026-08-21 — fixes a join-key bug, not a schema change). Keyed on
/// `node_id`, not `move_id`: `MoveGraded` is appended in exactly one place
/// in the whole codebase (`api/grading.rs`), always with the id of the
/// **test** move being graded — so a `move_id`-keyed join only ever credits
/// test-move tactics, and every teaching move that precedes the test in the
/// same node (`explain`/`ask`/`confront`, the ones that actually carry
/// `analogy`/`worked-example`/`formal-first`/`interactive-visual`) piles up
/// in `pending` forever and never joins anything. In practice the evidence
/// table was measuring exercise-writing style, not teaching effectiveness —
/// exactly backwards from what §7 needs to feed `decide_move`.
///
/// All `MoveGenerated` tactics accumulated for a node since its last credit
/// are credited **equally** when that node's `MoveGraded` arrives, then
/// cleared — a node graded again later (a fresh review cycle) only credits
/// what was generated since the previous grade, never double-counts.
/// Weighting is deliberately flat (every teaching move that preceded the
/// gate gets the same weight as any other) rather than by recency/proximity
/// to the gate: there is no telemetry yet to justify anything else, and
/// flat is the more honest default until there is (noted in PLAN.md S16,
/// not resolved silently).
pub fn tactic_outcomes(events: impl Iterator<Item = Event>) -> HashMap<String, TacticOutcome> {
    let mut pending: HashMap<String, Vec<String>> = HashMap::new();
    let mut out: HashMap<String, TacticOutcome> = HashMap::new();
    for event in events {
        let Some(node_id) = event.node_id else {
            continue;
        };
        match event.kind {
            EventKind::MoveGenerated { tactics, .. } => {
                pending.entry(node_id).or_default().extend(tactics);
            }
            EventKind::MoveGraded { grade, .. } => {
                let Some(tactics) = pending.remove(&node_id) else {
                    continue;
                };
                for tactic in tactics {
                    let entry = out.entry(tactic).or_default();
                    match grade {
                        Grade::Demonstrated => entry.demonstrated += 1,
                        Grade::Partial => entry.partial += 1,
                        Grade::NotDemonstrated => entry.not_demonstrated += 1,
                    }
                }
            }
            EventKind::SchemaViolation { .. }
            | EventKind::PlanDecided { .. }
            | EventKind::ObjectiveRevised { .. }
            | EventKind::NodeSkipped
            | EventKind::QuestionAsked { .. }
            | EventKind::AnnotationAdded { .. }
            | EventKind::NodeReadToEnd
            | EventKind::NodeGenerated { .. } => {}
        }
    }
    out
}

/// Counts feeding the S9 ladder-rung decision: schema violations and move
/// diversity are both "capability is measured, not assumed" signals.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LadderSignals {
    pub moves_generated: u32,
    pub schema_violations: u32,
    pub move_types_seen: HashSet<String>,
}

pub fn ladder_signals(events: impl Iterator<Item = Event>) -> LadderSignals {
    let mut signals = LadderSignals::default();
    for event in events {
        match event.kind {
            EventKind::MoveGenerated { move_type, .. } => {
                signals.moves_generated += 1;
                // `research` (§S13) is a structural interception, never a
                // real `decide_move` choice among content move types — it
                // never reaches `render()`. Counting it here would let an
                // ungrounded document (where `research` re-offers on every
                // node's first move, per `movement::prompt::menu`) dodge
                // the diversity-collapse check indefinitely just by being
                // ungrounded, even while every real move it picks is the
                // same type.
                if move_type != "research" {
                    signals.move_types_seen.insert(move_type);
                }
            }
            EventKind::SchemaViolation { .. } => {
                signals.schema_violations += 1;
            }
            EventKind::MoveGraded { .. }
            | EventKind::PlanDecided { .. }
            | EventKind::ObjectiveRevised { .. }
            | EventKind::NodeSkipped
            | EventKind::QuestionAsked { .. }
            | EventKind::AnnotationAdded { .. }
            | EventKind::NodeReadToEnd
            | EventKind::NodeGenerated { .. } => {}
        }
    }
    signals
}

/// Minimum sample before ladder telemetry acts (§9). A brand-new document
/// with only a couple of moves must not be judged on noise — same "don't
/// snap-judge" principle §6.2 already applies to abstraction calibration,
/// applied here to the model's own capability.
const MIN_MOVES_FOR_CALIBRATION: u32 = 5;

/// Diversity collapse needs a bigger sample than the violation-rate
/// check: a node legitimately closes in as few as one move (a `test`
/// forced by the S3 cost guard, or simply the right call when nothing
/// needed explaining), so a handful of moves across one or two nodes
/// picking the same type is not evidence of anything — it takes several
/// *nodes'* worth of moves before "always the same type" stops being
/// explainable by short, legitimately test-heavy stretches.
const MIN_MOVES_FOR_DIVERSITY_CHECK: u32 = 10;

/// One rung down, floor at `L0`. `AgentPolicy` carries no derivable
/// ordering (it's a closed enum, not a number), so this is a plain match
/// rather than arithmetic on a cast.
fn step_down(policy: AgentPolicy) -> AgentPolicy {
    match policy {
        AgentPolicy::L2 => AgentPolicy::L1,
        AgentPolicy::L1 | AgentPolicy::L0 => AgentPolicy::L0,
    }
}

/// §9 "mover o degrau por documento": calibrates the config-derived prior
/// rung DOWN (never up) for *this* document, from its own
/// [`LadderSignals`] fold — "capacidade é medida, não assumida", the same
/// pattern §6.2 already applies to abstraction level, applied here to the
/// model itself. Two signals, both already cheap one-pass folds over
/// `events.jsonl` (no new sidecar file — the log stays the only source of
/// truth, same reasoning as `node_states`/`revisit_suggestion` not
/// needing a `progress.json`):
///
/// - **Schema violations**: more than one in three generated moves needed
///   a repair round — the model is struggling with the current
///   menu/format, so constrain it further. NOTE: `moves_generated` counts
///   every `MoveGenerated` event in the document's log, including ones
///   that were never a `decide_move` choice at all — §8.2's remediation
///   path forces a `test` move outside the rung entirely (see its call
///   site in `api.rs`). Those moves can never violate (there's no repair
///   round on a Rust-constructed move) and can never collapse diversity
///   either, so a document with heavy remediation activity dilutes both
///   signals below their thresholds purely by inflating the denominator.
///   Accepted, not filtered: separating "decided by a rung" from "forced
///   by construction" needs a marker `MoveGenerated` doesn't carry yet,
///   and remediation activity is itself a real difficulty signal (the
///   learner keeps failing), so undercounting toward "everything's fine"
///   in that case is a defensible direction to be wrong in for a minimum
///   slice — but it means the thresholds below are measured against a
///   noisier denominator than "moves the rung actually decided."
/// - **Move-diversity collapse**: at `L1`/`L2` (where `decide_move` is a
///   real choice among several move types), never varying after enough
///   samples ([`MIN_MOVES_FOR_DIVERSITY_CHECK`], deliberately higher than
///   the violation-rate minimum — a node can legitimately close in a
///   single forced or correctly-chosen `test`, so a short stretch picking
///   one type isn't evidence of anything) means the model isn't actually
///   using the freedom it was given — the same failure mode a closed
///   menu (a lower rung) already forces into a fixed shape, so there is
///   nothing lost by imposing it. `L0` never calls the AI to decide
///   (`decide_move` is a pure Rust rule), so it has no diversity signal
///   to collapse and is already the floor — this only ever demotes
///   `L1`/`L2`.
///
/// Recomputed fresh from the full log on every call (no persisted
/// "current rung" to desync from it — the `AgentPolicy`/config
/// desync class of bug already closed once, S3): as violations dilute
/// across a growing log the rate can drop back below threshold and the
/// document recovers toward the prior on its own, with no separate
/// recovery mechanism to build or get wrong.
///
/// **Deliberately not a signal here:** grade/evaluator incoherence (§16 —
/// CLAUDE.md already tracks this as its own open risk). Detecting a
/// rubric grading nonsensically is a semantic judgment, not a count, and
/// is a materially harder problem than the two signals above — left for
/// a future slice rather than approximated badly here.
pub fn calibrate_rung(prior: AgentPolicy, signals: &LadderSignals) -> AgentPolicy {
    if signals.moves_generated >= MIN_MOVES_FOR_CALIBRATION {
        let violation_rate =
            f64::from(signals.schema_violations) / f64::from(signals.moves_generated);
        if violation_rate > 1.0 / 3.0 {
            return step_down(prior);
        }
    }
    if signals.moves_generated >= MIN_MOVES_FOR_DIVERSITY_CHECK
        && matches!(prior, AgentPolicy::L1 | AgentPolicy::L2)
        && signals.move_types_seen.len() <= 1
    {
        return step_down(prior);
    }
    prior
}

/// A node's status derived from the log (§S5) — the availability gate's
/// only input, folded in one pass rather than tracked in a second
/// `progress.json` that could desync from it (the `AgentPolicy`/config
/// desync class of bug, S3). Absence from the returned map means never
/// attempted or skipped: "locked" vs. "available" for that case is a
/// prerequisites check the caller makes, not state this fold owns.
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

/// Coarse activity counts (§7) feeding the profile distillation's
/// context text — "how much has happened", not a grade/rubric signal.
/// A separate one-pass fold rather than folded into `tactic_outcomes`
/// (different subject entirely — reading interactions and skips, not
/// move outcomes), same "several small aggregates" shape as the rest of
/// this module.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ActivityCounts {
    pub nodes_skipped: u32,
    pub questions_asked: u32,
    pub annotations_added: u32,
}

pub fn activity_counts(events: impl Iterator<Item = Event>) -> ActivityCounts {
    let mut out = ActivityCounts::default();
    for event in events {
        match event.kind {
            EventKind::NodeSkipped => out.nodes_skipped += 1,
            EventKind::QuestionAsked { .. } => out.questions_asked += 1,
            EventKind::AnnotationAdded { .. } => out.annotations_added += 1,
            _ => {}
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
/// from the log the same way it reconstructs `resumed_moves`/
/// `resumed_move_index` — otherwise a model that re-picks `research` on
/// every request it's still offered (nothing else caps it in-process) could
/// burn through `MAX_MOVES_PER_NODE`'s whole budget on research alone,
/// forcing the last slot's `Test` with zero teaching content — exactly the
/// zero-prose bug `MoveContext::observation`'s sibling guard,
/// `movement::enforce_teaching_before_test`, was built to prevent, except
/// that guard doesn't run on the forced-last-slot path at all.
pub fn research_attempted(mut events: impl Iterator<Item = Event>, node_id: &str) -> bool {
    events.any(|e| {
        e.node_id.as_deref() == Some(node_id)
            && matches!(&e.kind, EventKind::MoveGenerated { move_type, .. } if move_type == "research")
    })
}

/// What the learner did **since the last move settled** for `node_id` (§S18)
/// — folded from the event log, the "perception channel" PLAN.md's S18
/// describes, not a new sidecar. A single pass: the accumulator resets every
/// time a `MoveGenerated` for this node is seen, so only events after the
/// most recent one survive to the end — exactly the window between "a move
/// was persisted" and "the next `/generate` call for this node arrived",
/// which is what a per-move-request `decide_move` needs to react to.
///
/// Deliberately narrower than PLAN.md's S18 sketch (`dwell_ms_per_block`,
/// `selections`): neither has a supporting event yet — a selection only
/// becomes an event once it turns into a question, and dwell time needs a
/// new client-reporting channel that doesn't exist — so an always-empty
/// field would be prompt noise `decide_move` has to ignore, not signal.
/// `annotations` carries anchor block ids, not text: `AnnotationAdded`
/// itself never logged the note's content (`events.rs`'s doc comment), only
/// where it landed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObservationFrame {
    pub reached_end: bool,
    pub questions: Vec<String>,
    pub annotations: Vec<String>,
}

pub fn observation_frame(events: impl Iterator<Item = Event>, node_id: &str) -> ObservationFrame {
    let mut frame = ObservationFrame::default();
    for e in events {
        if e.node_id.as_deref() != Some(node_id) {
            continue;
        }
        match e.kind {
            // `respond` excluded from the reset (§S18, live-caught
            // 2026-08-21): it's `ask_question` appending its own
            // bookkeeping for the very question this fold is trying to
            // surface, not this node's own `generate_node` loop settling a
            // move — resetting on it would wipe the question out of the
            // frame before the node's real next move ever saw it.
            EventKind::MoveGenerated { ref move_type, .. } if move_type != "respond" => {
                frame = ObservationFrame::default()
            }
            EventKind::QuestionAsked { question, .. } => {
                if !question.is_empty() {
                    frame.questions.push(question);
                }
            }
            EventKind::AnnotationAdded { anchor_block } => frame.annotations.push(anchor_block),
            EventKind::NodeReadToEnd => frame.reached_end = true,
            _ => {}
        }
    }
    frame
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

    fn move_generated(move_type: &str) -> Event {
        Event {
            id: "e".to_string(),
            ts: 0,
            node_id: None,
            kind: EventKind::MoveGenerated {
                move_id: "m".to_string(),
                move_type: move_type.to_string(),
                tactics: Vec::new(),
                rung: "L2".to_string(),
            },
        }
    }

    /// §S13: a `research` move must never count toward move-type diversity
    /// — it's a structural interception, not a real `decide_move` choice.
    /// An ungrounded document re-offers (and can re-pick) `research` on
    /// every node's first move, so if it counted, a document whose every
    /// REAL choice is monotonous (`test` every time) would read as diverse
    /// forever, purely from staying ungrounded — defeating the collapse
    /// check for exactly the documents most likely to need it.
    #[test]
    fn research_never_counts_toward_move_type_diversity() {
        let mut events = vec![move_generated("research")];
        events.extend((0..10).map(|_| move_generated("test")));

        let signals = ladder_signals(events.into_iter());
        assert_eq!(
            signals.move_types_seen,
            HashSet::from(["test".to_string()]),
            "research must not appear in the diversity set"
        );
        assert_eq!(
            signals.moves_generated, 11,
            "but still counts toward the sample size"
        );

        // The monotonous-but-"diverse"-looking document still steps down.
        assert_eq!(
            calibrate_rung(AgentPolicy::L2, &signals),
            AgentPolicy::L1,
            "a document that only ever picks one real move type must still \
             collapse, even though `research` also fired"
        );
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
                rung: "L0".to_string(),
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
