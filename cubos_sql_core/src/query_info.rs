//! Query introspection result types shared between the proc macro and analyzer.
//!
//! These types represent the output of query analysis — parameter types,
//! output column types, and nullability information. They are produced by
//! both the live introspection path (`cubos_sql_macros::introspect`) and
//! the static analyzer (`cubos_sql_analyzer`).

use serde::{Deserialize, Serialize};

/// Information about a single output column from a query.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
pub struct ParamInfo {
    /// PostgreSQL type OID.
    pub pg_type_oid: u32,
    /// Rust type string (e.g. `"i64"`, `"String"`).
    pub rust_type: String,
    /// Whether this parameter is nullable (`$foo?` syntax).
    /// When true, the generated Rust type is `Option<T>`.
    #[serde(default)]
    pub nullable: bool,
    /// If this is a JSONB domain, the Rust type path from config.
    pub domain_rust_type: Option<String>,
    /// If this is a mapped enum type, the Rust type path from config.
    #[serde(default)]
    pub enum_rust_type: Option<String>,
    /// PostgreSQL type name for explicit cast in the generated SQL
    /// (e.g. `"jsonb"`, `"int8"`, `"text"`). `None` means no cast.
    /// Resolved from the base type after unwrapping domains.
    #[serde(default)]
    pub cast_type: Option<String>,
}

/// Combined introspection result for a query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryInfo {
    pub params: Vec<ParamInfo>,
    pub columns: Vec<ColumnInfo>,
}
