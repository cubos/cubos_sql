//! Internal shared core for the `cubos_sql` ecosystem.
//!
//! **Users should depend on the `cubos_sql` crate, not on `cubos_sql_core`
//! directly.** This crate contains foundational types and utilities shared
//! between the runtime (`cubos_sql`) and the proc macro (`cubos_sql_macros`).
//!
//! # Modules
//!
//! - [`config`] -- Parses `[package.metadata.cubos_sql]` from `Cargo.toml`.
//! - [`lexer`] -- Tokenizes SQL templates, rewriting `$name` parameters and
//!   `$..spread` parameters into positional placeholders.
//! - [`param`] -- Data structures for extracted parameters and lexer output.
//! - [`type_map`] -- Maps PostgreSQL type OIDs and names to Rust types.

pub mod config;
pub mod lexer;
pub mod param;
pub mod type_map;
