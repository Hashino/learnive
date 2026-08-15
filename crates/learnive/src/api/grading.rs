use super::reading::{grounding_for, spawn_profile_distillation};
use super::*;

// ---------------------------------------------------------------------------
// Answer → grading (§8) → remediation (§8.2) or advance.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct AnswerReq {
    answer: String,
}

#[derive(Serialize)]
pub struct AnswerResp {
    grades: Vec<ObjectiveGrade>,
    advance: bool,
    /// Remediation EXPLANATION prose (§8.2), sanitized and shown inline.
    #[serde(skip_serializing_if = "Option::is_none")]
    remediation_html: Option<String>,
}

pub async fn answer(
    State(state): State<AppState>,
    Path((doc_id, node_id)): Path<(String, String)>,
    Json(body): Json<AnswerReq>,
) -> Result<Json<AnswerResp>, ApiError> {
    let sidecar_json = state
        .store
        .read_doc_file(&doc_id, &format!("{node_id}.rubric.json"))?;
    let sidecar: RubricSidecar =
        serde_json::from_str(&sidecar_json).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    // Grabbed before any sidecar overwrite below (§8.2 replaces it on failure) —
    // this is the id `MoveGraded` must join back onto.
    let move_id = sidecar.move_id.clone();

    let ai = state.ai.load_full();
    let assessment =
        engine::grade(&ai, &sidecar.rubric, &sidecar.exercise_html, &body.answer).await?;

    let event_log = state.store.event_log(&doc_id)?;
    if let Err(e) = event_log.append(
        Some(&node_id),
        EventKind::MoveGraded {
            move_id,
            grade: reduce_grade(&assessment),
        },
    ) {
        eprintln!("event log append failed: {e}");
    }

    // Advancing requires every objective demonstrated (§8).
    if assessment.all_demonstrated() {
        spawn_profile_distillation(state.clone(), doc_id.clone(), true);
        return Ok(Json(AnswerResp {
            grades: assessment.grades,
            advance: true,
            remediation_html: None,
        }));
    }
    spawn_profile_distillation(state.clone(), doc_id.clone(), false);

    // Remediation (§8.2): similarity grows with the number of attempts.
    let node = state.store.read_node(&doc_id, &node_id)?;
    let attempt = node
        .interaction
        .iter()
        .filter(|i| {
            matches!(
                i,
                InteractionItem::Thread {
                    kind: ThreadKind::Remediation,
                    ..
                }
            )
        })
        .count() as u32
        + 1;
    // (a) Explanation: a worked solution of the problem they just missed (§8.2),
    // sanitized prose — it does NOT contain the next problem and must not leak it.
    let explanation = engine::remediate(
        &ai,
        &sidecar.title,
        &sidecar.exercise_html,
        &body.answer,
        &assessment.unmet(),
        attempt,
    )
    .await?;

    // (b) A NEW gradeable problem in the sandbox, similar to the failed one and
    // grounded in the same sources (§8/§8.2). Its rubric is freshly locked and the
    // answer is never revealed (EXERCISE_HTML_CONTRACT).
    let grounding = grounding_for(&state, &format!("{} {}", sidecar.topic, sidecar.title)).await;
    let er = engine::generate_remediation_exercise(
        &ai,
        &sidecar.title,
        &sidecar.exercise_html,
        attempt,
        &grounding,
    )
    .await?;

    // The new problem is a fresh graded artifact — its own move_id, so a future
    // submission's MoveGraded joins onto it and not the one just graded above.
    // It stays on the legacy remediation path (not `decide_move`/`generate_move`):
    // L0's rule has no slot for a second graded move in one node, and the new
    // problem is required by construction (§8.2), never a model choice.
    let new_move_id = engine::new_id();
    if let Err(e) = event_log.append(
        Some(&node_id),
        EventKind::MoveGenerated {
            move_id: new_move_id.clone(),
            move_type: MoveType::Test.to_string(),
            tactics: Vec::new(),
            rung: format!("{:?}", *state.policy.load_full()),
        },
    ) {
        eprintln!("event log append failed: {e}");
    }

    // The new problem becomes the node's ACTIVE check: overwrite the server-only
    // rubric sidecar so the next submission grades IT. This is grading state, not
    // user knowledge — the §5 non-destructive rule is upheld by the append-only
    // interaction layer, which retains the full attempt/remediation trajectory.
    let new_sidecar = RubricSidecar {
        move_id: new_move_id,
        rubric: er.rubric,
        exercise_html: er.exercise_html.clone(),
        title: sidecar.title.clone(),
        topic: sidecar.topic.clone(),
    };
    state.store.write_doc_file(
        &doc_id,
        &format!("{node_id}.rubric.json"),
        &serde_json::to_string(&new_sidecar).unwrap_or_default(),
    )?;

    // Append the explanation to the interaction layer (append-only, §4.3),
    // anchored to the original exercise.
    let anchor = node
        .content
        .exercise
        .as_ref()
        .map(|e| e.exercise_id.clone());
    state.store.append_interaction(
        &doc_id,
        &node_id,
        InteractionItem::Thread {
            id: engine::new_id(),
            kind: ThreadKind::Remediation,
            anchor_block: anchor,
            body_html: explanation.clone(),
            child_node_id: None,
        },
    )?;

    Ok(Json(AnswerResp {
        grades: assessment.grades,
        advance: false,
        remediation_html: Some(explanation),
    }))
}

/// Formats an SSE frame with JSON-encoded `data` (avoids newline problems).
pub(super) fn sse_frame(event: &str, data: &str) -> Bytes {
    let json = serde_json::to_string(data).unwrap_or_else(|_| "\"\"".to_string());
    Bytes::from(format!("event: {event}\ndata: {json}\n\n"))
}

/// Reduces a move's per-objective grades to one outcome (§7's evidence table
/// is per-move, not per-objective) — worst case wins: any not-demonstrated
/// objective makes the whole move not-demonstrated, else any partial makes it
/// partial, else it's fully demonstrated.
fn reduce_grade(assessment: &engine::Assessment) -> Grade {
    if assessment
        .grades
        .iter()
        .any(|g| g.grade == Grade::NotDemonstrated)
    {
        Grade::NotDemonstrated
    } else if assessment.grades.iter().any(|g| g.grade == Grade::Partial) {
        Grade::Partial
    } else {
        Grade::Demonstrated
    }
}
