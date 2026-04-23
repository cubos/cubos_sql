//! Static SQL type and nullability analyzer for `cubos_sql`.
//!
//! This crate does at compile time what a live PostgreSQL connection would do
//! at runtime: it understands a SQL template's parameter types, its output
//! columns, and the nullability of both — without requiring Docker or a
//! running database.
//!
//! # Public surface
//!
//! The entry point is [`Database`]. Everything else is either returned by
//! [`Database::analyze`] or used to configure it:
//!
//! | Item | Role |
//! |------|------|
//! | [`Database`] | Mutable schema: seed the PG18 catalog, then apply DDL and analyze queries against it. |
//! | [`AnalyzerConfig`] | Rust-type overrides for user-defined SQL types (domains, enums, custom types). |
//! | [`AnalyzedQuery`] | Result of analysis: rewritten SQL + typed parameters, spreads, and output columns. |
//! | [`AnalyzedParam`] | A named parameter with its inferred Rust type and the byte offsets where it appears. |
//! | [`AnalyzedSpread`] | A `$..name { ... }` spread with its insertion offset and typed fields. |
//! | [`AnalyzedSpreadField`] | A single field inside a spread. |
//! | [`AnalyzedColumn`] | A single output column: name, Rust type, nullability. |
//! | [`AnalyzeError`] | Errors returned by [`Database::analyze`]. |
//! | [`DdlError`] | Errors returned by [`Database::apply_sql`]. |
//!
//! # Typical flow
//!
//! ```ignore
//! let mut db = Database::new();
//! db.apply_sql("CREATE TABLE users (id bigint primary key, name text not null);")?;
//! let result = db.analyze(
//!     "SELECT id, name FROM users WHERE id = $id",
//!     &AnalyzerConfig::default(),
//! )?;
//! // result.columns[0].rust_type == "i64"
//! // result.params[0].rust_type  == "i64"
//! ```

mod coerce;
mod database;
mod ddl;
mod error;
mod expr;
mod functions;
mod lexer;
mod nullability;
mod param;
mod param_collector;
mod resolve;
mod scope;
mod seed;
mod types;

/// Re-exports of types defined in `cubos_sql_core` but used pervasively by
/// the analyzer. Kept here so downstream crates (and tests) can depend only
/// on `cubos_sql_analyzer`.
pub(crate) mod qualified_name {
    pub use cubos_sql_core::QualifiedName;
}

#[cfg(any(test, feature = "internal"))]
pub mod schema;
#[cfg(not(any(test, feature = "internal")))]
mod schema;

pub use cubos_sql_core::{ParseQualifiedNameError, QualifiedName};
pub use database::Database;
pub use ddl::DdlError;
pub use error::AnalyzeError;
pub use resolve::{
    AnalyzedColumn, AnalyzedParam, AnalyzedQuery, AnalyzedSpread, AnalyzedSpreadField,
};
pub use types::Type;
