//! PostgreSQL → Rust type mapping tables.
//!
//! The analyzer produces a [`typedpg_analyzer::Type`] that describes a
//! PostgreSQL type. The macro side owns the decision of how that PG type
//! maps into Rust — the tables below drive that mapping for built-in and
//! known-extension types. User-supplied overrides in
//! `[package.metadata.typedpg.{domains, enums, types}]` take precedence
//! over these tables.

/// Built-in PostgreSQL types, keyed by `(schema, pg_name)`.
///
/// Schema is always `pg_catalog` for true built-ins. Arrays are NOT listed
/// here — they are resolved generically via [`typedpg_analyzer::Type::Array`]
/// by recursing into the element type and wrapping the result in `Vec<…>`.
///
/// The mapped Rust types are emitted literally into generated code, so the
/// corresponding crates (`chrono`, `uuid`, …) must be added to the
/// consumer's `Cargo.toml` if they appear in any query.
static BUILTIN_MAP: &[(&str, &str, &str)] = &[
    // (schema, pg_name, rust_type)
    ("pg_catalog", "bool", "bool"),
    ("pg_catalog", "bytea", "Vec<u8>"),
    ("pg_catalog", "char", "String"),
    ("pg_catalog", "name", "String"),
    ("pg_catalog", "int8", "i64"),
    ("pg_catalog", "int2", "i16"),
    ("pg_catalog", "int4", "i32"),
    ("pg_catalog", "text", "String"),
    ("pg_catalog", "oid", "u32"),
    ("pg_catalog", "xid", "u32"),
    ("pg_catalog", "xid8", "u64"),
    ("pg_catalog", "json", "::serde_json::Value"),
    ("pg_catalog", "jsonb", "::serde_json::Value"),
    ("pg_catalog", "cidr", "String"),
    ("pg_catalog", "float4", "f32"),
    ("pg_catalog", "float8", "f64"),
    ("pg_catalog", "macaddr", "String"),
    ("pg_catalog", "inet", "String"),
    ("pg_catalog", "bpchar", "String"),
    ("pg_catalog", "varchar", "String"),
    ("pg_catalog", "date", "::chrono::NaiveDate"),
    ("pg_catalog", "time", "::chrono::NaiveTime"),
    ("pg_catalog", "timestamp", "::chrono::NaiveDateTime"),
    (
        "pg_catalog",
        "timestamptz",
        "::chrono::DateTime<::chrono::Utc>",
    ),
    ("pg_catalog", "interval", "String"),
    ("pg_catalog", "timetz", "String"),
    ("pg_catalog", "numeric", "::rust_decimal::Decimal"),
    ("pg_catalog", "regproc", "u32"),
    ("pg_catalog", "regprocedure", "u32"),
    ("pg_catalog", "regoper", "u32"),
    ("pg_catalog", "regoperator", "u32"),
    ("pg_catalog", "regclass", "u32"),
    ("pg_catalog", "regtype", "u32"),
    ("pg_catalog", "regnamespace", "u32"),
    ("pg_catalog", "regrole", "u32"),
    ("pg_catalog", "regcollation", "u32"),
    ("pg_catalog", "regconfig", "u32"),
    ("pg_catalog", "regdictionary", "u32"),
    ("pg_catalog", "anyelement", "String"),
    ("pg_catalog", "anyarray", "String"),
    ("pg_catalog", "uuid", "::uuid::Uuid"),
    ("pg_catalog", "pg_lsn", "String"),
    ("pg_catalog", "pg_ndistinct", "String"),
    ("pg_catalog", "pg_dependencies", "String"),
    ("pg_catalog", "pg_mcv_list", "String"),
    // UNKNOWN pseudo type — surfaces for empty VALUES lists and some literals
    // whose type PG hasn't finalized. Rendered as String so codegen produces
    // something usable instead of erroring.
    ("pg_catalog", "unknown", "String"),
    // Record pseudo type without known fields — surfaces when a subquery
    // returns `record` and we have no column list. Emit as String so the
    // generated struct compiles; callers typically cast at SQL level if they
    // need structure.
    ("pg_catalog", "record", "String"),
    ("pg_catalog", "void", "()"),
];

/// Types provided by well-known extensions.
///
/// Keyed by `(extension_name, pg_name)`. The `extension` comes from the
/// analyzer's [`typedpg_analyzer::Type::Basic`] (and friends) `extension`
/// field, which is set when the type was declared by a `CREATE EXTENSION`.
static EXTENSION_MAP: &[(&str, &str, &str)] = &[
    // (extension_name, pg_type_name, rust_type)
    ("vector", "vector", "::pgvector::Vector"),
    ("vector", "halfvec", "::pgvector::HalfVector"),
    ("vector", "sparsevec", "::pgvector::SparseVector"),
];

/// Look up a built-in PG type's Rust target. Returns `None` for types the
/// static table does not cover (extension types, user-defined, composites).
pub(crate) fn lookup_builtin(schema: &str, name: &str) -> Option<&'static str> {
    BUILTIN_MAP
        .iter()
        .find(|(s, n, _)| *s == schema && *n == name)
        .map(|(_, _, rt)| *rt)
}

/// Look up a type created by a known extension. Returns `None` when either
/// the extension or the specific type in it is not recognized.
pub(crate) fn lookup_extension(extension: &str, name: &str) -> Option<&'static str> {
    EXTENSION_MAP
        .iter()
        .find(|(ext, n, _)| *ext == extension && *n == name)
        .map(|(_, _, rt)| *rt)
}

/// PG text-family names whose target Rust type is `String`. When a query
/// parameter is typed against one of these, the generated code accepts any
/// `Into<String>`-like value as convenience (e.g. `&str`, `String`, `Cow`).
pub(crate) fn is_string_like(schema: &str, name: &str) -> bool {
    schema == "pg_catalog" && matches!(name, "text" | "varchar" | "bpchar" | "name" | "char")
}
