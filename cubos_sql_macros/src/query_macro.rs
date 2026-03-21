//! Parsing and orchestration for the `query!` proc macro.
//!
//! Parses the macro input, drives the lexer, Docker container, introspection,
//! and code generation pipeline.  Results are cached per query+migration hash
//! inside the `.cubos_sql/` directory.

use std::path::Path;
use std::sync::Mutex;

use proc_macro2::Span;
use syn::parse::{Parse, ParseStream};
use syn::{Expr, Ident, LitStr, Token};

use crate::codegen::{self, ParamAssignment};

// ---------------------------------------------------------------------------
// Connection cache
// ---------------------------------------------------------------------------

/// Cached connection to the compile-time PostgreSQL container.
/// Reused across `query!` invocations within the same build process.
static CACHED_CLIENT: Mutex<Option<CachedConnection>> = Mutex::new(None);

struct CachedConnection {
    client: postgres::Client,
    connection_string: String,
}

fn get_or_connect(
    conn_str: &str,
) -> Result<std::sync::MutexGuard<'static, Option<CachedConnection>>, syn::Error> {
    let mut guard = CACHED_CLIENT.lock().map_err(|_| {
        syn::Error::new(Span::call_site(), "failed to acquire connection lock")
    })?;

    let needs_connect = match guard.as_ref() {
        Some(cached) => cached.connection_string != conn_str,
        None => true,
    };

    if needs_connect {
        let client =
            postgres::Client::connect(conn_str, postgres::NoTls).map_err(|e| {
                syn::Error::new(
                    Span::call_site(),
                    format!("failed to connect to compile-time PG: {e}"),
                )
            })?;

        *guard = Some(CachedConnection {
            client,
            connection_string: conn_str.to_string(),
        });
    }

    Ok(guard)
}

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

    // Validate that all assignments match SQL params.
    for assignment in &input.assignments {
        if !lex_output
            .params
            .iter()
            .any(|p| p.name == assignment.name)
            && !lex_output
                .spreads
                .iter()
                .any(|s| s.name == assignment.name)
        {
            let available: Vec<String> = lex_output
                .params
                .iter()
                .map(|p| format!("${}", p.name))
                .chain(lex_output.spreads.iter().map(|s| format!("$..{}", s.name)))
                .collect();
            let available_str = if available.is_empty() {
                "none".to_string()
            } else {
                available.join(", ")
            };
            return Err(syn::Error::new(
                input.sql.span(),
                format!(
                    "unknown parameter `{}` — not found in SQL. Available parameters: {}",
                    assignment.name, available_str,
                ),
            ));
        }
    }

    // Validate spread constraints: field mapping is mandatory.
    for spread in &lex_output.spreads {
        if spread.fields.is_none() {
            return Err(syn::Error::new(
                input.sql.span(),
                format!(
                    "$..{} requires explicit field mapping: $..{} {{ field1, field2 }}",
                    spread.name, spread.name,
                ),
            ));
        }
    }

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

    let introspection_sql = if lex_output.spreads.is_empty() {
        lex_output.sql.clone()
    } else {
        build_spread_sample_sql(&lex_output)
    };

    let cubos_dir = crate::docker::cubos_sql_dir(manifest_path);
    let cache_path =
        crate::cache::query_cache_path(&cubos_dir, &migration_hash, &introspection_sql);

    let query_info = if let Some(cached) = crate::cache::get(&cache_path) {
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

        // 5. Connect (or reuse cached connection) and introspect.
        let conn_str = container_info.connection_string();
        let mut guard = get_or_connect(&conn_str)?;
        let client = &mut guard.as_mut().unwrap().client;

        let info =
            crate::introspect::introspect_query(client, &introspection_sql, &config.domains)
                .map_err(|e| {
                *guard = None;
                let msg = format_introspect_error(&e, &introspection_sql);
                syn::Error::new(input.sql.span(), msg)
            })?;

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

// ---------------------------------------------------------------------------
// Spread sample SQL generation
// ---------------------------------------------------------------------------

/// Build a "sample" SQL for introspection when the query contains spreads.
///
/// Replaces each spread insertion point with a single row of positional
/// placeholders. Field mapping is mandatory, so fields.len() gives the
/// column count.
fn build_spread_sample_sql(lex_output: &cubos_sql_core::param::LexOutput) -> String {
    let base_sql = &lex_output.sql;
    let num_regular_params = lex_output.params.len();
    let mut result = String::with_capacity(base_sql.len() + 64);
    let mut last_offset = 0;
    let mut param_counter = num_regular_params;

    for spread in &lex_output.spreads {
        result.push_str(&base_sql[last_offset..spread.offset]);
        let fields = spread.fields.as_ref().expect("spread must have fields");
        result.push('(');
        for (i, _) in fields.iter().enumerate() {
            if i > 0 {
                result.push_str(", ");
            }
            param_counter += 1;
            result.push('$');
            result.push_str(&param_counter.to_string());
        }
        result.push(')');
        last_offset = spread.offset;
    }

    result.push_str(&base_sql[last_offset..]);
    result
}

// ---------------------------------------------------------------------------
// Error formatting
// ---------------------------------------------------------------------------

/// Format an introspection error with SQL position info when available.
fn format_introspect_error(
    e: &crate::introspect::IntrospectError,
    sql: &str,
) -> String {
    match e {
        crate::introspect::IntrospectError::Postgres(pg_err) => {
            if let Some(db_err) = pg_err.as_db_error() {
                let mut msg = format!("SQL error: {}", db_err.message());
                if let Some(detail) = db_err.detail() {
                    msg.push_str(&format!("\n  detail: {detail}"));
                }
                if let Some(hint) = db_err.hint() {
                    msg.push_str(&format!("\n  hint: {hint}"));
                }
                if let Some(pos) = db_err.position() {
                    use postgres::error::ErrorPosition;
                    if let ErrorPosition::Original(p) = pos {
                        let adjusted = (*p as usize).saturating_sub(
                            crate::introspect::PREPARE_PREFIX_LEN,
                        );
                        msg.push_str(&format_sql_position(sql, adjusted));
                    }
                }
                msg
            } else {
                format!("query introspection failed: {e}")
            }
        }
        _ => format!("query introspection failed: {e}"),
    }
}

/// Format a visual position pointer into a SQL string.
fn format_sql_position(sql: &str, position: usize) -> String {
    if position == 0 || position > sql.len() {
        return format!("\n  at position {position}");
    }
    let pos = position - 1; // PG positions are 1-based
    let mut current_pos = 0;
    for (line_idx, line) in sql.lines().enumerate() {
        let line_end = current_pos + line.len();
        if pos <= line_end {
            let col = pos - current_pos;
            return format!(
                "\n  --> line {}:{}\n  |\n  | {}\n  | {}^",
                line_idx + 1,
                col + 1,
                line,
                " ".repeat(col),
            );
        }
        current_pos = line_end + 1;
    }
    format!("\n  at position {position}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use cubos_sql_core::lexer::lex;

    #[test]
    fn sample_sql_simple_spread() {
        let lo = lex("INSERT INTO users (name, email) VALUES $..users { name, email }").unwrap();
        let sample = build_spread_sample_sql(&lo);
        assert_eq!(sample, "INSERT INTO users (name, email) VALUES ($1, $2)");
    }

    #[test]
    fn sample_sql_spread_with_regular_params() {
        let lo = lex("INSERT INTO t (org, name) VALUES ($org), $..items { name }").unwrap();
        let sample = build_spread_sample_sql(&lo);
        assert_eq!(sample, "INSERT INTO t (org, name) VALUES ($1), ($2)");
    }

    #[test]
    fn sample_sql_spread_with_suffix() {
        let lo =
            lex("INSERT INTO users (name, email) VALUES $..users { name, email } RETURNING id")
                .unwrap();
        let sample = build_spread_sample_sql(&lo);
        assert_eq!(
            sample,
            "INSERT INTO users (name, email) VALUES ($1, $2) RETURNING id"
        );
    }

    #[test]
    fn sample_sql_three_fields() {
        let lo = lex("INSERT INTO t (a, b, c) VALUES $..items { a, b, c }").unwrap();
        let sample = build_spread_sample_sql(&lo);
        assert_eq!(sample, "INSERT INTO t (a, b, c) VALUES ($1, $2, $3)");
    }

    #[test]
    fn sample_sql_multiple_spreads() {
        let lo = lex(
            "WITH a AS (INSERT INTO t1 (x) VALUES $..s1 { x }) \
             INSERT INTO t2 (y) VALUES $..s2 { y }",
        )
        .unwrap();
        let sample = build_spread_sample_sql(&lo);
        assert!(sample.contains("VALUES ($1)"));
        assert!(sample.contains("VALUES ($2)"));
    }

    #[test]
    fn format_position_basic() {
        let s = format_sql_position("SELECT bad FROM t", 8);
        assert!(s.contains("line 1:8"));
        assert!(s.contains("^"));
    }
}
