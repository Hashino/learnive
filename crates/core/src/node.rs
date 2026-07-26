//! O nó e suas duas camadas (§4.3), com parsing/serialização do dialeto HTML
//! (§4.2) via `scraper`.
//!
//! Invariante central: a **camada de conteúdo é congelada**. Nós guardamos o
//! HTML do conteúdo (`ContentLayer::html`, normalizado pelo parser na ingestão)
//! e nunca o regeneramos a partir das partes estruturadas — só *lemos*
//! blocos/objetivos/citações dele. Interação é um `Vec` **append-only**;
//! `to_html` reemite o conteúdo congelado seguido dos itens de interação, sem
//! jamais tocar no primeiro.
//!
//! Nota: `inner_html()` do `scraper` não preserva a ordem dos atributos byte a
//! byte, então `ContentLayer::html` é a forma *normalizada na ingestão*, não uma
//! cópia crua da fonte. O que é garantido (e testado) é estrutural — IDs de
//! bloco, texto, objetivos, exercício — e que o append de interação nunca altera
//! o conteúdo. Uma serialização canônica (ordem de atributo estável, p/ diffs
//! limpos no espírito "arquivos legíveis" §4) fica para depois.

use scraper::{ElementRef, Html, Selector, node::Element};
use serde::{Deserialize, Serialize};

use crate::anchor::{Anchor, QuoteSelector, ResolvedAnchor, resolve_quote};

/// Tipo de objetivo de aprendizagem (§8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ObjectiveType {
    Knowledge,
    Application,
    Synthesis,
}

/// Objetivo de aprendizagem associado a um trecho do conteúdo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Objective {
    pub id: String,
    pub kind: ObjectiveType,
    pub text: String,
}

/// Marcador de citação de fonte (livro/capítulo via `source_id`+`locator`, ou
/// web via `source_url`) — §11.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Citation {
    pub source_id: Option<String>,
    pub source_url: Option<String>,
    pub locator: Option<String>,
    pub text: String,
}

/// Um bloco endereçável da camada de conteúdo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentBlock {
    pub id: String,
    /// Bloco interativo gerado (§4.4) rodando em sandbox.
    pub interactive: bool,
    /// Texto do bloco (para ancoragem por citação).
    pub text: String,
}

/// Exercício do nó; o `rubric_id` liga ao rubric travado na criação (§8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exercise {
    pub exercise_id: String,
    pub rubric_id: Option<String>,
}

/// Camada de conteúdo — congelada (§4.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentLayer {
    /// HTML do conteúdo, congelado na ingestão (normalizado pelo parser). Nunca
    /// editado depois; o append de interação jamais o altera.
    pub html: String,
    pub blocks: Vec<ContentBlock>,
    pub objectives: Vec<Objective>,
    pub citations: Vec<Citation>,
    pub exercise: Option<Exercise>,
}

impl ContentLayer {
    /// Resolve uma âncora contra esta camada: acha o bloco por ID e, se houver
    /// citação, o intervalo do trecho no texto do bloco.
    pub fn resolve(&self, anchor: &Anchor) -> Option<ResolvedAnchor> {
        let block = self.blocks.iter().find(|b| b.id == anchor.block_id)?;
        let range = match &anchor.quote {
            None => None,
            Some(quote) => Some(resolve_quote(&block.text, quote)?),
        };
        Some(ResolvedAnchor {
            block_id: block.id.clone(),
            range,
        })
    }
}

/// Tipo de thread na camada de interação (§4.3, §8.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThreadKind {
    Qa,
    Remediation,
}

/// Item da camada de interação (append-only). Sempre *referencia* IDs de
/// conteúdo, nunca os altera.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InteractionItem {
    Annotation {
        id: String,
        anchor: Anchor,
        body_html: String,
    },
    Thread {
        id: String,
        kind: ThreadKind,
        anchor_block: Option<String>,
        body_html: String,
    },
}

/// Um nó do documento vivo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub node_id: String,
    pub doc_id: String,
    /// Ponteiro para a versão anterior na cadeia (§5), se houver.
    pub prev_version: Option<String>,
    pub content: ContentLayer,
    pub interaction: Vec<InteractionItem>,
}

/// Erros de parsing do dialeto de nó.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    MissingArticle,
    MissingContentSection,
    MissingAttr(&'static str),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::MissingArticle => write!(f, "elemento <article data-node-id> ausente"),
            ParseError::MissingContentSection => {
                write!(f, "seção <section data-layer=\"content\"> ausente")
            }
            ParseError::MissingAttr(name) => write!(f, "atributo obrigatório ausente: {name}"),
        }
    }
}

impl std::error::Error for ParseError {}

impl Node {
    /// Lê um nó a partir do HTML do dialeto (§4.2/§4.3).
    pub fn parse(html: &str) -> Result<Node, ParseError> {
        let fragment = Html::parse_fragment(html);

        let article = fragment
            .select(&sel("article[data-node-id]"))
            .next()
            .ok_or(ParseError::MissingArticle)?;

        let node_id = required_attr(&article, "data-node-id")?;
        let doc_id = required_attr(&article, "data-doc-id")?;
        let prev_version = article
            .value()
            .attr("data-prev-version")
            .map(str::to_string);

        let content_el = article
            .select(&sel(r#"section[data-layer="content"]"#))
            .next()
            .ok_or(ParseError::MissingContentSection)?;

        let content = parse_content(&content_el);
        let interaction = parse_interaction(&article);

        Ok(Node {
            node_id,
            doc_id,
            prev_version,
            content,
            interaction,
        })
    }

    /// Serializa o nó de volta para o dialeto. O conteúdo congelado é reemitido
    /// verbatim; a interação é renderizada em ordem de acréscimo.
    pub fn to_html(&self) -> String {
        let mut s = String::new();
        s.push_str("<article");
        push_attr(&mut s, "data-node-id", &self.node_id);
        push_attr(&mut s, "data-doc-id", &self.doc_id);
        if let Some(pv) = &self.prev_version {
            push_attr(&mut s, "data-prev-version", pv);
        }
        s.push_str(">\n");

        s.push_str("  <section data-layer=\"content\">\n");
        s.push_str(self.content.html.trim());
        s.push('\n');
        s.push_str("  </section>\n");

        s.push_str("  <section data-layer=\"interaction\">\n");
        for item in &self.interaction {
            s.push_str(&render_interaction(item));
            s.push('\n');
        }
        s.push_str("  </section>\n");

        s.push_str("</article>\n");
        s
    }

    /// Acrescenta um item à camada de interação (append-only, §4.3). Nunca toca
    /// na camada de conteúdo.
    pub fn push_interaction(&mut self, item: InteractionItem) {
        self.interaction.push(item);
    }

    /// Resolve uma âncora contra a camada de conteúdo deste nó.
    pub fn resolve(&self, anchor: &Anchor) -> Option<ResolvedAnchor> {
        self.content.resolve(anchor)
    }
}

fn parse_content(content_el: &ElementRef) -> ContentLayer {
    let html = content_el.inner_html().trim().to_string();

    let blocks = content_el
        .select(&sel("[data-block-id]"))
        .filter_map(|el| {
            let id = el.value().attr("data-block-id")?.to_string();
            Some(ContentBlock {
                id,
                interactive: el.value().attr("data-interactive").is_some(),
                text: el.text().collect::<String>(),
            })
        })
        .collect();

    let objectives = content_el
        .select(&sel("span[data-objective-id]"))
        .filter_map(|el| {
            let id = el.value().attr("data-objective-id")?.to_string();
            let kind = el
                .value()
                .attr("data-objective-type")
                .map(parse_objective_type)
                .unwrap_or(ObjectiveType::Knowledge);
            Some(Objective {
                id,
                kind,
                text: el.text().collect::<String>(),
            })
        })
        .collect();

    let citations = content_el
        .select(&sel("cite"))
        .filter_map(|el| {
            let source_id = el.value().attr("data-source-id").map(str::to_string);
            let source_url = el.value().attr("data-source-url").map(str::to_string);
            if source_id.is_none() && source_url.is_none() {
                return None;
            }
            Some(Citation {
                source_id,
                source_url,
                locator: el.value().attr("data-locator").map(str::to_string),
                text: el.text().collect::<String>(),
            })
        })
        .collect();

    let exercise = content_el
        .select(&sel("form[data-exercise-id]"))
        .next()
        .map(|el| Exercise {
            exercise_id: el
                .value()
                .attr("data-exercise-id")
                .unwrap_or_default()
                .to_string(),
            rubric_id: el.value().attr("data-rubric-id").map(str::to_string),
        });

    ContentLayer {
        html,
        blocks,
        objectives,
        citations,
        exercise,
    }
}

fn parse_interaction(article: &ElementRef) -> Vec<InteractionItem> {
    let Some(inter_el) = article
        .select(&sel(r#"section[data-layer="interaction"]"#))
        .next()
    else {
        return Vec::new();
    };

    // Itera os filhos em ordem de documento para preservar a ordem de acréscimo.
    inter_el
        .children()
        .filter_map(ElementRef::wrap)
        .filter_map(|el| {
            let v = el.value();
            match v.name() {
                "aside" if v.attr("data-annotation-id").is_some() => {
                    Some(InteractionItem::Annotation {
                        id: v.attr("data-annotation-id").unwrap().to_string(),
                        anchor: Anchor {
                            block_id: v.attr("data-anchor-block").unwrap_or_default().to_string(),
                            quote: build_quote(v),
                        },
                        body_html: el.inner_html(),
                    })
                }
                "div" if v.attr("data-thread-id").is_some() => Some(InteractionItem::Thread {
                    id: v.attr("data-thread-id").unwrap().to_string(),
                    kind: v
                        .attr("data-thread-kind")
                        .map(parse_thread_kind)
                        .unwrap_or(ThreadKind::Qa),
                    anchor_block: v.attr("data-anchor-block").map(str::to_string),
                    body_html: el.inner_html(),
                }),
                _ => None,
            }
        })
        .collect()
}

fn build_quote(v: &Element) -> Option<QuoteSelector> {
    let exact = v.attr("data-anchor-quote")?;
    Some(QuoteSelector {
        exact: exact.to_string(),
        prefix: v.attr("data-anchor-prefix").map(str::to_string),
        suffix: v.attr("data-anchor-suffix").map(str::to_string),
    })
}

fn render_interaction(item: &InteractionItem) -> String {
    match item {
        InteractionItem::Annotation {
            id,
            anchor,
            body_html,
        } => {
            let mut s = String::from("    <aside");
            push_attr(&mut s, "data-annotation-id", id);
            push_attr(&mut s, "data-anchor-block", &anchor.block_id);
            if let Some(q) = &anchor.quote {
                push_attr(&mut s, "data-anchor-quote", &q.exact);
                if let Some(p) = &q.prefix {
                    push_attr(&mut s, "data-anchor-prefix", p);
                }
                if let Some(sf) = &q.suffix {
                    push_attr(&mut s, "data-anchor-suffix", sf);
                }
            }
            s.push('>');
            s.push_str(body_html);
            s.push_str("</aside>");
            s
        }
        InteractionItem::Thread {
            id,
            kind,
            anchor_block,
            body_html,
        } => {
            let mut s = String::from("    <div");
            push_attr(&mut s, "data-thread-id", id);
            push_attr(&mut s, "data-thread-kind", thread_kind_str(*kind));
            if let Some(b) = anchor_block {
                push_attr(&mut s, "data-anchor-block", b);
            }
            s.push('>');
            s.push_str(body_html);
            s.push_str("</div>");
            s
        }
    }
}

fn parse_objective_type(s: &str) -> ObjectiveType {
    match s {
        "application" => ObjectiveType::Application,
        "synthesis" => ObjectiveType::Synthesis,
        _ => ObjectiveType::Knowledge,
    }
}

fn parse_thread_kind(s: &str) -> ThreadKind {
    match s {
        "remediation" => ThreadKind::Remediation,
        _ => ThreadKind::Qa,
    }
}

fn thread_kind_str(k: ThreadKind) -> &'static str {
    match k {
        ThreadKind::Qa => "qa",
        ThreadKind::Remediation => "remediation",
    }
}

fn required_attr(el: &ElementRef, name: &'static str) -> Result<String, ParseError> {
    el.value()
        .attr(name)
        .map(str::to_string)
        .ok_or(ParseError::MissingAttr(name))
}

fn sel(s: &str) -> Selector {
    Selector::parse(s).expect("seletor estático válido")
}

fn push_attr(s: &mut String, name: &str, value: &str) {
    s.push(' ');
    s.push_str(name);
    s.push_str("=\"");
    s.push_str(&escape_attr(value));
    s.push('"');
}

fn escape_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<article data-node-id="n1" data-doc-id="d1">
  <section data-layer="content">
    <p data-block-id="b1">Primeiro parágrafo sobre limites.</p>
    <p data-block-id="b2">Segundo bloco com <span data-objective-id="o1" data-objective-type="application">transferência</span> e uma <cite data-source-id="s1" data-locator="chap:3;p:42">fonte</cite>.</p>
    <form data-exercise-id="e1" data-rubric-id="r1"></form>
  </section>
  <section data-layer="interaction">
  </section>
</article>"#;

    #[test]
    fn parses_layers() {
        let node = Node::parse(SAMPLE).unwrap();
        assert_eq!(node.node_id, "n1");
        assert_eq!(node.doc_id, "d1");
        assert_eq!(node.prev_version, None);
        assert_eq!(node.content.blocks.len(), 2);
        assert_eq!(node.content.blocks[0].id, "b1");
        assert_eq!(node.content.objectives.len(), 1);
        assert_eq!(node.content.objectives[0].kind, ObjectiveType::Application);
        assert_eq!(node.content.citations.len(), 1);
        assert_eq!(
            node.content.citations[0].locator.as_deref(),
            Some("chap:3;p:42")
        );
        assert_eq!(node.content.exercise.as_ref().unwrap().exercise_id, "e1");
        assert!(node.interaction.is_empty());
    }

    #[test]
    fn round_trips_content() {
        let node = Node::parse(SAMPLE).unwrap();
        let reparsed = Node::parse(&node.to_html()).unwrap();
        assert_eq!(node.node_id, reparsed.node_id);
        assert_eq!(node.content.blocks, reparsed.content.blocks);
        assert_eq!(node.content.objectives, reparsed.content.objectives);
        assert_eq!(node.content.exercise, reparsed.content.exercise);
    }

    #[test]
    fn append_interaction_leaves_content_frozen() {
        let mut node = Node::parse(SAMPLE).unwrap();
        let frozen = node.content.html.clone();

        node.push_interaction(InteractionItem::Annotation {
            id: "a1".to_string(),
            anchor: Anchor {
                block_id: "b1".to_string(),
                quote: Some(QuoteSelector {
                    exact: "limites".to_string(),
                    prefix: None,
                    suffix: None,
                }),
            },
            body_html: "<p>minha nota</p>".to_string(),
        });

        // Invariante real (§4.3): o append não toca no conteúdo congelado.
        assert_eq!(node.content.html, frozen);

        // A interação sobrevive ao round-trip; os blocos de conteúdo idem.
        let reparsed = Node::parse(&node.to_html()).unwrap();
        assert_eq!(reparsed.interaction.len(), 1);
        assert_eq!(reparsed.content.blocks, node.content.blocks);
        match &reparsed.interaction[0] {
            InteractionItem::Annotation { id, anchor, .. } => {
                assert_eq!(id, "a1");
                assert_eq!(anchor.block_id, "b1");
                assert_eq!(anchor.quote.as_ref().unwrap().exact, "limites");
            }
            _ => panic!("esperava anotação"),
        }
    }

    #[test]
    fn preserves_interaction_order() {
        let mut node = Node::parse(SAMPLE).unwrap();
        node.push_interaction(InteractionItem::Thread {
            id: "t1".to_string(),
            kind: ThreadKind::Remediation,
            anchor_block: Some("b2".to_string()),
            body_html: "<p>remediação</p>".to_string(),
        });
        node.push_interaction(InteractionItem::Annotation {
            id: "a2".to_string(),
            anchor: Anchor::block("b1"),
            body_html: "<p>depois</p>".to_string(),
        });

        let reparsed = Node::parse(&node.to_html()).unwrap();
        assert_eq!(reparsed.interaction.len(), 2);
        match &reparsed.interaction[0] {
            InteractionItem::Thread { id, kind, .. } => {
                assert_eq!(id, "t1");
                assert_eq!(*kind, ThreadKind::Remediation);
            }
            _ => panic!("esperava thread primeiro"),
        }
        match &reparsed.interaction[1] {
            InteractionItem::Annotation { id, .. } => assert_eq!(id, "a2"),
            _ => panic!("esperava anotação segundo"),
        }
    }

    #[test]
    fn resolve_block_and_quote() {
        let node = Node::parse(SAMPLE).unwrap();

        // Bloco inteiro.
        let r = node.resolve(&Anchor::block("b1")).unwrap();
        assert_eq!(r.block_id, "b1");
        assert!(r.range.is_none());

        // Trecho por citação.
        let r = node
            .resolve(&Anchor {
                block_id: "b1".to_string(),
                quote: Some(QuoteSelector {
                    exact: "limites".to_string(),
                    prefix: None,
                    suffix: None,
                }),
            })
            .unwrap();
        let (s, e) = r.range.unwrap();
        let block_text = &node.content.blocks[0].text;
        assert_eq!(&block_text[s..e], "limites");

        // Bloco inexistente.
        assert!(node.resolve(&Anchor::block("nope")).is_none());
    }

    #[test]
    fn version_chain_pointer_survives() {
        let src = r#"<article data-node-id="n2" data-doc-id="d1" data-prev-version="n1">
  <section data-layer="content"><p data-block-id="b1">v2</p></section>
  <section data-layer="interaction"></section>
</article>"#;
        let node = Node::parse(src).unwrap();
        assert_eq!(node.prev_version.as_deref(), Some("n1"));
        let reparsed = Node::parse(&node.to_html()).unwrap();
        assert_eq!(reparsed.prev_version.as_deref(), Some("n1"));
    }
}
