# S27g — Medições de casamento capítulo↔sumário

**Status:** documento vivo de medição. Criado 2026-08-30, na branch
`s27g-chapter-matching-bench`. Toda conclusão aqui vem do harness
`crates/learnive/src/source/toc_bench.rs` (test-only, `#[cfg(test)]`, nunca
entra no binário) rodando contra o acervo real em `learnive-data/library/`.

**Por que existe.** A primeira entrega do S27g (proposta de capítulo por
`{number, name}`, commit `ecc2488`) foi validada ao vivo com `n=3` num único
livro. O usuário rejeitou tirar conclusão dali — *"I think the whole problem
is that our sample size is too small. 3 cases in 1 book is too little"* — e
mandou montar acervo e medir. Este arquivo é o resultado, e a entrada para
decidir a política de falha total (o "caso 5" da cascata, ainda em aberto).

**Provider das medições ao vivo:** o configurado no `.env` — Groq,
`openai/gpt-oss-120b` no tier robusto (que é quem `propose_outline` usa).
Instrução explícita do usuário: *"it'll be the smallest model we'll support
as the robust model"*. Ou seja, tudo aqui é **piso**, não caso médio.

---

## 1. O acervo

Seis PDFs, escolhidos por eixo de falha e não por assunto (a lista de eixos
original está no histórico da conversa; a ideia era que cada livro cobrisse
algo que os outros não cobrem).

| Livro | Bookmarks | Profundidade | Numerados | Sub-numerados (`N.M`) |
|---|---|---|---|---|
| Think Python 1e (2012) | 270 | 2 | 19 | 0 |
| Think Python 2e (2015) | **0** | — | — | — |
| K&R, *The C Programming Language* (1978) | **0** | — | — | — |
| SICP | 138 | 3 | **0** | 0 |
| Pro Git (EN) | 519 | **4** | 10 | 0 |
| Stewart, *Calculus ET* | 26 | 0 (plano) | 17 | 0 |

### 1.1 Achados estruturais (locais, sem gastar API)

**(a) Sub-numeração não existe em bookmark embutido: 0 de 953 entradas.**
Nenhum livro do acervo carrega `4.10` num bookmark. O contra-exemplo K&R
§4.10 — que motivou o schema `{number, name}` inteiro e está hardcoded nos
few-shots do prompt e num teste unitário — **não é servível pelo caminho de
bookmark**, em nenhum livro. Só existe no caminho de dedução do S27k (LLM lê
a página de sumário impressa).

**(b) Número é raro: 46/953 ≈ 4,8%.** O tier "número E nome" que o usuário
propôs dispara em ~1 entrada a cada 21. Não é inútil (ver §2), mas **não pode
ser o tier primário** — o casamento por nome é que carrega a carga.

**(c) Dois de seis livros não têm sumário embutido nenhum.** Think Python 2e
e K&R. Isso reclassifica a cascata de dedução do S27k de "reserva pro PDF
ruim ocasional" para **carga principal de ~33% de um acervo real** — e é
justamente o caminho ainda **sem nenhuma medição**. Pior: os dois livros sem
bookmark são exatamente os que têm numeração `N.M` na página impressa, ou
seja, o único caminho capaz de entregar sub-seção é o único não medido.

**(d) Subseção É alcançável, só nunca por número.** SICP tem profundidade 3 e
Pro Git 4 — a hierarquia capítulo→seção→subseção está lá, em prosa. "Tree
Recursion" (SICP §1.2.2) é um bookmark de verdade. Então o caso estilo K&R é
resolvível **por nome**, nunca por número, no caminho de bookmark.

### 1.2 Cada livro tem seu próprio dialeto de sujeira

Achado transversal, e o mais prático de todos: normalização tem que ser
defensiva por padrão, porque cada PDF quebra de um jeito diferente.

- **Think Python 1e** — espaços **não-quebráveis** dentro da numeração
  (`"Chapter\u{a0}1.\u{a0}The Way of the Program"`). Derrotou silenciosamente
  a primeira versão do splitter nas 270 entradas; só apareceu porque o
  contador de "numeradas" deu 0/270 e isso era obviamente errado.
- **Stewart** — `\r` no fim de todo título, e espaço sobrando
  (`"4 - Applications of differentiation \r"`). Além de um **erro de digitação
  do próprio livro** no bookmark: `"Differentiaton rules"`.
- **K&R** — OCR ruidoso (`:ROGRAMMING LANGUAGE`, `UXIX`, `self-coniained`,
  `compulsiofl tcr`) e zero bookmarks. É um scan de 1978.
- **Pro Git** — a primeira aquisição era a **tradução chinesa** apesar do
  nome de arquivo em inglês; trocada depois. Colisão real de título: `"Git
  Basics"` aparece duas vezes (subseção do cap. 1 na p8, e título do cap. 2
  na p17).
- **Genérico repetido** — `"Summary"` (Pro Git) e `"Debugging"` (Think Python)
  aparecem em quase todo capítulo. É a armadilha de contenção por título curto
  que já tinha sido corrigida com "vence o mais longo" antes de qualquer
  medição.

---

## 2. Medição ao vivo — casamento de capítulo proposto

Metodologia: uma chamada real de `engine::propose_outline` por probe (tópico +
objetivo escolhidos pra que o livro-alvo apareça na lista de leitura); cada
filho `Chapter` proposto é casado contra o sumário real do livro por
`source::match_chapter`, em duas variantes:

- **como está em produção** — `ConfirmedTocEntry.number` é sempre `None` no
  caminho de bookmark (nada popula), título cru;
- **com a correção de split** — número impresso separado do título.

Correção semântica de cada casamento foi conferida **à mão**; taxa bruta de
casamento sozinha engana (um casamento errado é pior que nenhum, porque
fundamenta o nó no capítulo errado em silêncio).

### 2.1 Rodada de 5 livros (2026-08-30)

11 propostas pontuadas.

| Livro | Proposto | Produção | Com split | Correto? |
|---|---|---|---|---|
| SICP | `1.2` Procedures as Parameters | — | — | erro honesto (título real: "Procedures as **Arguments**") |
| SICP | `2.1` Data Abstraction | ✅ Intro to Data Abstraction | ✅ idem | ✅ |
| SICP | `4.1` The Metacircular Evaluator | ✅ exato | ✅ exato | ✅ |
| Pro Git (zh) | `2` Git Basics | — | ✅ Git 基础 p41 | ✅ |
| Pro Git (zh) | `3` Git Branching | ❌ "git branch" p511 (entrada de índice) | ✅ Git 分支 p75 | ✅ |
| Pro Git (zh) | `9` Git Internals | — | ❌ Git 与其他系统 | ✗ número do modelo errado |
| Stewart | `2` Limits and Continuity | — | ✅ Limits and derivatives | ✅ |
| Stewart | `3` Differentiation Rules | — | ✅ Differentiaton rules | ✅ |
| Stewart | `4` Applications of Differentiation | ✅ | ✅ | ✅ |
| Stewart | `5` Integration | ❌ "8 - Further applications" | ✅ Integrals p382 | ✅ |
| Stewart | `6` Fundamental Theorem of Calculus | — | ❌ Applications of integration | ✗ número do modelo errado |

|  | cobertura | precisão |
|---|---|---|
| Como está em produção | 3/11 (27%) | 3/5 (60%) |
| **Com split de número** | **8/11 (73%)** | **8/10 (80%)** |

**A correção de split melhora as duas métricas.** A preocupação registrada
antes de medir — de que ela só adicionaria casamentos errados — estava errada:
a precisão SOBE, porque o tier de número dispara primeiro e impede casamentos
ruins por contenção de nome (`"Integration"` → `"8 - Further applications of
integration"` era um erro confiante que a correção elimina).

### 2.2 Os dois erros residuais têm a mesma causa

Nenhum dos dois é falha do matcher: **é o modelo afirmando um número errado, e
o matcher obedecendo.**

- Stewart `6` "Fundamental Theorem of Calculus" → "Applications of
  integration" (FTC está no cap. 5; cap. 6 é outra coisa).
- Pro Git `9` "Git Internals" → "Git and Other Systems" (Internals é o cap.
  10).

Nos dois, **o nome discordava alto do capítulo casado** e a gente aceitou
assim mesmo. Isso valida a intuição do usuário de exigir que número E nome
concordem — e mostra onde o casamento difuso realmente entra: **não como tier
de reserva, mas como veto sobre o casamento por número.** O número propõe, o
nome confirma ou veta.

Exigir concordância *estrita* seria duro demais: rejeitaria "Limits and
Continuity"→"Limits and derivatives" e "Integration"→"Integrals", que estão
CERTOS. Daí o veto precisar ser difuso, e precisar de distância por token
(`"Integration"`/`"Integrals"`, `"Differentiation"`/`"Differentiaton"` — o
typo do próprio livro). `strsim` 0.11.1 já está no `Cargo.lock` transitivamente,
então o primitivo sai de graça; a agregação (cobertura assimétrica de token) é
que não existe pronta.

### 2.3 A variável dominante é granularidade, não o matcher

Mesmo livro, mesmo matcher, duas rodadas:

- rodada 1 — modelo propôs **subseções** (3.1, 3.6, 3.8, 4.2, 4.3) contra o
  sumário só-de-capítulo do Stewart → **1/5**
- rodada 2 — modelo propôs **capítulos** (2, 3, 4, 5, 6) → **4/5**

Nada mudou além da granularidade que o modelo escolheu sozinho, e ele é
**não-determinístico** nisso. SICP fez 2/3 justamente porque a profundidade 3
comporta proposta de subseção.

**Conclusão de maior alavancagem, e ela não está no matcher:** dar o sumário
real ao modelo e deixar ele **escolher**, em vez de pedir que ele **lembre**.
Isso dissolve granularidade, número errado e paráfrase de uma vez — é o mesmo
movimento "seleção, não recordação" que já estava desenhado como tier 4 de
último recurso, promovido a mecanismo primário.

### 2.4 Achados sobre o provider-piso

- **Think Python falhou o parse de `propose_outline` em 2 de 2 rodadas** —
  `Parse("could not read outline tree")`. O modelo-piso erra o schema em
  ~1 probe a cada 5. Bate com a nota já registrada no `.env` sobre taxa de
  falha de parse do gpt-oss no outline. Importante: **não tem nada a ver com o
  livro** — `propose_outline` nem abre o PDF, só recebe tópico + objetivo.
- **A lista de leitura propõe livros que o usuário não tem** — 3 obras não
  possuídas em 5 probes ("Expert C Programming", "C Programming: A Modern
  Approach", "The Little Schemer").

### 2.5 Rodada B — 6 livros, Pro Git em inglês (2026-08-30)

8 propostas pontuadas. Acervo corrigido: Think Python 1e restaurada, Pro Git
substituído pela edição em inglês.

| Livro | Proposto | Produção | Com split | Correto? |
|---|---|---|---|---|
| SICP | `—` Higher-order procedures | ✅ Formulating Abstractions with Higher-Order Procedures p88 | ✅ idem | ✅ |
| SICP | `—` Data abstraction | ✅ Introduction to Data Abstraction p119 | ✅ idem | ✅ |
| SICP | `—` Metalinguistic abstraction: building an interpreter | ✅ Metalinguistic Abstraction p443 | ✅ idem | ✅ |
| Pro Git | `3` Branching and Merging (including rebasing) | ❌ "Branching and Merging" p407 | ✅ Git Branching p44 | ✅ (só com split) |
| Stewart | `3` Derivatives | ❌ "2 - Limits and derivatives" | ✅ Differentiaton rules p200 | ✅ (só com split) |
| Stewart | `4` Applications of Differentiation (Related Rates and Optimization) | — | ✅ Applications of differentiation p298 | ✅ |
| Stewart | `5` Integrals | ❌ **"15 - Multiple integrals" p978** | ✅ Integrals p382 | ✅ (só com split) |
| Stewart | `6` Fundamental Theorem of Calculus | — | ❌ Applications of integration | ✗ número do modelo errado (**reproduziu** da rodada A) |

|  | cobertura | precisão |
|---|---|---|
| Como está em produção | 3/8 (38%) | 3/6 (50%) |
| **Com split de número** | **7/8 (88%)** | **7/8 (88%)** |

**SICP fez 3/3 com ZERO números.** O modelo propôs só nome (`chapter_number:
None`) para SICP — porque o livro não numera —, e o casamento por nome puro
acertou os três. É o melhor resultado do acervo inteiro, e vem do caminho que
NÃO usa número nenhum. Confirma §1.1(d): quando o título é prosa distintiva, o
nome sozinho basta, inclusive em profundidade 3.

**O erro do Stewart `6` (FTC) reproduziu exatamente** entre as rodadas A e B —
mesma proposta, mesmo casamento errado. Não é ruído: é um modo de falha
estável do modelo-piso (afirma que o Teorema Fundamental é o capítulo 6; está
no 5).

### 2.6 Agregado A+B — 19 propostas pontuadas

|  | cobertura | precisão |
|---|---|---|
| Como está em produção | 6/19 (32%) | 6/11 (55%) |
| **Com split de número** | **15/19 (79%)** | **15/18 (83%)** |

A correção de split é a mudança de maior efeito medida até aqui, e melhora
cobertura e precisão simultaneamente nas duas rodadas independentes.

### 2.7 Correção: "vence o mais longo" está errado na direção oposta

Achado novo e desconfortável da rodada B, contra código **já commitado**
(`ecc2488`): o desempate por título mais longo que eu adicionei ao
`match_chapter` **causou** um casamento errado.

Stewart `5` "Integrals", variante de produção (sem número):

- `needle` = `"integrals"`
- candidatos por contenção: `"5 integrals"` (11 chars) e
  `"15 multiple integrals"` (21 chars)
- "vence o mais longo" escolhe **`"15 - Multiple integrals"`, p978** — errado
  por 596 páginas.

O desempate foi introduzido para o caso oposto (`needle` longo, `hay` curto e
genérico: `"Functions"` engolindo `"Functions and Program Structure"`). Os dois
casos existem de verdade no acervo, e **nenhuma regra de comprimento resolve os
dois** — a regra certa não é "mais longo" nem "mais curto", é **mais parecido**:
minimizar a diferença entre `hay` e `needle`, não maximizar `hay`.

Isso reforça a §2.2 por outro caminho: contenção + desempate por comprimento
deve ser **substituída** por um escore de similaridade de verdade, não
remendada de novo. Note que o split de número mascarou esse bug em toda a
rodada B (o tier de número dispara antes), mas ele volta a morder em qualquer
livro sem numeração — exatamente a maioria do acervo (§1.1b).

### 2.8 O atrito dominante não é casamento, é o acervo

- **9 obras propostas que o usuário não possui**, em 6 probes.
- No probe do Think Python 1e, o modelo **não propôs nenhuma das duas edições**
  que estão no acervo — propôs "Python Programming: An Introduction to Computer
  Science" e "Automate the Boring Stuff". Zero propostas pontuáveis.

Ou seja: antes de o casamento capítulo↔sumário sequer entrar em cena, o passo
anterior já erra o livro com frequência alta. Isso é dado direto para o
"caso 5b" (capítulo que não existe no livro): a causa mais comum não vai ser
alucinação de capítulo dentro de um livro certo, e sim a lista de leitura
apontando para um livro que o acervo não tem.

---

## 3. O que ainda não foi medido

1. **A cascata de dedução do S27k** — caminho de ~33% do acervo (Think Python
   2e, K&R), zero medição. É onde o ruído de OCR do K&R vai doer de verdade, e
   é o único caminho capaz de entregar número de subseção.
2. **O veto difuso da §2.2** — desenhado a partir dos dados, ainda não
   implementado nem medido.
3. **"Seleção, não recordação" (§2.3)** — a mudança de maior alavancagem,
   ainda não construída.
4. **Política de falha total (caso 5)** — segue em aberto por decisão
   explícita: o usuário rejeitou tanto substituir subseção pelo livro inteiro
   quanto regerar a lista, e apontou que um capítulo alucinado tem um *papel*
   na lista que a remediação local não conserta. A proposta atual (dividir em
   5a "real mas não localizado" → estreitar por retrieval; 5b "não existe no
   livro" → re-planejamento local de um item só) depende de saber a
   distribuição de falha, que é o que este documento está juntando.

---

## 4. Como reproduzir

```
# grátis, local, sem chamada de API
cargo test -p learnive --bin learnive \
  source::toc_bench::tests::toc_shape_of_every_library_book -- --ignored --nocapture

# gasta orçamento real de API: uma chamada de propose_outline por probe
cargo test -p learnive --bin learnive \
  source::toc_bench::tests::live_match_rate_across_the_library -- --ignored --nocapture
```
