//! Proc macro crate for `cubos_sql`.
//!
//! Provides the `query!` macro which performs compile-time verification of SQL
//! queries against a real PostgreSQL schema. The macro spins up a Docker
//! container, runs migrations, introspects the query, and generates type-safe
//! Rust code.

extern crate proc_macro;

mod cache;
mod codegen;
mod docker;
mod introspect;
mod query_macro;

/// Compile-time verified SQL query.
///
/// # Syntax
///
/// ```text
/// query!(executor, "SQL with $named_params", param_name = value, ...)
/// ```
///
/// - The first argument is an expression implementing `cubos_sql::Executor`
///   (e.g. a `Pool` or `Transaction`).
/// - The second argument is a SQL string literal with `$name` placeholders.
/// - Remaining arguments are parameter bindings: `name = expr` for explicit
///   values or just `name` for scope capture.
///
/// Returns a query builder with methods:
/// - `.fetch_all().await?` → `Vec<Row>`
/// - `.fetch_one().await?` → `Row`
/// - `.fetch_optional().await?` → `Option<Row>`
/// - `.execute().await?` → `u64` (rows affected)
///
/// Where `Row` is an anonymous struct with typed fields matching the query
/// output columns.
#[proc_macro]
pub fn query(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = syn::parse_macro_input!(input as query_macro::QueryInput);
    match query_macro::expand(input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}
