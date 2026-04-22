//! Parsing and orchestration for the `sql!` proc macro.
//!
//! Parses the macro input, drives the lexer, builds a schema snapshot from
//! migrations via the DDL interpreter, runs static analysis, and generates
//! typed Rust code.

use std::cell::RefCell;
use std::path::Path;

use proc_macro2::Span;
use syn::parse::{Parse, ParseStream};
use syn::{Expr, Ident, LitStr, Token};

use crate::codegen::{self, ParamAssignment};

// ---------------------------------------------------------------------------
// Static analyzer integration
// ---------------------------------------------------------------------------

/// Cached schema snapshot (avoids re-building per `sql!` call).
struct CachedSnapshot {
    snapshot: cubos_sql_analyzer::schema::SchemaSnapshot,
    /// Cache key: migration hash.
    migration_hash: String,
}

thread_local! {
    static CACHED_SNAPSHOT: RefCell<Option<CachedSnapshot>> = const { RefCell::new(None) };
}

/// Build (or retrieve from cache) a schema snapshot from migrations using the
/// DDL interpreter.
fn get_or_build_snapshot(
    migrations_dirs: &[&Path],
    migration_hash: &str,
) -> Result<cubos_sql_analyzer::schema::SchemaSnapshot, syn::Error> {
    CACHED_SNAPSHOT.with(|cell| {
        let borrow = cell.borrow();
        if let Some(cached) = borrow.as_ref()
            && cached.migration_hash == migration_hash
        {
            return Ok(cached.snapshot.clone());
        }
        drop(borrow);

        let migrations = collect_migration_files(migrations_dirs)?;
        let (snapshot, warnings) =
            cubos_sql_analyzer::seed::build_schema_from_migrations(&migrations).map_err(|e| {
                syn::Error::new(Span::call_site(), format!("DDL interpretation failed: {e}"))
            })?;

        // Warnings are non-fatal. Print to stderr so they appear in build output.
        for w in &warnings {
            eprintln!("cubos_sql warning: {w}");
        }

        cell.borrow_mut().replace(CachedSnapshot {
            snapshot: snapshot.clone(),
            migration_hash: migration_hash.to_string(),
        });

        Ok(snapshot)
    })
}

/// Collect all migration SQL files from the given directories.
///
/// Returns `(filename, content)` pairs sorted by filename.
fn collect_migration_files(dirs: &[&Path]) -> Result<Vec<(String, String)>, syn::Error> {
    let mut files = Vec::new();

    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        let entries = std::fs::read_dir(dir).map_err(|e| {
            syn::Error::new(
                Span::call_site(),
                format!("failed to read migrations dir '{}': {e}", dir.display()),
            )
        })?;
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            let name = path.to_string_lossy().to_string();
            if name.ends_with(".sql") && !name.ends_with(".down.sql") {
                let filename = entry.file_name().to_string_lossy().to_string();
                let content = std::fs::read_to_string(&path).map_err(|e| {
                    syn::Error::new(
                        Span::call_site(),
                        format!("failed to read migration '{}': {e}", path.display()),
                    )
                })?;
                files.push((filename, content));
            }
        }
    }

    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

/// Analyze the query statically against a schema snapshot.
fn static_analyze(
    snapshot: &cubos_sql_analyzer::schema::SchemaSnapshot,
    sql: &str,
    config: &cubos_sql_core::config::ResolvedConfig<'_>,
    lex_output: &cubos_sql_analyzer::param::LexOutput,
) -> Result<cubos_sql_analyzer::query_info::QueryInfo, syn::Error> {
    let analyzer_config = cubos_sql_analyzer::resolve::AnalyzerConfig {
        domains: config.domains.clone(),
        enums: config.enums.clone(),
        types: config.types.clone(),
        param_nullability: {
            let mut v: Vec<Option<bool>> = lex_output.params.iter().map(|p| p.nullable).collect();
            for spread in &lex_output.spreads {
                if let Some(fields) = &spread.fields {
                    v.extend(
                        fields
                            .iter()
                            .map(|f| if f.nullable { Some(true) } else { None }),
                    );
                }
            }
            v
        },
    };

    cubos_sql_analyzer::resolve::analyze(snapshot, sql, &analyzer_config)
        .map_err(|e| syn::Error::new(Span::call_site(), e.to_string()))
}

// ---------------------------------------------------------------------------
// Macro input parsing
// ---------------------------------------------------------------------------

/// Parsed representation of `sql!([db = name,] executor, "SQL", param = value, ...)`.
pub struct QueryInput {
    pub db_name: Option<Ident>,
    pub executor: Expr,
    pub sql: LitStr,
    pub assignments: Vec<ParamAssignment>,
}

impl Parse for QueryInput {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        // Check for optional `db = <name>,` prefix.
        let db_name = if input.peek(Ident) {
            let fork = input.fork();
            let ident: Ident = fork.parse()?;
            if ident == "db" && fork.peek(Token![=]) {
                // Consume from the real stream.
                let _: Ident = input.parse()?;
                input.parse::<Token![=]>()?;
                let name: Ident = input.parse()?;
                input.parse::<Token![,]>()?;
                Some(name)
            } else {
                None
            }
        } else {
            None
        };

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
            db_name,
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
    let lex_output = cubos_sql_analyzer::lexer::lex(&sql_str)
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
                if !seen.insert(field.name.as_str()) {
                    return Err(syn::Error::new(
                        input.sql.span(),
                        format!(
                            "duplicate field '{}' in $..{} spread",
                            field.name, spread.name,
                        ),
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

    let db_name_str = input.db_name.as_ref().map(|i| i.to_string());
    let resolved = config.resolve(db_name_str.as_deref()).map_err(|e| {
        syn::Error::new(
            input
                .db_name
                .as_ref()
                .map(|i| i.span())
                .unwrap_or(Span::call_site()),
            e.to_string(),
        )
    })?;

    // 3. Compute migration hash for snapshot caching.
    let migrations_dir = resolved.migrations_dir(manifest_path);
    let extra_dirs = resolved.extra_migrations_dirs(manifest_path);
    let mut all_dirs: Vec<&Path> = vec![migrations_dir.as_path()];
    all_dirs.extend(extra_dirs.iter().map(|p| p.as_path()));

    let migration_hash = crate::migrations_hash::hash_migrations_dirs(&all_dirs).map_err(|e| {
        syn::Error::new(Span::call_site(), format!("failed to hash migrations: {e}"))
    })?;

    let introspection_sql = if lex_output.spreads.is_empty() {
        lex_output.sql.clone()
    } else {
        build_spread_sample_sql(&lex_output)
    };

    // 4. Build schema snapshot and run static analysis.
    let snapshot = get_or_build_snapshot(&all_dirs, &migration_hash)?;
    let mut query_info = static_analyze(&snapshot, &introspection_sql, &resolved, &lex_output)?;

    // 5. Merge nullable annotations: explicit `$foo?` forces nullable, `$foo!` forces
    //    non-nullable, no annotation keeps the analyzer result.
    {
        let mut nullable_flags: Vec<Option<bool>> =
            lex_output.params.iter().map(|p| p.nullable).collect();
        for spread in &lex_output.spreads {
            if let Some(fields) = &spread.fields {
                nullable_flags.extend(
                    fields
                        .iter()
                        .map(|f| if f.nullable { Some(true) } else { None }),
                );
            }
        }
        for (pi, &lexer_nullable) in query_info.params.iter_mut().zip(nullable_flags.iter()) {
            if let Some(explicit) = lexer_nullable {
                pi.nullable = explicit;
            }
        }
    }

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

/// Build a "sample" SQL for analysis when the query contains spreads.
///
/// Replaces each spread insertion point with a single row of positional
/// placeholders. Field mapping is mandatory, so fields.len() gives the
/// column count.
fn build_spread_sample_sql(lex_output: &cubos_sql_analyzer::param::LexOutput) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use cubos_sql_analyzer::lexer::lex;

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
}
