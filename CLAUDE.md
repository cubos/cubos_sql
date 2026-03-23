# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test Commands

```bash
cargo build                            # build all crates
cargo build -p cubos_sql               # build specific crate
cargo test                             # all unit tests (no Docker needed)
cargo test -p cubos_sql_core           # core crate only
cargo test -p cubos_sql_macros         # macro crate only
cargo test -p cubos_sql                # runtime crate only
cargo test --test migrate_integration  # integration tests (requires Docker)
cargo test -- --ignored                # Docker-dependent tests
cargo test -- test_name                # run a single test by name
```

Unit tests (lexer, config, type_map, cache) run without Docker. Integration tests use `testcontainers-modules` and require a running Docker daemon.

## Architecture

Workspace with 4 crates:

```
cubos_sql_cli (binary, stub)
    └── cubos_sql (runtime: Pool, Executor, migrate)
            ├── cubos_sql_core (shared: config, lexer, param types, type_map)
            └── cubos_sql_macros (proc macro: query!)
                    └── cubos_sql_core
```

### Compile-time pipeline (`query!` macro)

1. Parse macro input: `query!(executor, "SQL with $params", name = value)`
2. Lex SQL via `cubos_sql_core::lexer::lex()` — rewrites `$name` → `$1`, extracts `$..spread`
3. Load config from `[package.metadata.cubos_sql]` in consumer's `Cargo.toml`
4. `docker::ensure_container()` — hash migrations, spin up/reuse PG container, run migrations
5. `cache::get()` — check `.cubos_sql/<hash>/queries/<query_hash>.json`
6. `introspect::introspect_query()` — `PREPARE` + `pg_catalog` lookup for param/column types and nullability
7. `codegen::generate()` — emit anonymous output struct, typed query builder, `.fetch_all()/.fetch_one()/.fetch_optional()/.execute()` methods

**Key decision**: proc macro uses sync `postgres` crate (not `tokio-postgres`) because proc macros run synchronously at compile time. Runtime uses `tokio-postgres` + `deadpool-postgres`.

### Runtime

- `Pool` wraps `deadpool-postgres`, constructed from a connection URL
- `Executor` trait implemented for `Pool`, `&Pool`, `Client`, `Transaction<'_>`
- Migration runner uses advisory locks (`pg_advisory_lock`) and per-migration transactions (opt-out via `-- no-transaction` first line)

### Local state (`.cubos_sql/` directory)

- `.cubos_sql/<migration_hash>/container.json` — Docker container info (survives `cargo clean`)
- `.cubos_sql/<migration_hash>/queries/<hex>.json` — cached query introspection results
- `.cubos_sql/<migration_hash>/lock` — file lock preventing parallel macro race conditions

## Configuration

Users configure via `[package.metadata.cubos_sql]` in their `Cargo.toml`:

```toml
[package.metadata.cubos_sql.database]
docker_image = "postgres:16"    # default: "postgres"
migrations = "./migrations"

[package.metadata.cubos_sql.migrations]
table = "public._migrations"
lock_id = 713705
use_transaction = true

[package.metadata.cubos_sql.domains]
user_preferences = "crate::domains::UserPreferences"
```

## Implementation Status

Phase 1 is complete (config, lexer, migrations, integration tests). The `query!` macro is wired end-to-end but Docker/introspection/codegen paths are in progress. CLI is a stub. Full spec in `cubos_sql.md`, detailed plan in `ARCHITECTURE.md`.
