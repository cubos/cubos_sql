# cubos_sql — Arquitetura e Plano de Implementacao

## Estrutura de Crates (Workspace)

```
cubos_sql/
├── Cargo.toml                  # workspace root
├── cubos_sql/                  # crate principal (re-export + runtime)
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs              # re-exports publicos
│       ├── pool.rs             # Pool wrapper (thin layer sobre tokio-postgres)
│       ├── transaction.rs      # Transaction wrapper
│       ├── executor.rs         # trait Executor (Pool | Transaction)
│       ├── row.rs              # trait/helpers para row mapping
│       ├── error.rs            # CubosError enum
│       ├── types.rs            # conversoes de tipos PG <-> Rust
│       └── migrate/
│           ├── mod.rs
│           ├── runner.rs       # run(), status(), revert()
│           └── source.rs       # leitura e ordenacao de arquivos .sql
├── cubos_sql_core/             # tipos compartilhados entre runtime e macro
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── config.rs           # parse de [package.metadata.cubos_sql] do Cargo.toml
│       ├── lexer.rs            # lexer SQL para extracao de $params e $..spreads
│       ├── param.rs            # tipos Param, SpreadParam
│       └── type_map.rs         # mapeamento tipo PG OID -> tipo Rust
├── cubos_sql_macros/           # proc macro crate
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs              # entry point do proc macro
│       ├── query_macro.rs      # parse + expansao do query!
│       ├── docker.rs           # gerenciamento do container PG em compile-time
│       ├── introspect.rs       # PREPARE + extracao de tipos via PG
│       ├── codegen.rs          # geracao de codigo Rust (struct anonima, binds)
│       └── spread.rs           # logica de expansao do $..spread
├── cubos_sql_cli/              # binario CLI
│   ├── Cargo.toml
│   └── src/
│       └── main.rs             # cubos_sql migrate {run,status,revert}
└── tests/
    └── fixtures/
        └── migrations/         # migrations usadas nos testes de integracao
            ├── 0001_create_users.sql
            └── 0002_create_orders.sql
```

## Configuracao do Projeto Usuario

A config fica dentro do `Cargo.toml` do projeto que usa a lib, via `[package.metadata]`:

```toml
[package.metadata.cubos_sql]
docker_image = "postgres:16"
migrations = "./migrations"

[package.metadata.cubos_sql.domains]
user_preferences = "crate::domains::UserPreferences"
order_metadata = "crate::domains::OrderMetadata"
```

O proc macro le o `Cargo.toml` via `CARGO_MANIFEST_DIR` e extrai a secao `[package.metadata.cubos_sql]`.

Em runtime, a conexao usa a env var `DATABASE_URL`.

## Dependencias Principais

| Crate             | Dependencias                                                       |
|-------------------|--------------------------------------------------------------------|
| `cubos_sql`       | `tokio-postgres`, `deadpool-postgres`, `tokio`, `cubos_sql_core`   |
| `cubos_sql_core`  | `toml`, `serde`, `serde_json`                                     |
| `cubos_sql_macros`| `proc-macro2`, `quote`, `syn`, `cubos_sql_core`, `postgres` (sync)|
| `cubos_sql_cli`   | `cubos_sql`, `tokio`, `clap`                                      |

Notas:
- O proc macro usa `postgres` (sync, nao tokio) pois proc macros rodam em contexto sincrono.
- `deadpool-postgres` para connection pooling no runtime.
- O container Docker em compile-time usa `std::process::Command` (sem dep extra).

## Componentes-Chave

### 1. Lexer SQL (`cubos_sql_core::lexer`)

Maquina de estados que rastreia contexto (string, comentario, etc.) e extrai:
- `$param` -> `Param { name, position }`
- `$..spread` -> `SpreadParam { name, position }`
- `$..spread { field1, field2 }` -> `SpreadParam { name, fields, position }`

Output: SQL reescrito com `$1, $2, ...` + lista de parametros extraidos.

### 2. Config (`cubos_sql_core::config`)

Parse de `[package.metadata.cubos_sql]` do `Cargo.toml`:
```rust
pub struct Config {
    pub database: DatabaseConfig,
    pub domains: HashMap<String, String>,  // domain_name -> rust_type_path
}

pub struct DatabaseConfig {
    pub docker_image: String,
    pub migrations: PathBuf,
}
```

O proc macro localiza o `Cargo.toml` via `CARGO_MANIFEST_DIR`, faz parse do TOML inteiro e extrai `package.metadata.cubos_sql`.

### 3. Docker Manager (`cubos_sql_macros::docker`)

- Calcula hash SHA-256 do diretorio de migracoes
- Verifica se container com label `cubos_sql_hash=<hash>` existe e esta rodando
- Se nao: sobe novo container, roda migracoes, aplica label
- Retorna connection string para o container
- Usa arquivo de lock para evitar race conditions entre invocacoes paralelas do macro

### 4. Introspeccao (`cubos_sql_macros::introspect`)

- Conecta ao PG (sync) com a connection string do Docker Manager
- `PREPARE` a query reescrita
- Extrai tipos de input (parametros) e output (colunas) via `pg_catalog`
- Retorna `QueryInfo { params: Vec<ParamInfo>, columns: Vec<ColumnInfo> }`

### 5. Codegen (`cubos_sql_macros::codegen`)

Gera:
- Struct anonima para o resultado (campos tipados)
- Codigo de bind dos parametros (com conversao de tipo)
- Para domains JSONB: wrapping com `serde_json::to_value()` / `serde_json::from_value()`
- Metodos `.fetch_all()`, `.fetch_one()`, `.fetch_optional()`, `.execute()`

### 6. Executor Trait (`cubos_sql::executor`)

```rust
pub trait Executor {
    async fn query_raw(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<Row>>;
    async fn execute_raw(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64>;
}
```

Implementado por `Pool` e `Transaction`, permitindo que `query!` aceite ambos.

### 7. Migration Runner (`cubos_sql::migrate`)

- Tabela `_cubos_sql_migrations` para tracking
- `run()`: aplica pendentes em ordem
- `status()`: lista todas com estado (applied/pending)
- `revert()`: reverte a ultima (requer arquivo de rollback ou wrapper em transaction)

## Mapeamento de Tipos PG -> Rust

| PG Type          | Rust Type                   |
|------------------|-----------------------------|
| BIGINT/INT8      | i64                         |
| INT/INT4         | i32                         |
| SMALLINT/INT2    | i16                         |
| TEXT/VARCHAR      | String                      |
| BOOL             | bool                        |
| FLOAT4           | f32                         |
| FLOAT8           | f64                         |
| TIMESTAMPTZ      | chrono::DateTime<Utc>       |
| TIMESTAMP        | chrono::NaiveDateTime        |
| DATE             | chrono::NaiveDate            |
| UUID             | uuid::Uuid                  |
| JSONB/JSON       | serde_json::Value            |
| BYTEA            | Vec<u8>                     |
| NUMERIC          | rust_decimal::Decimal        |
| Domain(JSONB)    | tipo mapeado em Cargo.toml  |

Colunas nullable (detectadas via introspeccao) sao wrappadas em `Option<T>`.

---

## Entregaveis Incrementais

### Fase 1 — Fundacao (sem macro)

**1.1 — Workspace + Config**
- Criar workspace com os 4 crates (stubs)
- Implementar parse de `[package.metadata.cubos_sql]` do Cargo.toml (`cubos_sql_core::config`)
- Testes unitarios para config

**1.2 — Lexer SQL**
- Implementar lexer com maquina de estados
- Extracao de `$param` e `$..spread { fields }`
- Reescrita para `$1, $2, ...`
- Testes unitarios extensivos (strings, comentarios, dollar-quoting, edge cases)

**1.3 — Migration Runner**
- Leitura e ordenacao de arquivos de migracao
- Tabela `_cubos_sql_migrations`
- `run()`, `status()`, `revert()`
- Testes de integracao com testcontainers + Postgres

### Fase 2 — Compile-time Infrastructure

**2.1 — Docker Manager**
- Hash de migracoes
- Subir/reutilizar container PG
- Rodar migracoes no container
- Testes de integracao

**2.2 — Introspeccao de Queries**
- Conectar ao PG, PREPARE, extrair tipos
- Mapeamento OID -> tipo Rust
- Deteccao de nullability
- Resolucao de domains
- Testes de integracao

### Fase 3 — Proc Macro (MVP)

**3.1 — query! basico**
- Parse da invocacao do macro (pool, sql, params)
- Integracao com lexer + docker + introspeccao
- Geracao de struct anonima + metodos fetch
- Parametros nomeados com atribuicao explicita
- Testes de integracao (compilacao real com PG)

**3.2 — Captura de escopo + Executor trait**
- `$var` sem atribuicao captura do escopo
- Trait Executor para Pool e Transaction
- Testes

### Fase 4 — Features Avancadas

**4.1 — Domain types (JSONB)**
- Leitura de `[domains]` do Cargo.toml
- Serializacao/deserializacao automatica
- Validacao de tipo em compile-time
- Testes

**4.2 — Bulk insert ($..spread)**
- Suporte a tuplas (posicional)
- Suporte a structs com mapping `{ field1, field2 }`
- Expansao de VALUES para N rows
- Erro claro quando mapping falta
- Testes

### Fase 5 — CLI e Polish

**5.1 — CLI**
- `cargo sql migrate run`
- `cargo sql migrate status`
- `cargo sql migrate revert`

**5.2 — Erros amigaveis**
- Mensagens de erro com span correto (aponta para o trecho do SQL)
- Sugestoes de correcao

**5.3 — Cache e Performance**
- Cache de container com hash de migracoes
- Reuso de conexao entre invocacoes do macro no mesmo build
- Arquivo de lock para builds paralelos

### Fase 6 — Extras

**6.1 — VSCode Extension**
- Grammar injection para SQL dentro de `query!`
- Highlight de `$param` e `$..spread`

---

## Estrategia de Testes

| Tipo                | Ferramenta        | O que testa                                    |
|---------------------|-------------------|------------------------------------------------|
| Unitario            | `#[test]`         | Lexer, config parse, type mapping, codegen     |
| Integracao (runtime)| testcontainers-rs | Migration runner, queries reais contra PG      |
| Integracao (macro)  | trybuild          | Compilacao de query!, erros esperados           |
| Integracao (macro)  | testcontainers-rs | query! com PG real em compile+runtime           |

### testcontainers setup

```rust
use testcontainers::{clients::Cli, images::postgres::Postgres};

#[tokio::test]
async fn test_migration_run() {
    let docker = Cli::default();
    let pg = docker.run(Postgres::default());
    let port = pg.get_host_port_ipv4(5432);
    let conn_str = format!("host=localhost port={port} user=postgres password=postgres dbname=postgres");
    // ... test logic
}
```

### trybuild (para testar erros de compilacao)

```rust
#[test]
fn compile_tests() {
    let t = trybuild::TestCases::new();
    t.pass("tests/compile/pass/*.rs");
    t.compile_fail("tests/compile/fail/*.rs");
}
```
