# PLAN.md — Plano de desenvolvimento do learnive

> Documento vivo. Este plano **pode e deve** mudar conforme o desenvolvimento avança — especialmente porque quase todo o risco do projeto é de *calibração* (qualidade de avaliação, fidelidade do perfil, sensibilidade de cross-ref), coisa que só se aprende usando. As referências `§N` apontam para as seções do `README.md` (especificação autoritativa).

## Princípio de faseamento

A ordem não é "um subsistema completo por vez", e sim **loop completo primeiro, profundidade depois**. A Fase 1 exercita a tese central ponta-a-ponta com o mínimo de profundidade; a Fase 2 aprofunda cada subsistema até a qualidade da spec, ainda com **um único documento vivo**; a Fase 3 adiciona o grafo entre múltiplos documentos. Cada fase é usável de verdade ao terminar.

---

## Fase 1 — Loop completo mínimo (fatia vertical)

**Objetivo:** provar a tese central com um caminho ponta-a-ponta funcionando: um tema → nó gerado sob demanda → checagem de compreensão com rubric travado → avaliação dispara o próximo nó. Profundidade mínima em tudo; o que importa é o *ciclo* fechar.

**Fundação mínima (o que o loop exige para existir):**
- [ ] Servidor `axum` bindado só em `127.0.0.1`; token de sessão obrigatório; validação de `Origin`/CORS restritiva; nenhum endpoint mutável em GET (§3.1).
- [ ] Streaming SSE de conteúdo gerado servidor→cliente (§3).
- [ ] **Frontend**: HTMX (troca de HTML/SSE/formulários) + JS vanilla mínimo (seleção, linha de leitura por scroll, streaming token-a-token, UI otimista) + módulo **wasm** de ancoragem compilado do mesmo crate Rust da §4.3; assets embutidos, sem etapa de build de página. Sem framework JS de VDOM (§3).
- [ ] **Sandbox de blocos interativos gerados (§3.1, §4.4)**: HTML/JS gerado por LLM renderiza em `<iframe sandbox>` isolado da origem/token; exercícios interativos devolvem **artefato de resposta estruturado** via `postMessage`, com schema travado junto ao rubric antes da submissão.
- [ ] Provedor de IA: **OpenRouter OAuth como caminho default** (mesmo na Fase 1), com BYOK direto como opção secundária. O onboarding completo (demais provedores, polimento) é Fase 2 (§12).
- [ ] Armazenamento em arquivos: um diretório = um documento vivo, um arquivo HTML por nó (§4, §4.1).
- [ ] **Contrato do nó em duas camadas (§4.3)** — é o formato de dados foundational, definir primeiro: camada de conteúdo imutável com `data-block-id` estável + camada de interação append-only que referencia IDs; ancoragem por ID (fallback fuzzy por quote); linha de leitura é estado efêmero de UI, não persistida. Vocabulário v0 de tags do §4.3.

**Loop:**
- [ ] **Cold start (§6.1)**: tela com a pergunta única "O que vamos aprender?" → o agente decide entre iniciar o documento ou abrir conversa de negociação de escopo até definir o outline.
- [ ] Geração de outline inicial a partir do tema/ideia/problema do usuário (§6).
- [ ] Geração de nó sob demanda a partir do ponto de leitura (§6).
- [ ] Objetivos de aprendizagem gerados **junto** com o conteúdo do nó; rubric travado na criação; nota por objetivo em `{não demonstrado, parcial, demonstrado}`; ao menos um item de transferência não coberto no texto; exercício fundamentado no material original (§8).
- [ ] UI "documento vivo": parágrafos + **conteúdo generativo** (prosa e, quando ensina melhor, visualização interativa) + exercício gerado dinamicamente cuja modalidade não se limita a checkbox/textbox (§4.4, §9); linha de leitura em destaque; seleção de texto + pergunta que **edita o próprio documento** com a resposta (§9).
- [ ] **Remediação na falha (§8.2)**: em vez de avançar/regenerar em silêncio, abrir conversa com o tutor no contexto do exercício (exemplo resolvido / passo a passo) + novo problema similar cuja similaridade ao modelo **cresce a cada falha**; só avança quando o objetivo fica `demonstrado`.
- [ ] **Calibração de nível de abstração (§6.2)**: subir abstração quando o usuário avança sem errar/perguntar; baixar quando trava — por conceito.
- [ ] Perfil mínimo: registrar interações e alimentar o contexto recente do próximo nó (sem compactação de longo prazo ainda) (§7).
- [ ] Disparo automático do próximo nó ao avaliar o exercício, com pausa/redirecionamento pelo usuário (§9).

**Responsividade nesta fase (a Fase 1 tem que ser prazerosa, senão não cumpre seu propósito) (§14):**
- [ ] Streaming token-a-token no documento com foco em **time-to-first-token** (alvo ~1s até estar lendo), não tempo até completar.
- [ ] **Prefetch preditivo** do(s) provável(is) próximo(s) nó(s) sobre o outline enquanto o usuário lê/responde — cost-aware/ajustável (§6).
- [ ] Pipeline dentro do nó: prosa primeiro, exercício + rubric em paralelo (rubric travado antes da submissão, §8).
- [ ] Model tiering básico: modelo leve para exercício/correção/resumo, robusto para prosa (§12.1) — camada de modelo roteada por sub-tarefa e trocável.
- [ ] UI otimista: ação do usuário reflete na hora no documento, sem modal bloqueante.

**Setup de provedor/modelo nesta fase (§12, §12.1):**
- [ ] **OpenRouter OAuth como opção padrão**; BYOK direto como opção.
- [ ] Escolha por **intenção** (gratuito vs pago), não por nome de modelo; pairing recomendado aplicado automaticamente.
- [ ] Degradação graciosa: um só modelo serve os dois tiers (tiering nunca bloqueia começar).
- [ ] Controle de custo básico (§12.2): exibir gasto corrente + um limite simples que estrangula o prefetch antes de pausar a geração.

**Fundamentação de fonte nesta fase (crawl desde o início):**
- [ ] Crawl do **LibGen (livros) + arXiv (artigos)** já no loop, atrás de uma **interface de aquisição trocável** (§11.1). Versão simples — sem preferência de formato refinada nem normalização completa (Fase 2).
- [ ] **Fallback explícito para busca web** quando LibGen/arXiv não produzem fonte; conteúdo web atribuído inline ("segundo o site X ..."), links registrados em `SOURCES.md` (§11, §11.1).
- [ ] Nós citam livro/capítulo ou artigo; acervo imutável, download único reaproveitado (§11).

**Fora de escopo nesta fase:** múltiplos documentos e cross-referência; camada de retrieval/embeddings; cadeia de revisão versionada não-destrutiva (§5); compactação de perfil no longo prazo (§7.1); onboarding completo de provedores (§12); viewer da fonte polido; preferência de formato EPUB>PDF>DJVU e normalização (§11.1).

**Critério de pronto:** um usuário consegue partir de um tema, ler nós gerados a partir de fontes reais (LibGen/arXiv, ou web com atribuição explícita), ser avaliado, receber remediação na falha e ver o currículo avançar/ajustar — tudo sem sair do loop.

---

## Fase 2 — Aplicação completa, documento vivo único

**Objetivo:** levar cada subsistema à qualidade da spec, ainda dentro de **um único documento vivo** (sem grafo entre documentos). É aqui que o produto fica realmente bom; as decisões de calibração aprendidas na Fase 1 orientam a profundidade.

**Provedores de IA (§12, §12.1):**
- [ ] OpenRouter OAuth PKCE como caminho default (conexão em um clique).
- [ ] BYOK direto (Anthropic, OpenAI, OpenCode Zen) com validação imediata da chave e link para geração.
- [ ] Armazenamento de chave no keychain do SO.
- [ ] Pairings recomendados mantidos por provedor/tier + override avançado de modelo; explicação mínima com exemplos no setup.
- [ ] Tier gratuito com tratamento de rate limit (fila/fallback/degradação) sem quebrar a sessão.

**Aprofundamento do HTML generativo interativo (§4.4) — o sandbox+protocolo básico já existe desde a Fase 1:**
- [ ] Elevar qualidade/confiabilidade das visualizações e exercícios interativos gerados (o modo escolhido — JS arbitrário sempre em sandbox — tem variância; medir e endurecer prompts/validação do artefato).
- [ ] Cache/reaproveitamento de widgets e checagem do schema de artefato contra o rubric na geração.

**Aprofundamento de responsividade (§14) — o básico já existe desde a Fase 1:**
- [ ] Prefetch especulativo de múltiplos ramos com política cost-aware refinada; separar geração do esqueleto (previsível) do delta de calibração pós-nota.
- [ ] Tiering afinado por sub-tarefa (medir onde o modelo leve basta vs. onde degrada a qualidade).

**Aprofundamento da fundamentação de fontes (§11, §11.1) — o crawl básico já existe desde a Fase 1:**
- [ ] Seleção de versão robusta: edição mais recente, língua do usuário quando possível (fallback a outra língua).
- [ ] Preferência de formato **EPUB > PDF > DJVU**; normalização de qualquer formato para a representação interna (texto extraído + dialeto HTML).
- [ ] Viewer da fonte **só-leitura**; seleção na fonte roteia trecho citado para o documento vivo (§11).

**Grafo de conceitos versionado (§5):**
- [ ] Revisão não-destrutiva: revisitar conceito gera novo nó de versão; original intacto com anotações ancoradas.
- [ ] Cadeia de versão via front-matter/nome sequencial; referências futuras apontam para a ponta mais recente.

**Motor de currículo completo (§6):**
- [ ] Podar/expandir/reordenar nós conforme a avaliação; negociação de escopo durante a geração; granularidade atômica flexível.

**Motor de avaliação completo (§8, §8.1):**
- [ ] Exercícios de síntese cruzando nós distantes **dentro do documento** (teste de integração).
- [ ] Rubrics para domínios não-determinísticos: teste de Turing ideológico, mapeamento de posição, consistência ao longo do tempo.

**Memória / perfil completo (§7, §7.1):**
- [ ] Log de eventos imutável append-only + perfil como projeção materializada.
- [ ] Memória multi-resolução (recente verbatim → resumos → traços destilados + retenção por conceito).
- [ ] Índice de recuperação derivado/reconstruível (vector store embutido / sqlite) — necessário já aqui porque o perfil em uso por meses exige retrieval (§4, §10).
- [ ] Decaimento e revisão versionada de crenças do perfil (reusa a filosofia da §5).
- [ ] Perfil inspecionável e editável pelo usuário.
- [ ] Comportamento adversarial: construir o contra-argumento mais forte; distinguir discordância legítima de falha de compreensão.
- [ ] Anotações no documento vivo visíveis a usuário e IA.

**Fora de escopo nesta fase:** múltiplos documentos vivos; resumos/links/sidepanel entre documentos; controle de sensibilidade de cross-referência.

**Critério de pronto:** um único documento vivo funciona em toda a profundidade da spec — fontes reais do LibGen, revisão versionada, avaliação rica, perfil que sobrevive a uso prolongado.

---

## Fase 3 — Múltiplos documentos vivos + cross-referência ("segundo cérebro", §10)

**Objetivo:** transformar o conjunto de documentos vivos num grafo que se cross-referencia.

- [ ] Múltiplos documentos vivos como grafo cross-referenciado.
- [ ] Quando um aprendizado depende de conhecimento de outro documento: resumo breve + link, possivelmente abrindo sidepanel que renderiza o conteúdo + notas do usuário daquele outro documento/nó.
- [ ] Escalar a camada de retrieval (o índice da Fase 2) para todo o acervo de documentos.
- [ ] Exercícios de integração cruzando nós de **documentos diferentes** (§8).
- [ ] **Controle de sensibilidade de cross-referência** ajustável pelo usuário ao longo do tempo — a §10 marca isso como o maior risco (calibração, não técnico): acionar demais vira ruído, de menos perde valor.

**Critério de pronto:** o usuário navega entre documentos vivos com referências cruzadas úteis e ajustáveis, sem ruído.

---

## Riscos transversais (valem para todas as fases)

- **Qualidade da avaliação é a pedra angular** — todo o motor adaptativo adapta em cima do sinal "entendeu?". Sinal fraco = adaptação em ruído. Rubric travado na geração (§8) é a mitigação; ainda assim é a dependência mais arriscada.
- **Custo/latência dos nós atômicos** — ciclo curto multiplica chamadas de LLM. BYOK joga o custo pro usuário, mas a latência por nó é UX. Monitorar a tensão atômico ↔ número de rodadas.
- **Fidelidade do grounding** — extração de texto varia por formato; o LLM ainda pode se afastar da fonte. Citação + fonte visível auditam, não previnem.
- **Calibração da compactação de perfil** (§7.1) — decidir o que esquecer é julgamento, não algoritmo fechado. Log imutável torna erros recuperáveis; perfil inspecionável torna-os corrigíveis.
