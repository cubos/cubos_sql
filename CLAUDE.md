# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test Commands

Always use `cargo nextest run --release` instead of `cargo test` — nextest
aggregates results across test binaries into a single summary, and `--release`
runs *much* faster end-to-end on this workspace (the analyzer's DDL/query test
suite is heavy on parsing + interpretation; the optimized binary saves more
than the extra build time costs).

```bash
cargo build                                          # build all crates
cargo build -p cubos_sql                             # build specific crate
cargo nextest run --release                          # all tests (no Docker needed)
cargo nextest run --release -p cubos_sql_core        # core crate only
cargo nextest run --release -p cubos_sql_macros      # macro crate only
cargo nextest run --release -p cubos_sql_analyzer    # analyzer crate only
cargo nextest run --release -p cubos_sql             # runtime crate only
cargo nextest run --release --test migrate_integration  # integration tests (requires Docker)
cargo nextest run --release test_name                # run a single test by name
```

All compile-time tests run without Docker. Integration tests for the runtime migration runner use `testcontainers-modules` and require a running Docker daemon.

Note: doctests are not supported by nextest — for those, fall back to `cargo test --doc`.

## Regenerating `seed.json`

Never hand-migrate `cubos_sql_analyzer/src/seed.json` (e.g. with a Python
script) when changing catalog struct shapes. The seed loader tolerates empty
or stale seeds — `cubos_sql_seed` exports the catalog from a live PG via
testcontainers and overwrites the file. Always regenerate by running:

```bash
cargo run -p cubos_sql_seed   # requires Docker; takes ~10 seconds
```

Hand-rewriting the seed risks subtle FK drift (e.g. an old aggfinaltype
field repurposed as aggfinalfn would store pg_type oids where pg_proc oids
are expected).

## Architecture

Workspace with crates:

```
cubos_sql_cli (binary, stub)
    └── cubos_sql (runtime: Pool, Executor, migrate)
            ├── cubos_sql_core (shared config only — kept small so runtime does not pull pg_query)
            └── cubos_sql_macros (proc macro: sql!)
                    ├── cubos_sql_core
                    └── cubos_sql_analyzer (compile-time only: lexer, param types, query_info, type_map, static SQL analyzer)
```

### Compile-time pipeline (`sql!` macro)

1. Parse macro input: `sql!(executor, "SQL with $params", name = value)`
2. Lex SQL via `cubos_sql_analyzer::lexer::lex()` — rewrites `$name` → `$1`, extracts `$..spread`
3. Load config from `[package.metadata.cubos_sql]` in consumer's `Cargo.toml`
4. Build schema snapshot from seed + migrations via DDL interpreter (in-memory, no Docker)
5. Static analysis: parse SQL with `pg_query`, resolve types and nullability against snapshot
6. `codegen::generate()` — emit anonymous output struct, typed query builder, `.fetch_all()/.fetch_one()/.fetch_optional()/.execute()` methods

### Runtime

- `Pool` wraps `deadpool-postgres`, constructed from a connection URL
- `Executor` trait implemented for `Pool`, `&Pool`, `Client`, `Transaction<'_>`
- Migration runner uses advisory locks (`pg_advisory_lock`) and per-migration transactions (opt-out via `-- no-transaction` first line)

## Configuration

Users configure via `[package.metadata.cubos_sql]` in their `Cargo.toml`:

```toml
[package.metadata.cubos_sql.database]
migrations = "./migrations"

[package.metadata.cubos_sql.migrations]
table = "public._migrations"
lock_id = 713705
use_transaction = true

[package.metadata.cubos_sql.domains]
user_preferences = "crate::domains::UserPreferences"
```

## Implementation Status

The `sql!` macro is wired end-to-end with static analysis (no Docker needed at compile time). CLI is a stub. Full spec in `cubos_sql.md`, detailed plan in `ARCHITECTURE.md`.
