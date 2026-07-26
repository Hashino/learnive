//! Motor de currículo (§6) e avaliação (§8): transforma um tema num outline,
//! gera nós sob demanda e avalia respostas contra um rubric **travado na
//! criação** (§8).
//!
//! Divisão de responsabilidade com §14: a prosa (robusto, streamada) é gerada
//! pelo endpoint token-a-token; aqui ficam as partes puras/testáveis (prompts,
//! parsing, montagem) e as orquestrações não-streamadas (outline, exercício +
//! rubric, correção). O rubric é gerado numa chamada **separada** da prosa e
//! guardado server-only (§8) — o aluno nunca o vê.
//!
//! Consumido pelos endpoints do loop (Task #5b); daí o `allow` temporário.
#![allow(dead_code)]

use futures_util::StreamExt;
use rand::{Rng, distributions::Alphanumeric};
use serde::{Deserialize, Serialize};

use learnive_core::{Node, ObjectiveType, ensure_block_ids};

use crate::ai::{Ai, ChatMessage, ProviderError, Tier};

/// Nota por objetivo (§8): não é passa/falha.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grade {
    NotDemonstrated,
    Partial,
    Demonstrated,
}

/// Um item do outline (§6): um conceito a virar nó.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutlineItem {
    pub title: String,
}

/// Esqueleto do documento vivo (§6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outline {
    pub topic: String,
    pub items: Vec<OutlineItem>,
}

/// Um objetivo do rubric, travado na criação do nó (§8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RubricObjective {
    pub id: String,
    pub kind: ObjectiveType,
    pub description: String,
    /// Critério objetivo de avaliação (o que conta como demonstrado).
    pub criteria: String,
    /// Item de transferência (§8): exige aplicar a cenário não coberto no texto.
    #[serde(default)]
    pub transfer: bool,
}

/// Rubric completo — server-only, nunca servido ao cliente (§8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rubric {
    pub objectives: Vec<RubricObjective>,
}

/// Nota de um objetivo após a correção.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectiveGrade {
    pub objective_id: String,
    pub grade: Grade,
    pub feedback: String,
}

/// Resultado da avaliação de uma resposta.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assessment {
    pub grades: Vec<ObjectiveGrade>,
}

impl Assessment {
    /// Avançar exige todos os objetivos demonstrados (§8).
    pub fn all_demonstrated(&self) -> bool {
        !self.grades.is_empty() && self.grades.iter().all(|g| g.grade == Grade::Demonstrated)
    }

    /// Objetivos ainda não demonstrados — disparam a remediação (§8.2).
    pub fn unmet(&self) -> Vec<&ObjectiveGrade> {
        self.grades
            .iter()
            .filter(|g| g.grade != Grade::Demonstrated)
            .collect()
    }
}

/// Exercício + rubric gerados juntos (§8), numa chamada separada da prosa (§14).
#[derive(Debug, Clone)]
pub struct ExerciseAndRubric {
    pub exercise_html: String,
    pub rubric: Rubric,
}

/// Erros do motor.
#[derive(Debug)]
pub enum EngineError {
    Provider(ProviderError),
    Parse(String),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::Provider(e) => write!(f, "provedor: {e}"),
            EngineError::Parse(m) => write!(f, "resposta do modelo não pôde ser lida: {m}"),
        }
    }
}

impl std::error::Error for EngineError {}

impl From<ProviderError> for EngineError {
    fn from(e: ProviderError) -> Self {
        EngineError::Provider(e)
    }
}

/// ID curto aleatório (alfanumérico minúsculo), seguro para nome de arquivo/ID.
pub fn new_id() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(10)
        .map(char::from)
        .collect::<String>()
        .to_lowercase()
}

// ---------------------------------------------------------------------------
// Orquestrações (usam o provedor).
// ---------------------------------------------------------------------------

/// Gera o outline inicial a partir do tema (§6, §6.1). Tier leve (planejamento).
pub async fn generate_outline(ai: &Ai, topic: &str) -> Result<Outline, EngineError> {
    let text = collect(ai, Tier::Fast, prompt::outline(topic)).await?;
    let titles =
        parse::outline(&text).ok_or_else(|| EngineError::Parse("outline vazio".to_string()))?;
    Ok(Outline {
        topic: topic.to_string(),
        items: titles
            .into_iter()
            .map(|title| OutlineItem { title })
            .collect(),
    })
}

/// Gera o exercício + rubric de um nó (§8), separado da prosa (§14). Tier leve.
pub async fn generate_exercise_and_rubric(
    ai: &Ai,
    topic: &str,
    item_title: &str,
    prose: &str,
) -> Result<ExerciseAndRubric, EngineError> {
    let text = collect(
        ai,
        Tier::Fast,
        prompt::exercise_rubric(topic, item_title, prose),
    )
    .await?;
    parse::exercise_rubric(&text)
}

/// Corrige uma resposta contra o rubric travado (§8). Tier leve.
pub async fn grade(
    ai: &Ai,
    rubric: &Rubric,
    exercise_html: &str,
    answer: &str,
) -> Result<Assessment, EngineError> {
    let text = collect(
        ai,
        Tier::Fast,
        prompt::grading(rubric, exercise_html, answer),
    )
    .await?;
    parse::assessment(&text)
}

/// Conversa de remediação na falha (§8.2): explica o conceito no contexto do
/// exercício e propõe um novo problema similar cuja similaridade cresce a cada
/// tentativa (`attempt`). Tier robusto (é ensino/prosa). Devolve HTML.
pub async fn remediate(
    ai: &Ai,
    item_title: &str,
    exercise_html: &str,
    answer: &str,
    unmet: &[&ObjectiveGrade],
    attempt: u32,
) -> Result<String, EngineError> {
    let unmet_summary = unmet
        .iter()
        .map(|g| format!("- {}: {}", g.objective_id, g.feedback))
        .collect::<Vec<_>>()
        .join("\n");
    collect(
        ai,
        Tier::Robust,
        prompt::remediation(item_title, exercise_html, answer, &unmet_summary, attempt),
    )
    .await
}

/// Monta um nó do dialeto a partir da prosa gerada e do exercício (§4.2/§4.3).
/// O servidor atribui os IDs (blocos, exercício, rubric).
pub fn assemble_node(
    doc_id: &str,
    node_id: &str,
    prose_inner_html: &str,
    exercise_html: &str,
    exercise_id: &str,
    rubric_id: &str,
) -> Result<Node, EngineError> {
    let blocks = ensure_block_ids(prose_inner_html, &format!("{node_id}-b"));
    let form = ensure_form_ids(exercise_html, exercise_id, rubric_id);
    let article = format!(
        "<article data-node-id=\"{node_id}\" data-doc-id=\"{doc_id}\">\n  \
         <section data-layer=\"content\">\n{blocks}\n{form}\n  </section>\n  \
         <section data-layer=\"interaction\"></section>\n</article>"
    );
    Node::parse(&article).map_err(|e| EngineError::Parse(e.to_string()))
}

/// Coleta um stream completo numa String (para chamadas não-streamadas).
async fn collect(ai: &Ai, tier: Tier, messages: Vec<ChatMessage>) -> Result<String, EngineError> {
    let mut stream = ai.stream(tier, messages).await?;
    let mut out = String::new();
    while let Some(token) = stream.next().await {
        out.push_str(&token?);
    }
    Ok(out)
}

/// Injeta `data-exercise-id`/`data-rubric-id` na primeira `<form>` (envolvendo
/// numa se não houver).
fn ensure_form_ids(exercise_html: &str, exercise_id: &str, rubric_id: &str) -> String {
    let with_form = if exercise_html.contains("<form") {
        exercise_html.to_string()
    } else {
        format!("<form>{exercise_html}</form>")
    };
    match with_form.find("<form") {
        Some(pos) => {
            let insert_at = pos + "<form".len();
            let mut s = String::with_capacity(with_form.len() + 64);
            s.push_str(&with_form[..insert_at]);
            s.push_str(&format!(
                r#" data-exercise-id="{exercise_id}" data-rubric-id="{rubric_id}""#
            ));
            s.push_str(&with_form[insert_at..]);
            s
        }
        None => with_form,
    }
}

// ---------------------------------------------------------------------------
// Prompts (§6, §8). Em português — o conteúdo do app é PT.
// ---------------------------------------------------------------------------

pub mod prompt {
    use super::Rubric;
    use crate::ai::ChatMessage;

    /// Contrato de HTML para superfícies **sanitizadas** (prosa, remediação):
    /// esse conteúdo é inserido na origem da app e passa pelo `sanitizeHtml` do
    /// cliente (`assets/index.html`), que REMOVE silenciosamente o que estiver
    /// fora do contrato. Informar o modelo evita que ele gere algo que suma na
    /// renderização (§3.1/§4.4). **Mantenha em sincronia com o sanitizador** —
    /// a lista de tags/atributos bloqueados aqui espelha a de lá.
    pub const PROSE_HTML_CONTRACT: &str = "\
Regras de HTML (o conteúdo é sanitizado — o que violar some na renderização):\n\
- Use SÓ HTML semântico estático: <h2>-<h4>, <p>, <ul>/<ol>/<li>, <table>/<tr>/\
<td>, <code>/<pre>, <blockquote>, <strong>/<em>, <a href> (só http(s), mailto: \
ou #) e <img> (só src https: ou data:image/).\n\
- NUNCA use: <script>, <style>, <form>, <iframe>, <object>, <embed>, <link>, \
<meta>, <base>; nem atributos on* (onclick, onerror...), nem atributo style \
inline, nem URLs javascript:. Tudo isso é descartado.\n\
- Não gere atributos id/data-* — o servidor os atribui.\n\
- Precisa de interatividade, visualização dinâmica, JS ou CSS próprio? Isso NÃO \
cabe na prosa (seria removido); vai no bloco de exercício, que roda isolado num \
iframe sandbox.";

    /// Contrato do bloco de exercício: roda isolado num `<iframe sandbox>`
    /// (§4.4) — SEM same-origin, não vê o token nem o DOM da página — então
    /// NÃO é sanitizado e pode usar JS/CSS/SVG à vontade. Em troca precisa
    /// devolver a resposta pelo protocolo postMessage (§8: artefato estruturado
    /// travado junto com o rubric).
    pub const EXERCISE_HTML_CONTRACT: &str = "\
O exercise_html roda isolado num iframe sandbox (allow-scripts, SEM same-origin): \
não vê o token nem a página. Logo, pode usar HTML/CSS/JS/SVG à vontade — inclusive \
visualizações interativas, não só campos de texto.\n\
Como a resposta volta (§8):\n\
- Caso simples: inclua um <form> (ou um <input>/<textarea>) — a página injeta o \
botão \"Enviar\" e coleta os campos automaticamente.\n\
- Caso interativo/custom: quando o aluno concluir, chame você mesmo \
parent.postMessage({type:'learnive-answer', answer: JSON.stringify(ARTEFATO)}, '*'), \
onde ARTEFATO é um JSON estruturado que o rubric consiga avaliar.\n\
Nunca escreva os critérios de avaliação dentro do exercise_html (são server-only).";

    pub fn outline(topic: &str) -> Vec<ChatMessage> {
        vec![
            ChatMessage::system(
                "Você planeja um currículo de aprendizado. Dado um tema, responda \
                 APENAS com um array JSON de strings — os títulos dos conceitos, do \
                 mais básico ao avançado, atômicos (§6). Sem comentários, sem markdown.",
            ),
            ChatMessage::user(format!("Tema: {topic}")),
        ]
    }

    pub fn prose(topic: &str, item_title: &str, context: &str) -> Vec<ChatMessage> {
        vec![
            ChatMessage::system(format!(
                "Você é um tutor. Escreva blocos curtos de prosa explicativa em HTML \
                 semântico. Seja atômico: explique só este conceito e pare antes de \
                 qualquer exercício (a checagem vem numa etapa separada).\n\n\
                 {PROSE_HTML_CONTRACT}"
            )),
            ChatMessage::user(format!(
                "Tema geral: {topic}\nConceito deste nó: {item_title}\n\
                 Contexto do que já foi ensinado: {context}"
            )),
        ]
    }

    pub fn exercise_rubric(topic: &str, item_title: &str, prose: &str) -> Vec<ChatMessage> {
        vec![
            ChatMessage::system(format!(
                "Gere uma checagem de compreensão para o conceito e seu rubric de \
                 avaliação, JUNTOS (§8). Responda APENAS com JSON no formato: \
                 {{\"exercise_html\":\"<form>...</form>\",\"objectives\":[{{\"id\":\"o1\",\
                 \"kind\":\"knowledge|application|synthesis\",\"description\":\"...\",\
                 \"criteria\":\"o que conta como demonstrado\",\"transfer\":true|false}}]}}. \
                 Todo objetivo 'application' deve ter ao menos um item de transferência \
                 (transfer=true) para um cenário NÃO coberto no texto.\n\n\
                 {EXERCISE_HTML_CONTRACT}"
            )),
            ChatMessage::user(format!(
                "Tema: {topic}\nConceito: {item_title}\nProsa do nó:\n{prose}"
            )),
        ]
    }

    pub fn remediation(
        item_title: &str,
        exercise_html: &str,
        answer: &str,
        unmet_summary: &str,
        attempt: u32,
    ) -> Vec<ChatMessage> {
        vec![
            ChatMessage::system(format!(
                "Sessão de remediação (§8.2). O aluno errou a checagem. Explique o \
                 conceito NO CONTEXTO do exercício: dê um exemplo resolvido / passo a \
                 passo, e proponha um novo problema similar. Esta é a tentativa \
                 {attempt}: quanto maior, MAIS similar ao exemplo resolvido o novo \
                 problema deve ser (scaffolding por proximidade crescente).\n\n\
                 {PROSE_HTML_CONTRACT}"
            )),
            ChatMessage::user(format!(
                "Conceito: {item_title}\nExercício: {exercise_html}\n\
                 Resposta do aluno: {answer}\nObjetivos não demonstrados:\n{unmet_summary}"
            )),
        ]
    }

    pub fn grading(rubric: &Rubric, exercise_html: &str, answer: &str) -> Vec<ChatMessage> {
        let rubric_json = serde_json::to_string(rubric).unwrap_or_default();
        vec![
            ChatMessage::system(
                "Corrija a resposta do aluno CONTRA o rubric travado, sem leniência \
                 (§8). Para cada objetivo dê a nota {not_demonstrated|partial|\
                 demonstrated} e um feedback curto. Responda APENAS com JSON: \
                 {\"grades\":[{\"objective_id\":\"o1\",\"grade\":\"...\",\
                 \"feedback\":\"...\"}]}.",
            ),
            ChatMessage::user(format!(
                "Rubric: {rubric_json}\nExercício: {exercise_html}\nResposta do aluno: {answer}"
            )),
        ]
    }
}

// ---------------------------------------------------------------------------
// Parsing tolerante da saída do modelo.
// ---------------------------------------------------------------------------

pub mod parse {
    use super::{Assessment, EngineError, ExerciseAndRubric, Rubric, RubricObjective};
    use learnive_core::ObjectiveType;
    use serde::Deserialize;

    /// Extrai o primeiro bloco JSON (`{...}` ou `[...]`) do texto, tolerando
    /// cercas de markdown e texto ao redor.
    fn extract_json(text: &str) -> Option<&str> {
        let start = text.find(['{', '['])?;
        let open = text.as_bytes()[start];
        let close = if open == b'{' { b'}' } else { b']' };
        let end = text.rfind(close as char)?;
        if end > start {
            Some(&text[start..=end])
        } else {
            None
        }
    }

    /// Outline: array JSON de strings, com fallback para linhas com bullets.
    pub fn outline(text: &str) -> Option<Vec<String>> {
        if let Some(json) = extract_json(text)
            && let Ok(list) = serde_json::from_str::<Vec<String>>(json)
            && !list.is_empty()
        {
            return Some(list.into_iter().map(|s| s.trim().to_string()).collect());
        }
        // Fallback: uma linha por conceito, tirando bullets/numeração.
        let items: Vec<String> = text
            .lines()
            .map(|l| {
                l.trim()
                    .trim_start_matches(['-', '*', '#', '•'])
                    .trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c == ')')
                    .trim()
                    .to_string()
            })
            .filter(|l| !l.is_empty())
            .collect();
        (!items.is_empty()).then_some(items)
    }

    pub fn exercise_rubric(text: &str) -> Result<ExerciseAndRubric, EngineError> {
        #[derive(Deserialize)]
        struct Raw {
            exercise_html: String,
            objectives: Vec<RawObjective>,
        }
        #[derive(Deserialize)]
        struct RawObjective {
            id: String,
            #[serde(default = "knowledge")]
            kind: ObjectiveType,
            description: String,
            #[serde(default)]
            criteria: String,
            #[serde(default)]
            transfer: bool,
        }
        fn knowledge() -> ObjectiveType {
            ObjectiveType::Knowledge
        }

        let json = extract_json(text).ok_or_else(|| EngineError::Parse("sem JSON".to_string()))?;
        let raw: Raw = serde_json::from_str(json).map_err(|e| EngineError::Parse(e.to_string()))?;
        Ok(ExerciseAndRubric {
            exercise_html: raw.exercise_html,
            rubric: Rubric {
                objectives: raw
                    .objectives
                    .into_iter()
                    .map(|o| RubricObjective {
                        id: o.id,
                        kind: o.kind,
                        description: o.description,
                        criteria: o.criteria,
                        transfer: o.transfer,
                    })
                    .collect(),
            },
        })
    }

    pub fn assessment(text: &str) -> Result<Assessment, EngineError> {
        let json = extract_json(text).ok_or_else(|| EngineError::Parse("sem JSON".to_string()))?;
        serde_json::from_str(json).map_err(|e| EngineError::Parse(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{MockProvider, Models, Provider};

    fn mock_ai(reply: &str) -> Ai {
        Ai::new(
            Provider::Mock(MockProvider::new(reply)),
            Models::single("mock"),
        )
    }

    #[test]
    fn parse_outline_json_and_fallback() {
        let items = parse::outline(r#"["Intro", "Limites", "Derivadas"]"#).unwrap();
        assert_eq!(items, vec!["Intro", "Limites", "Derivadas"]);

        let items = parse::outline("- Intro\n- Limites\n2. Derivadas").unwrap();
        assert_eq!(items, vec!["Intro", "Limites", "Derivadas"]);
    }

    #[test]
    fn parse_exercise_rubric_with_fences() {
        let text = r#"```json
{"exercise_html":"<form><input name=\"a\"></form>",
 "objectives":[{"id":"o1","kind":"application","description":"aplicar","criteria":"acerta caso novo","transfer":true}]}
```"#;
        let er = parse::exercise_rubric(text).unwrap();
        assert!(er.exercise_html.contains("<form>"));
        assert_eq!(er.rubric.objectives.len(), 1);
        assert_eq!(er.rubric.objectives[0].kind, ObjectiveType::Application);
        assert!(er.rubric.objectives[0].transfer);
    }

    #[test]
    fn parse_assessment_json() {
        let a = parse::assessment(
            r#"{"grades":[{"objective_id":"o1","grade":"demonstrated","feedback":"ok"}]}"#,
        )
        .unwrap();
        assert!(a.all_demonstrated());
        assert!(a.unmet().is_empty());
    }

    #[test]
    fn assessment_unmet_blocks_advance() {
        let a = Assessment {
            grades: vec![
                ObjectiveGrade {
                    objective_id: "o1".into(),
                    grade: Grade::Demonstrated,
                    feedback: String::new(),
                },
                ObjectiveGrade {
                    objective_id: "o2".into(),
                    grade: Grade::Partial,
                    feedback: String::new(),
                },
            ],
        };
        assert!(!a.all_demonstrated());
        assert_eq!(a.unmet().len(), 1);
        assert_eq!(a.unmet()[0].objective_id, "o2");
    }

    #[test]
    fn assemble_node_wraps_prose_and_exercise() {
        let node = assemble_node(
            "d1",
            "n1",
            "<h2>Limites</h2><p>Explicação.</p>",
            "<form><input name=\"r\"></form>",
            "ex1",
            "ru1",
        )
        .unwrap();
        assert!(node.content.blocks.len() >= 2);
        let ex = node.content.exercise.unwrap();
        assert_eq!(ex.exercise_id, "ex1");
        assert_eq!(ex.rubric_id.as_deref(), Some("ru1"));
    }

    #[test]
    fn sanitized_surfaces_carry_the_prose_contract() {
        // Prosa e remediação vão para a origem da app e são sanitizadas, então
        // o modelo precisa ser avisado do contrato (senão gera algo que some).
        let prose_sys = &prompt::prose("t", "c", "ctx")[0].content;
        assert!(prose_sys.contains("NUNCA use"));
        assert!(prose_sys.contains("<script>"));

        let rem_sys = &prompt::remediation("c", "<form></form>", "a", "o1: x", 2)[0].content;
        assert!(rem_sys.contains(prompt::PROSE_HTML_CONTRACT));

        // O exercício roda no sandbox: contrato oposto (pode JS, deve postMessage).
        let ex_sys = &prompt::exercise_rubric("t", "c", "prosa")[0].content;
        assert!(ex_sys.contains("postMessage"));
        assert!(ex_sys.contains("sandbox"));
    }

    #[tokio::test]
    async fn generate_outline_via_mock() {
        let ai = mock_ai(r#"["Introdução", "Conjuntos", "Funções"]"#);
        let outline = generate_outline(&ai, "matemática").await.unwrap();
        assert_eq!(outline.topic, "matemática");
        assert_eq!(outline.items.len(), 3);
        assert_eq!(outline.items[1].title, "Conjuntos");
    }

    #[tokio::test]
    async fn grade_via_mock() {
        let ai = mock_ai(
            r#"{"grades":[{"objective_id":"o1","grade":"demonstrated","feedback":"bom"}]}"#,
        );
        let rubric = Rubric {
            objectives: vec![RubricObjective {
                id: "o1".into(),
                kind: ObjectiveType::Knowledge,
                description: "d".into(),
                criteria: "c".into(),
                transfer: false,
            }],
        };
        let a = grade(&ai, &rubric, "<form></form>", "minha resposta")
            .await
            .unwrap();
        assert!(a.all_demonstrated());
    }
}
