#!/usr/bin/env bash
set -euo pipefail

# ============================================================
# Argv
# ============================================================
if [[ $# -lt 1 ]]; then
  echo "Uso: $0 <path/PROJECT_GOAL.md>" >&2
  exit 1
fi

[[ -f "$1" ]] || { echo "Goal file não encontrado: $1" >&2; exit 1; }
goal_dir=$(cd "$(dirname "$1")" 2>/dev/null && pwd) || {
  echo "Não consegui resolver path do goal: $1" >&2; exit 1; }
GOAL_FILE="$goal_dir/$(basename "$1")"

cd "$goal_dir"
PROJECT_ROOT=$(git rev-parse --show-toplevel 2>/dev/null) || {
  echo "Goal file não está dentro de um repo git: $GOAL_FILE" >&2; exit 1; }
cd "$PROJECT_ROOT"
PROJECT_SLUG="${PROJECT_ROOT//\//-}"

# ============================================================
# Config
# ============================================================
ARCHIVE_DIR="${ARCHIVE_DIR:-$HOME/.agentic-loop/$PROJECT_SLUG}"
STATE_FILE="${STATE_FILE:-$ARCHIVE_DIR/PROJECT_STATE.md}"
NOTES_FILE="${NOTES_FILE:-$ARCHIVE_DIR/PROJECT_NOTES.md}"
# TASK_FILE é setado per-iteração dentro do loop, apontando para $TASK_DIR/TASK.md.
MAX_TASKS="${MAX_TASKS:-50}"
MAX_FOLLOWUPS="${MAX_FOLLOWUPS:-10}"
LOG_TAIL_TASKS="${LOG_TAIL_TASKS:-99999}"

# stream-json + verbose: cada chamada do claude vira um .stream.jsonl tail-able.
CLAUDE_BASE_FLAGS=(--output-format stream-json --verbose --dangerously-skip-permissions)

# ============================================================
# Prompts
# ============================================================
SURVEYOR_PROMPT_TEMPLATE='Você é o Agente Cartógrafo (Surveyor).

# Goal (PROJECT_GOAL.md)
%s

# Sua missão

Mapear o estado atual do repositório e sintetizar onde estamos em relação ao goal,
para servir de ponto de partida (snapshot inicial) ao trabalho de planejamento de
tasks que virá em seguida — sem que esse trabalho precise re-explorar o repo do
zero.

Inspecione: estrutura de diretórios, arquivos centrais (README, CLAUDE.md, spec,
arquivos de build/manifest, código principal), `git log --oneline -50`, branch atual.
Leia o conteúdo do que importa — não se limite a listagem.

Esta é uma FOTO do estado, não um plano. Não decida tasks; não escreva TODOs
prescritivos. Observe, sintetize, registre.

# Output

Escreva em %s (sobrescrevendo) um documento conciso com EXATAMENTE estas seções:

```
# Estado do Projeto — <data ISO> (snapshot inicial, pré-execução)

> Este documento é uma foto do repositório no momento em que o loop começou.
> É um ponto de partida para orientar o planejamento — não uma fonte autoritativa
> sobre o estado corrente. Quem ler depois deve verificar contra o repo vivo
> antes de tomar decisões.

## Onde estamos
2-4 parágrafos curtos descrevendo o estado real: o que existe, em que fase está
(spec only? scaffolding feito? primeiros componentes implementados?), nível de
testes presente, qualidade aparente do que existe.

## Mapa do goal vs realidade
Bullets dos marcos/componentes principais do goal, cada um marcado com:
- [✓] feito
- [~] em progresso / parcial
- [ ] não começado
Não exaustivo — foque nos eixos principais.

## Pontos de atenção
3-7 bullets do que parece faltar, está incompleto, ou levantou bandeira durante
a inspeção (código suspeito, débito visível, testes ausentes em área crítica).
Observações para quem for planejar, não uma lista de TODO.

## Decisões já fechadas
Bullets curtos das decisões já tomadas (na spec §<n>, em CLAUDE.md, em ADRs) que
constringem o trabalho daqui pra frente. Cite a fonte.
```

Não escreva mais nada além desse documento.'

DIRECTOR_PROMPT_TEMPLATE='Você é o Diretor de Projeto.

# Goal (PROJECT_GOAL.md)
%s

# Snapshot inicial do projeto (em %s)
%s

> ⚠️  O snapshot acima foi capturado UMA VEZ no início do loop, ANTES das tasks
> abaixo rodarem. Use como direcionamento e ponto de partida, **não como verdade
> corrente** — o repo evoluiu desde então. Não precisa re-validar tudo: investigue
> apenas o suficiente (`git log`, `git status`, leitura dos arquivos relevantes
> à task que está cogitando) para definir a próxima task com confiança. Se notar
> que o snapshot está estale num ponto que importa, prefira o que vê no repo —
> e considere registrar a correção em PROJECT_NOTES (instruções no fim).

# Notas acumuladas (em %s — append-only)
%s

> Estas notas foram adicionadas por planejamentos anteriores ao longo das iterações.
> São observações persistentes que valem para planejamentos futuros: correções ao
> snapshot inicial, padrões arquiteturais descobertos, constraints emergentes,
> decisões de fato tomadas. Mais recente que o snapshot — geralmente confiável,
> mas verifique se algo cheirar a desatualizado.

# Histórico de tasks já executadas (últimas %d, mais antigas no topo)
%s

# Sua missão

Decidir a PRÓXIMA task pequena, objetiva, auto-contida, que move o projeto rumo ao goal.
Antes de escrever, inspecione o repo: rode `git log --oneline -20` e `git status`, e leia
arquivos recém-modificados ou centrais à task que cogita. Olhe também CLAUDE.md e a spec
para constraints e decisões já fechadas.

# Filosofia

Você zela pela qualidade, completude e manutenibilidade do projeto. DX e UX devem ser
impecáveis: APIs ergonômicas, mensagens de erro claras, testes que cobrem caminho feliz,
edge cases e modos de falha. Ao mesmo tempo, recuse complexidade especulativa — código
direto, sem abstração prematura, sem feature flag para futuro hipotético, sem comentário
óbvio.

Refatorar é parte do trabalho, não luxo. Se você nota duplicação, módulo inchado, naming
ruim, função longa demais, ou interface confusa — emita uma task de REFACTOR. Não tenha
medo de mexer no que já existe quando isso melhora o sistema; leve o código na direção
certa mesmo que custe linhas.

Você zela pelo projeto **como um todo**, não só pelas frentes em aberto. Periodicamente
olhe para partes que já estão prontas e funcionam — abra-as no editor, leia. Só porque
o teste passa não significa que o código é bom: pode ter naming ambíguo, abstração
errada, algoritmo desnecessariamente complicado, padrão interno inconsistente com o
resto do projeto, cobertura de teste rasa (só caminho feliz), módulo que cresceu além
do que devia. Se notar isso, emita REFACTOR mesmo que ninguém tenha pedido — bom
estado geral é responsabilidade sua. Critério: a mudança deve **simplificar** ou
**organizar**, nunca complicar; se o refactor proposto adiciona complexidade pra
flexibilidade hipotética, não vale.

Esse zelo cobre também os testes: cobertura desbalanceada, asserts frouxos, fixture
duplicada, teste que não documenta intenção — tudo cabe em REFACTOR.

Antes de finalizar a task, **pare e reflita**:
- Existe um caminho mais simples e elegante para chegar ao goal a partir daqui?
- Estou pedindo a menor unidade de progresso significativo?
- A interface pública (HTTP, CLI, traits, tipos exportados) que será exercitada ficará
  bonita? Faria sentido para alguém vendo pela primeira vez?
- O que vai sair desta task é uma adição limpa, ou empurra entropia adiante?
- O projeto inteiro está bem cuidado, ou tem partes prontas que mereciam atenção e
  estou ignorando porque "funcionam"?

# Tipos de task

Use TDD. Tasks tipicamente vêm em pares: primeiro um teste vermelho, depois implementação.

- **TEST FIRST** — escreva testes que falham, marcados `#[ignore]` (ou equivalente),
  exercendo a interface pública desejada. Vale tanto para feature nova quanto para
  reproduzir bug. **Não implementa a feature/fix em si**, só os testes. Use o ato de
  escrever o teste para desenhar/refinar a API: nomes, assinaturas, tipos, ergonomia,
  mensagens de erro. Se ao escrever o teste a interface pública parecer feia, **mude-a**
  no teste — a implementação posterior se ajusta. Cubra caminho feliz, edge cases e erros.

- **IMPLEMENT** — remova `#[ignore]` de teste(s) específico(s) citados na task e
  faça-os passar. Sem ampliar escopo além do necessário para verde.

- **REFACTOR** — extrai, simplifica, renomeia, deduplica. Comportamento preservado;
  suíte de testes intocada (ou só ajustada a renames). Foco: código mais limpo, mais
  conciso, mais manutenível.

- **DESIGN** — fechar decisão pendente da spec (§10), atualizar a spec, ou registrar
  ADR. Use quando a próxima implementação depende dessa decisão.

- **FIX** — correção pequena e óbvia (típico: bug introduzido na task anterior, sem
  necessidade do ritual TDD completo). Prefira TEST FIRST + IMPLEMENT sempre que o bug
  for não-trivial.

- **SCAFFOLD** — bootstrap de tooling (criar `Cargo.toml`, configurar runner de teste,
  CI mínimo, etc). Use no começo do projeto, antes de TDD ser viável.

# Heurística para escolher

1. Há tasks REJECTED recentes? Avalie retry com escopo diferente.
2. Há testes `#[ignore]` esperando implementação? Tipicamente a próxima é IMPLEMENT.
3. As últimas "Notas para próximo planejamento" sinalizam pendência? Encare antes de abrir frente nova.
4. Padrão repetido / cheiro de código no diff recente sugere abstração faltando ou
   complexidade acidental? → REFACTOR.
5. Não rolou um REFACTOR em N tasks e o projeto cresceu? Faça uma passagem por
   módulos prontos e veja se algum mereceria limpeza, melhor cobertura, ou
   reestruturação. Não force se nada salta aos olhos — mas olhe.
6. Decisão de design bloqueando o próximo passo? → DESIGN.
7. Projeto ainda não tem tooling para rodar teste? → SCAFFOLD.
8. Caso contrário: a próxima fatia natural rumo ao goal — geralmente TEST FIRST.

# Constraints do projeto (não-negociáveis — leia CLAUDE.md para o resto)

- Tests rodam com `cargo nextest run` (não `cargo test`).
- Postgres URLs no formato `postgres://user:pass@host:port/db`.
- Spec/design docs em PT-BR; código, identificadores, commits, ADRs em inglês.
- Estado coordenado via Postgres, nunca em memória do processo.

# Output

Escreva a task em %s (sobrescrevendo). Estrutura obrigatória:

```
# <Título de 1 linha>

**Tipo:** TEST FIRST | IMPLEMENT | REFACTOR | DESIGN | FIX | SCAFFOLD

**Contexto:** 2-4 frases. Por que esta task agora; o que ela destrava.

**Escopo:** bullets do que fazer. Específico, verificável, não ambíguo.
- Se TEST FIRST: cite exatamente quais cenários cobrir e onde os testes vivem.
- Se IMPLEMENT: cite o(s) teste(s) específicos a destravar.
- Se REFACTOR: cite o alvo e o critério de "pronto" (ex.: "extrair X de Y; nenhuma
  duplicação restante de Z").

**Fora do escopo:** bullets do que NÃO fazer (anti-escopo). Inclua aqui qualquer coisa
tentadora que o implementador deve resistir.

**Critérios de aceitação:** bullets objetivos. Cada um checável pelo reviewer rodando
um comando ou lendo o diff.
```

Não implemente nada. Apenas escreva a task.

# Notas para planejamentos futuros (opcional)

Se durante esta investigação você descobriu algo PERSISTENTE que beneficiará
planejamentos futuros — ex.: o snapshot inicial está errado em ponto que importa,
padrão arquitetural não-óbvio, constraint que emergiu, decisão de fato tomada que
deve bindar próximas tasks — faça **append** ao final de %s.

Use o comando `cat >> ARQUIVO <<EOF ... EOF` ou a ferramenta Edit (anexando após
o último caractere do arquivo). **NUNCA use Write** nesse arquivo: Write sobrescreve
e destrói notas anteriores. Append-only é regra dura — o orquestrador detecta
violação e restaura o arquivo, perdendo sua nota.

Estrutura da entrada:

```
## Após task #%03d — <data ISO>
- bullet conciso e auto-contido
- outro bullet
```

Não anote o que já está em summaries (status de tasks, o que foi feito). Anote só
insight cross-task que num próximo planejamento te economizaria investigação. Se
nada digno surgiu nesta rodada, **não toque no arquivo**.

# Goal já atingido

Se o goal está totalmente atingido (incluindo qualidade do código, cobertura de testes,
docs alinhadas), escreva EXATAMENTE `GOAL_COMPLETE` como primeira e única linha de %s.'

IMPLEMENTER_OUTPUT_INSTRUCTIONS='Respeite o **Tipo** e o **Fora do escopo** da task acima.

# 🚫 NUNCA RODE `git commit` NEM `git add`

Esta é uma regra dura, sem exceção. O orquestrador usa o HEAD do git para detectar
o veredito da etapa de revisão posterior — qualquer commit seu quebra essa detecção
e corrompe o loop. Quem decide se a task vira commit é a revisão, não você.

Você PODE rodar livremente: `git status`, `git diff`, `git log`, `git show`,
qualquer comando de leitura. O que você NÃO pode é mexer no índice ou no HEAD:
nada de `git add`, `git commit`, `git stash`, `git reset`, `git checkout` em
arquivos, `git restore`. Deixe a árvore como está; a revisão decide.

- Se **TEST FIRST**: escreva apenas os testes (marcados `#[ignore]`). NÃO implemente a
  feature/fix. Confirme que os testes falham quando rodados com `--include-ignored` e que
  a suíte normal continua verde. Se ao escrever o teste a interface pública parece ruim,
  ajuste **o teste** para refletir a interface que você gostaria — a implementação virá
  em task futura.
- Se **IMPLEMENT**: faça os testes citados passarem removendo `#[ignore]`. Não introduza
  código que não seja exigido por algum teste.
- Se **REFACTOR**: preserve comportamento. Rode a suíte antes e depois.
- Se notar algo errado fora do escopo, **anote em "O QUE NÃO FIZ"** em vez de consertar.

**Comentários:** mínimo absoluto. Código deve ser entendível por nomes, tipos e
estrutura. Não escreva comentário que descreve "o quê" o código faz, nem cabeçalho
óbvio em função auto-explicativa, nem nota referenciando a task/PR atual, nem
"decoração" (`// ===== Section =====`). Comentário só se documenta um "porquê"
não-óbvio (constraint, workaround, invariante sutil). Na dúvida, não escreva.

Ao final, produza um output estruturado em markdown:
- **O QUE FIZ:** arquivos/funções alterados, com caminho relativo.
- **POR QUE:** decisões de design não-óbvias (e alternativas descartadas, se houver).
- **O QUE TESTEI:** comandos rodados e resultados; o que ficou sem validar e por quê.
- **O QUE NÃO FIZ:** pendências, escopo deliberadamente fora, problemas notados mas não
  corrigidos, sugestões/notas para o próximo planejamento.'

REVIEWER_PROMPT_TEMPLATE='Você é o Agente de Revisão (stateless — este é seu único view dessa task).

TASK.md:
---
%s
---

Histórico desta task (iterações anteriores nesta mesma task — tentativas de
implementação alternadas com o feedback que receberam, em ordem cronológica):
---
%s
---

Rode `git status` e `git diff HEAD` para ver o estado atual da árvore.

# O que avaliar

1. **Critérios de aceitação** — todos cumpridos? Cite cada um explicitamente e o status.
2. **Tipo da task** — foi respeitado?
   - TEST FIRST: existem testes novos marcados `#[ignore]`, falham com
     `--include-ignored`, e a feature/fix em si NÃO foi implementada. Se houve
     implementação além dos testes, isso é REJECTED.
   - IMPLEMENT: os testes citados na task agora passam, com `#[ignore]` removido, e a
     suíte completa está verde. Sem código fora do mínimo necessário.
   - REFACTOR: comportamento preservado, suíte continua passando, escopo respeitado.
3. **Anti-escopo** — "Fora do escopo" foi respeitado? Houve scope creep?
4. **Qualidade do código** — mesmo que critérios passem, aponte problemas no diff:
   duplicação nova, naming ruim, complexidade injustificada, abstração prematura,
   mensagens de erro pobres, ergonomia da API pública. Se forem leves, pode pedir
   ajuste; se forem graves, REJECTED.
5. **Comentários** — política do projeto é manter comentários no MÍNIMO. Código
   bom se explica sozinho via nomes, tipos e estrutura. Reprove sem hesitar:
   - comentários que descrevem **o quê** o código faz (`// incrementa contador`)
   - cabeçalhos/docstrings óbvios em funções com nome auto-explicativo
   - comentários "decorativos" (`// ===== Helpers =====`)
   - comentários referenciando a task/PR/issue que originou o código
   - notas histórias do tipo `// removido X`, `// antes era Y`
   - TODOs vagos sem owner/condição clara

   Aceitáveis: comentários que explicam **por quê** (constraint não-óbvia,
   workaround documentado, invariante sutil que não cabe no nome). Se o "por quê"
   é óbvio do contexto, também sai. Se removendo o comentário um leitor futuro
   entenderia igual, ele não devia existir.

6. **Histórico** — se há feedback anterior nesta task, valide explicitamente se foi
   endereçado.

# Veredito — o orquestrador detecta seu veredito pelo estado do git

Só existem três caminhos. O orquestrador classifica pelo estado do git pós-resposta,
não pela prosa.

- **DONE — aprovar e commitar.** Task completa, qualidade aceitável, **e só nesse
  caso** você commita. Rode `git add -A && git commit -m "<mensagem concisa em
  inglês>"`. O ato de commitar É o veredito DONE. Garanta `git status` limpo após
  o commit. Não existe commit parcial — se não vale aprovar 100%, não commite.

- **REJECTED — recusar.** Task mal especificada, abordagem fundamentalmente errada,
  ou o trabalho não respeitou o tipo/anti-escopo, **e não vale tentar continuar**.
  Não commite. Escreva EXATAMENTE `REJECTED` na primeira linha da resposta; depois
  da tag, descreva o motivo em prosa para o próximo planejamento reformular.

- **Pedir continuação/ajuste.** Quase pronto, ou erro localizado que vale mandar
  consertar. Não commite, não escreva REJECTED. Sua resposta inteira vira o próximo
  prompt do trabalho — seja específico: cite arquivos, linhas, o que mudar.

⚠️ Commit parcial não existe neste loop. Se a task ficou meio-feita e não vale
continuar, é REJECTED com nota explicando o que aproveitar; a próxima task
reformula. Se vale continuar, é continuação. Se está pronto, é DONE.'

SUMMARIZER_PROMPT_TEMPLATE='Você é o Agente Sumarizador.

TASK.md original:
---
%s
---

Histórico completo (todas as iterações + reviews):
---
%s
---

Veredito final: %s

Produza um resumo append-only nesta forma EXATA, sem nada antes ou depois:

## Task #%03d — %s — <título de 1 linha>
- **Tipo:** <TEST FIRST | IMPLEMENT | REFACTOR | DESIGN | FIX | SCAFFOLD> (do TASK.md)
- **Veredito:** %s
- **O que foi feito:** <1-3 frases>
- **Por que:** <1-2 frases>
- **Notas para próximo planejamento:** <pendências, débito, padrões observados, sugestões de
  refactor, testes ignorados que ficaram esperando IMPLEMENT; ou "nenhuma">

DONE limpo: poucas linhas. REJECTED: expanda "Notas" com o motivo e o que tentar
diferente na próxima.'

# ============================================================
# Helpers
# ============================================================
ts() { date +%Y%m%d_%H%M%S; }

# Roda claude streamando para um arquivo .jsonl tail-able.
# Args: <prompt> <stream_file> [resume_session_id]
# Preenche: CLAUDE_RESULT, CLAUDE_SESSION_ID
run_claude() {
  local prompt="$1" stream_file="$2" resume_id="${3:-}"
  local args=("${CLAUDE_BASE_FLAGS[@]}" -p "$prompt")
  [[ -n "$resume_id" ]] && args+=(--resume "$resume_id")

  claude "${args[@]}" > "$stream_file"

  local result_event
  result_event=$(jq -c 'select(.type=="result")' "$stream_file" | tail -1)
  if [[ -z "$result_event" ]]; then
    echo "[!] claude não emitiu evento 'result' em $stream_file" >&2
    return 3
  fi
  CLAUDE_RESULT=$(jq -r '.result' <<<"$result_event")
  CLAUDE_SESSION_ID=$(jq -r '.session_id' <<<"$result_event")
}

# Concatena histórico cronológico da task (impl outputs + followups intercalados)
build_task_history() {
  local task_dir="$1" up_to="$2" out=""
  for i in $(seq 1 "$up_to"); do
    if [[ -f "$task_dir/impl_${i}.output.md" ]]; then
      out+=$'\n### Iteração #'"$i"$' — tentativa de implementação\n'$(<"$task_dir/impl_${i}.output.md")$'\n'
    fi
    if [[ -f "$task_dir/followup_${i}.md" ]]; then
      out+=$'\n### Iteração #'"$i"$' — feedback recebido\n'$(<"$task_dir/followup_${i}.md")$'\n'
    fi
  done
  printf '%s' "$out"
}

# Junta os summary.md das últimas N tasks (cronológico, mais antigas no topo).
build_task_summaries() {
  local n="$1"
  local dirs
  dirs=$(ls -1d "$ARCHIVE_DIR"/task_*/ 2>/dev/null | sort | tail -n "$n")
  if [[ -z "$dirs" ]]; then
    echo "(nenhuma task anterior)"
    return 0
  fi
  while IFS= read -r d; do
    [[ -f "$d/summary.md" ]] || continue
    cat "$d/summary.md"
    echo
  done <<<"$dirs"
}

# ============================================================
# Pre-flight
# ============================================================
command -v jq     >/dev/null || { echo "jq necessário"; exit 1; }
command -v claude >/dev/null || { echo "claude CLI necessário"; exit 1; }
mkdir -p "$ARCHIVE_DIR"

echo "[loop] project: $PROJECT_ROOT"
echo "[loop] goal:    $GOAL_FILE"
echo "[loop] archive: $ARCHIVE_DIR"

if [[ ! -f "$NOTES_FILE" ]]; then
  cat > "$NOTES_FILE" <<'EOF'
# Notas Acumuladas — Planejamento

Append-only. Cada entrada é uma seção `## Após task #NNN — <data ISO>` com bullets
de insight cross-task: correções ao snapshot inicial, padrões arquiteturais
descobertos, constraints emergentes, decisões de fato tomadas que devem bindar
próximas tasks.

NÃO confundir com summaries per-task (`task_*/summary.md`): este arquivo é insight
cross-task, não chronicle de execução. NÃO sobrescrever — só anexar ao final.

EOF
fi

# ============================================================
# Surveyor — só roda se PROJECT_STATE.md ainda não existe
# ============================================================
if [[ ! -f "$STATE_FILE" ]]; then
  echo "[surveyor] $STATE_FILE ausente — mapeando estado do projeto..."
  SURVEY_TS=$(ts)
  SURVEY_DIR="$ARCHIVE_DIR/surveyor_${SURVEY_TS}"
  mkdir -p "$SURVEY_DIR"

  goal_content=$(<"$GOAL_FILE")
  surveyor_prompt=$(printf "$SURVEYOR_PROMPT_TEMPLATE" "$goal_content" "$STATE_FILE")

  echo "[surveyor] stream: $SURVEY_DIR/stream.jsonl  (tail -f para acompanhar)"
  run_claude "$surveyor_prompt" "$SURVEY_DIR/stream.jsonl"
  printf '%s\n' "$CLAUDE_SESSION_ID" > "$SURVEY_DIR/session_id"
  printf '%s\n' "$CLAUDE_RESULT"     > "$SURVEY_DIR/output.md"

  [[ -f "$STATE_FILE" ]] || { echo "[surveyor] não escreveu $STATE_FILE"; exit 2; }
  cp "$STATE_FILE" "$SURVEY_DIR/PROJECT_STATE.md"
  echo "[surveyor] estado escrito em $STATE_FILE"
else
  echo "[surveyor] $STATE_FILE já existe — pulando."
fi

# ============================================================
# Loop principal
# ============================================================
for task_num in $(seq 1 "$MAX_TASKS"); do
  TASK_TS=$(ts)
  TASK_DIR="$ARCHIVE_DIR/task_$(printf '%03d' "$task_num")_${TASK_TS}"
  TASK_FILE="$TASK_DIR/TASK.md"
  mkdir -p "$TASK_DIR"

  echo "============================================================"
  echo "Task #$task_num  —  $TASK_TS"
  echo "Dir: $TASK_DIR"
  echo "Streams ao vivo: $TASK_DIR/*.stream.jsonl"
  echo "============================================================"

  # ---- 1. Director ---------------------------------------------
  echo "[director] planejando..."
  goal_content=$(<"$GOAL_FILE")
  state_content=$(<"$STATE_FILE")
  notes_content=$(<"$NOTES_FILE")
  recent_log=$(build_task_summaries "$LOG_TAIL_TASKS")
  director_prompt=$(printf "$DIRECTOR_PROMPT_TEMPLATE" \
    "$goal_content" "$STATE_FILE" "$state_content" \
    "$NOTES_FILE" "$notes_content" \
    "$LOG_TAIL_TASKS" "$recent_log" "$TASK_FILE" \
    "$NOTES_FILE" "$task_num" "$TASK_FILE")

  # Salvaguarda append-only: snapshot do NOTES_FILE antes da chamada; depois
  # exigimos que o conteúdo novo comece exatamente com o conteúdo antigo.
  cp "$NOTES_FILE" "$TASK_DIR/PROJECT_NOTES.before.md"
  notes_before_size=$(wc -c < "$NOTES_FILE")

  run_claude "$director_prompt" "$TASK_DIR/director.stream.jsonl"
  printf '%s\n' "$CLAUDE_SESSION_ID" > "$TASK_DIR/director.session_id"
  printf '%s\n' "$CLAUDE_RESULT"     > "$TASK_DIR/director.output.md"

  if (( notes_before_size > 0 )); then
    if ! head -c "$notes_before_size" "$NOTES_FILE" 2>/dev/null \
         | cmp -s - "$TASK_DIR/PROJECT_NOTES.before.md"; then
      echo "[!] director violou append-only em $NOTES_FILE — restaurando snapshot pré-chamada."
      cp "$TASK_DIR/PROJECT_NOTES.before.md" "$NOTES_FILE"
    fi
  fi
  cp "$NOTES_FILE" "$TASK_DIR/PROJECT_NOTES.after.md"

  [[ -f "$TASK_FILE" ]] || { echo "[director] não escreveu $TASK_FILE"; exit 2; }

  if head -1 "$TASK_FILE" | grep -qE '^GOAL_COMPLETE\s*$'; then
    echo "[director] GOAL_COMPLETE — fim."
    break
  fi
  task_content=$(<"$TASK_FILE")

  # ---- 2. Subloop implementer ⇄ reviewer -----------------------
  IMPL_SESSION_ID=""
  attempt=1
  verdict=""

  while (( attempt <= MAX_FOLLOWUPS )); do
    echo "[impl] tentativa $attempt..."

    if (( attempt == 1 )); then
      impl_prompt="${task_content}

${IMPLEMENTER_OUTPUT_INSTRUCTIONS}"
      run_claude "$impl_prompt" "$TASK_DIR/impl_${attempt}.stream.jsonl"
    else
      impl_prompt=$(<"$TASK_DIR/followup_$((attempt-1)).md")
      run_claude "$impl_prompt" "$TASK_DIR/impl_${attempt}.stream.jsonl" "$IMPL_SESSION_ID"
    fi

    IMPL_SESSION_ID="$CLAUDE_SESSION_ID"
    printf '%s\n' "$CLAUDE_RESULT"     > "$TASK_DIR/impl_${attempt}.output.md"
    printf '%s\n' "$IMPL_SESSION_ID"   > "$TASK_DIR/impl.session_id"

    # ---- Reviewer (stateless, recebe histórico concatenado) ---
    echo "[review] avaliando..."
    history=$(build_task_history "$TASK_DIR" "$attempt")
    review_prompt=$(printf "$REVIEWER_PROMPT_TEMPLATE" "$task_content" "$history")

    head_before=$(git rev-parse HEAD)
    run_claude "$review_prompt" "$TASK_DIR/review_${attempt}.stream.jsonl"
    review_file="$TASK_DIR/review_${attempt}.output.md"
    printf '%s\n' "$CLAUDE_RESULT"     > "$review_file"
    printf '%s\n' "$CLAUDE_SESSION_ID" >> "$TASK_DIR/reviewer.session_ids"
    head_after=$(git rev-parse HEAD)

    # Classifica veredito pelo delta de HEAD (commit é o sinal primário).
    # Tag textual REJECTED é fallback quando não há commit.
    if [[ "$head_before" != "$head_after" ]]; then
      verdict="DONE"
      if [[ -n "$(git status --porcelain)" ]]; then
        echo "[!] tree não está limpa após commit — registrando warning."
        git status --porcelain > "$TASK_DIR/dirty_tree_warning.txt"
      fi
      break
    fi

    first_line=$(head -1 "$review_file" | tr -d '[:space:]')
    if [[ "$first_line" == "REJECTED" ]]; then
      verdict="REJECTED"
      break
    fi

    cp "$review_file" "$TASK_DIR/followup_${attempt}.md"
    attempt=$((attempt + 1))
  done

  if [[ -z "$verdict" ]]; then
    echo "[!] excedeu MAX_FOLLOWUPS — marcando REJECTED."
    verdict="REJECTED"
    printf 'REJECTED\nLoop excedeu MAX_FOLLOWUPS sem convergência.\n' \
      > "$TASK_DIR/review_timeout.md"
  fi

  printf '%s\n' "$verdict" > "$TASK_DIR/final_verdict.txt"
  echo "[verdict] $verdict"

  # ---- 3. Summarizer -------------------------------------------
  echo "[summary] gerando..."
  full_history=$(build_task_history "$TASK_DIR" "$attempt")
  for f in "$TASK_DIR"/review_*.output.md; do
    [[ -f "$f" ]] || continue
    full_history+=$'\n### '"$(basename "$f" .output.md)"$'\n'$(<"$f")$'\n'
  done

  summary_prompt=$(printf "$SUMMARIZER_PROMPT_TEMPLATE" \
    "$task_content" "$full_history" "$verdict" \
    "$task_num" "$TASK_TS" "$verdict")

  run_claude "$summary_prompt" "$TASK_DIR/summarizer.stream.jsonl"
  printf '%s\n' "$CLAUDE_SESSION_ID" > "$TASK_DIR/summarizer.session_id"
  printf '%s\n' "$CLAUDE_RESULT"     > "$TASK_DIR/summary.md"
done

echo "Loop agentico finalizado."
