# learnive — Especificação da Plataforma de Aprendizado Explorativo Autodirigido

## 1. Visão geral

Aplicação de aprendizado autodirigido que gera, para cada tema/ideia/problema que o usuário quer explorar, um currículo adaptativo em formato de "livro generativo". O sistema não entrega um currículo fixo: constrói o material progressivamente, avalia a compreensão real do usuário a cada etapa, e usa essa avaliação para decidir o que vem a seguir — incluindo revisitar e revisar conceitos já ensinados.

## 2. Público-alvo e princípio de design central

- Usuário-alvo primário: um polímata buscando um currículo de aprendizado holístico que ele mesmo controla.
- Princípio de design: **não** é um sistema desenhado "para usuários sofisticados" — é um sistema que deve ser **tão sofisticado quanto o usuário for**. A mesma necessidade de calibração de ritmo e uso de interesses pessoais para tornar o aprendizado prazeroso se aplica igualmente a alguém superdotado ou a alguém com dificuldade de aprendizado. A calibração é contínua e local (por conceito/objetivo), não um nível geral fixado uma vez.

## 3. Arquitetura geral

- **Linguagem**: Rust — facilita compilação de binários para múltiplos sistemas operacionais.
- **Topologia**: backend Rust rodando localmente como servidor HTTP; frontend renderizado no navegador do próprio usuário (não é um webview embutido tipo Tauri/Electron). O backend faz toda leitura/escrita de arquivo — o navegador nunca acessa o filesystem diretamente, só fala com o backend via rede local.
- **Streaming**: Server-Sent Events (SSE) para empurrar conteúdo gerado incrementalmente pro documento em tela — o fluxo principal é servidor→cliente. Ações do usuário (selecionar texto, perguntar, responder exercício) são requisições HTTP normais.
- **Framework HTTP**: `axum` (suporte nativo a SSE).

### 3.1 Segurança do servidor local

O servidor guarda as chaves de API do usuário e roda como HTTP acessível por qualquer aba do navegador — precisa de proteção contra CSRF/DNS rebinding:

- Bind exclusivamente em `127.0.0.1`, nunca `0.0.0.0`.
- Token de sessão obrigatório em toda requisição (mesmo padrão usado pelo Jupyter: token na URL/cookie).
- Validação restritiva de `Origin`/CORS — nunca `Access-Control-Allow-Origin: *`.
- Nenhum endpoint que mude estado responde a GET (previne CSRF via tag de imagem/link simples).

## 4. Formato de armazenamento

- Estrutura de diretórios e arquivos legível por humano — sem banco de dados binário proprietário — no espírito de como o Obsidian usa arquivos markdown para o "segundo cérebro".
- **HTML** para os documentos vivos (conteúdo gerado + anotações do usuário).
- **PDF** para os livros-fonte usados como referência (material original, imutável).
- Diretório/subdiretório serve apenas para navegação humana solta. As relações reais do grafo (pré-requisito, referência cruzada) são expressas como links dentro do conteúdo, nunca implícitas pela posição no filesystem — a mesma lógica do Obsidian: pasta não carrega o grafo, os `[[links]]` dentro dos arquivos carregam.
- **Arquivos são a fonte da verdade; índices são artefatos derivados.** A recuperação em escala (§10) e o perfil no longo prazo (§7.1) exigem um índice binário (vector store embutido / sqlite) para performance — isso não contradiz o "sem banco de dados proprietário". O índice é sempre um **cache reconstruível** a partir dos arquivos; apagá-lo nunca perde dados, só força reindexação. O espírito Obsidian é preservado porque o dado canônico continua em arquivos legíveis.

### 4.1 Granularidade de arquivo

Um arquivo HTML por nó de conceito, dentro de um diretório por documento vivo. Mapeia diretamente no modelo de grafo versionado (seção 5). Cadeia de versão expressa via nome de arquivo sequencial ou front-matter apontando pro nó anterior/seguinte.

### 4.2 Dialeto HTML próprio da aplicação

O HTML gerado segue uma convenção própria (tags/atributos semânticos), não é HTML solto — necessário para: ID de nó, ponteiro de cadeia de versão, tag de objetivo de aprendizagem associado a um trecho, span de anotação do usuário, marcador de citação para livro/capítulo-fonte. Parsing em Rust via `html5ever`/`scraper`/`lol_html`.

### 4.3 Modelo de camadas do nó e ancoragem

Resolve a tensão aparente entre §5 (nó imutável) e §9 (a IA "edita o documento", usuário anota): um nó é um arquivo HTML com **duas camadas lógicas**.

- **Camada de conteúdo — imutável após a criação.** A prosa gerada, os objetivos de aprendizagem em trechos, o exercício/formulário e as citações de fonte. Congelada no momento da criação do nó (§5). Todo bloco endereçável carrega um **ID estável e único** atribuído na geração (ex.: `data-block-id`). Esse ID é o alvo de ancoragem.
- **Camada de interação — append-only.** Tudo que se acumula depois: anotações do usuário, a conversa de remediação (§8.2), threads de Q&A que o tutor escreve. São elementos que **referenciam** IDs da camada de conteúdo, nunca a alteram. Append-only = novos elementos são adicionados, os existentes não são reescritos (preserva a trajetória, §7.1).

**Ancoragem:**
- Primária: por **ID de bloco estável** — sobrevive a streaming, regeneração e reflow, porque a camada de conteúdo é congelada, então o ID é permanente.
- Sub-bloco (um trecho dentro de um parágrafo): âncora **fuzzy por citação** — quote exato + contexto de prefixo/sufixo (estilo W3C Web Annotation / hypothes.is), resolvido contra o texto congelado do bloco. Como o bloco é imutável, resolve deterministicamente; o fuzzy é só robustez contra normalização mínima.

**Cadeia de versão (§5):** revisar um conceito gera um novo arquivo de nó cujo front-matter/atributo aponta para o ID da versão anterior. O arquivo antigo (ambas as camadas) permanece intacto — as anotações continuam ancoradas ao conteúdo congelado sobre o qual foram feitas. Referências futuras resolvem para a ponta da cadeia.

**Vocabulário v0 (ilustrativo, a refinar na implementação):**
- Raiz do nó: `<article data-node-id data-doc-id data-prev-version>`
- Bloco de conteúdo: qualquer elemento com `data-block-id`
- Objetivo de aprendizagem: `<span data-objective-id data-objective-type="knowledge|application|synthesis">` envolvendo o trecho que um item de rubric mira
- Citação: `<cite data-source-id data-locator="chap:3;p:42">`; para fonte web (§11.1), `data-source-url`
- Exercício: `<form data-exercise-id data-rubric-id>` com campos gerados
- Camada de interação: `<aside data-annotation-id data-anchor-block data-anchor-quote>` (anotação); `<div data-thread-id data-anchor-block>` (Q&A/remediação)
- **A linha de leitura em destaque (§9) NÃO é persistida** — é estado efêmero de UI (posição de scroll), vive só no cliente, não entra no arquivo do nó.

## 5. Modelo de dados: grafo de nós de conceito versionados

- O currículo é internamente um **grafo**: nós de conceito com arestas de pré-requisito/relação entre eles.
- A leitura apresentada ao usuário é uma **linearização** (travessia) do grafo naquele momento — pode ser replanejada conforme o grafo muda, sem quebrar a sensação de "livro sendo lido".
- **Sem edição destrutiva**: quando o usuário revisita um conceito antigo com uma dúvida nova (informada por aprendizados posteriores), o sistema gera um **novo nó de versão** na cadeia daquele conceito. O nó original permanece intacto, com as anotações do usuário ainda ancoradas corretamente nele. A partir daquele momento, referências futuras àquele conceito apontam para a ponta mais recente da cadeia.
- Isso preserva o histórico de trajetória pedagógica (seção 7) e evita o problema de anotações ancoradas em texto que muda por baixo delas.
- Fora desse mecanismo de revisão, o sistema se move **só para frente** — geração incremental a partir do ponto de leitura atual, nunca reescrita retroativa in-place.

## 6. Motor de currículo

- **Geração de outline**: um mapa hierárquico/grafo provisório é gerado no início (a partir do tema/ideia/problema inicial do usuário), servindo de esqueleto. Cada nó real de conteúdo é gerado só quando chega a vez de ser lido, usando o mapa como guia — podendo podar, expandir ou reordenar nós conforme a avaliação (seção 8) revela o que o usuário já sabe ou não retém.
- **Negociação de escopo**: acontece não só na geração inicial do outline, mas ocasionalmente durante a geração de nós futuros também.
- **Granularidade de geração flexível**: não é fixada em formato de "capítulo". Conceitos simples podem ocupar um nó só; conceitos complexos que não se dividem bem em unidades grandes são decompostos em blocos menores e mais atômicos sempre que possível. O objetivo é manter o ciclo aprendizado → feedback o mais curto possível: cada nó, do tamanho que for, termina com uma checagem de compreensão, em vez de acumular vários conceitos antes de testar.

### 6.1 Cold start / entrada de tema

A tela inicial é **uma única pergunta genérica** ("O que vamos aprender?") e uma caixa de texto. A partir do que o usuário digita, o agente decide adaptativamente entre dois caminhos:

- **Documento vivo começa direto**: quando o tema já é claro/delimitado o bastante para gerar um outline.
- **Sessão de conversa com o tutor**: quando ainda é vago/amplo, uma conversa de negociação de escopo (§6) roda até se definir o ponto de partida e o outline do documento.

A escolha entre os dois é feita pelo próprio agente a partir do input — é a frente leve da mesma maquinaria de negociação de escopo que a §6 já descreve.

### 6.2 Calibração de nível de abstração

Mecanismo concreto do princípio "sofisticação acompanha o usuário" (§2). É contínuo e local (por conceito/objetivo), nunca um nível global fixado uma vez.

- **Sinal**: taxa de erros + perguntas por conceito. Avançar **sem errar e sem perguntar** ⇒ o nível de abstração está baixo demais.
- **Resposta quando está baixo demais**: linguagem mais rica de significado; conteúdo explicado de forma mais superficial (menos hand-holding); exercícios que exigem compreensão maior — tanto atômica (mais fundo no conceito isolado) quanto de integração (§8, síntese cruzando nós).
- **Inverso quando o usuário trava**: abstração menor, explicação mais detalhada/explícita, exercícios mais escafoldados.
- A calibração vive no perfil (§7) e parametriza a geração (§6).

## 7. Sistema de memória / perfil do usuário

Todas as interações do usuário — perguntas feitas, respostas a exercícios, pedidos de alteração de currículo, anotações no documento — alimentam um perfil de trajetória pedagógica cumulativo, usado para:

- Antecipar, na geração de nós futuros, o tipo de pergunta que aquele usuário costuma fazer.
- Mensurar que tipo de explicação/exercício gera mais aprendizado para aquele usuário especificamente.
- Identificar como o usuário pensa e **explicitamente confrontá-lo** com sua base filosófica/premissas e as limitações delas — de forma adversarial: o sistema constrói o contra-argumento mais forte contra a posição do usuário, não apenas a descreve de forma genérica ou complacente (LLMs tendem a bajular/espelhar em vez de desafiar; esse comportamento deve ser ativamente evitado).
- Reconhecer o que gera mais interesse no usuário.
- Distinguir explicitamente **discordância legítima** de **falha de compreensão**: se o usuário demonstra entender o mecanismo de um conceito mas diverge dele, isso não é tratado como objetivo não atingido — vira uma posição registrada no perfil, e pode alimentar um nó futuro que a confronta dialeticamente.
- **Seleção de texto + pergunta é o sinal mais importante do sistema**: é curiosidade/confusão explícita e localizada, mais informativa que qualquer inferência implícita. Influencia diretamente a direção do próximo nó gerado — nunca fica registrada apenas como conversa lateral desconectada do currículo.

### 7.1 Arquitetura de memória no longo prazo (meses/anos)

O perfil não é o replay de todas as interações — isso é insustentável e desnecessário. Separação em duas camadas, no espírito de event sourcing / materialized view:

- **Log de eventos imutável, append-only** (perguntas, respostas, seleções, pedidos de alteração, anotações) = fonte da verdade em disco. Nunca é carregado inteiro no contexto. É o que permite recuperar uma nuance que uma compactação anterior descartou.
- **Perfil = projeção materializada** desse log: compacto, curado, é o que efetivamente entra na geração de nó. Um *modelo* do usuário mantido incrementalmente, não o histórico bruto.

Sobre essa base:

- **Memória multi-resolução**: interações recentes em alta fidelidade (verbatim); médio prazo em resumos por sessão/tópico; longo prazo em traços destilados estáveis + estado de retenção por conceito (análogo a spaced-repetition). Um nó gerado meses depois consome a conclusão destilada, não o exercício verbatim.
- **Recuperação, não contexto inteiro**: recupera-se a fatia do perfil relevante ao tópico do nó atual (mesma camada de retrieval da §10), não o perfil todo.
- **Estruturado onde dá, prosa só onde precisa**: retenção por conceito, posições/discordâncias registradas (§7, §8.1) e tags de interesse são dados estruturados e consultáveis; só o "como esse usuário pensa" é prosa mantida por LLM — curta e re-sintetizada periodicamente a partir dos sinais estruturados + eventos recentes.
- **Decaimento e revisão, não só acumulação**: crenças do perfil têm timestamp e podem se tornar falsas com o tempo (o usuário muda ao longo de anos). O perfil revisa/depreca crenças reaproveitando a filosofia da §5 — sem edição destrutiva: uma crença revisada vira nova versão na cadeia, a antiga permanece no histórico. A qualidade da feature adversarial (confrontar as premissas do usuário) é limitada pela fidelidade do perfil; mirar um espantalho do usuário é o pior modo de falha.
- **Perfil inspecionável e editável pelo usuário**: como tudo já é visível ao usuário e à IA (§9), o modelo que o sistema tem do usuário é exposto e corrigível por ele. Humano no loop é a mitigação mais barata para drift e compactação errada — e coerente com a tese de que o usuário controla o próprio currículo.

O gargalo residual aqui não é de escala/armazenamento (resolvido pelas técnicas acima), mas de **calibração da compactação lossy**: decidir o que destilar/esquecer é julgamento, não algoritmo fechado. O log imutável garante que erros de compactação sejam recuperáveis; o perfil inspecionável garante que sejam corrigíveis.

## 8. Motor de avaliação

- **Objetivos de aprendizagem gerados junto com o conteúdo do nó**, não depois — o rubric de avaliação é travado no momento da criação do nó, evitando o viés de leniência de LLM-como-avaliador (LLM tende a validar respostas rasas como corretas quando julga sem critério pré-definido).
- Todo objetivo do tipo "aplicação" tem ao menos um item que exige transferência para um cenário **não coberto no texto do nó** — esse é o teste que generaliza para qualquer domínio, porque não é satisfazível por memorização/reconhecimento de padrão.
- **Metáfora unitário/integração**: objetivo de nó = teste unitário (conceito isolado foi entendido?). Exercício ocasional de síntese cruzando nós distantes no grafo = teste de integração (o usuário consegue conectar aprendizados de contextos diferentes numa aplicação nova?).
- **Grounding dos exercícios no material original**: o exercício e sua solução são fundamentados na mesma fonte (livro/capítulo) que embasa o nó, tornando o rubric mais objetivo e reduzindo a leniência. Funciona bem em exatas; em áreas menos determinísticas o grounding é mais frouxo e o peso recai sobre os rubrics da §8.1 — limitação reconhecida, não resolvida.
- **Nota estruturada por objetivo**: cada objetivo é avaliado em `{não demonstrado, parcial, demonstrado}`, não só passa/falha. Avançar exige todos os objetivos demonstrados; qualquer um não demonstrado dispara a remediação (§8.2). A nota alimenta o estado de retenção por conceito (§7.1).

### 8.1 Avaliação em domínios não-determinísticos (filosofia, ética, etc.)

- **Teste de Turing ideológico**: o usuário articula a posição contrária à sua da forma mais forte possível; o rubric avalia se a articulação seria reconhecida como justa por um proponente real daquela posição — testa compreensão genuína separada de concordância.
- **Mapeamento de posição contra o território conhecido**: o rubric define o espaço de posições defensáveis num debate (não uma resposta certa única) e avalia se o usuário reconhece onde se situa nesse espaço e por quê.
- **Consistência ao longo do tempo** como análogo do teste de integração: checa se a posição do usuário permanece coerente quando reexaminada de ângulos diferentes em momentos diferentes.

### 8.2 Remediação na falha

Quando o usuário falha uma checagem de compreensão, o sistema **não** apenas avança nem regenera em silêncio: o nó entra numa **sessão de conversa com o tutor**, na camada de interação do nó (§4.3), no contexto do exercício falhado.

- O tutor explica o conceito testado **no contexto do exercício**: dá exemplos de como usar o conceito para resolver um problema similar, ou mostra o passo a passo de resolver exatamente o problema que o usuário errou.
- Em seguida propõe um **novo problema similar**. A cada falha subsequente, o novo problema fica **cada vez mais similar** ao exemplo resolvido — convergindo em direção à solução demonstrada — até o usuário conseguir resolver um caso quase idêntico; depois a dificuldade volta a subir. (Scaffolding por proximidade crescente ao modelo, retirado gradualmente conforme o usuário acerta.)
- A conversa é sinal de alto valor para o perfil (§7): qual explicação/exemplo finalmente "pegou" alimenta o "que explicação funciona para esse usuário".
- É **append-only** no nó (§4.3): o histórico da dificuldade fica preservado na trajetória (§7.1).
- Só quando o objetivo passa a **demonstrado** (§8) o disparo do próximo nó acontece (§9).

## 9. Interface — "documento vivo"

- Parágrafos explicando um conceito, seguidos de uma pergunta e um chatbox/formulário gerado dinamicamente (HTML generativo — a modalidade do exercício varia por conteúdo/domínio, decidida na própria geração, não fixada pelo sistema).
- Uma linha central em destaque (highlight) acompanha a posição de leitura atual do usuário (baseada em posição de scroll, não eye-tracking).
- O usuário pode, a qualquer momento: selecionar um bloco de texto e dizer algo, ou dizer algo sem seleção (contexto = a linha em destaque).
- A resposta da IA **edita o próprio documento** para conter a resposta — não aparece em um widget separado.
- O documento funciona simultaneamente como material de referência e notas pessoais: o usuário pode fazer anotações/marcações diretamente nele. É o **único** local de notas do usuário — a fonte original (§11) é só-leitura; qualquer coisa que o usuário queira registrar da fonte é trazida para o documento vivo.
- Anotações e marcações são visíveis tanto para o usuário quanto para a IA tutora que avalia e gera texto.
- **Disparo de geração do próximo nó**: automático assim que o exercício é avaliado, mas o usuário pode pausar ou redirecionar o currículo a qualquer momento.

## 10. Segundo cérebro — grafo entre documentos vivos

- O usuário tem múltiplos documentos vivos, funcionando como um grafo que se cross-referencia.
- Quando um aprendizado depende de conhecimento/habilidade já aprendida em outro documento, o sistema mostra um breve resumo com link — possivelmente abrindo em um sidepanel que renderiza o conteúdo + as notas do usuário daquele outro documento/nó.
- Acima de um certo volume acumulado, o conjunto de documentos não cabe mais inteiro em nenhuma janela de contexto de modelo — é necessária uma camada de recuperação (embeddings + índice) para decidir o que trazer como contexto a cada momento. A tecnologia específica (vector store, modelo de embedding, chunking de PDF) fica a critério da implementação. Esse índice é um artefato derivado reconstruível a partir dos arquivos (§4), não fonte da verdade — e é a mesma camada de retrieval reaproveitada pela memória de perfil (§7.1).
- O maior risco dessa camada não é técnico, é de calibração: acionar referências cruzadas com frequência demais vira ruído irritante; acionar de menos perde o valor da feature. Precisa de um controle de sensibilidade ajustável pelo usuário ao longo do tempo.

## 11. Fundamentação em fontes (livros e artigos reais)

- O "ground truth" preferencial do conteúdo gerado são fontes reais escritas por humanos: livros e artigos (§11.1).
- Os nós são reescritas/combinações em prosa do conteúdo das fontes, citando de qual livro/capítulo ou artigo a informação veio. Para conteúdo fundamentado em busca na web (fallback, §11.1), a atribuição é **explícita e inline** ("segundo o site X ...") apontando para o link.
- Sidepanel permite **apenas visualizar** a fonte original para o usuário conferir — sem função de anotação sobre a fonte. O documento vivo (§9) é o único lugar de notas do usuário: fonte é acervo imutável (§4), notas moram no documento vivo.
- **Seleção na fonte é interação, não marcação persistente**: o usuário pode selecionar um trecho da fonte no viewer e agir sobre ele (perguntar, pedir explicação, "trazer isso pro meu documento"). A ação roteia para o documento vivo — o trecho é inserido/respondido lá, já **citado** (livro + capítulo). Nada de marca persistente é gravado sobre a fonte.
- Como as notas moram no documento vivo mas o nó cita capítulo (e pode deep-linkar para a passagem exata na fonte), a marginália não se perde: a nota aponta de volta para o local na fonte sem duplicar o acervo de notas.

### 11.1 Aquisição/origem dos livros

- **Origem**: crawl do **Library Genesis (LibGen)** para livros e do **arXiv** para artigos. O agente de IA decide, durante a geração, quais fontes são pertinentes para fundamentar um nó/tema e as baixa sob demanda, adicionando ao acervo da aplicação.
- **Módulo de aquisição trocável**: cada fonte (LibGen, arXiv, busca web) é uma implementação por trás de uma interface comum de aquisição. LibGen não é soldado ao sistema — pode ser substituído/estendido sem tocar no resto (importa para o endgame de hospedagem, §15, e para o risco legal, §16).
- **Cadeia de fallback, explícita ao usuário**: LibGen/arXiv → se nenhuma fonte adequada for encontrada, **fallback explícito para busca na internet**, sinalizado ao usuário. O conteúdo web-fundamentado é sempre atribuído inline ("segundo o site X ...", §11).
- **`SOURCES.md`**: os links dos resultados de busca web usados são registrados num `SOURCES.md`; as referências no documento vivo apontam para esses links. Mantém rastreável de onde veio cada afirmação não coberta por livro/artigo.
- **Seleção de versão**: baixa a edição/versão mais recente disponível e, sempre que possível, na língua do usuário (fallback para outra língua quando não houver na do usuário).
- **Formato preferido**: entre os formatos disponíveis para o mesmo livro, a ordem de preferência é **EPUB > PDF > DJVU**. EPUB é XHTML estruturado (extração de texto limpa, capítulos nativos via spine/TOC, menor download, encaixe direto no stack/dialeto HTML da app §4.2). PDF é o fallback universal com boa citação por página. DJVU só é aceito quando é o único formato (formato de *scan*, sem estrutura, depende de OCR embutido/próprio). Como o viewer da fonte é só-leitura (§11), o reflow do EPUB não tem desvantagem — não é preciso página tipografada fiel.
- **Normalização**: independentemente do formato de origem, a ingestão normaliza tudo para uma representação interna (texto extraído + HTML no dialeto da app). EPUB chega quase pronto; PDF passa por extração; DJVU por OCR/conversão. Depois da ingestão, o formato-fonte vira detalhe de aquisição, transparente para o resto do sistema.
- **Ingestão no acervo**: o livro baixado entra no acervo imutável de fontes e fica disponível para citação (§11) e para a camada de recuperação/embeddings (§10). Um mesmo livro é baixado uma única vez e reaproveitado entre nós e documentos vivos.
- O download é decisão do agente, não ação manual do usuário — faz parte do fluxo de geração, disparado quando o material já presente no acervo não cobre o que o nó precisa fundamentar.

## 12. Integração com provedores de IA (bring your own AI)

- Usuário traz seu próprio provedor de IA — não é a aplicação que paga pelo uso.
- **Anthropic e OpenAI diretos**: apenas chave de API (BYOK). Token OAuth de assinatura (Claude Pro/Max, ChatGPT Plus/Pro) em ferramentas de terceiros está fora dos termos de uso da Anthropic (banido ativamente desde abril de 2026) e não é um fluxo OAuth genérico disponível publicamente do lado da OpenAI (limitado ao Codex) — a integração não é construída em cima desses fluxos.
- **OpenRouter**: caminho principal/default — fluxo OAuth PKCE oficial e documentado, feito especificamente para apps de terceiros, conecta a conta do usuário em um clique, sem copiar/colar chave. Dá acesso a Anthropic, OpenAI e dezenas de outros provedores por trás de uma única integração, incluindo modelos gratuitos (sufixo `:free`, com rate limit).
- **OpenCode Zen**: opção adicional de provedor gratuito/pago — chave de API simples (sem cartão de crédito para o tier gratuito), endpoint compatível com formato OpenAI.
- **BYOK direto** (Anthropic Console, OpenAI Platform, OpenCode Zen) como opção avançada secundária, com validação imediata da chave antes de salvar e link direto para a página de geração de chave de cada provedor.
- Armazenamento de chave: arquitetura local-first — chave guardada no keychain do sistema operacional, não em banco de dados centralizado.

### 12.1 Configuração de modelos (model tiering) para usuários não-técnicos

O sistema usa **dois níveis de modelo** (§14): um leve/rápido para tarefas frequentes e baratas (gerar exercício, corrigir contra rubric, resumos, embeddings, decisão de cross-ref) e um robusto para a prosa explicativa e a confrontação adversarial. Mas o usuário **não escolhe dois modelos por nome** no caminho comum — tiering é preocupação do sistema, não etapa de setup.

- **Pairings recomendados e mantidos pela app**, por provedor/tier (fast + robusto), atualizados conforme o cenário de modelos muda. OpenRouter one-click: conecta a conta e pronto, zero escolha de modelo. BYOK direto: a app auto-seleciona o par fast/robusto daquele provedor. Tier gratuito: um par `:free`.
- **O usuário expressa intenção, não modelos.** Uma pergunta de alto nível no setup — "modelos gratuitos (mais lentos, com limite de uso) ou sua conta paga (mais rápido, custa por uso)?" — deriva *os dois tiers* de uma única escolha.
- **Explicação mínima com exemplos na tela de setup**: mostra qual par será usado por padrão e uma linha do porquê (rápido para checagens, forte para ensinar); editável, mas **não obrigatório**.
- **Config manual dos dois modelos é override avançado, opcional** — para o usuário que sabe o que quer. Aceita-se que esse usuário precise de conhecimento; o não-técnico nunca vê nome de modelo.
- **Degradação graciosa com um só modelo**: se só há um modelo disponível/configurado, ele serve os dois tiers. **Tiering é otimização, não requisito** — nunca é barreira para começar.
- **Tier gratuito respeita rate limit**: o par `:free` precisa lidar com limite de uso (fila/fallback/degradação), sem quebrar a sessão de estudo.

### 12.2 Controle e visibilidade de custo

Como é BYOK e a app é *deliberadamente* pesada em tokens (nós atômicos, prefetch especulativo, confrontação, retrieval), o usuário em chave paga não pode ser surpreendido pela fatura.

- **Tela de configuração/visualização de gastos**: o usuário define limites **diário/semanal/mensal** e vê quanto já gastou em cada janela.
- **Aplicação dos limites**: ao aproximar/atingir o teto, o sistema estrangula **primeiro o prefetch especulativo** (§14), depois avisa/pausa a geração — degradando responsividade antes de bloquear o estudo.
- **Custo corrente sempre visível** para que o design token-heavy nunca seja opaco.
- **Tier gratuito** mostra status de rate limit em vez de valor monetário.

## 13. Abordagem de implementação

- **Loop completo primeiro, profundidade depois.** O desenvolvimento começa por uma fatia vertical que fecha o ciclo central ponta-a-ponta (tema → nó gerado sob demanda → checagem com rubric travado → avaliação dispara o próximo nó), e só então cada subsistema é aprofundado até a qualidade desta spec. O motivo: quase todo o risco do projeto é de *calibração* (qualidade de avaliação, fidelidade do perfil, sensibilidade de cross-ref), e isso só se aprende usando — construir a maquinaria elaborada antes de saber qual sinal prevê aprendizado é caro e provavelmente errado.
- O objetivo final continua sendo a aplicação completa (não só o motor central isolado); o faseamento é sobre a *ordem* de construção, não sobre entregar menos.
- **Crawl do LibGen (§11.1) faz parte do loop desde o início** — a fundamentação em fontes reais não é adiada; o que se aprofunda depois é a qualidade (preferência de formato, normalização, viewer).
- O plano de fases detalhado e vivo fica em `PLAN.md` (fase 1: loop mínimo; fase 2: aplicação completa com documento vivo único; fase 3: múltiplos documentos vivos com cross-referência).

## 14. Latência e responsividade

A aplicação precisa ser **prazerosa de usar agora**, sobre modelos autoregressivos de API com latência real — uma sessão de estudo em que a maior parte do tempo é olhar um spinner é fracasso. Isso é um problema de **arquitetura/UX, não de modelo**: o modelo mais rápido é uma alavanca futura que não se controla enquanto a stack não é auto-hospedada, então **não pode ser load-bearing**. O objetivo não é eliminar a latência, é fazer o usuário **quase nunca estar bloqueado**. A métrica-alvo é **time-to-first-token / tempo até estar lendo** (~1s), não tempo até o nó ficar completo.

Alavancas, da maior para a menor:

- **Streaming + otimizar TTFT (já na §3).** Leitura humana é mais lenta que a taxa de geração de prosa; se o nó streama token-a-token enquanto o usuário lê, ele nunca alcança o spinner. A latência percebida vira só o time-to-first-token.
- **Prefetch preditivo sobre o outline (§6).** Enquanto o usuário lê o nó N e faz o exercício (segundos a minutos de tempo humano), gera-se em background o(s) provável(is) próximo(s) nó(s). Separar **"o que vem a seguir"** (previsível pelo outline → gerar adiantado) de **"como calibrar"** (depende da avaliação → delta pequeno pós-nota). Profundidade/largura do prefetch é **cost-aware/ajustável**, porque em BYOK trabalho especulativo desperdiçado é dinheiro do usuário.
- **Pipeline dentro do nó.** Prosa streama primeiro; exercício + rubric geram em paralelo enquanto o usuário lê. A §8 exige o rubric **travado antes da submissão**, não na mesma chamada de LLM — então isso preserva o invariante sem serializar a espera. A correção sobrepõe com o prefetch.
- **Model tiering (§12.1).** Modelo leve/rápido para as tarefas frequentes (exercício, correção contra rubric, resumos, embeddings, cross-ref); modelo robusto só para prosa e confrontação adversarial. Como a maioria das rodadas do loop atômico são as pequenas, isso ataca direto o "espera constante" — e é um knob disponível hoje via BYOK.
- **UI otimista.** A ação do usuário (submeter resposta, perguntar) reflete na hora no documento; "pensando" acontece no fluxo do documento, nunca em modal bloqueante.

**Modelo como knob trocável, não dependência.** LLMs de difusão (ex. linha Mercury / Gemini Diffusion) prometem TTFT/throughput muito melhores e são uma alavanca futura plausível quando a stack for auto-hospedada — mas a experiência não pode depender deles. A decisão durável é uma **camada de modelo roteada por sub-tarefa e trocável** (que o BYOK já força); um modelo mais rápido é multiplicador sobre uma arquitetura que já esconde a latência, não o resgate de uma que bloqueia.

**Síntese com os nós atômicos (§6):** nós atômicos = mais rodadas de LLM = risco de mais spinner. As alavancas acima (prefetch + tiering + streaming) são exatamente o que torna a densidade de nós atômicos acessível — o princípio do loop curto e a responsividade se resolvem pela mesma arquitetura.

## 15. Endgame — hospedagem e portabilidade

- A primeira encarnação é uma **aplicação desktop** local-first (§3, §4). Enquanto isso, a stack de modelo é BYOK (§12) e não há infraestrutura própria.
- Quando a aplicação desktop tiver tração suficiente, o plano é **hospedá-la e monetizar com doações em Monero** — no espírito de como o The Pirate Bay operou e o LibGen opera. Nesse ponto a aplicação é **100% portátil**.
- Consequências de design que já valem agora: manter tudo **local-first e portátil** (arquivos como fonte da verdade §4, chave no keychain §12, índices reconstruíveis §10), e manter o **módulo de aquisição de fontes trocável** (§11.1) para o modelo de negócio não ficar soldado a uma fonte específica.

## 16. Decisões em aberto / riscos registrados

Itens conhecidos, ainda não resolvidos ou a revisitar — não bloqueiam o início, mas não devem ser esquecidos.

- **Concorrência/resumabilidade (baseline a revisitar):** proposta inicial — sessão de estudo ativa única por documento; geração idempotente/resumível (um nó carrega estado de geração para que um stream SSE interrompido por sleep/rede resuma ou regenere deterministicamente, sem corromper); múltiplas abas = uma sessão autoritativa, as demais espelham em leitura. A definir com uso real.
- **Risco legal/operacional do LibGen:** baixar automaticamente obras protegidas tem exposição diferente do download manual, sobretudo no endgame de hospedagem (§15). Mitigação estrutural: módulo de aquisição trocável (§11.1). Postura jurídica final fica fora do escopo desta spec.
- **Grounding em áreas não-exatas:** o grounding de exercícios no material original (§8) funciona bem em exatas; em domínios menos determinísticos é mais frouxo e a qualidade da avaliação depende mais dos rubrics da §8.1. Risco de calibração reconhecido, não resolvido.
- **Detecção de falha do avaliador:** a tese depende da qualidade da avaliação; falta um mecanismo explícito de telemetria/correção (affordance de "isso está errado/inútil" que realimente) para *detectar* leniência, grounding alucinado ou confrontação contra espantalho. A ser desenhado durante a Fase 1, quando o loop existir para ser observado.
- **Backup/sync entre máquinas** (o polímata com laptop + desktop): pós-Fase-1; o layout de arquivos (§4) deve preservar essa possibilidade.
