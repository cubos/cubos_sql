//! Proc macro crate for `cubos_sql`.
//!
//! This crate will provide the `query!` macro, which performs compile-time
//! verification of SQL queries against a real PostgreSQL schema. The macro
//! rewrites named parameters (`$name`) into positional placeholders and
//! generates type-safe Rust code for query execution.
//!
//! **Status:** coming in Phase 3 of the implementation roadmap.

extern crate proc_macro;

// Stub — proc macro implementation comes in Phase 3
