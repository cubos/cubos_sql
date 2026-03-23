//! Static SQL type and nullability analyzer for `cubos_sql`.
//!
//! This crate provides an alternative to live PostgreSQL introspection for
//! determining query parameter types, output column types, and nullability.
//!
//! # How it works
//!
//! 1. **Schema export**: [`export::export_schema`] queries `pg_catalog` once
//!    (post-migration) and produces a serializable [`schema::SchemaSnapshot`].
//!
//! 2. **Static analysis**: [`resolve::analyze`] parses SQL using `pg_query`
//!    and walks the AST, resolving types and nullability against the snapshot.
//!
//! The analyzer produces the same [`cubos_sql_core::query_info::QueryInfo`] as
//! the live introspection path, but with more precise nullability (e.g., it
//! correctly tracks LEFT JOIN nullability and expression-level NOT NULL).

pub mod coerce;
pub mod error;
pub mod export;
pub mod expr;
pub mod functions;
pub mod introspect;
pub mod nullability;
pub mod params;
pub mod resolve;
pub mod schema;
pub mod scope;
