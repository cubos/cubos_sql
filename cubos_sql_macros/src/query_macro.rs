//! Parsing and orchestration for the `query!` proc macro.
//!
//! Parses the macro input, drives the lexer, Docker container, introspection,
//! and code generation pipeline.  Results are cached per query+migration hash
//! inside the `.cubos_sql/` directory.

use std::path::Path;

use proc_macro2::Span;
use syn::parse::{Parse, ParseStream};
use syn::{Expr, Ident, LitStr, Token};

use crate::codegen::{self, ParamAssignment};

// ---------------------------------------------------------------------------
// Macro input parsing
// ---------------------------------------------------------------------------

/// Parsed representation of `query!(executor, "SQL", param = value, ...)`.
pub struct QueryInput {
    pub executor: Expr,
    pub sql: LitStr,
    pub assignments: Vec<ParamAssignment>,
}

impl Parse for QueryInput {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let executor: Expr = input.parse()?;
        input.parse::<Token![,]>()?;
        let sql: LitStr = input.parse()?;

        let mut assignments = Vec::new();
        while input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
            if input.is_empty() {
                break;
            }
            let name: Ident = input.parse()?;
            if input.peek(Token![=]) {
                input.parse::<Token![=]>()?;
                let expr: Expr = input.parse()?;
                assignments.push(ParamAssignment {
                    name: name.to_string(),
                    expr: Some(expr),
                });
            } else {
                assignments.push(ParamAssignment {
                    name: name.to_string(),
                    expr: None,
                });
            }
        }

        Ok(Self {
            executor,
            sql,
            assignments,
        })
    }
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

/// Execute the full `query!` pipeline and return the generated `TokenStream`.
pub fn expand(input: QueryInput) -> Result<proc_macro2::TokenStream, syn::Error> {
    let sql_str = input.sql.value();

    // 1. Lex the SQL template.
    let lex_output = cubos_sql_core::lexer::lex(&sql_str)
        .map_err(|e| syn::Error::new(input.sql.span(), e.to_string()))?;

    // 2. Load project config from Cargo.toml.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").map_err(|_| {
        syn::Error::new(
            Span::call_site(),
            "CARGO_MANIFEST_DIR not set — are you running inside cargo build?",
        )
    })?;
    let manifest_path = Path::new(&manifest_dir);
    let cargo_toml_path = manifest_path.join("Cargo.toml");
    let config = cubos_sql_core::config::Config::from_cargo_toml(&cargo_toml_path)
        .map_err(|e| syn::Error::new(Span::call_site(), format!("failed to load config: {e}")))?;

    // 3. Compute migration hash and check cache BEFORE starting any container.
    let migrations_dir = config.migrations_dir(manifest_path);
    let migration_hash = crate::docker::hash_migrations_dir(&migrations_dir).map_err(|e| {
        syn::Error::new(Span::call_site(), format!("failed to hash migrations: {e}"))
    })?;

    let cubos_dir = crate::docker::cubos_sql_dir(manifest_path);
    let cache_path =
        crate::cache::query_cache_path(&cubos_dir, &migration_hash, &lex_output.sql);

    let query_info = if let Some(cached) = crate::cache::get(&cache_path) {
        // Cache hit — no Docker needed.
        cached
    } else {
        // 4. Cache miss — ensure Docker container is running.
        let (container_info, _) =
            crate::docker::ensure_container(&config, manifest_path).map_err(|e| {
                syn::Error::new(
                    Span::call_site(),
                    format!("failed to start compile-time PG container: {e}"),
                )
            })?;

        // 5. Connect and introspect.
        let conn_str = container_info.connection_string();
        let mut client =
            postgres::Client::connect(&conn_str, postgres::NoTls).map_err(|e| {
                syn::Error::new(
                    Span::call_site(),
                    format!("failed to connect to compile-time PG: {e}"),
                )
            })?;

        let info =
            crate::introspect::introspect_query(&mut client, &lex_output.sql, &config.domains)
                .map_err(|e| {
                    syn::Error::new(
                        input.sql.span(),
                        format!("query introspection failed: {e}"),
                    )
                })?;

        // Cache the result for next build.
        let _ = crate::cache::put(&cache_path, &info);
        info
    };

    // 6. Generate typed Rust code.
    codegen::generate(
        &lex_output,
        &query_info,
        &input.executor,
        &input.assignments,
    )
}
