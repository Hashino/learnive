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

/// Joins each `MoveGenerated`'s tactics with the `MoveGraded` that
/// follows for the same `move_id`, one pass over the log.
pub fn tactic_outcomes(events: impl Iterator<Item = Event>) -> HashMap<String, TacticOutcome> {
    let mut pending: HashMap<String, Vec<String>> = HashMap::new();
    let mut out: HashMap<String, TacticOutcome> = HashMap::new();
    for event in events {
        match event.kind {
            EventKind::MoveGenerated {
                move_id, tactics, ..
            } => {
                pending.insert(move_id, tactics);
            }
            EventKind::MoveGraded { move_id, grade } => {
                let Some(tactics) = pending.remove(&move_id) else {
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
            | EventKind::NodeReadToEnd => {}
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
                signals.move_types_seen.insert(move_type);
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
            | EventKind::NodeReadToEnd => {}
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
