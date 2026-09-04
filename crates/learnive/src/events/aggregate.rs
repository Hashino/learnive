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

/// S33-3 spaced review scheduler (n·2ᵏ, user decision 2026-09-03) — the
/// concrete policy PLAN.md's S24 "due-for-review" queue waited on, and the
/// replacement for the deleted `revisit_suggestion` (which could only point
/// back at skipped nodes and had no schedule at all). When a chapter CLOSES
/// (its last member node demonstrates or is skipped), reviews come due at
/// fixed offsets measured in nodes-completed-since-then: 5, 10, 20, 40, 80…
/// (`REVIEW_BASE_NODES * 2^(level-1)` — the user's exact sequence, each
/// level's gap doubling). Pure zero-token arithmetic over the event log:
/// no call, no persisted state to desync — the whole point of the S33 cut
/// line ("the source answers 'what is true'; telemetry answers 'what comes
/// next'").
///
/// Deliberate choices, each load-bearing:
///
/// - **Only a node's FIRST `Demonstrated` grade counts** toward the counter
///   (a "practice again" re-grade of an already-demonstrated node is not
///   progress through the curriculum), and **review nodes never count**:
///   they arrive as `OutlineItem`s with `mode: Review`, so the api layer
///   leaves them out of `ReviewChapter::members` AND out of the counting
///   set — a completed review must not push the next review further out,
///   or the schedule would outrun itself exactly when it's working.
/// - **A review level is consumed when its node was GENERATED**
///   (`NodeGenerated`, i.e. finalized), not when it demonstrated. A
///   partially-generated review resumes on the next visit; suggesting the
///   same level again would duplicate a review that already exists.
/// - **Levels fire in order, by construction**: the due level is always
///   `1 + (generated reviews for this chapter)` — ignoring review 1 and
///   studying on means review 1 keeps being the suggestion (still due,
///   still open), never a level-2 review appearing out of nowhere.
/// - **Several chapters due at once**: the one that closed EARLIEST wins
///   (smallest close position, then lowest level) — the oldest material is
///   the most decayed, which is exactly what a review is for.
///
/// The api layer (`api::reading::review_chapters`) derives the per-chapter
/// member lists from the outline; this fold never reads the outline, so it
/// stays a pure function of the log like every other aggregate here.
pub const REVIEW_BASE_NODES: u64 = 5;

/// One scheduler-input chapter: the outline members whose
/// `Demonstrated`/`Skipped` state closes it. For a decomposed chapter these
/// are its non-review direct children; for a chapter that generated as a
/// single node (genuine `NoSplit`), the chapter item's own id — it carries
/// its own grades in that shape.
#[derive(Clone)]
pub struct ReviewChapter {
    pub id: String,
    pub members: Vec<String>,
}

/// A review that is due right now. `level` is 1-based; the api layer turns
/// `(chapter_id, level)` into the review item id
/// (`{chapter_id}_review{level}`) and the display title (the chapter's own).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueReview {
    pub chapter_id: String,
    pub level: u32,
}

/// The id of the `level`-th review node for a chapter:
/// `{chapter_id}_review{level}`. An underscore, not `::`, because node ids
/// must pass `store::ensure_safe_id` (alphanumeric plus `-`/`_`) on their
/// way to node files.
pub fn review_node_id(chapter_id: &str, level: u32) -> String {
    format!("{chapter_id}_review{level}")
}

/// Inverse of [`review_node_id`], resolved by exact match against the known
/// chapters — never by string splitting — so a chapter id that happened to
/// contain `_review` can't produce a collision.
pub fn parse_review_node_id(chapters: &[ReviewChapter], id: &str) -> Option<(String, u32)> {
    chapters.iter().find_map(|c| {
        let prefix = format!("{}_review", c.id);
        id.strip_prefix(&prefix)
            .and_then(|rest| rest.parse::<u32>().ok())
            .filter(|level| *level >= 1)
            .map(|level| (c.id.clone(), level))
    })
}

/// Whether `id` is one of this scheduler's own review nodes. Such nodes are
/// the scheduler's OUTPUT, never curriculum progress: their completion must
/// not advance the spacing counter, or every finished review would push the
/// next one further out exactly when the schedule is working.
pub fn is_review_node(chapters: &[ReviewChapter], id: &str) -> bool {
    parse_review_node_id(chapters, id).is_some()
}

pub fn due_review(
    events: impl Iterator<Item = Event>,
    chapters: &[ReviewChapter],
) -> Option<DueReview> {
    // Nodes-completed counter (first `Demonstrated` per counting node) and
    // per-member satisfaction (Demonstrated OR Skipped — a deliberate skip
    // satisfies the chapter's gate, `engine::effective_state`'s rule, so it
    // closes the chapter for scheduling too).
    let mut total: u64 = 0;
    let mut demonstrated: HashSet<String> = HashSet::new();
    let mut satisfied: HashSet<String> = HashSet::new();
    // (chapter id, total-at-close) in close order; a chapter closes once.
    let mut closed: Vec<(String, u64)> = Vec::new();
    // Node ids with a `NodeGenerated` event — the review-consumed signal.
    let mut generated: HashSet<String> = HashSet::new();

    let chapter_of_member = |node: &str| -> Vec<&ReviewChapter> {
        chapters
            .iter()
            .filter(|c| c.members.iter().any(|m| m == node))
            .collect()
    };

    for event in events {
        let Some(node_id) = event.node_id.clone() else {
            continue;
        };
        match event.kind {
            EventKind::MoveGraded {
                grade: Grade::Demonstrated,
                ..
            } => {
                if !is_review_node(chapters, &node_id) && demonstrated.insert(node_id.clone()) {
                    total += 1;
                }
                satisfied.insert(node_id.clone());
                for ch in chapter_of_member(&node_id) {
                    try_close(total, &satisfied, ch, &mut closed);
                }
            }
            EventKind::NodeSkipped => {
                satisfied.insert(node_id.clone());
                for ch in chapter_of_member(&node_id) {
                    try_close(total, &satisfied, ch, &mut closed);
                }
            }
            EventKind::NodeGenerated { .. } => {
                generated.insert(node_id);
            }
            _ => {}
        }
    }

    let mut best: Option<(u64, u32, String)> = None;
    for (chapter_id, close_at) in &closed {
        let done = generated
            .iter()
            .filter(|g| {
                parse_review_node_id(chapters, g).map(|(c, _)| c).as_deref() == Some(chapter_id)
            })
            .count() as u32;
        let level = done + 1;
        let threshold = REVIEW_BASE_NODES << (level - 1);
        if close_at + threshold <= total {
            let key = (*close_at, level);
            if best.as_ref().is_none_or(|(bc, _, _)| key.0 < *bc) {
                best = Some((*close_at, level, chapter_id.clone()));
            }
        }
    }
    best.map(|(_, level, chapter_id)| DueReview { chapter_id, level })
}

/// A chapter closes exactly once, the moment its LAST member reaches the
/// gate-satisfying state (`Demonstrated` or `Skipped` — `engine::
/// effective_state`'s rule). `total` is the counter value at this instant.
fn try_close(
    total: u64,
    satisfied: &HashSet<String>,
    ch: &ReviewChapter,
    closed: &mut Vec<(String, u64)>,
) {
    if closed.iter().any(|(id, _)| id == &ch.id) {
        return;
    }
    if ch.members.iter().all(|m| satisfied.contains(m)) {
        closed.push((ch.id.clone(), total));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Event;

    fn chapter(id: &str, members: &[&str]) -> ReviewChapter {
        ReviewChapter {
            id: id.to_string(),
            members: members.iter().map(|m| m.to_string()).collect(),
        }
    }

    fn gen_review(chapter_id: &str, level: u32, ts: u64) -> Event {
        Event {
            id: "e".to_string(),
            ts,
            node_id: Some(format!("{chapter_id}_review{level}")),
            kind: EventKind::NodeGenerated {
                move_id: "m".to_string(),
            },
        }
    }

    fn skipped(node_id: &str, ts: u64) -> Event {
        Event {
            id: "e".to_string(),
            ts,
            node_id: Some(node_id.to_string()),
            kind: EventKind::NodeSkipped,
        }
    }

    #[test]
    fn no_chapter_no_review() {
        let events = vec![graded("n1", 1, Grade::Demonstrated)];
        assert_eq!(due_review(events.into_iter(), &[]), None);
    }

    #[test]
    fn review_is_not_due_before_the_first_threshold() {
        // NoSplit shape: the chapter generated as one node, so its OWN id
        // is the member. It closes at total 1; only 3 more nodes complete —
        // under 5.
        let ch = chapter("c1", &["c1"]);
        let events = vec![
            graded("c1", 1, Grade::Demonstrated),
            graded("n1", 2, Grade::Demonstrated),
            graded("n2", 3, Grade::Demonstrated),
            graded("n3", 4, Grade::Demonstrated),
        ];
        assert_eq!(due_review(events.into_iter(), &[ch]), None);
    }

    #[test]
    fn first_review_comes_due_five_nodes_after_close() {
        let ch = chapter("c1", &["n1", "n2"]);
        let mut events = vec![
            graded("n1", 1, Grade::Demonstrated),
            graded("n2", 2, Grade::Demonstrated), // c1 closes at total 2
        ];
        for i in 0..5u64 {
            events.push(graded(&format!("x{i}"), 3 + i, Grade::Demonstrated));
        }
        assert_eq!(
            due_review(events.into_iter(), &[ch]),
            Some(DueReview {
                chapter_id: "c1".to_string(),
                level: 1,
            })
        );
    }

    #[test]
    fn a_generated_review_is_consumed_and_the_next_level_waits_for_double() {
        let ch = chapter("c1", &["n1", "n2"]);
        let mut events = vec![
            graded("n1", 1, Grade::Demonstrated),
            graded("n2", 2, Grade::Demonstrated), // c1 closes at 2
            // review 1 finalized (total 3): consumed...
            gen_review("c1", 1, 3),
            // ...then 5 more nodes (total 8): level 2 needs close+10 = 12.
            graded("a", 4, Grade::Demonstrated),
            graded("b", 5, Grade::Demonstrated),
            graded("c", 6, Grade::Demonstrated),
            graded("d", 7, Grade::Demonstrated),
            graded("e", 8, Grade::Demonstrated),
        ];
        assert_eq!(
            due_review(events.clone().into_iter(), std::slice::from_ref(&ch)),
            None
        );
        // Ten nodes past the close (total 12): level 2 due.
        for i in 0..5u64 {
            events.push(graded(&format!("y{i}"), 9 + i, Grade::Demonstrated));
        }
        assert_eq!(
            due_review(events.into_iter(), &[ch]),
            Some(DueReview {
                chapter_id: "c1".to_string(),
                level: 2,
            })
        );
    }

    #[test]
    fn a_completed_review_never_pushes_the_schedule_back() {
        // The review node's own Demonstrated grade must not count toward
        // the counter: here it is exactly the difference between level 2
        // firing and not. Close at 2, nine real nodes (total 11), then the
        // review itself demonstrates — if that counted, total would be 12 =
        // close + 10 and level 2 would fire one node early.
        let ch = chapter("c1", &["n1", "n2"]);
        let mut events = vec![
            graded("n1", 1, Grade::Demonstrated),
            graded("n2", 2, Grade::Demonstrated), // c1 closes at 2
            gen_review("c1", 1, 3),
        ];
        for i in 0..9u64 {
            events.push(graded(&format!("x{i}"), 4 + i, Grade::Demonstrated));
        }
        events.push(graded("c1_review1", 13, Grade::Demonstrated));
        assert_eq!(
            due_review(events.clone().into_iter(), std::slice::from_ref(&ch)),
            None
        );
        events.push(graded("y0", 14, Grade::Demonstrated));
        assert_eq!(
            due_review(events.into_iter(), &[ch]),
            Some(DueReview {
                chapter_id: "c1".to_string(),
                level: 2,
            })
        );
    }

    #[test]
    fn a_partial_review_is_consumed_and_never_suggested_twice() {
        let ch = chapter("c1", &["n1"]);
        let events = vec![
            graded("n1", 1, Grade::Demonstrated), // close at 1
            // review 1 generated but never demonstrated (partial) — its
            // NodeGenerated alone consumes level 1, so the suggestion must
            // not come back as level 1 once the first threshold passes;
            // level 2's doubled threshold is still far away, so nothing is
            // due at all.
            gen_review("c1", 1, 2),
            graded("x1", 3, Grade::Demonstrated),
            graded("x2", 4, Grade::Demonstrated),
            graded("x3", 5, Grade::Demonstrated),
            graded("x4", 6, Grade::Demonstrated),
            graded("x5", 7, Grade::Demonstrated), // total 6 >= close 1 + 5
        ];
        assert_eq!(due_review(events.into_iter(), &[ch]), None);
    }

    #[test]
    fn a_skipped_member_still_closes_the_chapter() {
        let ch = chapter("c1", &["n1", "n2"]);
        let events = vec![
            graded("n1", 1, Grade::Demonstrated),
            skipped("n2", 2), // closes c1 at 1
            graded("x1", 3, Grade::Demonstrated),
            graded("x2", 4, Grade::Demonstrated),
            graded("x3", 5, Grade::Demonstrated),
            graded("x4", 6, Grade::Demonstrated),
            graded("x5", 7, Grade::Demonstrated), // total 6 = close 1 + 5
        ];
        assert_eq!(
            due_review(events.into_iter(), &[ch]),
            Some(DueReview {
                chapter_id: "c1".to_string(),
                level: 1,
            })
        );
    }

    #[test]
    fn the_earliest_closed_chapter_wins_when_several_are_due() {
        let first = chapter("c1", &["n1"]);
        let second = chapter("c2", &["n2"]);
        let events = vec![
            graded("n1", 1, Grade::Demonstrated), // c1 closes at 1
            graded("x1", 2, Grade::Demonstrated),
            graded("x2", 3, Grade::Demonstrated),
            graded("x3", 4, Grade::Demonstrated),
            graded("x4", 5, Grade::Demonstrated),
            graded("x5", 6, Grade::Demonstrated), // c1 due (1+5 <= 6)
            graded("n2", 7, Grade::Demonstrated), // c2 closes at 7 — not due yet
        ];
        assert_eq!(
            due_review(events.into_iter(), &[first, second]),
            Some(DueReview {
                chapter_id: "c1".to_string(),
                level: 1,
            })
        );
    }

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
