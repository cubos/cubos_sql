//! Shared setup, helpers, and assertion utilities for analyzer tests.

#![allow(dead_code)]

use std::collections::HashMap;

pub use cubos_sql_analyzer::query_info::{ColumnInfo, QueryInfo};
pub use cubos_sql_analyzer::resolve::{AnalyzerConfig, analyze};
pub use cubos_sql_analyzer::schema::SchemaSnapshot;

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

/// Build a schema snapshot from the test migration using the DDL interpreter.
pub fn setup() -> SchemaSnapshot {
    let migrations = vec![("0001.sql".to_string(), MIGRATION.to_string())];
    let (snapshot, _warnings) =
        cubos_sql_analyzer::seed::build_schema_from_migrations(&migrations).unwrap();
    snapshot
}

pub fn default_config() -> AnalyzerConfig {
    AnalyzerConfig {
        domains: HashMap::new(),
        enums: HashMap::new(),
        types: HashMap::new(),
        param_nullability: Vec::new(),
    }
}

pub fn config_with_nullable(nullable: &[Option<bool>]) -> AnalyzerConfig {
    AnalyzerConfig {
        domains: HashMap::new(),
        enums: HashMap::new(),
        types: HashMap::new(),
        param_nullability: nullable.to_vec(),
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Assertion helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Run the static analyzer on a query.
pub fn static_analyze(snapshot: &SchemaSnapshot, sql: &str) -> QueryInfo {
    analyze(snapshot, sql, &default_config()).unwrap()
}

pub fn col<'a>(info: &'a QueryInfo, name: &str) -> &'a ColumnInfo {
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

/// Assert two QueryInfos have identical types (ignoring nullability).
pub fn assert_same_types(a: &QueryInfo, b: &QueryInfo, context: &str) {
    assert_eq!(
        a.columns.len(),
        b.columns.len(),
        "{context}: column count mismatch"
    );
    for (ca, cb) in a.columns.iter().zip(b.columns.iter()) {
        assert_eq!(ca.name, cb.name, "{context}: column name mismatch");
        assert_eq!(
            ca.rust_type, cb.rust_type,
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
        assert_eq!(
            pa.rust_type, pb.rust_type,
            "{context}: param {i} type mismatch"
        );
    }
}

/// Assert two QueryInfos are completely identical (types + nullability).
pub fn assert_identical(a: &QueryInfo, b: &QueryInfo, context: &str) {
    assert_same_types(a, b, context);
    for (ca, cb) in a.columns.iter().zip(b.columns.iter()) {
        assert_eq!(
            ca.nullable, cb.nullable,
            "{context}: nullability mismatch for column '{}' (a={}, b={})",
            ca.name, ca.nullable, cb.nullable
        );
    }
}
