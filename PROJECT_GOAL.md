# Goal — pgsafe

## Visão

Uma lib Rust que entrega **acesso tipado a PostgreSQL com verificação 100% em
compile-time**, sem Docker, sem servidor externo, sem subset restrito de SQL.
O usuário escreve SQL real; a macro `sql!` lê as migrations do projeto, monta
o schema em memória e valida cada query — tipos, nullability, parâmetros,
coerções — antes do binário existir.

## O objetivo

O coração do projeto é a macro `sql!`. Quando o usuário escreve uma query, a
macro lê as migrations do projeto, reconstrói o schema do Postgres em memória
e analisa a query estaticamente. O resultado dessa análise tem que ser
**indistinguível do que o Postgres real faria** ao processar a mesma query
naquele schema:

- Para **qualquer query que o Postgres aceitaria** — qualquer construção da
  linguagem que o parser do Postgres reconhece e que o planejador do Postgres
  aceita executar — o analisador do `pgsafe` aceita também e chega ao
  mesmo resultado de tipagem que o Postgres chegaria. Mesmas regras de
  promoção, mesmas regras de coerção, mesmas resoluções de operador e
  função, mesmas decisões de common-type em `CASE`/`COALESCE`/`UNION`. Os
  tipos não são *parecidos* com os que o Postgres daria; são os mesmos —
  ou, em alguns casos, **mais precisos** (ver "Onde a gente vai além do
  Postgres" abaixo).

- Para **qualquer query que o Postgres rejeitaria** — quer o erro venha do
  parser, quer venha da análise semântica (`parse_analyze`), quer venha do
  planejador — o analisador do `pgsafe` também rejeita, em compile-time,
  com uma mensagem que aponta o problema na origem. Coluna inexistente,
  função inexistente, tipo incompatível, ambiguidade de overload, referência
  fora de escopo, função set-returning em posição inválida: tudo que o
  Postgres não engole, a macro não engole.

A vinculação ao comportamento do Postgres é literal, não aspiracional.
Sempre que o projeto precisar de um algoritmo de resolução de tipos —
coerção implícita, escolha de overload de função, common-type entre dois
tipos, resolução de cast, resolução de operador — a implementação **reproduz
o algoritmo do Postgres operando sobre o catálogo do Postgres**, não uma
heurística próxima. O catálogo (tipos, funções, operadores, casts,
agregados) é a fonte de verdade do analisador, exportado de uma instância
real e embutido no projeto.

## O catálogo como espelho do Postgres

A consequência arquitetural da fidelidade é que o analisador carrega
internamente uma representação do `pg_catalog` **estruturalmente idêntica
à do Postgres**: as mesmas relações (`pg_type`, `pg_proc`, `pg_operator`,
`pg_cast`, `pg_aggregate`, `pg_class`, `pg_attribute`, `pg_namespace`,
`pg_collation`, ...), com as mesmas colunas e os mesmos significados que
existem no source-tree do Postgres. Não há tradução para uma estrutura
"amigável ao Rust" que renomeia, agrupa ou colapsa campos — `typcategory`,
`typispreferred`, `typelem`, `proargtypes`, `proallargtypes`,
`proargmodes`, `proretset`, `castcontext`, `castmethod`, `oprcode`,
`aggtransfn`, `aggfinalfn` aparecem com o mesmo nome e a mesma semântica
que têm no catálogo real.

É essa equivalência estrutural que torna possível a reprodução literal
dos algoritmos. Quando o analisador escolhe um overload de função, ele
percorre `pg_proc` e aplica `func_select_candidate` operando sobre os
mesmos campos que o Postgres usa; quando resolve um cast, consulta
`pg_cast.castcontext` e `pg_cast.castmethod` diretamente; quando computa
common-type para `CASE`/`COALESCE`/`UNION`, lê `typcategory` e
`typispreferred` da mesma maneira que `select_common_type` lê; quando
resolve um operador, percorre `pg_operator` por `oprname`/`oprleft`/
`oprright` como `oper_select_candidate` faz. Cada rotina é portada
contra a estrutura original — não contra um modelo paralelo que
reinterpreta a informação. As colunas do catálogo são a fonte de
verdade; as funções de resolução são consumidoras dessa estrutura, do
mesmo jeito que são no Postgres.

Divergências em relação ao layout do Postgres aparecem **só onde o
ambiente de compile-time exige** e ficam confinadas ao mínimo: OIDs
sintéticos a partir de 100_000 para objetos criados pelas migrations do
usuário, elisão de campos relevantes apenas ao executor (estatísticas de
planner, paths de storage, TOAST, visibilidade MVCC) e adaptações
pontuais de serialização para o `seed.json`. Nomes de relação, nomes de
coluna, tipos de cada coluna, foreign keys entre catálogos e convenções
de NULL seguem o Postgres. Se a rotina do Postgres lê `pg_type.typelem`
para achar o tipo de elemento de um array, a rotina correspondente no
analisador lê o campo de mesmo nome na mesma relação. A regra é: quando
em dúvida sobre como modelar algo no catálogo interno, copia o Postgres.

## Onde a gente vai além do Postgres

Fidelidade ao Postgres é o piso da análise — não o teto. O Postgres opera
sob restrições do protocolo de wire e do executor: os tipos que ele expõe
no `RowDescription` colapsam informação que está disponível em
compile-time mas que ele não tem como (ou interesse em) carregar. O
`pgsafe` não tem esse limite. Sempre que a análise estática puder
**preservar mais informação do que o Postgres surfaceia**, sem violar a
fidelidade ao comportamento do Postgres, ela preserva.

**Nullability mais precisa.** O Postgres não distingue entre "tecnicamente
pode ser NULL nesse contexto" e "vai ser NULL na prática"; o `pgsafe`
distingue, e usa essa distinção para gerar `T` em vez de `Option<T>`
sempre que puder provar que o NULL não é alcançável. Isso vale para o lado
da leitura (colunas e expressões da SELECT-list) e para o lado da escrita
(parâmetros). Uma coluna NOT NULL atrás de um `LEFT JOIN` que sempre casa
ainda é nullable do ponto de vista do schema, mas o desenvolvedor pode
dizer isso com uma anotação e a macro confia. `COALESCE` com fallback
non-null elimina a nullability. `COUNT(*)` nunca é null. Um agregado sem
`GROUP BY` em tabela potencialmente vazia é. Um parâmetro que alimenta uma
coluna NOT NULL exige `T`; um que alimenta uma coluna nullable aceita
`Option<T>`. O `Option` só aparece quando o `None` é um caso real que o
código de chamada precisa tratar.

**Identidade de domain preservada.** Quando uma coluna ou expressão é
tipada como um domain do usuário, o `pgsafe` carrega a identidade do
domain através da análise — não colapsa cedo para o tipo base. Isso
permite mapear `CREATE DOMAIN user_id AS BIGINT` para um newtype Rust
distinto de `i64`, dando type safety nominal que o Postgres não tem como
expressar. O comportamento de coerção continua o do Postgres (um domain é
implicitamente compatível com seu tipo base nos lugares onde o Postgres
diz que é); o que muda é o tipo Rust resultante.

**Estrutura de record analisada em profundidade.** Quando uma expressão
produz um record (subquery em `FROM`, função composite, `ROW(...)`, tabela
em posição de valor), o Postgres muitas vezes degenera o tipo para
`record` opaco no protocolo. A análise estática reconstrói os campos
nomeados e seus tipos individuais a partir do AST e do schema, e expõe a
estrutura inteira para a macro gerar uma struct Rust com fields tipados
corretamente, em vez de um blob anônimo.

A regra geral: **onde o Postgres *poderia* ter sido mais específico mas
não foi** — porque o protocolo não comporta, porque o executor não
precisa, porque o catálogo guarda só o tipo base — o `pgsafe` é. A
fidelidade ao Postgres governa o que a query *significa*; a precisão extra
governa como esse significado é apresentado em Rust.

## DX como produto

A macro `sql!` é a interface pública do projeto, e a experiência de usá-la
é o produto. Isso impõe três coisas:

**Erro útil é feature.** Toda condição que vira erro de compilação aponta
arquivo, posição no SQL quando aplicável, e descreve a causa em uma frase
clara. Stack trace de proc-macro vazando para o usuário é regressão.

**Inferência puxa pelo contexto, não pela sorte.** Tipo de parâmetro vem da
posição sintática em que ele aparece — coluna alvo de `UPDATE`, operando de
operador conhecido, argumento de função resolvida — usando as mesmas regras
que o Postgres usa para inferir tipo de placeholder. O usuário não escreve
`$id::int4`; a macro descobre.

**SQL real, sem dialeto.** Qualquer construção que o Postgres aceita, a
macro aceita. CTEs recursivos, window functions, `LATERAL`, `DISTINCT ON`,
`RETURNING`, set-returning functions em qualquer posição válida, `FOR
UPDATE`, expressões compostas — não há subset, não há "feature suportada".
Se compila no Postgres, compila aqui.

## Cobertura de tipos

O suporte a tipos não é uma lista; é uma promessa: **qualquer tipo que
exista no schema do usuário tem mapeamento Rust definido**. Built-ins do
Postgres têm mapeamento canônico embutido. Domains preservam identidade
ao longo da análise e podem ser mapeados para newtypes Rust pelo usuário;
domains sobre JSONB viram structs Rust com serde quando declarados.
Enums viram `String` por default e enum Rust quando declarados. Tipos
compostos preservam identidade nominal e shape, e records anônimos
preservam shape. Extensões instaladas via `CREATE EXTENSION` — incluindo
pgvector, com seus operadores de distância — entram no catálogo da
análise como entram no Postgres. O que ainda não tem mapping built-in, o
usuário aponta para o tipo Rust em `Cargo.toml` e a macro respeita.

## Runtime, CLI, migrations

O runtime existe para servir o que a macro gera: `Pool` sobre
`deadpool-postgres` (ou `bb8` opt-in), trait `Executor` que aceita pool,
client e transaction de forma uniforme, structs concretas sem reflection
nem acesso por string. Zero overhead — a macro gera código que um humano
escreveria à mão.

Migrations são `.sql` files versionados pelo usuário. O migration runner
aplica em ordem, com advisory lock para deploys concorrentes e
transação-por-arquivo (com opt-out explícito quando o conteúdo exige).
A CLI (`cargo sql migrate ...`) é a interface ergonômica para criar,
aplicar, reverter e inspecionar status; lê config do `Cargo.toml` e
conecta via `DATABASE_URL`.

Tanto o runtime quanto a CLI são ferramentas em volta da macro — a
ambição deles é serem corretos e invisíveis, não serem extensos.

## Restrições não-negociáveis

- **Postgres only.** Sem abstração para outros bancos. Sintaxe específica
  do Postgres é feature, não bug.
- **Sem Docker em build-time.** Análise é puramente estática, alimentada
  por seed do catálogo + migrations do projeto.
- **Spec/design docs em PT-BR; código, identificadores, commits e ADRs em
  inglês.**
- **`cargo nextest run --release` é o test runner.** Doctests caem em
  `cargo test --doc` por limitação do nextest.
- **`postgres` (sync) no proc macro; `tokio-postgres` + `deadpool-postgres`
  no runtime.** Sem mistura.
- **Estado coordenado via Postgres**, nunca em memória do processo (vale
  para o migration runner — advisory locks são a fonte de verdade).

## Princípios de design

- **Fidelidade ao Postgres como contrato.** Onde a gente reimplementa um
  algoritmo do Postgres (resolução de tipo, escolha de overload, regras de
  coerção), a gente reproduz o algoritmo, não aproxima.
- **Erros antes do binário.** Cada bug que dá pra pegar em compile-time
  deve ser pego. Runtime errors são reservados a coisas que só o banco
  real pode dizer.
- **Sem complexidade especulativa.** Sem feature flag para futuro
  hipotético, sem abstração prematura, sem comentário óbvio. Refatorar é
  parte do trabalho — quando o código pede limpeza, limpa.

## Fora de escopo

Suporte a outros bancos. Query builder programático (a interface é SQL
textual). ORM. Geração automática de DDL a partir de structs Rust —
migrations continuam sendo arquivos `.sql` mantidos pelo usuário.
