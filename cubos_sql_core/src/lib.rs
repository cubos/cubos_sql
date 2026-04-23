//! Internal shared core for the `cubos_sql` ecosystem.
//!
//! **Users should depend on the `cubos_sql` crate, not on `cubos_sql_core`
//! directly.** This crate contains foundational types shared between the
//! runtime (`cubos_sql`) and the proc macro (`cubos_sql_macros`).
//!
//! Kept intentionally small so that depending on it does not pull in heavy
//! compile-time-only dependencies (like the SQL parser used by the analyzer).
//!
//! # Modules
//!
//! - [`config`] -- Parses `[package.metadata.cubos_sql]` from `Cargo.toml`.
//! - [`qualified_name`] -- PostgreSQL schema-qualified identifiers.

pub mod config;
mod qualified_name;

pub use qualified_name::{ParseQualifiedNameError, QualifiedName};
