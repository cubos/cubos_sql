//! Shared core crate for the `cubos_sql` ecosystem.
//!
//! This crate provides foundational types and utilities used by both the runtime
//! (`cubos_sql`) and the proc macro (`cubos_sql_macros`) crates:
//!
//! - **Config parser** ([`config`]) — reads `[package.metadata.cubos_sql]` from `Cargo.toml`.
//! - **SQL lexer** ([`lexer`]) — tokenizes SQL templates, rewriting named parameters
//!   (`$name`) and spread parameters (`$..items`) into positional placeholders.
//! - **Parameter types** ([`param`]) — data structures representing extracted parameters
//!   and the lexer output.

pub mod config;
pub mod lexer;
pub mod param;
pub mod type_map;
