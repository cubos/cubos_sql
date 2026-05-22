//! Internal shared core for the `pgsafe` ecosystem.
//!
//! **Users should depend on the `pgsafe` crate, not on `pgsafe_core`
//! directly.** This crate contains foundational types shared between the
//! runtime (`pgsafe`) and the proc macro (`pgsafe_macros`).
//!
//! Kept intentionally small so that depending on it does not pull in heavy
//! compile-time-only dependencies (like the SQL parser used by the analyzer).
//!
//! # Modules
//!
//! - [`config`] -- Parses `[package.metadata.pgsafe]` from `Cargo.toml`.
//! - [`build`] -- Build-script helper that re-runs `sql!` when migrations change.
//! - [`qualified_name`] -- PostgreSQL schema-qualified identifiers.

pub mod build;
pub mod config;
mod qualified_name;

pub use qualified_name::{ParseQualifiedNameError, QualifiedName};
