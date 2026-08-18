//! Curriculum objective (§S4 of the agentic-loop build — see PLAN.md's "core
//! evolution: agentic loop" section) — the living document's single anchor,
//! confirmed at cold start and revised only by an approved `plan` move.
//!
//! Versioned like a concept node (§5): a revision never overwrites, it
//! appends a new [`ObjectiveVersion`] to `<doc>/objective.json`'s list;
//! [`ObjectiveLog::current`] is always the last entry. Same append-only shape
//! as `events::EventLog`, reused here rather than a `objective.v{n}.json`
//! file-per-version scheme the store has no helper for — a document-level
//! objective is small enough that "one JSON list" is the whole file.

use serde::{Deserialize, Serialize};

/// Where a version came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectiveSource {
    /// Confirmed during cold start (§6.1).
    ColdStart,
    /// Directly edited by the learner (§S4/§5: "editável a qualquer
    /// momento") — the human-in-the-loop path, always available.
    UserEdit,
    /// Revised by an approved `plan` move (§S4/§5). Not yet a real code
    /// path — `plan` proposes OUTLINE changes today (`generate_node`'s
    /// `outline.proposal.json`); a `plan` move that also revises the
    /// objective text itself is a later refinement, not S4's scope.
    Plan,
}

/// One locked version of the objective.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectiveVersion {
    pub version: u32,
    pub text: String,
    pub source: ObjectiveSource,
    /// Unix epoch milliseconds — same convention as `events::Event::ts`.
    pub ts: u64,
    /// A frozen, self-contained HTML fragment replaying the exchange that
    /// produced this version — the raw topic typed and the confirmed
    /// prerequisite tree, rendered once (`cold_start::render_topic_transcript`)
    /// and never rebuilt, same "render once, persist, replay the identical
    /// string" contract `grading::render_attempt` already uses for a graded
    /// attempt. `None` for a version with nothing to replay (a `plan`/
    /// `user_edit` revision, or any version written before this field
    /// existed) — the client just shows nothing at that point, the same
    /// graceful-degradation convention every other §S-era optional field
    /// here uses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_html: Option<String>,
}

/// The full version chain — `<doc>/objective.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ObjectiveLog {
    pub versions: Vec<ObjectiveVersion>,
}

impl ObjectiveLog {
    /// The active version every `decide_move` call anchors on. `None` only
    /// for a document with no objective yet (pre-S4 documents, or a read
    /// failure the caller has already decided to treat as "none") — callers
    /// degrade gracefully on this the same way `MoveContext::objective`
    /// already degrades on an empty string.
    pub fn current(&self) -> Option<&ObjectiveVersion> {
        self.versions.last()
    }

    /// Appends a new version (§5 non-destructive) — never mutates or removes
    /// any prior entry.
    pub fn push(&mut self, text: String, source: ObjectiveSource) {
        self.push_with_transcript(text, source, None);
    }

    /// Same as [`Self::push`], carrying the frozen replay fragment for a
    /// version that came from an actual topic/prerequisite exchange (cold
    /// start today; a future `next`-topic continuation the same way) —
    /// kept as a separate method rather than a third `push` parameter every
    /// existing caller would have to pass `None` for.
    pub fn push_with_transcript(
        &mut self,
        text: String,
        source: ObjectiveSource,
        transcript_html: Option<String>,
    ) {
        let version = self.versions.last().map_or(1, |v| v.version + 1);
        self.versions.push(ObjectiveVersion {
            version,
            text,
            source,
            ts: now_ms(),
            transcript_html,
        });
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_is_non_destructive_and_increments_version() {
        let mut log = ObjectiveLog::default();
        log.push("Learn calculus".to_string(), ObjectiveSource::ColdStart);
        assert_eq!(log.versions.len(), 1);
        assert_eq!(log.current().unwrap().version, 1);

        log.push(
            "Learn calculus, focused on limits and derivatives".to_string(),
            ObjectiveSource::Plan,
        );
        assert_eq!(log.versions.len(), 2, "old version must stay in place");
        assert_eq!(log.versions[0].text, "Learn calculus", "v1 untouched");
        assert_eq!(log.current().unwrap().version, 2);
        assert_eq!(log.current().unwrap().source, ObjectiveSource::Plan);
    }

    #[test]
    fn current_is_none_on_an_empty_log() {
        assert!(ObjectiveLog::default().current().is_none());
    }
}
