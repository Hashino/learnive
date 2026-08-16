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
    /// `finalize` (api/reading.rs) has written the node's complete content
    /// layer (prose through the graded move) — the explicit completion
    /// signal `prepare`'s regen guard reads. Not derivable from a node file
    /// merely existing on disk: content now persists progressively, per
    /// move, so existence no longer means "done" (the split this comment
    /// and `NodeReadToEnd`'s above were both waiting on). Not derivable from
    /// `MoveGraded` either — that only fires once the learner *answers* the
    /// exercise, long after generation. `move_id` joins back to the graded
    /// move's `MoveGenerated` event, same pattern as `PlanDecided`.
    NodeGenerated { move_id: String },
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

pub mod aggregate;

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
