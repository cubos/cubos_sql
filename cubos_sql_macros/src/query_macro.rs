//! Parsing and orchestration for the `sql!` proc macro.
//!
//! Parses the macro input, builds a cached [`PgCatalog`] from migrations,
//! runs static analysis, and generates typed Rust code.

use std::cell::RefCell;
use std::path::Path;

use cubos_sql_analyzer::PgCatalog;
use proc_macro2::Span;
use syn::parse::{Parse, ParseStream};
use syn::{Expr, Ident, LitStr, Token};

use crate::codegen::{self, ParamAssignment};

// ---------------------------------------------------------------------------
// PgCatalog caching
// ---------------------------------------------------------------------------

struct CachedPgCatalog {
    catalog: PgCatalog,
    /// Cache key: migration hash.
    migration_hash: String,
}

thread_local! {
    static CACHED_PG_CATALOG: RefCell<Option<CachedPgCatalog>> = const { RefCell::new(None) };
}

/// Build (or retrieve from cache) a [`PgCatalog`] from migration files.
fn get_or_build_pg_catalog(
    migrations_dirs: &[&Path],
    migration_hash: &str,
) -> Result<PgCatalog, syn::Error> {
    CACHED_PG_CATALOG.with(|cell| {
        let borrow = cell.borrow();
        if let Some(cached) = borrow.as_ref()
            && cached.migration_hash == migration_hash
        {
            return Ok(cached.catalog.clone());
        }
        drop(borrow);

        let migrations = collect_migration_files(migrations_dirs)?;
        let mut catalog = PgCatalog::new().map_err(|e| {
            syn::Error::new(
                Span::call_site(),
                format!("failed to load embedded PG catalog seed: {e}"),
            )
        })?;
        for (filename, sql) in &migrations {
            catalog.apply_sql(sql).map_err(|e| {
                syn::Error::new(
                    Span::call_site(),
                    format!("DDL interpretation failed in '{filename}': {e}"),
                )
            })?;
        }

        cell.borrow_mut().replace(CachedPgCatalog {
            catalog: catalog.clone(),
            migration_hash: migration_hash.to_string(),
        });

        Ok(catalog)
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

    // 1. Load project config from Cargo.toml.
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

    // 2. Build (or reuse cached) PgCatalog from migrations.
    let migrations_dir = resolved.migrations_dir(manifest_path);
    let extra_dirs = resolved.extra_migrations_dirs(manifest_path);
    let mut all_dirs: Vec<&Path> = vec![migrations_dir.as_path()];
    all_dirs.extend(extra_dirs.iter().map(|p| p.as_path()));

    let migration_hash = crate::migrations_hash::hash_migrations_dirs(&all_dirs).map_err(|e| {
        syn::Error::new(Span::call_site(), format!("failed to hash migrations: {e}"))
    })?;

    let catalog = get_or_build_pg_catalog(&all_dirs, &migration_hash)?;

    // 3. Analyze the SQL (lex + type inference in one pass).
    let analyzed = catalog
        .analyze(&sql_str)
        .map_err(|e| syn::Error::new(input.sql.span(), e.to_string()))?;

    // 4. Validate that all assignments match SQL params/spreads.
    for assignment in &input.assignments {
        if !analyzed.params.iter().any(|p| p.name == assignment.name)
            && !analyzed.spreads.iter().any(|s| s.name == assignment.name)
        {
            let available: Vec<String> = analyzed
                .params
                .iter()
                .map(|p| format!("${}", p.name))
                .chain(analyzed.spreads.iter().map(|s| format!("$..{}", s.name)))
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

    // 5. Validate spread constraints: field names must be unique within each spread.
    for spread in &analyzed.spreads {
        let mut seen = std::collections::HashSet::new();
        for field in &spread.fields {
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

    // 6. Generate typed Rust code.
    codegen::generate(&analyzed, &resolved, &input.executor, &input.assignments)
}
