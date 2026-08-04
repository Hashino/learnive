//! Event substrate (§7.1) — S1 of the agentic-loop build (see PLAN.md's
//! "core evolution: agentic loop" section).
//!
//! `<doc>/events.jsonl` is the append-only source of truth for a living
//! document; everything else derived from it (the evidence profile §7, the
//! policy-ladder telemetry §9) is a **rebuildable cache** — same pattern as
//! the retrieval index (§10, `crate::retrieval::index`). The log is never
//! fully loaded: [`EventLog::iter`] streams lines lazily, and aggregates fold
//! over the iterator in one pass.
//!
//! `EventLog::append` is wired into `api.rs` (S3): `generate_node`/`answer`
//! append `MoveGenerated`/`MoveGraded`/`SchemaViolation` as moves happen.
//! `EventLog::iter` and the `aggregate` module (the read side — §7's evidence
//! table, §9's ladder telemetry) are both wired into `api.rs` as of S7/S9.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::engine::Grade;

/// One entry in the event log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    /// Unix epoch milliseconds.
    pub ts: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    #[serde(flatten)]
    pub kind: EventKind,
}

/// Event payloads. Deliberately limited to what S1/S2 can actually emit — a
/// move is generated, graded, or fails to validate (§9's telemetry signal).
/// Later slices (skip §5, selection→question §6, ...) add their own variants
/// rather than this module guessing their shape now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    /// A move was generated for a node (§6 ABI). `tactics` are the
    /// self-labels emitted in the *same* generation call (§7) — profile
    /// attribution is a join over the log, not a reflection call. `move_type`
    /// and `rung` stay `String` here (not `engine::MoveType`/a ladder enum):
    /// those types land in S2, and the persisted log outliving today's Rust
    /// types is the point of an event-sourced log.
    MoveGenerated {
        move_id: String,
        move_type: String,
        #[serde(default)]
        tactics: Vec<String>,
        rung: String,
    },
    /// A graded move's outcome (§8) — the other half of the profile's
    /// intervention→outcome join.
    MoveGraded { move_id: String, grade: Grade },
    /// Model output failed to validate against the move schema (§9: ladder
    /// telemetry that decides when to step down a rung).
    SchemaViolation { move_type: String, detail: String },
    /// A `plan` move's proposed outline revision was approved or rejected
    /// (§5/§S4) — joins back to the `MoveGenerated` event for the same
    /// `move_id`, the acceptance-rate signal §9's ladder telemetry wants.
    PlanDecided { move_id: String, approved: bool },
    /// The document objective was revised (§S4/§5) — `version` is the new
    /// `ObjectiveLog` entry's version number, so this can be joined back to
    /// `objective.json` without duplicating the text into the event log.
    ObjectiveRevised { version: u32 },
    /// The learner skipped a node (§S5, "botão pular") instead of answering
    /// it — a high-signal event in its own right (§7), and the input to
    /// `aggregate::node_states`' `Skipped` state: the node stays open
    /// (available), just not the one currently being worked.
    NodeSkipped,
    /// A question was asked during reading (§S6, §9 "the document is the
    /// answer") — via a text selection or, with no selection, the current
    /// reading line (itself never persisted, §9 — only the resulting
    /// anchor is). `move_id` joins to the `InteractionItem::Thread` of the
    /// same id, the same pattern `RubricSidecar.move_id` uses.
    QuestionAsked {
        move_id: String,
        anchor_block: String,
    },
    /// A user-authored note was added to the living document (§S6, §9/§11:
    /// the document is the only place for notes — the source viewer is
    /// read-only).
    AnnotationAdded { anchor_block: String },
    /// The learner scrolled a node's content to the end (§S6 "Ritmo":
    /// content → reading interactions → scroll-to-end). Captured now as a
    /// pure signal; nothing consumes it yet — gating the next `decide_move`
    /// on it needs the per-node move loop split across requests (draft
    /// persistence + `finalize` merging interactions accumulated mid-draft),
    /// deferred to its own slice, not part of §S6.
    NodeReadToEnd,
}

/// Event-log errors.
#[derive(Debug)]
pub enum EventError {
    Io(io::Error),
    Serialize(serde_json::Error),
}

impl std::fmt::Display for EventError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EventError::Io(e) => write!(f, "I/O error: {e}"),
            EventError::Serialize(e) => write!(f, "could not encode event: {e}"),
        }
    }
}

impl std::error::Error for EventError {}

impl From<io::Error> for EventError {
    fn from(e: io::Error) -> Self {
        EventError::Io(e)
    }
}

impl From<serde_json::Error> for EventError {
    fn from(e: serde_json::Error) -> Self {
        EventError::Serialize(e)
    }
}

/// Append-only log rooted at `<doc>/events.jsonl`. Only constructible via
/// `Store::event_log` — `ensure_safe_id` (store.rs) is the codebase's single
/// path-traversal gate, and this type must not offer a way around it.
pub struct EventLog {
    path: PathBuf,
}

impl EventLog {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Appends one event (its `id`/`ts` are assigned here) and returns it.
    /// A single `write_all` of the full JSON line — regular `O_APPEND` files
    /// don't get the pipe atomicity guarantee, so the line must go out whole.
    pub fn append(&self, node_id: Option<&str>, kind: EventKind) -> Result<Event, EventError> {
        let event = Event {
            id: crate::engine::new_id(),
            ts: now_millis(),
            node_id: node_id.map(str::to_string),
            kind,
        };
        let mut line = serde_json::to_string(&event)?;
        line.push('\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(line.as_bytes())?;
        Ok(event)
    }

    /// Streams events in append order. A missing file (no event appended
    /// yet) is an empty iterator, not an error. Malformed lines (e.g. a
    /// truncated write from an interrupted process) are skipped rather than
    /// failing the whole read — the immutable-log philosophy (§7.1) treats
    /// that as recoverable, not fatal.
    pub fn iter(&self) -> Result<Box<dyn Iterator<Item = Event>>, EventError> {
        let file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(Box::new(std::iter::empty()));
            }
            Err(e) => return Err(e.into()),
        };
        let reader = io::BufReader::new(file);
        Ok(Box::new(reader.lines().filter_map(|line| {
            let line = line.ok()?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            serde_json::from_str::<Event>(trimmed).ok()
        })))
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Aggregates (§7.1, §9) — rebuildable caches computed over the log, one pass.
// ---------------------------------------------------------------------------

pub mod aggregate {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn append_then_iter_roundtrips_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::new(dir.path().join("events.jsonl"));

        log.append(
            Some("n0"),
            EventKind::MoveGenerated {
                move_id: "m1".into(),
                move_type: "explicar".into(),
                tactics: vec!["analogy".into()],
                rung: "l1".into(),
            },
        )
        .unwrap();
        log.append(
            Some("n0"),
            EventKind::MoveGraded {
                move_id: "m1".into(),
                grade: Grade::Demonstrated,
            },
        )
        .unwrap();

        let events: Vec<Event> = log.iter().unwrap().collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].node_id.as_deref(), Some("n0"));
        assert!(matches!(events[0].kind, EventKind::MoveGenerated { .. }));
        assert!(matches!(events[1].kind, EventKind::MoveGraded { .. }));
        // ids are assigned and unique; ts is populated.
        assert_ne!(events[0].id, events[1].id);
        assert!(events[0].ts > 0);
    }

    #[test]
    fn iter_on_missing_file_is_empty_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::new(dir.path().join("events.jsonl"));
        assert_eq!(log.iter().unwrap().count(), 0);
    }

    #[test]
    fn skips_malformed_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = EventLog::new(path.clone());
        log.append(
            None,
            EventKind::SchemaViolation {
                move_type: "testar".into(),
                detail: "missing field".into(),
            },
        )
        .unwrap();
        // Simulate a truncated/corrupted append (e.g. interrupted process).
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{not json\n").unwrap();
        log.append(
            None,
            EventKind::SchemaViolation {
                move_type: "testar".into(),
                detail: "second".into(),
            },
        )
        .unwrap();

        let events: Vec<Event> = log.iter().unwrap().collect();
        assert_eq!(events.len(), 2, "the malformed line is skipped, not fatal");
    }

    #[test]
    fn tactic_outcomes_joins_generated_and_graded() {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::new(dir.path().join("events.jsonl"));

        log.append(
            None,
            EventKind::MoveGenerated {
                move_id: "m1".into(),
                move_type: "explicar".into(),
                tactics: vec!["analogy".into(), "worked-example".into()],
                rung: "l1".into(),
            },
        )
        .unwrap();
        log.append(
            None,
            EventKind::MoveGraded {
                move_id: "m1".into(),
                grade: Grade::Demonstrated,
            },
        )
        .unwrap();
        log.append(
            None,
            EventKind::MoveGenerated {
                move_id: "m2".into(),
                move_type: "testar".into(),
                tactics: vec!["analogy".into()],
                rung: "l1".into(),
            },
        )
        .unwrap();
        log.append(
            None,
            EventKind::MoveGraded {
                move_id: "m2".into(),
                grade: Grade::Partial,
            },
        )
        .unwrap();
        // A generated move with no grade yet (still open) must not appear.
        log.append(
            None,
            EventKind::MoveGenerated {
                move_id: "m3".into(),
                move_type: "explicar".into(),
                tactics: vec!["interactive-visual".into()],
                rung: "l1".into(),
            },
        )
        .unwrap();

        let table = aggregate::tactic_outcomes(log.iter().unwrap());
        let analogy = table.get("analogy").unwrap();
        assert_eq!(analogy.demonstrated, 1);
        assert_eq!(analogy.partial, 1);

        let worked = table.get("worked-example").unwrap();
        assert_eq!(worked.demonstrated, 1);

        assert!(
            !table.contains_key("interactive-visual"),
            "ungraded moves contribute no evidence yet"
        );
    }

    #[test]
    fn ladder_signals_count_violations_and_diversity() {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::new(dir.path().join("events.jsonl"));

        for (move_type, rung) in [("explicar", "l1"), ("testar", "l1"), ("explicar", "l1")] {
            log.append(
                None,
                EventKind::MoveGenerated {
                    move_id: crate::engine::new_id(),
                    move_type: move_type.into(),
                    tactics: vec![],
                    rung: rung.into(),
                },
            )
            .unwrap();
        }
        log.append(
            None,
            EventKind::SchemaViolation {
                move_type: "testar".into(),
                detail: "bad json".into(),
            },
        )
        .unwrap();

        let signals = aggregate::ladder_signals(log.iter().unwrap());
        assert_eq!(signals.moves_generated, 3);
        assert_eq!(signals.schema_violations, 1);
        assert_eq!(signals.move_types_seen.len(), 2);
    }

    #[test]
    fn calibrate_rung_ignores_a_thin_sample() {
        use crate::movement::AgentPolicy;

        // Below MIN_MOVES_FOR_CALIBRATION: even a 100% violation rate must
        // not demote a brand-new document judged on almost nothing.
        let signals = aggregate::LadderSignals {
            moves_generated: 2,
            schema_violations: 2,
            move_types_seen: HashSet::new(),
        };
        assert_eq!(
            aggregate::calibrate_rung(AgentPolicy::L2, &signals),
            AgentPolicy::L2
        );
    }

    #[test]
    fn calibrate_rung_steps_down_on_high_violation_rate() {
        use crate::movement::AgentPolicy;

        let mut move_types_seen = HashSet::new();
        move_types_seen.insert("explicar".to_string());
        move_types_seen.insert("testar".to_string());
        // 2/5 violations > 1/3, past the minimum sample.
        let signals = aggregate::LadderSignals {
            moves_generated: 5,
            schema_violations: 2,
            move_types_seen,
        };
        assert_eq!(
            aggregate::calibrate_rung(AgentPolicy::L2, &signals),
            AgentPolicy::L1
        );
    }

    #[test]
    fn calibrate_rung_steps_down_on_move_diversity_collapse() {
        use crate::movement::AgentPolicy;

        let mut move_types_seen = HashSet::new();
        move_types_seen.insert("explicar".to_string());
        // Zero violations, but the same move type every single time at L1,
        // past MIN_MOVES_FOR_DIVERSITY_CHECK — several nodes' worth, not
        // just one short (legitimately test-heavy) stretch.
        let signals = aggregate::LadderSignals {
            moves_generated: 10,
            schema_violations: 0,
            move_types_seen,
        };
        assert_eq!(
            aggregate::calibrate_rung(AgentPolicy::L1, &signals),
            AgentPolicy::L0
        );
    }

    #[test]
    fn calibrate_rung_does_not_flag_a_short_test_heavy_stretch_as_collapse() {
        use crate::movement::AgentPolicy;

        // A handful of nodes that legitimately closed in a single `test`
        // each (the S3 cost guard, or simply nothing needed explaining) look
        // identical to real collapse by move-type alone — below
        // MIN_MOVES_FOR_DIVERSITY_CHECK, that must not demote.
        let mut move_types_seen = HashSet::new();
        move_types_seen.insert("testar".to_string());
        let signals = aggregate::LadderSignals {
            moves_generated: 6,
            schema_violations: 0,
            move_types_seen,
        };
        assert_eq!(
            aggregate::calibrate_rung(AgentPolicy::L1, &signals),
            AgentPolicy::L1
        );
    }

    #[test]
    fn calibrate_rung_floors_at_l0_and_ignores_l0s_own_lack_of_diversity() {
        use crate::movement::AgentPolicy;

        // L0 never calls the AI to decide, so a single "move type" (its
        // fixed rule) is not a collapse signal — and there is nowhere lower
        // to step down to regardless.
        let mut move_types_seen = HashSet::new();
        move_types_seen.insert("explicar".to_string());
        let signals = aggregate::LadderSignals {
            moves_generated: 10,
            schema_violations: 0,
            move_types_seen,
        };
        assert_eq!(
            aggregate::calibrate_rung(AgentPolicy::L0, &signals),
            AgentPolicy::L0
        );
    }

    #[test]
    fn calibrate_rung_leaves_healthy_signals_at_the_prior() {
        use crate::movement::AgentPolicy;

        let mut move_types_seen = HashSet::new();
        move_types_seen.insert("explicar".to_string());
        move_types_seen.insert("testar".to_string());
        move_types_seen.insert("perguntar".to_string());
        let signals = aggregate::LadderSignals {
            moves_generated: 8,
            schema_violations: 1,
            move_types_seen,
        };
        assert_eq!(
            aggregate::calibrate_rung(AgentPolicy::L2, &signals),
            AgentPolicy::L2
        );
    }

    #[test]
    fn node_states_covers_absent_attempted_skipped_and_demonstrated() {
        use aggregate::NodeState;

        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::new(dir.path().join("events.jsonl"));

        // n0: failed once, still open (Attempted).
        log.append(
            Some("n0"),
            EventKind::MoveGraded {
                move_id: "m1".into(),
                grade: Grade::Partial,
            },
        )
        .unwrap();
        // n1: skipped, never graded.
        log.append(Some("n1"), EventKind::NodeSkipped).unwrap();
        // n2: demonstrated.
        log.append(
            Some("n2"),
            EventKind::MoveGraded {
                move_id: "m2".into(),
                grade: Grade::Demonstrated,
            },
        )
        .unwrap();

        let states = aggregate::node_states(log.iter().unwrap());
        assert_eq!(states.get("n0"), Some(&NodeState::Attempted));
        assert_eq!(states.get("n1"), Some(&NodeState::Skipped));
        assert_eq!(states.get("n2"), Some(&NodeState::Demonstrated));
        assert_eq!(states.get("n3"), None, "never touched has no entry");
    }

    #[test]
    fn node_states_demonstrated_never_downgrades() {
        use aggregate::NodeState;

        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::new(dir.path().join("events.jsonl"));

        log.append(
            Some("n0"),
            EventKind::MoveGraded {
                move_id: "m1".into(),
                grade: Grade::Demonstrated,
            },
        )
        .unwrap();
        // A later skip (e.g. a revisit) must not undo demonstration.
        log.append(Some("n0"), EventKind::NodeSkipped).unwrap();

        let states = aggregate::node_states(log.iter().unwrap());
        assert_eq!(states.get("n0"), Some(&NodeState::Demonstrated));
    }

    #[test]
    fn revisit_suggestion_picks_the_oldest_still_skipped_node() {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::new(dir.path().join("events.jsonl"));

        log.append(Some("n0"), EventKind::NodeSkipped).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        log.append(Some("n1"), EventKind::NodeSkipped).unwrap();

        // n0 was skipped first, so it's the more-overdue suggestion.
        assert_eq!(
            aggregate::revisit_suggestion(log.iter().unwrap()),
            Some("n0".to_string())
        );
    }

    #[test]
    fn revisit_suggestion_drops_a_node_once_it_is_graded() {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::new(dir.path().join("events.jsonl"));

        log.append(Some("n0"), EventKind::NodeSkipped).unwrap();
        log.append(
            Some("n0"),
            EventKind::MoveGraded {
                move_id: "m1".into(),
                grade: Grade::Partial,
            },
        )
        .unwrap();

        assert_eq!(aggregate::revisit_suggestion(log.iter().unwrap()), None);
    }

    #[test]
    fn activity_counts_tallies_skips_questions_and_annotations() {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::new(dir.path().join("events.jsonl"));

        log.append(Some("n0"), EventKind::NodeSkipped).unwrap();
        log.append(
            Some("n0"),
            EventKind::QuestionAsked {
                move_id: "m1".into(),
                anchor_block: "b1".into(),
            },
        )
        .unwrap();
        log.append(
            Some("n0"),
            EventKind::AnnotationAdded {
                anchor_block: "b1".into(),
            },
        )
        .unwrap();
        log.append(
            Some("n0"),
            EventKind::AnnotationAdded {
                anchor_block: "b2".into(),
            },
        )
        .unwrap();
        // Unrelated events must not be tallied.
        log.append(
            Some("n0"),
            EventKind::MoveGraded {
                move_id: "m1".into(),
                grade: Grade::Demonstrated,
            },
        )
        .unwrap();

        let counts = aggregate::activity_counts(log.iter().unwrap());
        assert_eq!(counts.nodes_skipped, 1);
        assert_eq!(counts.questions_asked, 1);
        assert_eq!(counts.annotations_added, 2);
    }

    #[test]
    fn revisit_suggestion_is_none_when_nothing_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::new(dir.path().join("events.jsonl"));
        assert_eq!(aggregate::revisit_suggestion(log.iter().unwrap()), None);
    }
}
