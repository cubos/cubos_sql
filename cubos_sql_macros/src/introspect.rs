//! Compile-time query introspection against a live PostgreSQL connection.
//!
//! Connects to the compile-time container, prepares a query, and extracts
//! parameter types and output column types using `pg_catalog` and the
//! `postgres` crate's statement metadata.

use std::collections::HashMap;

use postgres::types::Kind;
use serde::{Deserialize, Serialize};

use cubos_sql_core::type_map;

// ──────────────────────────────────────────────────────────────────────────────
// Public output types
// ──────────────────────────────────────────────────────────────────────────────

/// Information about a single output column from a query.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct ColumnInfo {
    /// Column name as returned by PostgreSQL.
    pub name: String,
    /// PostgreSQL type OID.
    pub pg_type_oid: u32,
    /// Rust type string for output (e.g. `"i64"`, `"String"`,
    /// `"chrono::DateTime<chrono::Utc>"`).
    pub rust_type: String,
    /// Whether this column can be NULL.
    pub nullable: bool,
    /// If this is a JSONB domain type, the Rust type path from config.
    pub domain_rust_type: Option<String>,
}

/// Information about a single query parameter.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct ParamInfo {
    /// PostgreSQL type OID.
    pub pg_type_oid: u32,
    /// Rust type string (e.g. `"i64"`, `"String"`).
    pub rust_type: String,
    /// If this is a JSONB domain, the Rust type path from config.
    pub domain_rust_type: Option<String>,
}

/// Combined introspection result for a query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryInfo {
    pub params: Vec<ParamInfo>,
    pub columns: Vec<ColumnInfo>,
}

// ──────────────────────────────────────────────────────────────────────────────
// Error type
// ──────────────────────────────────────────────────────────────────────────────

/// Errors that can occur during compile-time query introspection.
#[derive(Debug)]
#[allow(dead_code)]
pub enum IntrospectError {
    /// A PostgreSQL protocol/network error.
    Postgres(postgres::Error),
    /// The query returned a column whose OID is not in the supported type map.
    UnknownType { oid: u32, column: String },
    /// Nullability detection failed (non-fatal; caller may default to non-nullable).
    NullabilityCheck(String),
}

impl std::fmt::Display for IntrospectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IntrospectError::Postgres(e) => write!(f, "postgres error: {}", e),
            IntrospectError::UnknownType { oid, column } => {
                write!(f, "unsupported PostgreSQL type OID {} for column '{}'", oid, column)
            }
            IntrospectError::NullabilityCheck(msg) => {
                write!(f, "nullability check failed: {}", msg)
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
// Internal constants
// ──────────────────────────────────────────────────────────────────────────────

/// Name used for the prepared statement in `pg_prepared_statements`.
const STMT_NAME: &str = "__cubos_sql_stmt";

/// OID of the built-in `jsonb` type in PostgreSQL.
const JSONB_OID: u32 = 3802;

// ──────────────────────────────────────────────────────────────────────────────
// Public API
// ──────────────────────────────────────────────────────────────────────────────

/// Introspect a query that has already been rewritten to use positional
/// parameters (`$1`, `$2`, …).
///
/// Steps performed:
/// 1. `PREPARE __cubos_sql_stmt AS <sql>`
/// 2. Query `pg_catalog.pg_prepared_statements` for parameter types.
/// 3. `client.prepare(sql)` to obtain column metadata via `stmt.columns()`.
/// 4. Attempt nullability detection via a temporary view (best-effort).
/// 5. `DEALLOCATE __cubos_sql_stmt`
///
/// # Arguments
/// * `client`  – An open synchronous `postgres::Client` to the compile-time container.
/// * `sql`     – The query with `$1`/`$2`/… positional placeholders.
/// * `domains` – Map of `domain_name → rust_type_path` from the project config.
pub fn introspect_query(
    client: &mut postgres::Client,
    sql: &str,
    domains: &HashMap<String, String>,
) -> Result<QueryInfo, IntrospectError> {
    // ── 1. PREPARE ────────────────────────────────────────────────────────────
    let prepare_sql = format!("PREPARE {} AS {}", STMT_NAME, sql);
    client.batch_execute(&prepare_sql)?;

    // ── 2. Parameter types via pg_catalog ─────────────────────────────────────
    let param_type_rows = client.query(
        "SELECT unnest(parameter_types)::oid AS type_oid \
         FROM pg_catalog.pg_prepared_statements \
         WHERE name = $1",
        &[&STMT_NAME],
    )?;

    let mut param_oids: Vec<u32> = Vec::with_capacity(param_type_rows.len());
    for row in &param_type_rows {
        let oid: u32 = row.get("type_oid");
        param_oids.push(oid);
    }

    // ── 3. Column metadata via client.prepare ─────────────────────────────────
    // `client.prepare` re-parses the query and gives us rich type objects.
    let stmt = client.prepare(sql)?;

    // Collect the postgres::types::Type for each parameter (needed for NULL
    // cast generation and domain detection).
    let param_pg_types: Vec<postgres::types::Type> = stmt.params().to_vec();

    // ── 4. Nullability via pg_attribute lookup (best-effort) ────────────────
    let nullability = detect_nullability(client, stmt.columns());

    // ── 5. Build ParamInfo list ───────────────────────────────────────────────
    let params = build_params(&param_pg_types, domains)?;

    // ── 6. Build ColumnInfo list ──────────────────────────────────────────────
    let columns = build_columns(stmt.columns(), &nullability, domains)?;

    // ── 7. DEALLOCATE ─────────────────────────────────────────────────────────
    // Best-effort; ignore errors so we don't mask the real result.
    let _ = client.batch_execute(&format!("DEALLOCATE {}", STMT_NAME));

    Ok(QueryInfo { params, columns })
}

// ──────────────────────────────────────────────────────────────────────────────
// Internal helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Build the list of [`ParamInfo`] from the prepared-statement parameter types.
fn build_params(
    pg_types: &[postgres::types::Type],
    domains: &HashMap<String, String>,
) -> Result<Vec<ParamInfo>, IntrospectError> {
    let mut params = Vec::with_capacity(pg_types.len());

    for pg_type in pg_types {
        let (effective_oid, domain_rust_type) = resolve_type(pg_type, domains);

        let rust_type = type_map::from_oid(effective_oid)
            .map(|info| info.rust_param_type.to_owned())
            .ok_or_else(|| IntrospectError::UnknownType {
                oid: effective_oid,
                column: format!("$param({})", pg_type.name()),
            })?;

        params.push(ParamInfo {
            pg_type_oid: effective_oid,
            rust_type,
            domain_rust_type,
        });
    }

    Ok(params)
}

/// Build the list of [`ColumnInfo`] from statement column metadata.
fn build_columns(
    columns: &[postgres::Column],
    nullability: &HashMap<String, bool>,
    domains: &HashMap<String, String>,
) -> Result<Vec<ColumnInfo>, IntrospectError> {
    let mut col_infos = Vec::with_capacity(columns.len());

    for col in columns {
        let col_name = col.name().to_owned();
        let pg_type = col.type_();

        let (effective_oid, domain_rust_type) = resolve_type(pg_type, domains);

        let rust_type = type_map::from_oid(effective_oid)
            .map(|info| info.rust_output_type.to_owned())
            .ok_or_else(|| IntrospectError::UnknownType {
                oid: effective_oid,
                column: col_name.clone(),
            })?;

        // Nullability: use the view-based result, default to non-nullable when
        // the view approach was not applicable (INSERT/UPDATE/DELETE, etc.).
        let nullable = nullability.get(&col_name).copied().unwrap_or(false);

        col_infos.push(ColumnInfo {
            name: col_name,
            pg_type_oid: effective_oid,
            rust_type,
            nullable,
            domain_rust_type,
        });
    }

    Ok(col_infos)
}

/// Resolve a `postgres::types::Type` to `(effective_oid, domain_rust_type)`.
///
/// * For domain types whose base type is JSONB and whose domain name appears in
///   the `domains` config, returns `(JSONB_OID, Some(rust_type_path))`.
/// * For domain types with any other base type, unwraps to the base type OID.
/// * For all other types, returns the type's own OID and `None`.
fn resolve_type(
    pg_type: &postgres::types::Type,
    domains: &HashMap<String, String>,
) -> (u32, Option<String>) {
    match pg_type.kind() {
        Kind::Domain(base_type) => {
            let base_oid = base_type.oid();
            if base_oid == JSONB_OID {
                let domain_name = pg_type.name();
                if let Some(rust_path) = domains.get(domain_name) {
                    return (JSONB_OID, Some(rust_path.clone()));
                }
            }
            // Not a configured JSONB domain — fall through to the base type.
            (base_oid, None)
        }
        _ => (pg_type.oid(), None),
    }
}

/// Detect column nullability by looking up each column in `pg_attribute`
/// on the actual source tables (matched by column name and type OID).
///
/// Returns a map of `column_name → is_nullable`. Columns that cannot be
/// matched (computed expressions, aliases) default to nullable for safety.
fn detect_nullability(
    client: &mut postgres::Client,
    columns: &[postgres::Column],
) -> HashMap<String, bool> {
    let mut map = HashMap::with_capacity(columns.len());

    for col in columns {
        let name = col.name();
        let oid = col.type_().oid();

        // Find all table columns with matching name and type.
        let rows = client
            .query(
                "SELECT a.attnotnull \
                 FROM pg_catalog.pg_attribute a \
                 JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 WHERE a.attname = $1 \
                   AND a.atttypid = $2 \
                   AND a.attnum > 0 \
                   AND NOT a.attisdropped \
                   AND n.nspname NOT IN ('pg_catalog', 'information_schema', 'pg_toast') \
                   AND c.relkind IN ('r', 'p')",
                &[&name, &oid],
            )
            .unwrap_or_default();

        // If ALL matching columns are NOT NULL → not nullable.
        // If no matches or any match is nullable → assume nullable.
        let nullable = rows.is_empty()
            || !rows.iter().all(|r| r.get::<_, bool>(0));
        map.insert(name.to_owned(), nullable);
    }

    map
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: start or reuse a shared PG container, return a fresh client.
    ///
    /// Uses a fixed directory so all tests in this module share one container.
    /// The container is NOT cleaned up automatically (reused across test runs).
    fn setup_pg() -> postgres::Client {
        let test_dir = std::env::temp_dir().join("cubos_sql_introspect_tests");
        let mig_path = test_dir.join("migrations");
        std::fs::create_dir_all(&mig_path).unwrap();
        std::fs::write(
            mig_path.join("0001_create_users.sql"),
            "CREATE TABLE IF NOT EXISTS users (\
                id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
                name TEXT NOT NULL, \
                email TEXT NOT NULL UNIQUE, \
                age INT, \
                created_at TIMESTAMPTZ NOT NULL DEFAULT now()\
            );",
        ).unwrap();

        let config = cubos_sql_core::config::Config {
            database: cubos_sql_core::config::DatabaseConfig {
                docker_image: "postgres".to_string(),
                migrations: mig_path.clone(),
            },
            migrations: cubos_sql_core::config::MigrationsConfig::default(),
            domains: HashMap::new(),
        };

        let (info, _hash) = crate::docker::ensure_container(&config, &test_dir).unwrap();
        postgres::Client::connect(&info.connection_string(), postgres::NoTls).unwrap()
    }

    #[test]
    #[ignore] // Requires Docker
    fn introspect_simple_select() {
        let mut client = setup_pg();

        let domains = HashMap::new();
        let info = introspect_query(
            &mut client,
            "SELECT id, name, email FROM users WHERE age > $1",
            &domains,
        ).unwrap();

        assert_eq!(info.params.len(), 1, "should have 1 parameter");
        assert_eq!(info.params[0].rust_type, "i32");

        assert_eq!(info.columns.len(), 3, "should have 3 columns");
        assert_eq!(info.columns[0].name, "id");
        assert_eq!(info.columns[0].rust_type, "i64");
        assert!(!info.columns[0].nullable);

        assert_eq!(info.columns[1].name, "name");
        assert_eq!(info.columns[1].rust_type, "String");
        assert!(!info.columns[1].nullable);

        assert_eq!(info.columns[2].name, "email");
        assert_eq!(info.columns[2].rust_type, "String");
    }

    #[test]
    #[ignore] // Requires Docker
    fn introspect_nullable_column() {
        let mut client = setup_pg();

        let domains = HashMap::new();
        let info = introspect_query(
            &mut client,
            "SELECT id, age FROM users",
            &domains,
        ).unwrap();

        assert_eq!(info.columns.len(), 2);
        // id is NOT NULL
        assert!(!info.columns[0].nullable, "id should not be nullable");
        // age is nullable (INT without NOT NULL)
        assert!(info.columns[1].nullable, "age should be nullable");
    }

    #[test]
    #[ignore] // Requires Docker
    fn introspect_insert_returning() {
        let mut client = setup_pg();

        let domains = HashMap::new();
        let info = introspect_query(
            &mut client,
            "INSERT INTO users (name, email) VALUES ($1, $2) RETURNING id, created_at",
            &domains,
        ).unwrap();

        assert_eq!(info.params.len(), 2, "should have 2 parameters");
        assert_eq!(info.params[0].rust_type, "String"); // name is TEXT
        assert_eq!(info.params[1].rust_type, "String"); // email is TEXT

        assert_eq!(info.columns.len(), 2, "should have 2 return columns");
        assert_eq!(info.columns[0].name, "id");
        assert_eq!(info.columns[0].rust_type, "i64");
        assert_eq!(info.columns[1].name, "created_at");
        assert_eq!(info.columns[1].rust_type, "chrono::DateTime<chrono::Utc>");
    }

    #[test]
    #[ignore] // Requires Docker
    fn introspect_no_params() {
        let mut client = setup_pg();

        let domains = HashMap::new();
        let info = introspect_query(
            &mut client,
            "SELECT id, name FROM users",
            &domains,
        ).unwrap();

        assert_eq!(info.params.len(), 0);
        assert_eq!(info.columns.len(), 2);
    }

    #[test]
    #[ignore] // Requires Docker
    fn introspect_unknown_type_errors() {
        let mut client = setup_pg();

        // Create a type that's not in our type_map
        let _ = client.batch_execute("CREATE TYPE mood AS ENUM ('happy', 'sad')");
        let _ = client.batch_execute("ALTER TABLE users ADD COLUMN IF NOT EXISTS mood mood");

        let domains = HashMap::new();
        let result = introspect_query(
            &mut client,
            "SELECT mood FROM users",
            &domains,
        );

        assert!(result.is_err(), "should fail on unknown type");
    }
}
