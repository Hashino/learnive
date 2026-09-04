//! Event substrate (§7.1) — S1 of the agentic-loop build (see PLAN.md's
//! "core evolution: agentic loop" section).
//!
//! `<doc>/events.jsonl` is the append-only source of truth for a living
//! document; everything else derived from it (node state, the scaffolding
//! level, review scheduling) is a **rebuildable cache** — same pattern as
//! the retrieval index (§10, `crate::retrieval::index`). The log is never
//! fully loaded: [`EventLog::iter`] streams lines lazily, and aggregates fold
//! over the iterator in one pass.
//!
//! `EventLog::append` is wired into the api layer (S3): `generate_node`/
//! `answer` append `MoveGenerated`/`MoveGraded`/`SchemaViolation` as moves
//! happen. `EventLog::iter` and the `aggregate` module (the read side) are
//! both wired in as of S7/S9.

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
    /// A move was generated for a node (§6 ABI). `tactics`/`rung` are
    /// legacy fields from the pre-S33 model-driven move choice (§7 tactic
    /// self-labels, the ladder rung) — kept on the struct so old logs
    /// deserialize; since S33 new events stamp `rung: "deterministic"` and
    /// an empty tactics list, and move choice is Rust, not the model.
    /// `move_type` stays `String` for the same reason: the persisted log
    /// outliving today's Rust types is the point of an event-sourced log.
    MoveGenerated {
        move_id: String,
        move_type: String,
        #[serde(default)]
        tactics: Vec<String>,
        rung: String,
    },
    /// A graded move's outcome (§8) — the intervention→outcome half of the
    /// scaffolding-level fold.
    MoveGraded { move_id: String, grade: Grade },
    /// Model output failed to validate against the move schema. Kept as a
    /// diagnostic counter after S33 (the ladder telemetry that used to
    /// consume it is deleted); still logged because it costs zero tokens
    /// and says something real about model quality.
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
    /// same id, the same pattern `RubricSidecar.move_id` uses. `question`
    /// (S18 precondition) carries the actual text, not just the anchor —
    /// §7 calls the question the highest-signal input in the system, and
    /// an anchor alone can't feed `ObservationFrame.questions` or steer the
    /// next `decide_move`. `#[serde(default)]` so event logs written before
    /// this field existed still deserialize (empty string, not an error).
    QuestionAsked {
        move_id: String,
        anchor_block: String,
        #[serde(default)]
        question: String,
    },
    /// A user-authored note was added to the living document (§S6, §9/§11:
    /// the document is the only place for notes — the source viewer is
    /// read-only).
    AnnotationAdded { anchor_block: String },
    /// The learner scrolled a node's content to the end (§S6 "Ritmo":
    /// content → reading interactions → scroll-to-end). The client
    /// (`node.js`) uses crossing the sentinel as the trigger to reopen
    /// `/generate` for the node's next move, now that the move loop is
    /// split across requests.
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
    /// Generation refused outright for this node (S27m, PLAN.md, 2026-08-29
    /// — "o nó se funda no livro dele, ou não nasce"): its bibliographic
    /// source has no approved, indexed match in the local library. Nothing
    /// is persisted to the frozen content layer when this fires — it is
    /// appended instead of a `NodeGenerated`, not alongside one. `reason` is
    /// the human-readable refusal (no approved source / acervo gate failed /
    /// nothing retrievable), the minimum floor PLAN.md's S27m asks for since
    /// the failure *experience* (retry UX) is explicitly deferred.
    GenerationBlocked { reason: String },
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
    fn revisit_suggestion_is_none_when_nothing_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::new(dir.path().join("events.jsonl"));
        assert_eq!(aggregate::revisit_suggestion(log.iter().unwrap()), None);
    }
}
