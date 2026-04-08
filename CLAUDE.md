# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test Commands

```bash
cargo build                            # build all crates
cargo build -p cubos_sql               # build specific crate
cargo test                             # all tests (no Docker needed)
cargo test -p cubos_sql_core           # core crate only
cargo test -p cubos_sql_macros         # macro crate only
cargo test -p cubos_sql_analyzer       # analyzer crate only
cargo test -p cubos_sql                # runtime crate only
cargo test --test migrate_integration  # integration tests (requires Docker)
cargo test -- test_name                # run a single test by name
```

All compile-time tests run without Docker. Integration tests for the runtime migration runner use `testcontainers-modules` and require a running Docker daemon.

## Architecture

Workspace with crates:

```
cubos_sql_cli (binary, stub)
    └── cubos_sql (runtime: Pool, Executor, migrate)
            ├── cubos_sql_core (shared: config, lexer, param types, type_map)
            └── cubos_sql_macros (proc macro: sql!)
                    ├── cubos_sql_core
                    └── cubos_sql_analyzer (static SQL type/nullability analyzer)
```

### Compile-time pipeline (`sql!` macro)

1. Parse macro input: `sql!(executor, "SQL with $params", name = value)`
2. Lex SQL via `cubos_sql_core::lexer::lex()` — rewrites `$name` → `$1`, extracts `$..spread`
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
