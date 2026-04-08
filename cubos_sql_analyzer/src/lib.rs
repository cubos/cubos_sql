//! Static SQL type and nullability analyzer for `cubos_sql`.
//!
//! This crate determines query parameter types, output column types, and
//! nullability by statically analyzing SQL against an in-memory schema
//! snapshot built from migration files via the DDL interpreter.
//!
//! # How it works
//!
//! 1. **Schema construction**: [`seed::build_schema_from_migrations`] parses
//!    migration SQL files using `pg_query` and applies DDL statements to build
//!    a [`schema::SchemaSnapshot`] in memory — no running PostgreSQL needed.
//!
//! 2. **Static analysis**: [`resolve::analyze`] parses a query using `pg_query`
//!    and walks the AST, resolving types and nullability against the snapshot.
//!
//! The analyzer produces a [`cubos_sql_core::query_info::QueryInfo`] with
//! precise nullability tracking (e.g., LEFT JOIN nullability, expression-level
//! NOT NULL via COALESCE/COUNT/CASE).

pub mod coerce;
pub mod ddl;
pub mod error;
pub mod expr;
pub mod functions;
pub mod nullability;
pub mod params;
pub mod resolve;
pub mod schema;
pub mod scope;
pub mod seed;
