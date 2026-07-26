//! Ancoragem (§4.3).
//!
//! Primária: por **ID de bloco estável** — resolve deterministicamente porque a
//! camada de conteúdo é congelada. Sub-bloco (um trecho dentro de um bloco):
//! âncora **fuzzy por citação** — quote exato + prefixo/sufixo de contexto
//! (estilo W3C Web Annotation / hypothes.is). Como o texto do bloco é imutável,
//! o fuzzy é só robustez contra normalização mínima de espaço em branco.
//!
//! Toda a resolução é pura e sem I/O — é exatamente o que será compilado para
//! wasm e reutilizado no cliente.

use serde::{Deserialize, Serialize};

/// Seletor de citação para ancorar um trecho dentro de um bloco.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteSelector {
    /// Trecho exato selecionado.
    pub exact: String,
    /// Contexto imediatamente antes, para desambiguar ocorrências repetidas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// Contexto imediatamente depois, para desambiguar ocorrências repetidas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suffix: Option<String>,
}

/// Âncora: um bloco, opcionalmente refinada para um trecho por citação.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    pub block_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<QuoteSelector>,
}

impl Anchor {
    /// Âncora de bloco inteiro (sem trecho).
    pub fn block(block_id: impl Into<String>) -> Self {
        Self {
            block_id: block_id.into(),
            quote: None,
        }
    }
}

/// Resultado da resolução: o bloco e, quando houver citação, o intervalo de
/// bytes `[start, end)` dentro do texto do bloco.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedAnchor {
    pub block_id: String,
    pub range: Option<(usize, usize)>,
}

/// Resolve uma citação contra o texto (congelado) de um bloco, devolvendo o
/// intervalo de bytes no texto original.
///
/// Estratégia: (1) match exato — se único, pronto; (2) se houver múltiplos
/// exatos, desambigua por prefixo/sufixo; (3) se não houver exato, busca
/// flexível a espaço em branco (robustez §4.3).
pub fn resolve_quote(text: &str, quote: &QuoteSelector) -> Option<(usize, usize)> {
    if quote.exact.is_empty() {
        return None;
    }

    let exacts = find_all(text, &quote.exact);
    match exacts.len() {
        1 => {
            let start = exacts[0];
            Some((start, start + quote.exact.len()))
        }
        0 => find_flexible(text, &quote.exact),
        _ => {
            let start = disambiguate(text, &exacts, quote)?;
            Some((start, start + quote.exact.len()))
        }
    }
}

/// Índices de byte de todas as ocorrências (não sobrepostas) de `needle`.
fn find_all(hay: &str, needle: &str) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut start = 0;
    while let Some(rel) = hay[start..].find(needle) {
        let idx = start + rel;
        out.push(idx);
        start = idx + needle.len();
    }
    out
}

/// Escolhe entre múltiplas ocorrências exatas usando prefixo/sufixo.
fn disambiguate(text: &str, matches: &[usize], quote: &QuoteSelector) -> Option<usize> {
    let end_of = |start: usize| start + quote.exact.len();
    let mut candidates = matches.iter().copied().filter(|&start| {
        let prefix_ok = quote
            .prefix
            .as_deref()
            .map(|p| text[..start].ends_with(p))
            .unwrap_or(true);
        let suffix_ok = quote
            .suffix
            .as_deref()
            .map(|s| text[end_of(start)..].starts_with(s))
            .unwrap_or(true);
        prefix_ok && suffix_ok
    });
    candidates.next()
}

/// Busca em que cada corrida de espaço em branco no texto casa com um único
/// espaço no `needle` normalizado. Devolve o intervalo de bytes no texto
/// original. Usada quando o match exato falha (ex.: espaço colapsado no reflow).
fn find_flexible(hay: &str, needle: &str) -> Option<(usize, usize)> {
    let normalized = collapse_ws(needle.trim());
    if normalized.is_empty() {
        return None;
    }
    let needle_chars: Vec<char> = normalized.chars().collect();
    let hay_chars: Vec<(usize, char)> = hay.char_indices().collect();

    for start in 0..hay_chars.len() {
        if let Some(end) = try_match_at(&hay_chars, start, &needle_chars, hay.len()) {
            return Some((hay_chars[start].0, end));
        }
    }
    None
}

/// Tenta casar `needle` (normalizado) a partir de `start` em `hay`. Espaço no
/// needle casa com uma ou mais posições de espaço em branco no hay.
fn try_match_at(
    hay: &[(usize, char)],
    start: usize,
    needle: &[char],
    hay_len: usize,
) -> Option<usize> {
    let mut hi = start;
    let mut ni = 0;
    while ni < needle.len() {
        if needle[ni] == ' ' {
            if hi >= hay.len() || !hay[hi].1.is_whitespace() {
                return None;
            }
            while hi < hay.len() && hay[hi].1.is_whitespace() {
                hi += 1;
            }
            ni += 1;
        } else {
            if hi >= hay.len() || hay[hi].1 != needle[ni] {
                return None;
            }
            hi += 1;
            ni += 1;
        }
    }
    Some(if hi < hay.len() { hay[hi].0 } else { hay_len })
}

/// Colapsa corridas de espaço em branco em um único espaço. Não apara as pontas
/// (quem chama apara o `needle` antes).
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(exact: &str) -> QuoteSelector {
        QuoteSelector {
            exact: exact.to_string(),
            prefix: None,
            suffix: None,
        }
    }

    #[test]
    fn exact_single_match() {
        let text = "Primeiro parágrafo sobre limites.";
        let (s, e) = resolve_quote(text, &q("limites")).unwrap();
        assert_eq!(&text[s..e], "limites");
    }

    #[test]
    fn missing_quote_is_none() {
        let text = "nada aqui";
        assert!(resolve_quote(text, &q("ausente")).is_none());
    }

    #[test]
    fn whitespace_flexible_fallback() {
        let text = "Primeiro parágrafo sobre limites.";
        // O quote pede dois espaços; o texto tem um só — match flexível.
        let (s, e) = resolve_quote(text, &q("parágrafo  sobre")).unwrap();
        assert_eq!(collapse_ws(&text[s..e]), "parágrafo sobre");
    }

    #[test]
    fn disambiguate_by_prefix() {
        let text = "alfa X beta X gama";
        let quote = QuoteSelector {
            exact: "X".to_string(),
            prefix: Some("alfa ".to_string()),
            suffix: None,
        };
        let (s, e) = resolve_quote(text, &quote).unwrap();
        assert_eq!(s, 5); // segundo caractere após "alfa "
        assert_eq!(&text[s..e], "X");
    }

    #[test]
    fn disambiguate_by_suffix() {
        let text = "alfa X beta X gama";
        let quote = QuoteSelector {
            exact: "X".to_string(),
            prefix: None,
            suffix: Some(" gama".to_string()),
        };
        let (s, e) = resolve_quote(text, &quote).unwrap();
        assert_eq!(&text[s..e], "X");
        // Deve ser a segunda ocorrência (a seguida de " gama").
        assert_eq!(&text[..s], "alfa X beta ");
    }
}
