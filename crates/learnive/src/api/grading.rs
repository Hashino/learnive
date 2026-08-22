use super::cold_start::suggested_revisit;
use super::reading::{
    escape_html, grounding_for, objective_for, profile_for, spawn_profile_distillation,
    topic_and_title,
};
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
    advance: bool,
    /// The frozen exercise + the learner's answer + per-objective feedback
    /// (§4.3), already sanitized-HTML-shaped and already appended to the
    /// interaction layer under `ThreadKind::Attempt` before this response is
    /// built — what the client renders here and what a reload reconstructs
    /// are the same string, not two independently-maintained renderings.
    attempt_html: String,
    /// Remediation EXPLANATION prose (§8.2), sanitized and shown inline.
    #[serde(skip_serializing_if = "Option::is_none")]
    remediation_html: Option<String>,
}

/// Renders one graded submission (§8) as a self-contained, sanitizable HTML
/// fragment: the exercise as it was asked (frozen via `freeze_exercise_html`
/// — the client's `sanitizeHtml` drops any `<form>` subtree wholesale, so the
/// wrapping tag is unwrapped server-side rather than trusting the client to
/// preserve the question text inside it), the learner's raw answer, and the
/// per-objective feedback. This is the ONLY place this markup is built — the
/// live response and every future reload render this exact string (see
/// `AnswerResp::attempt_html`).
fn render_attempt(
    exercise_html: &str,
    answer: &str,
    grades: &[ObjectiveGrade],
    locale: crate::locale::Locale,
) -> String {
    let grades_html: String = grades
        .iter()
        .map(|g| {
            format!(
                "<div class=\"grade {}\"><strong>{}</strong> — {}</div>",
                grade_class(g.grade),
                grade_label(g.grade, locale),
                escape_html(&g.feedback),
            )
        })
        .collect();
    format!(
        "<div class=\"attempt-exercise\">{}</div>\
         <div class=\"attempt-answer\"><strong>{}</strong>{}</div>\
         <div class=\"grades\">{grades_html}</div>",
        learnive_core::freeze_exercise_html(exercise_html),
        crate::locale::pick(locale, "Your answer:", "Sua resposta:"),
        render_answer(answer),
    )
}

/// Renders the raw `learnive-answer` postMessage artifact (§4.4/§8) for a
/// human, not a debugger: the exercise's answer schema is whatever the model
/// (or the auto-collecting form harness, `engine::render_sandbox_frame`)
/// chose, so this can't assume field names — only that it's JSON. A flat
/// object (the common case: one `<form>` field per named input) becomes a
/// label→value list; anything else (a single unlabeled field, a custom
/// interactive artifact's array/nesting, or unparsable text) falls back to a
/// value or a pretty-printed block, never the compact single-line dump
/// `serde_json::to_string` produces.
fn render_answer(answer: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(answer) {
        Ok(serde_json::Value::Object(map)) if !map.is_empty() => {
            let rows: String = map
                .iter()
                .map(|(key, val)| {
                    format!(
                        "<div class=\"answer-field\">{}{}</div>",
                        // A single, generically-named field (the harness's
                        // fallback for a bare textarea/input, or the common
                        // one-field form) carries no meaningful label — the
                        // value alone is the answer.
                        if map.len() > 1 || !matches!(key.as_str(), "answer" | "value") {
                            format!("<span class=\"answer-label\">{}</span>", escape_html(key))
                        } else {
                            String::new()
                        },
                        render_answer_value(val),
                    )
                })
                .collect();
            rows
        }
        Ok(other) => render_answer_value(&other),
        // Not valid JSON at all — the artifact contract requires JSON, but
        // render whatever arrived rather than hide it.
        Err(_) => format!("<pre>{}</pre>", escape_html(answer)),
    }
}

/// One answer value: a plain string renders as its own text (a code block
/// gets `<pre>`, a short line stays inline) rather than as a quoted JSON
/// string; anything else (number, bool, array, nested object) is
/// pretty-printed JSON, since there's no schema to render it more precisely
/// against.
fn render_answer_value(val: &serde_json::Value) -> String {
    if let serde_json::Value::String(s) = val {
        return if s.contains('\n') {
            format!("<pre>{}</pre>", escape_html(s))
        } else {
            format!("<code>{}</code>", escape_html(s))
        };
    }
    let pretty = serde_json::to_string_pretty(val).unwrap_or_else(|_| val.to_string());
    format!("<pre>{}</pre>", escape_html(&pretty))
}

fn grade_class(g: Grade) -> &'static str {
    match g {
        Grade::NotDemonstrated => "not_demonstrated",
        Grade::Partial => "partial",
        Grade::Demonstrated => "demonstrated",
    }
}

fn grade_label(g: Grade, locale: crate::locale::Locale) -> &'static str {
    let (nd, p, d) = (
        crate::locale::pick(locale, "not demonstrated", "não demonstrado"),
        crate::locale::pick(locale, "partial", "parcial"),
        crate::locale::pick(locale, "demonstrated", "demonstrado"),
    );
    match g {
        Grade::NotDemonstrated => nd,
        Grade::Partial => p,
        Grade::Demonstrated => d,
    }
}

pub async fn answer(
    State(state): State<AppState>,
    Path((doc_id, node_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<AnswerReq>,
) -> Result<Json<AnswerResp>, ApiError> {
    let locale = crate::locale::Locale::from_header(&headers);
    let sidecar_json = state
        .store
        .read_doc_file(&doc_id, &format!("{node_id}.rubric.json"))?;
    let sidecar: RubricSidecar =
        serde_json::from_str(&sidecar_json).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    // Grabbed before any sidecar overwrite below (§8.2 replaces it on failure) —
    // this is the id `MoveGraded` must join back onto.
    let move_id = sidecar.move_id.clone();
    // Read before this submission's own interaction append below, so the
    // remediation-attempt count a few lines down still reflects only PRIOR
    // attempts.
    let node = state.store.read_node(&doc_id, &node_id)?;

    let ai = state.ai.load_full();
    let assessment = engine::grade(
        &ai,
        &sidecar.rubric,
        &sidecar.exercise_html,
        &body.answer,
        &sidecar.reference_solution,
        locale,
    )
    .await?;

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

    // Persist THIS attempt — exercise, answer, feedback — for every
    // submission, passing or failing (§4.3: the document is a full record of
    // learning, never a client-only rendering with nothing behind it).
    let exercise_anchor = node
        .content
        .exercise
        .as_ref()
        .map(|e| e.exercise_id.clone());
    let attempt_html = render_attempt(
        &sidecar.exercise_html,
        &body.answer,
        &assessment.grades,
        locale,
    );
    state.store.append_interaction(
        &doc_id,
        &node_id,
        InteractionItem::Thread {
            id: engine::new_id(),
            kind: ThreadKind::Attempt,
            anchor_block: exercise_anchor.clone(),
            body_html: attempt_html.clone(),
            child_node_id: None,
        },
    )?;

    // Advancing requires every objective demonstrated (§8).
    if assessment.all_demonstrated() {
        spawn_profile_distillation(state.clone(), doc_id.clone(), true);
        return Ok(Json(AnswerResp {
            advance: true,
            attempt_html,
            remediation_html: None,
        }));
    }
    spawn_profile_distillation(state.clone(), doc_id.clone(), false);

    // §S15 item 4: a failed transfer/synthesis objective is a cheap signal
    // that a PRIOR concept (a child node folded into this integration
    // exercise) may be the real gap, not this node's own explanation —
    // surface the document's existing revisit suggestion explicitly inside
    // THIS remediation thread, instead of only in the sidebar. Deliberately
    // does NOT try to identify which specific child implicated the failure
    // (PLAN.md marks full cross-node diagnosis out of scope, unproven
    // without live failure-rate data) — it only links two mechanisms that
    // already exist (`suggested_revisit`, the remediation thread), no new
    // grading, no new call.
    let structural_failure = assessment.unmet().iter().any(|g| {
        sidecar
            .rubric
            .objectives
            .iter()
            .find(|o| o.id == g.objective_id)
            .is_some_and(|o| o.transfer || o.kind == ObjectiveType::Synthesis)
    });
    let revisit_hint = if structural_failure {
        suggested_revisit(&state, &doc_id)
            .ok()
            .flatten()
            .filter(|id| id != &node_id)
            .and_then(|id| topic_and_title(&state, &doc_id, &id).ok())
            .filter(|(_, title)| !title.is_empty())
            .map(|(_, title)| {
                format!(
                    "<p class=\"revisit-hint\">{} <strong>{}</strong>.</p>",
                    crate::locale::pick(
                        locale,
                        "↺ This might help: revisiting",
                        "↺ Isto pode ajudar: revisitar"
                    ),
                    escape_html(&title)
                )
            })
    } else {
        None
    };

    // Remediation (§8.2): similarity grows with the number of attempts.
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
    // §S17: remediation now generates through the move ABI
    // (`movement::generate_move_complete`/`generate_move`) instead of the
    // standalone `engine::remediate`/`generate_remediation_exercise` calls —
    // same profile/grounding/citations context and real tactic self-labels
    // any other move gets, joined to this node's evidence table via the
    // `MoveGenerated` events appended below. WHICH move type fires stays a
    // Rust decision either way (§8.2: never the model's choice) — this
    // slice only unifies the generation path, not who decides.
    let unmet_summary = assessment
        .unmet()
        .iter()
        .map(|g| format!("- {}: {}", g.objective_id, g.feedback))
        .collect::<Vec<_>>()
        .join("\n");
    let failed_attempt = format!(
        "Exercise:\n{}\n\nStudent's answer:\n{}",
        sidecar.exercise_html, body.answer
    );
    let grounding = grounding_for(&state, &sidecar.title).await;
    let policy = *state.policy.load_full();
    let ctx = MoveContext {
        topic: sidecar.topic.clone(),
        item_title: sidecar.title.clone(),
        // The chapter already read (§8.2: lets a move tell "already
        // explained, don't repeat" apart from "genuinely never covered"),
        // same source `engine::remediate` used to take as `chapter_html` —
        // `node_tail` truncates to the same §14 budget every other move's
        // tail already uses.
        node_tail: node.content.html.clone(),
        objective: objective_for(&state, &doc_id),
        profile: profile_for(&state, &doc_id),
        grounding,
        locale,
        failed_attempt: Some(failed_attempt),
        unmet_objectives: Some(unmet_summary),
        remediation_attempt: Some(attempt),
        ..Default::default()
    };

    // (a) Explanation: a worked solution of the problem they just missed
    // (§8.2), sanitized prose — it does NOT contain the next problem and
    // must not leak it (`remediation_addendum`'s instruction).
    let explain_move_id = engine::new_id();
    let explain_move = movement::generate_move_complete(&ai, MoveType::Explain, &ctx).await?;
    if let Err(e) = event_log.append(
        Some(&node_id),
        EventKind::MoveGenerated {
            move_id: explain_move_id,
            move_type: MoveType::Explain.to_string(),
            tactics: explain_move.tactics.clone(),
            rung: format!("{policy:?}"),
        },
    ) {
        eprintln!("event log append failed: {e}");
    }
    // Rendered here, not at assembly: this prose lands in the interaction
    // layer as `body_html`/`remediation_html` and never passes through
    // `assemble_node`, which is where every other path picks up math
    // rendering (same reasoning `tag_move_html` documents for the
    // content-layer per-move path).
    let mut explanation = render_math(&explain_move.html);
    if let Some(hint) = &revisit_hint {
        explanation.push_str(hint);
    }

    // (b) A NEW gradeable problem in the sandbox, similar to the failed one and
    // grounded in the same sources (§8/§8.2). Its rubric is freshly locked and the
    // answer is never revealed (EXERCISE_HTML_CONTRACT). Its own move_id, so a
    // future submission's MoveGraded joins onto it and not the one just graded
    // above.
    let new_move = movement::generate_move(&ai, policy, MoveType::Test, &ctx).await?;
    let new_move_id = engine::new_id();
    if let Err(e) = event_log.append(
        Some(&node_id),
        EventKind::MoveGenerated {
            move_id: new_move_id.clone(),
            move_type: MoveType::Test.to_string(),
            tactics: new_move.tactics.clone(),
            rung: format!("{policy:?}"),
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
        rubric: new_move
            .rubric
            .expect("MoveType::Test always parses with graded=true and a rubric"),
        exercise_html: new_move.html.clone(),
        reference_solution: new_move.reference_solution.clone(),
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
    state.store.append_interaction(
        &doc_id,
        &node_id,
        InteractionItem::Thread {
            id: engine::new_id(),
            kind: ThreadKind::Remediation,
            anchor_block: exercise_anchor,
            body_html: explanation.clone(),
            child_node_id: None,
        },
    )?;

    Ok(Json(AnswerResp {
        advance: false,
        attempt_html,
        remediation_html: Some(explanation),
    }))
}

/// On-demand retrieval practice on an already-`Demonstrated` node (§S15 item
/// 5, §2.1's testing-effect reframing — the `test` move is already the
/// retrieval instrument, this just lets the learner fire it again
/// deliberately once mastery is behind them, complementary to item 4's
/// revisit hint and NOT a substitute for it: a learner may not know a
/// specific child node is the real gap and just want more reps here).
///
/// `node_states`'s rank (`events::aggregate`) makes `Demonstrated` the
/// highest state and a later `MoveGraded` here can never lower it back down
/// — so this needs no isolation from the real gate: it reuses the exact
/// mechanism remediation already uses to swap in a fresh check (§8.2,
/// `answer` below), overwriting the same `{node_id}.rubric.json` sidecar in
/// place. `exercise_frame` then serves it and `answer` grades it with zero
/// changes to either. Guarded to `Demonstrated` nodes only: calling this on
/// a live, ungated node would silently replace its real check.
pub async fn practice_node(
    State(state): State<AppState>,
    Path((doc_id, node_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let locale = crate::locale::Locale::from_header(&headers);
    let event_log = state.store.event_log(&doc_id)?;
    let states = node_states(
        event_log
            .iter()
            .map_err(|e| ApiError::Internal(e.to_string()))?,
    );
    if !matches!(states.get(&node_id), Some(NodeState::Demonstrated)) {
        return Err(ApiError::BadRequest(
            "practice is only available on an already-demonstrated node".to_string(),
        ));
    }

    // If the sidecar already holds a practice attempt nobody answered yet
    // (the learner clicked "Practice again", generated a fresh problem, then
    // navigated away before submitting), reuse it instead of paying for
    // another `test` move (§12.2 BYOK cost discipline) — the sidecar's
    // `move_id` only ever gets a `MoveGraded` once that specific move is
    // actually answered, so "no matching `MoveGraded`" means it's still live.
    // The very first practice call after demonstration doesn't hit this: the
    // sidecar still holds the ORIGINAL passing move, which is graded by
    // definition (that grade is what made the node `Demonstrated`).
    if let Ok(existing_json) = state
        .store
        .read_doc_file(&doc_id, &format!("{node_id}.rubric.json"))
        && let Ok(existing) = serde_json::from_str::<RubricSidecar>(&existing_json)
    {
        let graded = event_log
            .iter()
            .map_err(|e| ApiError::Internal(e.to_string()))?
            .any(|e| matches!(e.kind, EventKind::MoveGraded { move_id, .. } if move_id == existing.move_id));
        if !graded {
            return Ok(StatusCode::NO_CONTENT);
        }
    }

    let node = state.store.read_node(&doc_id, &node_id)?;
    let (topic, title) = topic_and_title(&state, &doc_id, &node_id)?;
    let ai = state.ai.load_full();
    let policy = *state.policy.load_full();
    let grounding = grounding_for(&state, &title).await;
    let ctx = MoveContext {
        topic: topic.clone(),
        item_title: title.clone(),
        node_tail: node.content.html.clone(),
        objective: objective_for(&state, &doc_id),
        profile: profile_for(&state, &doc_id),
        grounding,
        locale,
        ..Default::default()
    };
    let new_move = movement::generate_move(&ai, policy, MoveType::Test, &ctx).await?;
    let new_move_id = engine::new_id();
    if let Err(e) = event_log.append(
        Some(&node_id),
        EventKind::MoveGenerated {
            move_id: new_move_id.clone(),
            move_type: MoveType::Test.to_string(),
            tactics: new_move.tactics.clone(),
            rung: format!("{policy:?}"),
        },
    ) {
        eprintln!("event log append failed: {e}");
    }

    // Same overwrite-in-place remediation already does (§8.2): this is
    // grading state, not user knowledge — the §5 non-destructive rule is
    // upheld by the append-only interaction layer `answer` writes to below,
    // which keeps every practice attempt on the record (§4.3).
    let sidecar = RubricSidecar {
        move_id: new_move_id,
        rubric: new_move
            .rubric
            .expect("MoveType::Test always parses with graded=true and a rubric"),
        exercise_html: new_move.html,
        reference_solution: new_move.reference_solution,
        title,
        topic,
    };
    state.store.write_doc_file(
        &doc_id,
        &format!("{node_id}.rubric.json"),
        &serde_json::to_string(&sidecar).unwrap_or_default(),
    )?;

    Ok(StatusCode::NO_CONTENT)
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
