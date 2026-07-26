//! Endpoints do loop de currículo (§6, §8, §8.2).
//!
//! Fluxo: cold start (POST cria documento + outline) → geração de nó (POST que
//! **streama** a prosa token-a-token no formato SSE) → resposta (POST corrige
//! contra o rubric travado; na falha abre remediação §8.2; no sucesso sinaliza
//! avançar).
//!
//! Sobre streaming e §3.1: a §3 pede SSE, mas `EventSource` do navegador não
//! envia cabeçalho de token nem faz POST, e a §3.1 proíbe mutação em GET. Então
//! streamamos o **formato de fio SSE sobre um POST** (lido via `fetch`): mantém
//! a semântica da §3 e honra a §3.1 (POST + token no cabeçalho + Origin).

use axum::{
    Json,
    body::{Body, Bytes},
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

use learnive_core::{InteractionItem, ThreadKind};

use crate::ai::{Ai, MockProvider, Models, OpenAiCompat, Provider, Tier};
use crate::app::AppState;
use crate::engine::{self, ObjectiveGrade, Outline, Rubric};
use crate::store::StoreError;

/// Erro de API mapeado para status HTTP.
pub enum ApiError {
    Engine(engine::EngineError),
    Store(StoreError),
    BadRequest(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            // Falha do provedor ou saída ilegível do modelo: gateway ruim.
            ApiError::Engine(e) => (StatusCode::BAD_GATEWAY, e.to_string()),
            ApiError::Store(StoreError::NotFound(_)) => {
                (StatusCode::NOT_FOUND, "não encontrado".to_string())
            }
            ApiError::Store(StoreError::InvalidId(_)) => (
                StatusCode::BAD_REQUEST,
                "identificador inválido".to_string(),
            ),
            ApiError::Store(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
        };
        (status, msg).into_response()
    }
}

impl From<engine::EngineError> for ApiError {
    fn from(e: engine::EngineError) -> Self {
        ApiError::Engine(e)
    }
}
impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        ApiError::Store(e)
    }
}

/// Arquivo auxiliar server-only do nó: rubric travado + o exercício + contexto
/// para a correção/remediação. Nunca servido ao cliente (§8).
#[derive(Serialize, Deserialize)]
struct RubricSidecar {
    rubric: Rubric,
    exercise_html: String,
    title: String,
    topic: String,
}

// ---------------------------------------------------------------------------
// Cold start (§6.1): tema → outline.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CreateReq {
    topic: String,
}

#[derive(Serialize)]
pub struct CreateResp {
    doc_id: String,
    titles: Vec<String>,
}

pub async fn create_document(
    State(state): State<AppState>,
    Json(body): Json<CreateReq>,
) -> Result<Json<CreateResp>, ApiError> {
    if body.topic.trim().is_empty() {
        return Err(ApiError::BadRequest("tema vazio".to_string()));
    }
    let outline = engine::generate_outline(&state.ai, &body.topic).await?;
    let doc_id = engine::new_id();
    state.store.create_document(&doc_id)?;
    state.store.write_doc_file(
        &doc_id,
        "outline.json",
        &serde_json::to_string(&outline).unwrap_or_default(),
    )?;
    Ok(Json(CreateResp {
        doc_id,
        titles: outline.items.iter().map(|i| i.title.clone()).collect(),
    }))
}

// ---------------------------------------------------------------------------
// Geração de nó (§6) com streaming de prosa (§14).
// ---------------------------------------------------------------------------

/// Streama o formato SSE sobre um POST. Eventos: `token` (prosa, repetido),
/// `exercise` (form, após a prosa), `done` (node_id), `error`.
pub async fn generate_node(
    State(state): State<AppState>,
    Path((doc_id, index)): Path<(String, usize)>,
) -> Response {
    // O trabalho falível que não emite tokens fica em `prepare`/`finalize`; o
    // gerador só contém os `yield` (async_stream não reescreve `yield` através
    // de um macro aninhado).
    let stream = async_stream::stream! {
        let prep = match prepare(&state, &doc_id, index).await {
            Ok(p) => p,
            Err(e) => {
                yield Ok::<Bytes, std::io::Error>(sse_frame("error", &e));
                return;
            }
        };

        // Prosa: robusto, streamada token-a-token (TTFT, §14).
        let mut prose = String::new();
        let prose_prompt = engine::prompt::prose(&prep.topic, &prep.title, &prep.context);
        match state.ai.stream(Tier::Robust, prose_prompt).await {
            Ok(mut s) => {
                while let Some(tok) = s.next().await {
                    match tok {
                        Ok(t) => {
                            prose.push_str(&t);
                            yield Ok(sse_frame("token", &t));
                        }
                        Err(e) => {
                            yield Ok(sse_frame("error", &e.to_string()));
                            return;
                        }
                    }
                }
            }
            Err(e) => {
                yield Ok(sse_frame("error", &e.to_string()));
                return;
            }
        }

        // Exercício + rubric numa chamada separada, montar e persistir (§8/§14).
        match finalize(&state, &doc_id, &prep, &prose).await {
            Ok(exercise_html) => {
                yield Ok(sse_frame("exercise", &exercise_html));
                yield Ok(sse_frame("done", &prep.node_id));
            }
            Err(e) => {
                yield Ok(sse_frame("error", &e));
                return;
            }
        }
    };

    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .expect("resposta de stream válida")
}

/// Dados prontos para gerar um nó.
struct NodePrep {
    topic: String,
    title: String,
    context: String,
    node_id: String,
}

/// Carrega o outline e resolve o item pedido (trabalho falível sem `yield`).
async fn prepare(state: &AppState, doc_id: &str, index: usize) -> Result<NodePrep, String> {
    let outline_json = state
        .store
        .read_doc_file(doc_id, "outline.json")
        .map_err(|e| e.to_string())?;
    let outline: Outline = serde_json::from_str(&outline_json).map_err(|e| e.to_string())?;
    let item = outline
        .items
        .get(index)
        .cloned()
        .ok_or_else(|| "índice fora do outline".to_string())?;
    let context = outline.items[..index]
        .iter()
        .map(|i| i.title.as_str())
        .collect::<Vec<_>>()
        .join("; ");
    Ok(NodePrep {
        topic: outline.topic,
        title: item.title,
        context,
        node_id: format!("n{index}"),
    })
}

/// Gera exercício + rubric, monta o nó e persiste (nó + sidecar server-only).
/// Devolve o HTML do exercício para o cliente. Idempotente por `node_id` (§16).
async fn finalize(
    state: &AppState,
    doc_id: &str,
    prep: &NodePrep,
    prose: &str,
) -> Result<String, String> {
    let er = engine::generate_exercise_and_rubric(&state.ai, &prep.topic, &prep.title, prose)
        .await
        .map_err(|e| e.to_string())?;

    let exercise_id = format!("{}-ex", prep.node_id);
    let rubric_id = format!("{}-ru", prep.node_id);
    let node = engine::assemble_node(
        doc_id,
        &prep.node_id,
        prose,
        &er.exercise_html,
        &exercise_id,
        &rubric_id,
    )
    .map_err(|e| e.to_string())?;

    state.store.write_node(&node).map_err(|e| e.to_string())?;

    let sidecar = RubricSidecar {
        rubric: er.rubric,
        exercise_html: er.exercise_html.clone(),
        title: prep.title.clone(),
        topic: prep.topic.clone(),
    };
    state
        .store
        .write_doc_file(
            doc_id,
            &format!("{}.rubric.json", prep.node_id),
            &serde_json::to_string(&sidecar).unwrap_or_default(),
        )
        .map_err(|e| e.to_string())?;

    Ok(er.exercise_html)
}

// ---------------------------------------------------------------------------
// Resposta → correção (§8) → remediação (§8.2) ou avanço.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct AnswerReq {
    answer: String,
}

#[derive(Serialize)]
pub struct AnswerResp {
    grades: Vec<ObjectiveGrade>,
    advance: bool,
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

    let assessment = engine::grade(
        &state.ai,
        &sidecar.rubric,
        &sidecar.exercise_html,
        &body.answer,
    )
    .await?;

    // Avançar exige todos demonstrados (§8).
    if assessment.all_demonstrated() {
        return Ok(Json(AnswerResp {
            grades: assessment.grades,
            advance: true,
            remediation_html: None,
        }));
    }

    // Remediação (§8.2): a similaridade cresce com o número de tentativas.
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
    let html = engine::remediate(
        &state.ai,
        &sidecar.title,
        &sidecar.exercise_html,
        &body.answer,
        &assessment.unmet(),
        attempt,
    )
    .await?;

    // Append-only na camada de interação (§4.3), ancorado no exercício.
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
            body_html: html.clone(),
        },
    )?;

    Ok(Json(AnswerResp {
        grades: assessment.grades,
        advance: false,
        remediation_html: Some(html),
    }))
}

/// Formata um quadro SSE com `data` JSON-encodado (evita problemas de newline).
fn sse_frame(event: &str, data: &str) -> Bytes {
    let json = serde_json::to_string(data).unwrap_or_else(|_| "\"\"".to_string());
    Bytes::from(format!("event: {event}\ndata: {json}\n\n"))
}

// ---------------------------------------------------------------------------
// Seleção de provedor (§12) — OpenRouter default quando há chave; senão modo
// demo offline para o loop rodar sem chave.
// ---------------------------------------------------------------------------

/// Constrói o `Ai` a partir do ambiente. OpenRouter (default, §12) quando
/// `LEARNIVE_OPENROUTER_KEY` está setada; senão cai no modo demo.
pub fn build_ai() -> Ai {
    match std::env::var("LEARNIVE_OPENROUTER_KEY") {
        Ok(key) if !key.is_empty() => {
            let fast = std::env::var("LEARNIVE_MODEL_FAST")
                .unwrap_or_else(|_| "openai/gpt-4o-mini".into());
            let robust =
                std::env::var("LEARNIVE_MODEL_ROBUST").unwrap_or_else(|_| "openai/gpt-4o".into());
            Ai::new(
                Provider::OpenAiCompat(OpenAiCompat::openrouter(Some(key))),
                Models::new(fast, robust),
            )
        }
        _ => {
            eprintln!(
                "Nenhuma chave de IA configurada — rodando em MODO DEMO (conteúdo canned). \
                 Configure OpenRouter no setup para conteúdo real."
            );
            demo_ai()
        }
    }
}

/// `Ai` em modo demo: um mock que responde diferente por sub-tarefa, fechando o
/// loop inteiro offline.
pub fn demo_ai() -> Ai {
    Ai::new(
        Provider::Mock(MockProvider::scripted(demo_responder)),
        Models::single("demo"),
    )
}

fn demo_responder(req: &crate::ai::ChatRequest) -> String {
    let text = req
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    if text.contains("array JSON de strings") {
        r#"["Introdução ao tema", "Conceito central", "Aplicação prática"]"#.to_string()
    } else if text.contains("exercise_html") {
        r#"{"exercise_html":"<form><p>Explique o conceito com suas palavras e aplique-o a um caso novo:</p><textarea name=\"resposta\" rows=\"4\"></textarea></form>","objectives":[{"id":"o1","kind":"application","description":"Aplicar o conceito a um caso novo","criteria":"A resposta transfere o conceito a um cenário não coberto no texto","transfer":true}]}"#.to_string()
    } else if text.contains("Corrija a resposta") {
        // Demo: sempre demonstrado, para o loop avançar de ponta a ponta.
        r#"{"grades":[{"objective_id":"o1","grade":"demonstrated","feedback":"Boa transferência do conceito para um caso novo."}]}"#.to_string()
    } else if text.contains("remediação") {
        "<p>Vamos revisar com um exemplo resolvido e um novo problema parecido.</p>".to_string()
    } else {
        // Prosa (default).
        "<h2>Conceito central</h2><p>Este é um parágrafo explicativo gerado em <strong>modo demo</strong> (sem chave de IA). Configure um provedor para conteúdo real, fundamentado em fontes.</p><p>Cada nó é atômico e termina numa checagem de compreensão.</p>".to_string()
    }
}
