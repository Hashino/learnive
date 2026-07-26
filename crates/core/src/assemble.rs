//! Montagem do dialeto a partir de conteúdo gerado (§4.2/§4.3).
//!
//! O modelo gera blocos semânticos de prosa **sem** IDs; o servidor atribui um
//! `data-block-id` estável a cada bloco de topo, garantindo controle e
//! unicidade dos IDs (a ancoragem §4.3 depende deles). É lógica de dialeto, sem
//! I/O, então mora no core wasm-safe.

use scraper::{Html, Selector};

/// Atribui `data-block-id` sequencial (`{prefix}{n}`) a cada elemento de topo do
/// HTML que ainda não tenha um. Reserializa via `scraper`.
pub fn ensure_block_ids(inner_html: &str, prefix: &str) -> String {
    let wrapped = format!(r#"<div id="__lv_root">{inner_html}</div>"#);
    let frag = Html::parse_fragment(&wrapped);
    let sel = Selector::parse("#__lv_root > *").expect("seletor estático");

    let mut out = String::new();
    let mut n = 1;
    for el in frag.select(&sel) {
        let html = el.html();
        if el.value().attr("data-block-id").is_some() {
            out.push_str(&html);
        } else {
            out.push_str(&inject_attr(&html, &format!("{prefix}{n}")));
            n += 1;
        }
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// Insere ` data-block-id="id"` logo após o nome da tag de abertura.
fn inject_attr(html: &str, id: &str) -> String {
    let Some(rest) = html.strip_prefix('<') else {
        return html.to_string();
    };
    let tag_end = rest
        .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .unwrap_or(rest.len());
    let insert_at = 1 + tag_end;
    let mut s = String::with_capacity(html.len() + 32);
    s.push_str(&html[..insert_at]);
    s.push_str(&format!(r#" data-block-id="{id}""#));
    s.push_str(&html[insert_at..]);
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Node;

    #[test]
    fn assigns_ids_to_idless_blocks() {
        let inner = "<h2>Título</h2>\n<p>Parágrafo um.</p>\n<p>Parágrafo dois.</p>";
        let out = ensure_block_ids(inner, "b");
        assert!(out.contains(r#"data-block-id="b1""#));
        assert!(out.contains(r#"data-block-id="b2""#));
        assert!(out.contains(r#"data-block-id="b3""#));
    }

    #[test]
    fn produces_parseable_node_with_three_blocks() {
        let inner = ensure_block_ids("<p>a</p><p>b</p>", "b");
        let html = format!(
            "<article data-node-id=\"n1\" data-doc-id=\"d1\">\
             <section data-layer=\"content\">{inner}</section>\
             <section data-layer=\"interaction\"></section></article>"
        );
        let node = Node::parse(&html).unwrap();
        assert_eq!(node.content.blocks.len(), 2);
        assert_eq!(node.content.blocks[0].id, "b1");
        assert_eq!(node.content.blocks[1].id, "b2");
    }

    #[test]
    fn keeps_existing_ids() {
        let inner = r#"<p data-block-id="keep">x</p><p>y</p>"#;
        let out = ensure_block_ids(inner, "b");
        assert!(out.contains(r#"data-block-id="keep""#));
        assert!(out.contains(r#"data-block-id="b1""#));
    }
}
