# Tasks

> Gerado por /director.
> Status: [ ] pendente · [~] em progresso · [x] concluída · [!] bloqueada

## Lote 1 — 2026-05-18

**Objetivo do lote:** fechar três lacunas concretas entre a implementação e o
goal — DDL de sequences (única feature com testes ainda `#[ignore]`), síntese de
structs Rust para tipos compostos/records (hoje colapsados em `String`) e
posicionamento preciso dos erros de compilação dentro do literal SQL.

### 1.1 — Implementar DDL de sequences (CREATE / ALTER / DROP SEQUENCE)
- **Status:** [x]
- **Tipo:** IMPLEMENT
- **Descrição:** Os 20 testes em
  [pgsafe_analyzer/tests/ddl/sequences.rs](pgsafe_analyzer/tests/ddl/sequences.rs)
  são a única suíte ainda `#[ignore]` no projeto — a TEST FIRST já existe.
  Hoje [ddl/mod.rs:173-174](pgsafe_analyzer/src/ddl/mod.rs#L173-L174) trata
  `CreateSeqStmt` e `AlterSeqStmt` como no-ops. Implementar:
  - **CREATE SEQUENCE:** registrar uma linha `PgClass` mínima com
    `relkind: RelKind::Sequence` no namespace alvo (o padrão de inserção via
    `insert_pg_class` está em [ddl/tables.rs:117-124](pgsafe_analyzer/src/ddl/tables.rs#L117-L124)).
    Honrar `IF NOT EXISTS` (silencioso em duplicata) e, sem ela, retornar
    `DdlError::DuplicateObject` com mensagem `"already exists"`.
  - **ALTER SEQUENCE:** validar que a sequence existe — erro
    `DdlError::TableNotFound` (`"does not exist"`) salvo `missing_ok` — e
    aceitar todas as opções (`RESTART`, `INCREMENT BY`, etc.) como no-op, pois
    não afetam tipagem.
  - **ALTER SEQUENCE RENAME TO:** adicionar `ObjectType::ObjectSequence` ao
    braço de `rename_relation` em [ddl/alter.rs:22-25](pgsafe_analyzer/src/ddl/alter.rs#L22-L25).
    `SET SCHEMA` já cobre sequences em [ddl/alter.rs:321](pgsafe_analyzer/src/ddl/alter.rs#L321).
  - **DROP SEQUENCE:** `drop_relation` em
    [ddl/drop.rs:64](pgsafe_analyzer/src/ddl/drop.rs#L64) hoje fixa a palavra
    `"table"` na mensagem de objeto inexistente e não valida que o `relkind`
    bate com o `ObjectType` pedido. Propagar o `ObjectType` para que a mensagem
    de não-encontrado diga `"sequence"` e para emitir `"is not a sequence"`
    quando o nome resolve para uma tabela.
  - Verificar que `nextval`/`currval`/`lastval`/`setval` resolvem para `int8`
    NOT NULL — já estão em `is_not_null_nonstrict`
    ([functions.rs:635](pgsafe_analyzer/src/functions.rs#L635)); ajustar se
    os testes de função revelarem que a coerção do literal para `regclass`
    falha.
  - Remover o `#[ignore]` dos 20 testes ao final.
- **Verificação:** `cargo nextest run --release -p pgsafe_analyzer` — os 20
  testes de `sequences.rs` passam e nenhum outro regride.

### 1.2 — Síntese de struct Rust para tipos compostos e records anônimos
- **Status:** [x]
- **Tipo:** IMPLEMENT
- **Descrição:** O goal exige que tipos compostos e records anônimos virem
  structs Rust com fields tipados (seção "Estrutura de record analisada em
  profundidade" e "Cobertura de tipos"). O analyzer já reconstrói os campos —
  `Type::Composite { fields }` e `Type::AnonymousRecord { fields }` em
  [pgsafe_analyzer/src/types.rs](pgsafe_analyzer/src/types.rs), exercitado
  por [tests/query/records.rs](pgsafe_analyzer/tests/query/records.rs) — mas
  o codegen joga essa informação fora:
  [codegen.rs:298-334](pgsafe_macros/src/codegen.rs#L298-L334) mapeia
  `Type::Composite` e `Type::AnonymousRecord` para `String` quando não há
  override em `[types]`. Implementar:
  - Codegen que sintetiza uma struct Rust aninhada, um field tipado por campo
    do composite (mapeando o tipo de cada campo recursivamente, incluindo
    composite-de-composite e domain-sobre-composite). Composites nomeados
    continuam honrando o override `[types]` primeiro; sem override, gera-se a
    struct. Records anônimos sempre geram struct.
  - Desserialização do formato wire de `record` do Postgres para a struct
    gerada no runtime (`pgsafe`) — investigar e decidir o mecanismo
    (`postgres-types`/`FromSql` sobre o layout binário de record) como primeiro
    passo da tarefa.
  - Cobertura e2e: nova migration
    `pgsafe_e2e/migrations/0005_composite_types.sql` com `CREATE TYPE ... AS
    (...)` e uma tabela com coluna composta; novo arquivo
    `pgsafe_e2e/tests/composites.rs` exercitando SELECT de coluna composta,
    `ROW(...)` e subquery em `FROM` com acesso a fields da struct gerada.
- **Verificação:** `cargo nextest run --release` (suíte de compile-time verde) +
  `cargo nextest run --release --test composites` no crate `pgsafe_e2e`
  (requer Docker) — roundtrip de coluna composta passa.

### 1.3 — Erros de query apontam o token ofensor dentro do literal SQL
- **Status:** [!] BLOQUEADA — depende de feature instável do compilador.
  `proc_macro2::Literal::subspan` delega para `proc_macro::Literal::subspan`,
  atrás da feature instável `proc_macro_span`. O proc-macro roda no compilador
  do crate *consumidor*, então uma biblioteca não pode exigir nightly dos seus
  usuários. No toolchain stable (1.94.1) `subspan` retorna sempre `None`, logo a
  "degradação graciosa" para `input.sql.span()` vira o caso único e o payoff
  visível (apontar o token) não é entregável. A infra de offsets (mapa do lexer,
  `AnalyzeError` com offset) é correta e barata, mas inerte sem `subspan`.
  Reavaliar com /director.
- **Tipo:** IMPLEMENT
- **Descrição:** O goal ("Erro útil é feature") pede que cada erro de compilação
  aponte a posição no SQL. Hoje todos os erros de análise de query são
  reportados com `input.sql.span()` — o literal SQL inteiro — em
  [query_macro.rs:219](pgsafe_macros/src/query_macro.rs#L219),
  [query_macro.rs:238](pgsafe_macros/src/query_macro.rs#L238) e
  [query_macro.rs:253](pgsafe_macros/src/query_macro.rs#L253). As variantes
  de `AnalyzeError` em [error.rs](pgsafe_analyzer/src/error.rs) (`UndefinedColumn`,
  `UndefinedTable`, `UndefinedFunction`, `TypeMismatch`, …) não carregam
  offset, embora os nós do AST do `pg_query` exponham `location` (byte offset
  no SQL) e o lexer já rastreie offsets de parâmetros. Implementar:
  - Adicionar um offset de origem opcional (`Option<usize>`, byte offset no SQL
    pós-lexer) às variantes semânticas de `AnalyzeError`, populado a partir do
    `location` do nó do AST que originou o erro.
  - No proc-macro, mapear esse offset para um sub-span do literal SQL (via
    `proc_macro2::Literal::subspan`, com degradação graciosa para
    `input.sql.span()` quando o subspan não estiver disponível), levando em
    conta o ajuste de offset que o lexer aplica ao reescrever `$name` → `$1`.
- **Verificação:** `cargo nextest run --release` permanece verde; inspeção
  manual via `cargo build -p pgsafe_example` após introduzir uma coluna
  inexistente numa query multi-linha — o erro aponta a linha/coluna do token,
  não o literal inteiro.

## Lote 2 — 2026-05-18

**Objetivo do lote:** fechar lacunas concretas de fidelidade ao PostgreSQL
(subscripting de `jsonb`, valores composite como parâmetro), entregar
diagnósticos que apontam a posição no SQL por uma via que funciona em stable, e
cobrir pgvector com um roundtrip e2e contra um Postgres real.

### 2.1 — Subscripting de `jsonb` / `json` (`data['key']`, encadeado)
- **Status:** [x]
- **Tipo:** IMPLEMENT
- **Descrição:** O PostgreSQL 14+ aceita subscripting genérico sobre `jsonb` —
  `data['key']`, `data['key']['nested']`, `data[0]` — e o goal exige "qualquer
  construção que o Postgres aceita". Hoje o analyzer rejeita: o passo `AIndices`
  não-slice em `infer_indirection`
  ([expr.rs:943-963](pgsafe_analyzer/src/expr.rs#L943-L963)) chama
  `resolve_array_element`
  ([expr.rs:1189-1213](pgsafe_analyzer/src/expr.rs#L1189-L1213)), que retorna
  `AnalyzeError::Unsupported("subscript on non-array type 'jsonb'")` para
  qualquer tipo cuja `typcategory` não seja `Array`. Implementar:
  - Em `resolve_array_element` (ou num ramo novo no `AIndices` de
    `infer_indirection`), quando `current.type_oid` for `json`/`jsonb`: o
    resultado de cada passo de subscript é `jsonb` (mesmo sobre `json`, o
    subscript jsonb retorna `jsonb`), sempre nullable (subscript fora de
    chave/índice → NULL). Subscripts encadeados sobre `jsonb` continuam
    `jsonb`. O índice (`ai.uidx`/`ai.lidx`) é coagido a `text` (chave de
    objeto) ou `int4` (índice de array) — o PG aceita ambos; basta inferir os
    bounds sem forçar `int4`, que hoje é o `TypeGoal` fixo em
    [expr.rs:913-921](pgsafe_analyzer/src/expr.rs#L913-L921). Slice
    (`data['a':'b']`) sobre `jsonb` **não** é aceito pelo PG — manter o erro.
  - Atualizar o comentário stale em
    [expr.rs:804-807](pgsafe_analyzer/src/expr.rs#L804-L807), que afirma que
    slicing "falls back to unsupported" embora `arr[1:3]` já seja suportado
    desde [expr.rs:925-942](pgsafe_analyzer/src/expr.rs#L925-L942).
  - Escopo: apenas posição de expressão (SELECT-list, WHERE, etc.). Subscript
    como alvo de `UPDATE ... SET` fica fora deste lote.
  - Cobertura: novos testes em
    [tests/query/json_operators.rs](pgsafe_analyzer/tests/query/json_operators.rs)
    exercitando `SELECT prefs['theme'] FROM users`, subscript encadeado e
    subscript por índice numérico — assertando tipo `jsonb` e nullability.
- **Verificação:** `cargo nextest run --release -p pgsafe_analyzer` — os
  testes novos passam e nenhum outro regride. Com Docker, `feature pg_sanity`
  confirma que o tipo/erro batem com o PG real.

### 2.2 — Erros de query apontam linha/coluna dentro do literal SQL
- **Status:** [ ]
- **Tipo:** IMPLEMENT
- **Descrição:** Releitura da tarefa 1.3, que ficou bloqueada por depender de
  `proc_macro2::Literal::subspan` (feature instável `proc_macro_span`, indisponível
  no toolchain do crate consumidor). O ponteiro de posição **não precisa** de um
  sub-span do compilador: pode viver no **texto da mensagem de erro**, que é
  renderizada pelo `syn::Error` no span do literal inteiro. Isso é entregável em
  stable. Implementar:
  - Adicionar um campo de offset opcional (`Option<usize>`, byte offset no SQL
    pós-lexer) às variantes semânticas de `AnalyzeError`
    ([error.rs](pgsafe_analyzer/src/error.rs): `UndefinedColumn`,
    `UndefinedTable`, `UndefinedFunction`, `UndefinedOperator`, `TypeMismatch`,
    `Invalid`, …), populado a partir do `location` do nó do AST `pg_query` que
    originou o erro. Onde o nó não expõe `location`, o offset fica `None`.
  - No proc-macro ([query_macro.rs:218-219](pgsafe_macros/src/query_macro.rs#L218-L219)
    e os dois outros sites em
    [query_macro.rs:238](pgsafe_macros/src/query_macro.rs#L238) e
    [query_macro.rs:253](pgsafe_macros/src/query_macro.rs#L253)), converter o
    offset pós-lexer de volta para a posição no SQL original (revertendo o
    deslocamento que o lexer aplica ao reescrever `$name` → `$1`) e **anexar à
    mensagem** um trecho `at line N, column M:` seguido da linha de origem com
    um `^` sob o token ofensor. O `input.sql.span()` continua sendo o span do
    `syn::Error`.
  - Garantir que o contrato do `pg_sanity` não quebra: ele exige que a mensagem
    *comece* com a mensagem do PG verbatim — o trecho de posição vai **depois**,
    portanto é "detalhe extra" permitido. Conferir lendo
    [pg_sanity.rs](pgsafe_analyzer/src/pg_sanity.rs) o ponto de match de
    prefixo.
- **Verificação:** `cargo nextest run --release` permanece verde. Inspeção
  manual: introduzir uma coluna inexistente numa query multi-linha em
  `pgsafe_example` e rodar `cargo build -p pgsafe_example` — a mensagem de
  erro mostra a linha/coluna e o trecho com o caret apontando o token.

### 2.3 — Roundtrip e2e de pgvector contra Postgres real
- **Status:** [x]
- **Tipo:** TEST FIRST
- **Descrição:** O goal cita pgvector explicitamente. O analyzer já resolve o
  tipo `vector` e seus operadores de distância (cobertura em
  [tests/ddl/extensions.rs](pgsafe_analyzer/tests/ddl/extensions.rs)) e o
  codegen mapeia `vector`/`halfvec`/`sparsevec` para `::pgvector::Vector` &
  cia. em [pg_type_map.rs:89-91](pgsafe_macros/src/pg_type_map.rs#L89-L91) —
  mas nada exercita esse mapeamento contra um banco real: `pgsafe_e2e/`
  não tem nenhum teste de `vector`. Implementar:
  - Nova migration `pgsafe_e2e/migrations/0006_vectors.sql` com
    `CREATE EXTENSION vector;` e uma tabela com coluna `vector(N)`.
  - Novo arquivo `pgsafe_e2e/tests/pgvector.rs` que sobe seu próprio
    container — a imagem default de
    [tests/common/mod.rs](pgsafe_e2e/tests/common/mod.rs) não traz a
    extensão, então usar a imagem `pgvector/pgvector` via `ImageExt` (mesmo
    padrão de container já presente em `common`). O teste insere e lê de volta
    um `vector`, e exercita `sql!` com um operador de distância (`<->`),
    assertando que o `$param` é inferido como `vector` e o roundtrip do valor
    funciona.
  - Adicionar `pgvector` (com a feature de integração `postgres`) às
    `dependencies` de [pgsafe_e2e/Cargo.toml](pgsafe_e2e/Cargo.toml).
  - Como o teste exige Docker e uma imagem específica, marcá-lo de forma
    consistente com os demais testes e2e do crate (que já são gated por
    Docker), não com `#[ignore]`.
- **Verificação:** `cargo nextest run --release -p pgsafe_e2e --test pgvector`
  (requer Docker) passa; `cargo build -p pgsafe_e2e` continua verde sem
  Docker.

### 2.4 — Valores composite/record aceitos como parâmetro de query
- **Status:** [ ]
- **Tipo:** IMPLEMENT
- **Descrição:** O goal pede "qualquer construção que o Postgres aceita" — e o
  PG aceita `WHERE addr = $1` com `$1` de tipo composite. Hoje o codegen
  rejeita: `reject_record_param`
  ([codegen.rs:577-585](pgsafe_macros/src/codegen.rs#L577-L585)) devolve o
  erro `"composite / record-typed query parameters are not supported …"` sempre
  que um parâmetro é tipado como `Record`/`VecOfRecord`. O analyzer já infere o
  tipo composite do parâmetro corretamente; falta só o lado do codegen. A
  struct sintetizada para o composite já é gerada (Lote 1, tarefa 1.2).
  Implementar:
  - **Primeiro passo — decidir o mecanismo:** investigar se a struct
    sintetizada pode derivar `postgres_types::ToSql` (com `#[postgres(name =
    "<schema.tipo>")]`) ou se é preciso emitir um `impl ToSql` manual escrevendo
    o formato wire binário de composite (`int32` nfields + por campo: OID,
    tamanho, dados). Registrar a decisão num comentário no codegen.
  - Aceitar composites **nomeados** como parâmetro (anonymous records não têm
    tipo PG concreto para vincular um `$param`, então continuam rejeitados — com
    mensagem ajustada). Quando o composite tem override `[types]`, o valor do
    usuário precisa ser convertido para a struct sintetizada antes do encode,
    espelhando o caminho de decode field-by-field já existente.
  - Substituir o erro de `reject_record_param` pela geração do parâmetro; manter
    a mensagem (atualizada) apenas para o caso de anonymous record.
  - Cobertura: teste de compile-time em
    [tests/query/records.rs](pgsafe_analyzer/tests/query/records.rs) ou
    em `pgsafe_macros` confirmando que `WHERE composite_col = $1` compila; e
    estender [pgsafe_e2e/tests/composites.rs](pgsafe_e2e/tests/composites.rs)
    com um roundtrip que passa um composite como parâmetro e o lê de volta.
- **Verificação:** `cargo nextest run --release` (compile-time verde) +
  `cargo nextest run --release -p pgsafe_e2e --test composites` (requer
  Docker) — o parâmetro composite faz roundtrip.
