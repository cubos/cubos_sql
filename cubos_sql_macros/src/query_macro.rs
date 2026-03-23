//! Parsing and orchestration for the `sql!` proc macro.
//!
//! Parses the macro input, drives the lexer, Docker container, introspection,
//! and code generation pipeline.  Results are cached per query+migration hash
//! inside the `.cubos_sql/` directory.

use std::cell::RefCell;
use std::path::Path;

use proc_macro2::Span;
use syn::parse::{Parse, ParseStream};
use syn::{Expr, Ident, LitStr, Token};

use crate::codegen::{self, ParamAssignment};

// ---------------------------------------------------------------------------
// Connection cache (thread-local)
// ---------------------------------------------------------------------------

/// Cached connection to the compile-time PostgreSQL container.
/// Reused across `sql!` invocations within the same build process.
/// Uses `thread_local!` because `postgres::Client` is not `Send`.
struct CachedConnection {
    client: postgres::Client,
    connection_string: String,
}

thread_local! {
    static CACHED_CLIENT: RefCell<Option<CachedConnection>> = const { RefCell::new(None) };
}

/// Run a closure with a mutable reference to the cached `postgres::Client`.
///
/// Connects (or reconnects) if the connection string changed or the connection
/// is dead, then passes `&mut Client` to `f`. On error from `f`, the cached
/// connection is dropped so the next call gets a fresh one.
fn with_client<T>(
    conn_str: &str,
    f: impl FnOnce(&mut postgres::Client) -> Result<T, syn::Error>,
) -> Result<T, syn::Error> {
    CACHED_CLIENT.with(|cell| {
        let mut borrow = cell.borrow_mut();

        let needs_connect = match borrow.as_mut() {
            Some(cached) => {
                if cached.connection_string != conn_str {
                    true
                } else {
                    cached.client.simple_query("").is_err()
                }
            }
            None => true,
        };

        if needs_connect {
            let client = postgres::Client::connect(conn_str, postgres::NoTls).map_err(|e| {
                syn::Error::new(
                    Span::call_site(),
                    format!("failed to connect to compile-time PG: {e}"),
                )
            })?;

            *borrow = Some(CachedConnection {
                client,
                connection_string: conn_str.to_string(),
            });
        }

        let cached = borrow.as_mut().unwrap();
        let result = f(&mut cached.client);

        if result.is_err() {
            *borrow = None;
        }

        result
    })
}

// ---------------------------------------------------------------------------
// Static analyzer integration
// ---------------------------------------------------------------------------

/// Cached schema snapshot (avoids re-reading + deserializing JSON per `sql!` call).
struct CachedSnapshot {
    snapshot: cubos_sql_analyzer::schema::SchemaSnapshot,
    path: String,
}

thread_local! {
    static CACHED_SNAPSHOT: RefCell<Option<CachedSnapshot>> = const { RefCell::new(None) };
}

/// Load a schema snapshot, using a thread-local cache to avoid repeated IO + deserialization.
fn load_snapshot(snapshot_path: &Path) -> Option<cubos_sql_analyzer::schema::SchemaSnapshot> {
    let path_str = snapshot_path.to_string_lossy().to_string();

    CACHED_SNAPSHOT.with(|cell| {
        let borrow = cell.borrow();
        if let Some(cached) = borrow.as_ref() {
            if cached.path == path_str {
                return Some(cached.snapshot.clone());
            }
        }
        drop(borrow);

        let content = std::fs::read_to_string(snapshot_path).ok()?;
        let snapshot: cubos_sql_analyzer::schema::SchemaSnapshot =
            serde_json::from_str(&content).ok()?;

        cell.borrow_mut().replace(CachedSnapshot {
            snapshot: snapshot.clone(),
            path: path_str,
        });

        Some(snapshot)
    })
}

/// Try to analyze the query statically from a cached schema snapshot.
///
/// Returns `Some(Ok(info))` on success, `None` to signal fallback.
/// Does NOT require a live database connection.
fn try_static_analyze_from_snapshot(
    snapshot_path: &Path,
    sql: &str,
    config: &cubos_sql_core::config::Config,
) -> Option<Result<cubos_sql_core::query_info::QueryInfo, syn::Error>> {
    let snapshot = load_snapshot(snapshot_path)?;

    let analyzer_config = cubos_sql_analyzer::resolve::AnalyzerConfig {
        domains: config.domains.clone(),
        enums: config.enums.clone(),
        types: config.types.clone(),
    };

    match cubos_sql_analyzer::resolve::analyze(&snapshot, sql, &analyzer_config) {
        Ok(info) => Some(Ok(info)),
        Err(_) => None,
    }
}

/// Export schema snapshot from a live connection and cache to disk.
fn export_and_cache_snapshot(
    client: &mut postgres::Client,
    snapshot_path: &Path,
) -> Result<(), syn::Error> {
    let snapshot = cubos_sql_analyzer::export::export_schema(client)
        .map_err(|e| syn::Error::new(Span::call_site(), format!("schema export failed: {e}")))?;

    if let Some(parent) = snapshot_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let json = serde_json::to_string(&snapshot)
        .map_err(|e| syn::Error::new(Span::call_site(), format!("schema serialize: {e}")))?;
    std::fs::write(snapshot_path, json)
        .map_err(|e| syn::Error::new(Span::call_site(), format!("schema write: {e}")))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Macro input parsing
// ---------------------------------------------------------------------------

/// Parsed representation of `sql!(executor, "SQL", param = value, ...)`.
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

/// Execute the full `sql!` pipeline and return the generated `TokenStream`.
pub fn expand(input: QueryInput) -> Result<proc_macro2::TokenStream, syn::Error> {
    let sql_str = input.sql.value();

    // 1. Lex the SQL template.
    let lex_output = cubos_sql_core::lexer::lex(&sql_str)
        .map_err(|e| syn::Error::new(input.sql.span(), e.to_string()))?;

    // Validate that all assignments match SQL params.
    for assignment in &input.assignments {
        if !lex_output.params.iter().any(|p| p.name == assignment.name)
            && !lex_output.spreads.iter().any(|s| s.name == assignment.name)
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

    // Validate spread constraints: field mapping is mandatory, fields must be unique.
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
        if let Some(fields) = &spread.fields {
            let mut seen = std::collections::HashSet::new();
            for field in fields {
                if !seen.insert(field.as_str()) {
                    return Err(syn::Error::new(
                        input.sql.span(),
                        format!("duplicate field '{}' in $..{} spread", field, spread.name,),
                    ));
                }
            }
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

    // Hash the type-mapping config so cache invalidates when domains/enums/types change.
    let config_hash = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        let mut domains: Vec<_> = config.domains.iter().collect();
        domains.sort();
        for (k, v) in &domains {
            h.update(k.as_bytes());
            h.update(v.as_bytes());
        }
        let mut enums: Vec<_> = config.enums.iter().collect();
        enums.sort();
        for (k, v) in &enums {
            h.update(k.as_bytes());
            h.update(v.as_bytes());
        }
        let mut types: Vec<_> = config.types.iter().collect();
        types.sort();
        for (k, v) in &types {
            h.update(k.as_bytes());
            h.update(v.as_bytes());
        }
        format!("{:x}", h.finalize())
    };

    let cache_path = crate::cache::query_cache_path(
        &cubos_dir,
        &migration_hash,
        &introspection_sql,
        &config_hash,
    );

    let query_info = if let Some(cached) = crate::cache::get(&cache_path) {
        cached
    } else {
        // 4. Try static analyzer WITHOUT Docker (if schema snapshot exists).
        let snapshot_path = cubos_dir.join(&migration_hash).join("schema.json");
        let static_result = if snapshot_path.exists() {
            try_static_analyze_from_snapshot(&snapshot_path, &introspection_sql, &config)
        } else {
            None
        };

        if let Some(Ok(info)) = static_result {
            let _ = crate::cache::put(&cache_path, &info);
            info
        } else {
            // 5. Need Docker — ensure container is running.
            let (container_info, _) =
                crate::docker::ensure_container(&config, manifest_path).map_err(|e| {
                    let msg = e.to_string();
                    let hint = if msg.contains("not found") || msg.contains("No such file") {
                        "\nhint: is Docker installed and in your PATH?"
                    } else if msg.contains("Cannot connect")
                        || msg.contains("connection refused")
                        || msg.contains("Is the docker daemon running")
                    {
                        "\nhint: is the Docker daemon running? Try `docker info`."
                    } else if msg.contains("permission denied") {
                        "\nhint: does your user have permission to use Docker? Try `sudo usermod -aG docker $USER`."
                    } else {
                        ""
                    };
                    syn::Error::new(
                        Span::call_site(),
                        format!("failed to start compile-time PG container: {e}{hint}"),
                    )
                })?;

            let conn_str = container_info.connection_string();
            let sql_span = input.sql.span();

            // 6. Export schema snapshot if missing (for future Docker-free builds).
            if !snapshot_path.exists() {
                let _ = with_client(&conn_str, |client| {
                    export_and_cache_snapshot(client, &snapshot_path)
                });
            }

            // 7. Try static analyzer with (now available) snapshot.
            let info =
                try_static_analyze_from_snapshot(&snapshot_path, &introspection_sql, &config)
                    .unwrap_or_else(|| {
                        // 8. Final fallback: live introspection.
                        with_client(&conn_str, |client| {
                            crate::introspect::introspect_query(
                                client,
                                &introspection_sql,
                                &config.domains,
                                &config.enums,
                                &config.types,
                            )
                            .map_err(|e| {
                                let msg = format_introspect_error(&e, &introspection_sql);
                                syn::Error::new(sql_span, msg)
                            })
                        })
                    })?;

            let _ = crate::cache::put(&cache_path, &info);
            info
        }
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
fn format_introspect_error(e: &crate::introspect::IntrospectError, sql: &str) -> String {
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
                        let adjusted =
                            (*p as usize).saturating_sub(crate::introspect::PREPARE_PREFIX_LEN);
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
        let lo = lex("WITH a AS (INSERT INTO t1 (x) VALUES $..s1 { x }) \
             INSERT INTO t2 (y) VALUES $..s2 { y }")
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
