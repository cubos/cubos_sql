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
    /// If this is a mapped enum type, the Rust type path from config.
    #[serde(default)]
    pub enum_rust_type: Option<String>,
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
    /// If this is a mapped enum type, the Rust type path from config.
    #[serde(default)]
    pub enum_rust_type: Option<String>,
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
    UnknownType {
        oid: u32,
        column: String,
        pg_name: String,
    },
    /// Nullability detection failed (non-fatal; caller may default to non-nullable).
    NullabilityCheck(String),
}

impl std::fmt::Display for IntrospectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IntrospectError::Postgres(e) => write!(f, "postgres error: {}", e),
            IntrospectError::UnknownType {
                oid,
                column,
                pg_name,
            } => {
                write!(
                    f,
                    "unsupported PostgreSQL type '{}' (OID {}) for column '{}'. \
                     Supported types: {}. \
                     If this is a custom type, consider using a domain over a supported base type.",
                    pg_name,
                    oid,
                    column,
                    type_map::supported_type_names(),
                )
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

/// Length of the `PREPARE __cubos_sql_stmt AS ` prefix prepended to user SQL.
/// Used by error position adjustment in query_macro.
pub const PREPARE_PREFIX_LEN: usize = "PREPARE __cubos_sql_stmt AS ".len();

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
    enums: &HashMap<String, String>,
    custom_types: &HashMap<String, String>,
) -> Result<QueryInfo, IntrospectError> {
    // ── 1. PREPARE + introspect (with guaranteed DEALLOCATE ALL on all paths) ─
    let result = introspect_inner(client, sql, domains, enums, custom_types);

    // Always clean up all prepared statements, even on error.
    let _ = client.batch_execute("DEALLOCATE ALL");

    result
}

/// Inner implementation that does the actual PREPARE + introspection.
/// Separated so the caller can guarantee DEALLOCATE ALL runs on all paths.
fn introspect_inner(
    client: &mut postgres::Client,
    sql: &str,
    domains: &HashMap<String, String>,
    enums: &HashMap<String, String>,
    custom_types: &HashMap<String, String>,
) -> Result<QueryInfo, IntrospectError> {
    let prepare_sql = format!("PREPARE {} AS {}", STMT_NAME, sql);
    client.batch_execute(&prepare_sql)?;

    let stmt = client.prepare(sql)?;
    let param_pg_types: Vec<postgres::types::Type> = stmt.params().to_vec();
    let nullability = detect_nullability(client, stmt.columns());
    let params = build_params(&param_pg_types, domains, enums, custom_types)?;
    let columns = build_columns(stmt.columns(), &nullability, domains, enums, custom_types)?;

    Ok(QueryInfo { params, columns })
}

// ──────────────────────────────────────────────────────────────────────────────
// Internal helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Build the list of [`ParamInfo`] from the prepared-statement parameter types.
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
            domain_rust_type: resolved.domain_rust_type,
            enum_rust_type: resolved.enum_rust_type,
        });
    }

    Ok(params)
}

/// Parse a nullability annotation from a column name.
///
/// Column names ending with `!` force non-nullable, and `?` forces nullable.
/// These are written as quoted aliases in SQL:
/// ```sql
/// SELECT COALESCE(age, 0) as "safe_age!" FROM users   -- force non-null
/// SELECT p.title as "title?" FROM users LEFT JOIN ...  -- force nullable
/// ```
///
/// Returns the stripped name and the override (`Some(false)` = non-null,
/// `Some(true)` = nullable, `None` = no annotation).
fn parse_nullability_annotation(name: &str) -> (String, Option<bool>) {
    if let Some(stripped) = name.strip_suffix('!') {
        (stripped.to_owned(), Some(false))
    } else if let Some(stripped) = name.strip_suffix('?') {
        (stripped.to_owned(), Some(true))
    } else {
        (name.to_owned(), None)
    }
}

/// Build the list of [`ColumnInfo`] from statement column metadata.
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

/// Result of resolving a PostgreSQL type to a Rust type.
struct ResolvedType {
    effective_oid: u32,
    /// Rust type string (e.g. `"i32"`, `"String"`, `"Vec<i64>"`). `None` if unknown.
    rust_type: Option<String>,
    domain_rust_type: Option<String>,
    enum_rust_type: Option<String>,
}

/// Maximum recursion depth when resolving nested domain / array types.
const MAX_RESOLVE_DEPTH: usize = 32;

/// Resolve a `postgres::types::Type` into Rust type strings.
///
/// Handles (in priority order):
/// 1. **Domain over JSONB** mapped in `[domains]` → serde_json::Value + domain_rust_type
/// 2. **Domain over other base** → recursively unwrap to the base type
/// 3. **Enum** mapped in `[enums]` → String + enum_rust_type
/// 4. **Enum** unmapped → String
/// 5. **Array** → `Vec<element_type>` (resolved recursively)
/// 6. **Built-in type** → static type_map lookup by OID
/// 7. **Custom type** from `[types]` config → matched by `schema.name` or `name`
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
        // ── Domains ──────────────────────────────────────────────────────
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

        // ── Enums ────────────────────────────────────────────────────────
        Kind::Enum(_) => {
            let enum_name = pg_type.name();
            ResolvedType {
                effective_oid: pg_type.oid(),
                rust_type: Some("String".to_owned()),
                domain_rust_type: None,
                enum_rust_type: enums.get(enum_name).cloned(),
            }
        }

        // ── Arrays (generic: resolve element, wrap in Vec<>) ─────────────
        Kind::Array(element_type) => {
            let inner = resolve_type_inner(element_type, domains, enums, custom_types, depth + 1);
            ResolvedType {
                effective_oid: pg_type.oid(),
                rust_type: inner.rust_type.map(|t| format!("Vec<{t}>")),
                // Arrays of domains/enums lose the special handling — they use
                // the resolved base types (Vec<serde_json::Value>, Vec<String>).
                domain_rust_type: None,
                enum_rust_type: None,
            }
        }

        // ── Base types: static map, then custom types config ─────────────
        _ => {
            // 1. Check the built-in static type map.
            if let Some(info) = type_map::from_oid(pg_type.oid()) {
                return ResolvedType {
                    effective_oid: pg_type.oid(),
                    rust_type: Some(info.rust_type.to_owned()),
                    domain_rust_type: None,
                    enum_rust_type: None,
                };
            }

            // 2. Check custom types config (by "schema.name" then by "name").
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

            // 3. Unknown type.
            ResolvedType {
                effective_oid: pg_type.oid(),
                rust_type: None,
                domain_rust_type: None,
                enum_rust_type: None,
            }
        }
    }
}

/// Detect column nullability using the `table_oid` and `column_id` from
/// the prepared statement's `RowDescription`.
///
/// Each column in a prepared statement carries the OID of its source table
/// and the attribute number within that table (both `None` if computed).
/// This allows a precise `pg_attribute` lookup without name ambiguity.
///
/// **Note**: view-based detection was considered but PostgreSQL does NOT
/// propagate NOT NULL constraints to view columns in `pg_attribute` —
/// `attnotnull` is always `false` for views. So we rely solely on the
/// prepared statement metadata, which is the same approach used by `sqlx`.
///
/// Known limitations:
/// - LEFT/RIGHT/FULL JOINs: columns report the base table's NOT NULL even
///   though the JOIN may produce NULLs. User can annotate with `?`.
/// - Computed expressions (COUNT, COALESCE, etc.): no `table_oid` → defaults
///   to nullable. User can annotate with `!` to force non-null.
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
                         WHERE a.attrelid = $1 \
                           AND a.attnum = $2",
                        &[&table_oid, &col_id],
                    )
                    .ok()
                    .flatten();
                row.map(|r| r.get::<_, bool>(0)).unwrap_or(true)
            }
            // Computed column (no source table) → assume nullable.
            _ => true,
        };
        map.insert(col.name().to_owned(), nullable);
    }

    map
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Helpers ──────────────────────────────────────────────────────────

    const MIGRATION_SQL: &str = "\
        CREATE TABLE IF NOT EXISTS users (\
            id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
            name TEXT NOT NULL, \
            email TEXT NOT NULL UNIQUE, \
            age INT, \
            created_at TIMESTAMPTZ NOT NULL DEFAULT now()\
        );\
        \
        CREATE TABLE IF NOT EXISTS posts (\
            id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
            user_id BIGINT NOT NULL REFERENCES users(id), \
            title TEXT NOT NULL, \
            body TEXT, \
            published_at TIMESTAMPTZ\
        );\
        \
        CREATE TABLE IF NOT EXISTS comments (\
            id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
            post_id BIGINT NOT NULL REFERENCES posts(id), \
            author_name TEXT NOT NULL, \
            content TEXT NOT NULL, \
            rating INT\
        );\
    ";

    /// Start or reuse a shared PG container, return a fresh client.
    fn setup_pg() -> postgres::Client {
        let test_dir = std::env::temp_dir().join("cubos_sql_introspect_tests");
        let mig_path = test_dir.join("migrations");
        std::fs::create_dir_all(&mig_path).unwrap();
        std::fs::write(mig_path.join("0001_schema.sql"), MIGRATION_SQL).unwrap();

        let config = cubos_sql_core::config::Config {
            database: cubos_sql_core::config::DatabaseConfig {
                docker_image: "postgres".to_string(),
                migrations: mig_path.clone(),
            },
            migrations: cubos_sql_core::config::MigrationsConfig::default(),
            domains: HashMap::new(),
            enums: HashMap::new(),
            types: HashMap::new(),
        };

        let (info, _hash) = crate::docker::ensure_container(&config, &test_dir).unwrap();
        postgres::Client::connect(&info.connection_string(), postgres::NoTls).unwrap()
    }

    fn empty_maps() -> (
        HashMap<String, String>,
        HashMap<String, String>,
        HashMap<String, String>,
    ) {
        (HashMap::new(), HashMap::new(), HashMap::new())
    }

    fn query(client: &mut postgres::Client, sql: &str) -> QueryInfo {
        let (domains, enums, types) = empty_maps();
        introspect_query(client, sql, &domains, &enums, &types).unwrap()
    }

    fn col<'a>(info: &'a QueryInfo, name: &str) -> &'a ColumnInfo {
        info.columns
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("column '{}' not found", name))
    }

    // ── Type resolution tests ───────────────────────────────────────────

    #[test]
    #[ignore] // Requires Docker
    fn types_simple_select() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "SELECT id, name, email FROM users WHERE age > $1",
        );

        assert_eq!(info.params.len(), 1);
        assert_eq!(info.params[0].rust_type, "i32");

        assert_eq!(info.columns.len(), 3);
        assert_eq!(col(&info, "id").rust_type, "i64");
        assert_eq!(col(&info, "name").rust_type, "String");
        assert_eq!(col(&info, "email").rust_type, "String");
    }

    #[test]
    #[ignore] // Requires Docker
    fn types_no_params() {
        let mut client = setup_pg();
        let info = query(&mut client, "SELECT id, name FROM users");

        assert_eq!(info.params.len(), 0);
        assert_eq!(info.columns.len(), 2);
    }

    #[test]
    #[ignore] // Requires Docker
    fn types_insert_returning() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "INSERT INTO users (name, email) VALUES ($1, $2) RETURNING id, created_at",
        );

        assert_eq!(info.params.len(), 2);
        assert_eq!(info.params[0].rust_type, "String");
        assert_eq!(info.params[1].rust_type, "String");

        assert_eq!(info.columns.len(), 2);
        assert_eq!(col(&info, "id").rust_type, "i64");
        assert_eq!(
            col(&info, "created_at").rust_type,
            "chrono::DateTime<chrono::Utc>"
        );
    }

    #[test]
    #[ignore] // Requires Docker
    fn types_unmapped_enum_resolves_to_string() {
        let mut client = setup_pg();
        let _ = client.batch_execute(
            "DO $$ BEGIN CREATE TYPE mood AS ENUM ('happy', 'sad'); EXCEPTION WHEN duplicate_object THEN NULL; END $$"
        );
        let _ = client.batch_execute("ALTER TABLE users ADD COLUMN IF NOT EXISTS mood mood");

        let info = query(&mut client, "SELECT mood FROM users");
        assert_eq!(col(&info, "mood").rust_type, "String");
    }

    // ── Nullability: basic table columns ────────────────────────────────

    #[test]
    #[ignore] // Requires Docker
    fn null_not_null_column() {
        let mut client = setup_pg();
        let info = query(&mut client, "SELECT id, name FROM users");

        assert!(!col(&info, "id").nullable, "id is NOT NULL");
        assert!(!col(&info, "name").nullable, "name is NOT NULL");
    }

    #[test]
    #[ignore] // Requires Docker
    fn null_nullable_column() {
        let mut client = setup_pg();
        let info = query(&mut client, "SELECT id, age FROM users");

        assert!(!col(&info, "id").nullable);
        assert!(col(&info, "age").nullable, "age should be nullable");
    }

    #[test]
    #[ignore] // Requires Docker
    fn null_mixed_columns() {
        let mut client = setup_pg();
        let info = query(&mut client, "SELECT id, name, age, created_at FROM users");

        assert!(!col(&info, "id").nullable);
        assert!(!col(&info, "name").nullable);
        assert!(col(&info, "age").nullable);
        assert!(!col(&info, "created_at").nullable);
    }

    // ── Nullability: JOINs ──────────────────────────────────────────────

    #[test]
    #[ignore] // Requires Docker
    fn null_inner_join_preserves_not_null() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "SELECT u.name, p.title \
             FROM users u \
             INNER JOIN posts p ON p.user_id = u.id",
        );

        // INNER JOIN: table_oid points to each base table.
        // Both columns are NOT NULL → correct.
        assert!(
            !col(&info, "name").nullable,
            "INNER JOIN: name stays NOT NULL"
        );
        assert!(
            !col(&info, "title").nullable,
            "INNER JOIN: title stays NOT NULL"
        );
    }

    #[test]
    #[ignore] // Requires Docker
    fn null_left_join_already_nullable_column() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "SELECT u.name, p.body \
             FROM users u \
             LEFT JOIN posts p ON p.user_id = u.id",
        );

        // body is already nullable in the table → correctly reported.
        assert!(!col(&info, "name").nullable);
        assert!(col(&info, "body").nullable);
    }

    #[test]
    #[ignore] // Requires Docker
    fn null_multiple_joins() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "SELECT u.name, p.title, c.rating \
             FROM users u \
             INNER JOIN posts p ON p.user_id = u.id \
             LEFT JOIN comments c ON c.post_id = p.id",
        );

        assert!(!col(&info, "name").nullable, "INNER JOIN left: NOT NULL");
        assert!(!col(&info, "title").nullable, "INNER JOIN right: NOT NULL");
        // rating is nullable in the table AND comes from LEFT JOIN side.
        assert!(
            col(&info, "rating").nullable,
            "LEFT JOIN + nullable col: nullable"
        );
    }

    // ── Nullability: computed expressions ────────────────────────────────
    //
    // Computed expressions have no table_oid → default to nullable.
    // This is conservative: some expressions (COUNT, COALESCE) are never
    // NULL, but we can't distinguish them without SQL parsing.

    #[test]
    #[ignore] // Requires Docker
    fn null_sum_is_nullable() {
        let mut client = setup_pg();
        let info = query(&mut client, "SELECT SUM(age) as total FROM users");

        // SUM returns NULL for zero rows → correctly detected as nullable
        // (no table_oid for computed expressions).
        assert!(col(&info, "total").nullable, "SUM can be NULL");
    }

    #[test]
    #[ignore] // Requires Docker
    fn null_case_without_else_is_nullable() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "SELECT CASE WHEN age > 18 THEN 'adult' END as category FROM users",
        );

        // CASE without ELSE can produce NULL → correctly detected.
        assert!(
            col(&info, "category").nullable,
            "CASE without ELSE is nullable"
        );
    }

    // ── Nullability: subqueries ─────────────────────────────────────────

    #[test]
    #[ignore] // Requires Docker
    fn null_scalar_subquery_is_nullable() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "SELECT id, (SELECT title FROM posts WHERE posts.user_id = users.id LIMIT 1) as first_post \
             FROM users",
        );

        // Scalar subquery has no table_oid → defaults to nullable (correct).
        assert!(!col(&info, "id").nullable);
        assert!(
            col(&info, "first_post").nullable,
            "scalar subquery is nullable"
        );
    }

    // ── Nullability: UNION ──────────────────────────────────────────────

    #[test]
    #[ignore] // Requires Docker
    fn null_union_nullable_if_any_branch_nullable() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "SELECT name as val FROM users \
             UNION ALL \
             SELECT body as val FROM posts",
        );

        // UNION has no table_oid → defaults to nullable.
        // Correct here since body is nullable.
        assert!(
            col(&info, "val").nullable,
            "UNION with nullable branch is nullable"
        );
    }

    // ── Nullability: DML RETURNING ──────────────────────────────────────

    #[test]
    #[ignore] // Requires Docker
    fn null_insert_returning_not_null() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "INSERT INTO users (name, email) VALUES ($1, $2) RETURNING id, name, created_at",
        );

        // DML RETURNING: table_oid points to the target table.
        assert!(!col(&info, "id").nullable, "RETURNING: id is NOT NULL");
        assert!(!col(&info, "name").nullable, "RETURNING: name is NOT NULL");
        assert!(
            !col(&info, "created_at").nullable,
            "RETURNING: created_at is NOT NULL"
        );
    }

    #[test]
    #[ignore] // Requires Docker
    fn null_insert_returning_nullable_col() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "INSERT INTO users (name, email) VALUES ($1, $2) RETURNING id, age",
        );

        assert!(!col(&info, "id").nullable);
        assert!(col(&info, "age").nullable, "RETURNING: age is nullable");
    }

    #[test]
    #[ignore] // Requires Docker
    fn null_update_returning() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "UPDATE users SET age = $1 WHERE id = $2 RETURNING id, name, age",
        );

        assert!(!col(&info, "id").nullable);
        assert!(!col(&info, "name").nullable);
        assert!(col(&info, "age").nullable);
    }

    #[test]
    #[ignore] // Requires Docker
    fn null_delete_returning() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "DELETE FROM users WHERE id = $1 RETURNING id, name, age",
        );

        assert!(!col(&info, "id").nullable);
        assert!(!col(&info, "name").nullable);
        assert!(col(&info, "age").nullable);
    }

    // ── Nullability: CTE with DML ──────────────────────────────────────

    #[test]
    #[ignore] // Requires Docker
    fn null_cte_with_insert() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "WITH new_user AS (\
                INSERT INTO users (name, email) VALUES ($1, $2) RETURNING id, name, age\
             ) SELECT id, name, age FROM new_user",
        );

        // CTE wrapping DML: table_oid still points to the base table
        // through the CTE, so nullability is correct.
        assert!(!col(&info, "id").nullable);
        assert!(!col(&info, "name").nullable);
        assert!(col(&info, "age").nullable);
    }

    // ── Nullability: WHERE filters don't affect nullability ─────────────

    #[test]
    #[ignore] // Requires Docker
    fn null_where_is_not_null_does_not_change_result() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "SELECT id, age FROM users WHERE age IS NOT NULL",
        );

        // WHERE filters don't change column metadata. The column is still
        // nullable per the table definition. This is correct — the type
        // system reflects the schema, not runtime filters.
        assert!(
            col(&info, "age").nullable,
            "WHERE IS NOT NULL does not change column nullability"
        );
    }

    // ── Nullability: column name ambiguity resolved ─────────────────────

    #[test]
    #[ignore] // Requires Docker
    fn null_ambiguous_column_name_resolved_correctly() {
        let mut client = setup_pg();

        // Both users.id and posts.id are NOT NULL. The old name-based
        // approach could be ambiguous if different tables had columns with
        // the same name but different nullability. table_oid resolves this
        // because each column carries its exact source table OID.
        let info = query(
            &mut client,
            "SELECT u.id as user_id, p.id as post_id, p.body \
             FROM users u \
             INNER JOIN posts p ON p.user_id = u.id",
        );

        assert!(!col(&info, "user_id").nullable);
        assert!(!col(&info, "post_id").nullable);
        assert!(col(&info, "body").nullable, "posts.body is nullable");
    }

    // ── Nullability: auto-detection limitations fixed with annotations ──
    //
    // These tests demonstrate cases where auto-detection gives wrong results
    // and show how user annotations (`!` / `?`) fix them correctly.

    #[test]
    #[ignore] // Requires Docker
    fn fix_left_join_not_null_col_with_annotation() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "SELECT u.name, p.title as \"title?\" \
             FROM users u \
             LEFT JOIN posts p ON p.user_id = u.id",
        );

        // Without annotation, title would report NOT NULL (from base table).
        // The `?` annotation correctly forces it to nullable.
        assert!(!col(&info, "name").nullable);
        assert!(col(&info, "title").nullable, "? fixes LEFT JOIN");
    }

    #[test]
    #[ignore] // Requires Docker
    fn fix_right_join_not_null_col_with_annotation() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "SELECT u.name as \"name?\", p.title \
             FROM users u \
             RIGHT JOIN posts p ON p.user_id = u.id",
        );

        assert!(col(&info, "name").nullable, "? fixes RIGHT JOIN");
        assert!(!col(&info, "title").nullable);
    }

    #[test]
    #[ignore] // Requires Docker
    fn fix_full_join_not_null_cols_with_annotation() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "SELECT u.name as \"name?\", p.title as \"title?\" \
             FROM users u \
             FULL OUTER JOIN posts p ON p.user_id = u.id",
        );

        assert!(col(&info, "name").nullable, "? fixes FULL JOIN left");
        assert!(col(&info, "title").nullable, "? fixes FULL JOIN right");
    }

    #[test]
    #[ignore] // Requires Docker
    fn fix_count_with_annotation() {
        let mut client = setup_pg();
        let info = query(&mut client, "SELECT COUNT(*) as \"cnt!\" FROM users");

        assert!(!col(&info, "cnt").nullable, "! fixes COUNT");
    }

    #[test]
    #[ignore] // Requires Docker
    fn fix_coalesce_with_annotation() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "SELECT COALESCE(age, 0) as \"safe_age!\" FROM users",
        );

        assert!(!col(&info, "safe_age").nullable, "! fixes COALESCE");
    }

    #[test]
    #[ignore] // Requires Docker
    fn fix_literal_with_annotation() {
        let mut client = setup_pg();
        let info = query(&mut client, "SELECT 'constant' as \"label!\" FROM users");

        assert!(!col(&info, "label").nullable, "! fixes literal");
    }

    #[test]
    #[ignore] // Requires Docker
    fn fix_case_with_else_with_annotation() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "SELECT CASE WHEN age > 18 THEN 'adult' ELSE 'minor' END as \"category!\" FROM users",
        );

        assert!(!col(&info, "category").nullable, "! fixes CASE with ELSE");
    }

    #[test]
    #[ignore] // Requires Docker
    fn fix_union_all_not_null_with_annotation() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "SELECT name as \"val!\" FROM users \
             UNION ALL \
             SELECT title as \"val!\" FROM posts",
        );

        assert!(!col(&info, "val").nullable, "! fixes UNION");
    }

    #[test]
    #[ignore] // Requires Docker
    fn fix_coalesce_in_dml_returning_with_annotation() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "INSERT INTO users (name, email) VALUES ($1, $2) \
             RETURNING id, COALESCE(age, 0) as \"safe_age!\"",
        );

        assert!(!col(&info, "id").nullable);
        assert!(
            !col(&info, "safe_age").nullable,
            "! fixes COALESCE in RETURNING"
        );
    }

    #[test]
    #[ignore] // Requires Docker
    fn fix_left_join_in_dml_cte_with_annotation() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "WITH ins AS (\
                INSERT INTO posts (user_id, title) VALUES ($1, $2) RETURNING id, user_id\
             ) \
             SELECT ins.id, u.name as \"name?\" \
             FROM ins \
             LEFT JOIN users u ON u.id = ins.user_id",
        );

        assert!(!col(&info, "id").nullable);
        assert!(col(&info, "name").nullable, "? fixes LEFT JOIN in DML CTE");
    }

    // ── Nullability: user annotations (`!` / `?`) ───────────────────────

    #[test]
    fn parse_annotation_bang() {
        let (name, ov) = parse_nullability_annotation("safe_age!");
        assert_eq!(name, "safe_age");
        assert_eq!(ov, Some(false));
    }

    #[test]
    fn parse_annotation_question() {
        let (name, ov) = parse_nullability_annotation("title?");
        assert_eq!(name, "title");
        assert_eq!(ov, Some(true));
    }

    #[test]
    fn parse_annotation_none() {
        let (name, ov) = parse_nullability_annotation("id");
        assert_eq!(name, "id");
        assert_eq!(ov, None);
    }

    #[test]
    #[ignore] // Requires Docker
    fn annotation_bang_forces_non_null() {
        let mut client = setup_pg();

        // COUNT is never NULL but has no table_oid → auto-detected as nullable.
        // The `!` annotation forces it to non-nullable.
        let info = query(&mut client, "SELECT COUNT(*) as \"cnt!\" FROM users");

        assert_eq!(col(&info, "cnt").name, "cnt");
        assert!(!col(&info, "cnt").nullable, "! annotation forces non-null");
    }

    #[test]
    #[ignore] // Requires Docker
    fn annotation_bang_coalesce() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "SELECT COALESCE(age, 0) as \"safe_age!\" FROM users",
        );

        assert_eq!(col(&info, "safe_age").name, "safe_age");
        assert!(!col(&info, "safe_age").nullable);
    }

    #[test]
    #[ignore] // Requires Docker
    fn annotation_bang_literal() {
        let mut client = setup_pg();
        let info = query(&mut client, "SELECT 'constant' as \"label!\" FROM users");

        assert!(!col(&info, "label").nullable);
    }

    #[test]
    #[ignore] // Requires Docker
    fn annotation_question_forces_nullable() {
        let mut client = setup_pg();

        // LEFT JOIN: title is NOT NULL in the table but can be NULL due to JOIN.
        // The `?` annotation forces it to nullable.
        let info = query(
            &mut client,
            "SELECT u.name, p.title as \"title?\" \
             FROM users u \
             LEFT JOIN posts p ON p.user_id = u.id",
        );

        assert!(!col(&info, "name").nullable);
        assert_eq!(col(&info, "title").name, "title");
        assert!(col(&info, "title").nullable, "? annotation forces nullable");
    }

    #[test]
    #[ignore] // Requires Docker
    fn annotation_question_on_not_null_column() {
        let mut client = setup_pg();

        // Even though id is NOT NULL, `?` overrides it.
        let info = query(&mut client, "SELECT id as \"id?\" FROM users");

        assert!(col(&info, "id").nullable, "? overrides NOT NULL");
    }

    #[test]
    #[ignore] // Requires Docker
    fn annotation_bang_on_nullable_column() {
        let mut client = setup_pg();

        // age is nullable but `!` forces non-null.
        let info = query(&mut client, "SELECT age as \"age!\" FROM users");

        assert!(!col(&info, "age").nullable, "! overrides nullable");
    }

    #[test]
    #[ignore] // Requires Docker
    fn annotation_mixed_with_auto() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "SELECT id, name, age as \"age!\", COUNT(*) as \"cnt!\" FROM users GROUP BY id, name, age",
        );

        // id and name: auto-detected NOT NULL (correct).
        assert!(!col(&info, "id").nullable);
        assert!(!col(&info, "name").nullable);
        // age: auto-detected nullable, but `!` forces non-null.
        assert!(!col(&info, "age").nullable);
        // cnt: no table_oid → auto nullable, but `!` forces non-null.
        assert!(!col(&info, "cnt").nullable);
    }

    #[test]
    #[ignore] // Requires Docker
    fn annotation_in_dml_returning() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "INSERT INTO users (name, email) VALUES ($1, $2) \
             RETURNING id, COALESCE(age, 0) as \"safe_age!\"",
        );

        assert!(!col(&info, "id").nullable);
        assert!(!col(&info, "safe_age").nullable, "! in RETURNING works");
    }

    #[test]
    #[ignore] // Requires Docker
    fn annotation_question_in_dml_cte_left_join() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "WITH ins AS (\
                INSERT INTO posts (user_id, title) VALUES ($1, $2) RETURNING id, user_id\
             ) \
             SELECT ins.id, u.name as \"name?\" \
             FROM ins \
             LEFT JOIN users u ON u.id = ins.user_id",
        );

        assert!(!col(&info, "id").nullable);
        assert!(col(&info, "name").nullable, "? fixes LEFT JOIN in DML CTE");
    }

    #[test]
    #[ignore] // Requires Docker
    fn annotation_union_bang() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "SELECT name as \"val!\" FROM users \
             UNION ALL \
             SELECT title as \"val!\" FROM posts",
        );

        assert!(!col(&info, "val").nullable, "! fixes UNION nullability");
    }
}
