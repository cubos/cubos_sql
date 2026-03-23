//! Live query introspection against a PostgreSQL connection.
//!
//! This is the "classic" approach: `PREPARE` the query, then inspect
//! `pg_catalog.pg_attribute` via `table_oid`/`column_id` from the
//! `RowDescription`. Same approach used by `sqlx`.
//!
//! Known limitations (compared to the static analyzer):
//! - LEFT/RIGHT/FULL JOINs: columns report the base table's NOT NULL
//! - Computed expressions (COUNT, COALESCE, etc.): default to nullable

use std::collections::HashMap;

use postgres::types::Kind;

use cubos_sql_core::query_info::{ColumnInfo, ParamInfo, QueryInfo};
use cubos_sql_core::type_map;

// ──────────────────────────────────────────────────────────────────────────────
// Error type
// ──────────────────────────────────────────────────────────────────────────────

/// Errors that can occur during live query introspection.
#[derive(Debug)]
pub enum IntrospectError {
    Postgres(postgres::Error),
    UnknownType {
        oid: u32,
        column: String,
        pg_name: String,
    },
    NullabilityCheck(String),
}

impl std::fmt::Display for IntrospectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IntrospectError::Postgres(e) => write!(f, "postgres error: {e}"),
            IntrospectError::UnknownType {
                oid,
                column,
                pg_name,
            } => write!(
                f,
                "unsupported PostgreSQL type '{pg_name}' (OID {oid}) for column '{column}'. \
                 Supported types: {}. \
                 If this is a custom type, consider using a domain over a supported base type.",
                type_map::supported_type_names(),
            ),
            IntrospectError::NullabilityCheck(msg) => {
                write!(f, "nullability check failed: {msg}")
            }
        }
    }
}

impl std::error::Error for IntrospectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            IntrospectError::Postgres(e) => Some(e),
            _ => None,
        }
    }
}

impl From<postgres::Error> for IntrospectError {
    fn from(e: postgres::Error) -> Self {
        IntrospectError::Postgres(e)
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Constants
// ──────────────────────────────────────────────────────────────────────────────

const STMT_NAME: &str = "__cubos_sql_stmt";

/// Length of the `PREPARE __cubos_sql_stmt AS ` prefix.
/// Used by error position adjustment in the proc macro.
pub const PREPARE_PREFIX_LEN: usize = "PREPARE __cubos_sql_stmt AS ".len();

const JSONB_OID: u32 = 3802;

// ──────────────────────────────────────────────────────────────────────────────
// Public API
// ──────────────────────────────────────────────────────────────────────────────

/// Introspect a query via PREPARE + pg_catalog lookups.
pub fn introspect_query(
    client: &mut postgres::Client,
    sql: &str,
    domains: &HashMap<String, String>,
    enums: &HashMap<String, String>,
    custom_types: &HashMap<String, String>,
) -> Result<QueryInfo, IntrospectError> {
    let result = introspect_inner(client, sql, domains, enums, custom_types);
    let _ = client.batch_execute("DEALLOCATE ALL");
    result
}

fn introspect_inner(
    client: &mut postgres::Client,
    sql: &str,
    domains: &HashMap<String, String>,
    enums: &HashMap<String, String>,
    custom_types: &HashMap<String, String>,
) -> Result<QueryInfo, IntrospectError> {
    let prepare_sql = format!("PREPARE {STMT_NAME} AS {sql}");
    client.batch_execute(&prepare_sql)?;

    let stmt = client.prepare(sql)?;
    let param_pg_types: Vec<postgres::types::Type> = stmt.params().to_vec();
    let nullability = detect_nullability(client, stmt.columns());
    let params = build_params(&param_pg_types, domains, enums, custom_types)?;
    let columns = build_columns(stmt.columns(), &nullability, domains, enums, custom_types)?;

    Ok(QueryInfo { params, columns })
}

// ──────────────────────────────────────────────────────────────────────────────
// Params
// ──────────────────────────────────────────────────────────────────────────────

fn build_params(
    pg_types: &[postgres::types::Type],
    domains: &HashMap<String, String>,
    enums: &HashMap<String, String>,
    custom_types: &HashMap<String, String>,
) -> Result<Vec<ParamInfo>, IntrospectError> {
    let mut params = Vec::with_capacity(pg_types.len());
    for pg_type in pg_types {
        let resolved = resolve_type(pg_type, domains, enums, custom_types);
        let rust_type = resolved
            .rust_type
            .ok_or_else(|| IntrospectError::UnknownType {
                oid: pg_type.oid(),
                column: format!("$param({})", pg_type.name()),
                pg_name: pg_type.name().to_owned(),
            })?;
        params.push(ParamInfo {
            pg_type_oid: resolved.effective_oid,
            rust_type,
            nullable: false,
            domain_rust_type: resolved.domain_rust_type,
            enum_rust_type: resolved.enum_rust_type,
        });
    }
    Ok(params)
}

// ──────────────────────────────────────────────────────────────────────────────
// Columns
// ──────────────────────────────────────────────────────────────────────────────

fn parse_nullability_annotation(name: &str) -> (String, Option<bool>) {
    if let Some(stripped) = name.strip_suffix('!') {
        (stripped.to_owned(), Some(false))
    } else if let Some(stripped) = name.strip_suffix('?') {
        (stripped.to_owned(), Some(true))
    } else {
        (name.to_owned(), None)
    }
}

fn build_columns(
    columns: &[postgres::Column],
    nullability: &HashMap<String, bool>,
    domains: &HashMap<String, String>,
    enums: &HashMap<String, String>,
    custom_types: &HashMap<String, String>,
) -> Result<Vec<ColumnInfo>, IntrospectError> {
    let mut col_infos = Vec::with_capacity(columns.len());
    for col in columns {
        let raw_name = col.name();
        let (col_name, nullable_override) = parse_nullability_annotation(raw_name);
        let pg_type = col.type_();
        let resolved = resolve_type(pg_type, domains, enums, custom_types);
        let rust_type = resolved
            .rust_type
            .ok_or_else(|| IntrospectError::UnknownType {
                oid: pg_type.oid(),
                column: col_name.clone(),
                pg_name: pg_type.name().to_owned(),
            })?;
        let auto_nullable = nullability.get(raw_name).copied().unwrap_or(false);
        let nullable = nullable_override.unwrap_or(auto_nullable);
        col_infos.push(ColumnInfo {
            name: col_name,
            pg_type_oid: resolved.effective_oid,
            rust_type,
            nullable,
            domain_rust_type: resolved.domain_rust_type,
            enum_rust_type: resolved.enum_rust_type,
        });
    }
    Ok(col_infos)
}

// ──────────────────────────────────────────────────────────────────────────────
// Type resolution
// ──────────────────────────────────────────────────────────────────────────────

struct ResolvedType {
    effective_oid: u32,
    rust_type: Option<String>,
    domain_rust_type: Option<String>,
    enum_rust_type: Option<String>,
}

const MAX_RESOLVE_DEPTH: usize = 32;

fn resolve_type(
    pg_type: &postgres::types::Type,
    domains: &HashMap<String, String>,
    enums: &HashMap<String, String>,
    custom_types: &HashMap<String, String>,
) -> ResolvedType {
    resolve_type_inner(pg_type, domains, enums, custom_types, 0)
}

fn resolve_type_inner(
    pg_type: &postgres::types::Type,
    domains: &HashMap<String, String>,
    enums: &HashMap<String, String>,
    custom_types: &HashMap<String, String>,
    depth: usize,
) -> ResolvedType {
    if depth > MAX_RESOLVE_DEPTH {
        return ResolvedType {
            effective_oid: pg_type.oid(),
            rust_type: None,
            domain_rust_type: None,
            enum_rust_type: None,
        };
    }

    match pg_type.kind() {
        Kind::Domain(base_type) => {
            let domain_name = pg_type.name();
            if base_type.oid() == JSONB_OID {
                if let Some(rust_path) = domains.get(domain_name) {
                    return ResolvedType {
                        effective_oid: JSONB_OID,
                        rust_type: Some("serde_json::Value".to_owned()),
                        domain_rust_type: Some(rust_path.clone()),
                        enum_rust_type: None,
                    };
                }
            }
            resolve_type_inner(base_type, domains, enums, custom_types, depth + 1)
        }
        Kind::Enum(_) => {
            let enum_name = pg_type.name();
            ResolvedType {
                effective_oid: pg_type.oid(),
                rust_type: Some("String".to_owned()),
                domain_rust_type: None,
                enum_rust_type: enums.get(enum_name).cloned(),
            }
        }
        Kind::Array(element_type) => {
            let inner = resolve_type_inner(element_type, domains, enums, custom_types, depth + 1);
            ResolvedType {
                effective_oid: pg_type.oid(),
                rust_type: inner.rust_type.map(|t| format!("Vec<{t}>")),
                domain_rust_type: None,
                enum_rust_type: None,
            }
        }
        _ => {
            if let Some(info) = type_map::from_oid(pg_type.oid()) {
                return ResolvedType {
                    effective_oid: pg_type.oid(),
                    rust_type: Some(info.rust_type.to_owned()),
                    domain_rust_type: None,
                    enum_rust_type: None,
                };
            }
            let qualified_name = format!("{}.{}", pg_type.schema(), pg_type.name());
            let custom_rt = custom_types
                .get(&qualified_name)
                .or_else(|| custom_types.get(pg_type.name()));
            if let Some(rt) = custom_rt {
                return ResolvedType {
                    effective_oid: pg_type.oid(),
                    rust_type: Some(rt.clone()),
                    domain_rust_type: None,
                    enum_rust_type: None,
                };
            }
            ResolvedType {
                effective_oid: pg_type.oid(),
                rust_type: None,
                domain_rust_type: None,
                enum_rust_type: None,
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Nullability detection (table_oid + column_id)
// ──────────────────────────────────────────────────────────────────────────────

fn detect_nullability(
    client: &mut postgres::Client,
    columns: &[postgres::Column],
) -> HashMap<String, bool> {
    let mut map = HashMap::with_capacity(columns.len());
    for col in columns {
        let nullable = match (col.table_oid(), col.column_id()) {
            (Some(table_oid), Some(col_id)) => {
                let row = client
                    .query_opt(
                        "SELECT NOT a.attnotnull \
                         FROM pg_catalog.pg_attribute a \
                         WHERE a.attrelid = $1 AND a.attnum = $2",
                        &[&table_oid, &col_id],
                    )
                    .ok()
                    .flatten();
                row.map(|r| r.get::<_, bool>(0)).unwrap_or(true)
            }
            _ => true,
        };
        map.insert(col.name().to_owned(), nullable);
    }
    map
}
