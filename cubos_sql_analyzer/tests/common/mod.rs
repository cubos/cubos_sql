//! Shared setup, helpers, and assertion utilities for analyzer tests.

#![allow(dead_code)]

pub use cubos_sql_analyzer::{AnalyzedColumn, AnalyzedQuery, Database, QualifiedName, Type};

/// Terse helper for building a [`QualifiedName`] in tests.
pub fn qn(schema: &str, name: &str) -> QualifiedName {
    QualifiedName::new(schema, name)
}

// ──────────────────────────────────────────────────────────────────────────────
// Setup
// ──────────────────────────────────────────────────────────────────────────────

pub const MIGRATION: &str = "\
    CREATE TYPE user_role AS ENUM ('admin', 'editor', 'viewer');
    CREATE DOMAIN user_prefs AS JSONB;
    CREATE SCHEMA whatsapp;
    CREATE DOMAIN whatsapp.health_data AS JSONB;
    CREATE TABLE users (\
        id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
        name TEXT NOT NULL, \
        email TEXT NOT NULL UNIQUE, \
        age INT, \
        role user_role NOT NULL DEFAULT 'viewer', \
        preferences user_prefs, \
        created_at TIMESTAMPTZ NOT NULL DEFAULT now()\
    );\
    CREATE TABLE posts (\
        id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
        user_id BIGINT NOT NULL REFERENCES users(id), \
        title TEXT NOT NULL, \
        body TEXT, \
        published_at TIMESTAMPTZ\
    );\
    CREATE TABLE comments (\
        id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
        post_id BIGINT NOT NULL REFERENCES posts(id), \
        author_name TEXT NOT NULL, \
        content TEXT NOT NULL, \
        rating INT\
    );\
    CREATE TABLE whatsapp.channels (\
        channel_id BIGINT PRIMARY KEY, \
        health whatsapp.health_data, \
        updated_at TIMESTAMPTZ NOT NULL DEFAULT now()\
    );\
    CREATE TABLE whatsapp.contacts (\
        channel_id BIGINT NOT NULL, \
        id TEXT NOT NULL, \
        name TEXT, \
        pushname TEXT, \
        is_business BOOLEAN, \
        profile_pic TEXT, \
        profile_pic_full TEXT, \
        status TEXT, \
        saved BOOLEAN, \
        PRIMARY KEY (channel_id, id)\
    );\
";

/// Build a [`Database`] from the shared test migration.
pub fn setup() -> Database {
    let mut db = Database::new();
    db.apply_sql(MIGRATION).unwrap();
    db
}

// ──────────────────────────────────────────────────────────────────────────────
// Type constructors — reduce boilerplate in asserts
// ──────────────────────────────────────────────────────────────────────────────

pub fn basic(schema: &str, name: &str) -> Type {
    Type::Basic {
        schema: schema.into(),
        name: name.into(),
        extension: None,
    }
}

pub fn basic_ext(schema: &str, name: &str, extension: &str) -> Type {
    Type::Basic {
        schema: schema.into(),
        name: name.into(),
        extension: Some(extension.into()),
    }
}

pub fn array_of(element: Type) -> Type {
    Type::Array {
        element: Box::new(element),
    }
}

pub fn domain(schema: &str, name: &str, base: Type) -> Type {
    Type::Domain {
        schema: schema.into(),
        name: name.into(),
        base: Box::new(base),
        extension: None,
    }
}

pub fn enum_ty(schema: &str, name: &str, labels: &[&str]) -> Type {
    Type::Enum {
        schema: schema.into(),
        name: name.into(),
        labels: labels.iter().map(|s| s.to_string()).collect(),
        extension: None,
    }
}

pub fn range_of(schema: &str, name: &str, subtype: Type) -> Type {
    Type::Range {
        schema: schema.into(),
        name: name.into(),
        subtype: Box::new(subtype),
        extension: None,
    }
}

pub fn anon_record(attrs: Vec<(&str, Type)>) -> Type {
    Type::AnonymousRecord {
        attributes: attrs.into_iter().map(|(n, t)| (n.into(), t)).collect(),
    }
}

// Commonly used built-ins (short aliases).

pub fn bool_ty() -> Type {
    basic("pg_catalog", "bool")
}
pub fn bytea() -> Type {
    basic("pg_catalog", "bytea")
}
pub fn int2() -> Type {
    basic("pg_catalog", "int2")
}
pub fn int4() -> Type {
    basic("pg_catalog", "int4")
}
pub fn int8() -> Type {
    basic("pg_catalog", "int8")
}
pub fn float4() -> Type {
    basic("pg_catalog", "float4")
}
pub fn float8() -> Type {
    basic("pg_catalog", "float8")
}
pub fn numeric() -> Type {
    basic("pg_catalog", "numeric")
}
pub fn text() -> Type {
    basic("pg_catalog", "text")
}
pub fn varchar() -> Type {
    basic("pg_catalog", "varchar")
}
pub fn bpchar() -> Type {
    basic("pg_catalog", "bpchar")
}
pub fn name_ty() -> Type {
    basic("pg_catalog", "name")
}
pub fn uuid() -> Type {
    basic("pg_catalog", "uuid")
}
pub fn date() -> Type {
    basic("pg_catalog", "date")
}
pub fn time_ty() -> Type {
    basic("pg_catalog", "time")
}
pub fn timestamp() -> Type {
    basic("pg_catalog", "timestamp")
}
pub fn timestamptz() -> Type {
    basic("pg_catalog", "timestamptz")
}
pub fn interval() -> Type {
    basic("pg_catalog", "interval")
}
pub fn json_ty() -> Type {
    basic("pg_catalog", "json")
}
pub fn jsonb() -> Type {
    basic("pg_catalog", "jsonb")
}
pub fn oid_ty() -> Type {
    basic("pg_catalog", "oid")
}
pub fn unknown() -> Type {
    basic("pg_catalog", "unknown")
}
pub fn void() -> Type {
    basic("pg_catalog", "void")
}

// ──────────────────────────────────────────────────────────────────────────────
// Column / param specs for concise shape asserts
// ──────────────────────────────────────────────────────────────────────────────

/// What a test expects a column to look like. Compared against
/// [`AnalyzedColumn`] ignoring fields the analyzer reasonably derives (none
/// today — everything we track is in the spec).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColSpec {
    pub name: String,
    pub ty: Type,
    pub nullable: bool,
}

/// Non-null column.
pub fn c(name: &str, ty: Type) -> ColSpec {
    ColSpec {
        name: name.into(),
        ty,
        nullable: false,
    }
}

/// Nullable column.
pub fn cn(name: &str, ty: Type) -> ColSpec {
    ColSpec {
        name: name.into(),
        ty,
        nullable: true,
    }
}

/// Assert the query's output columns match the expected specs exactly.
#[track_caller]
pub fn assert_cols(analyzed: &AnalyzedQuery, expected: Vec<ColSpec>) {
    let actual: Vec<ColSpec> = analyzed
        .columns
        .iter()
        .map(|c| ColSpec {
            name: c.name.clone(),
            ty: c.pg_type.clone(),
            nullable: c.nullable,
        })
        .collect();
    assert_eq!(actual, expected, "columns mismatch");
}

/// What a test expects a param to look like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamSpec {
    pub ty: Type,
    pub nullable: bool,
    pub cast_type: Option<String>,
}

/// Non-null param, no cast.
pub fn p(ty: Type) -> ParamSpec {
    let cast_type = ty.cast_name();
    ParamSpec {
        ty,
        nullable: false,
        cast_type,
    }
}

/// Nullable param, no cast.
pub fn pn(ty: Type) -> ParamSpec {
    let cast_type = ty.cast_name();
    ParamSpec {
        ty,
        nullable: true,
        cast_type,
    }
}

/// Assert the query's params match the expected specs (order-sensitive).
#[track_caller]
pub fn assert_params(analyzed: &AnalyzedQuery, expected: Vec<ParamSpec>) {
    let actual: Vec<ParamSpec> = analyzed
        .params
        .iter()
        .map(|p| ParamSpec {
            ty: p.pg_type.clone(),
            nullable: p.nullable,
            cast_type: p.cast_type.clone(),
        })
        .collect();
    assert_eq!(actual, expected, "params mismatch");
}

// ──────────────────────────────────────────────────────────────────────────────
// Lookup helpers
// ──────────────────────────────────────────────────────────────────────────────

pub fn col<'a>(info: &'a AnalyzedQuery, name: &str) -> &'a AnalyzedColumn {
    info.columns
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| {
            panic!(
                "column '{name}' not found in: {:?}",
                info.columns.iter().map(|c| &c.name).collect::<Vec<_>>()
            )
        })
}

/// Assert two [`AnalyzedQuery`]s have identical types (ignoring nullability).
#[track_caller]
pub fn assert_same_types(a: &AnalyzedQuery, b: &AnalyzedQuery, context: &str) {
    assert_eq!(
        a.columns.len(),
        b.columns.len(),
        "{context}: column count mismatch"
    );
    for (ca, cb) in a.columns.iter().zip(b.columns.iter()) {
        assert_eq!(ca.name, cb.name, "{context}: column name mismatch");
        assert_eq!(
            ca.pg_type, cb.pg_type,
            "{context}: type mismatch for column '{}'",
            ca.name
        );
    }
    assert_eq!(
        a.params.len(),
        b.params.len(),
        "{context}: param count mismatch"
    );
    for (i, (pa, pb)) in a.params.iter().zip(b.params.iter()).enumerate() {
        assert_eq!(pa.pg_type, pb.pg_type, "{context}: param {i} type mismatch");
    }
}

/// Assert two [`AnalyzedQuery`]s are completely identical (types + nullability).
#[track_caller]
pub fn assert_identical(a: &AnalyzedQuery, b: &AnalyzedQuery, context: &str) {
    assert_same_types(a, b, context);
    for (ca, cb) in a.columns.iter().zip(b.columns.iter()) {
        assert_eq!(
            ca.nullable, cb.nullable,
            "{context}: nullability mismatch for column '{}' (a={}, b={})",
            ca.name, ca.nullable, cb.nullable
        );
    }
}
