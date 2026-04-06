//! Shared setup, helpers, and assertion utilities for comparative tests.

#![allow(dead_code)]

use std::collections::HashMap;

pub use cubos_sql_analyzer::export::export_schema;
pub use cubos_sql_analyzer::introspect;
pub use cubos_sql_analyzer::resolve::{AnalyzerConfig, analyze};
pub use cubos_sql_analyzer::schema::SchemaSnapshot;
pub use cubos_sql_core::query_info::{ColumnInfo, QueryInfo};

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
";

/// Connect to the shared PG container and return a base client (connected to
/// the `cubos_sql` maintenance database).
fn base_connect() -> postgres::Client {
    let search_dirs = [
        std::env::temp_dir()
            .join("cubos_sql_introspect_tests")
            .join(".cubos_sql"),
        std::env::temp_dir()
            .join("cubos_sql_analyzer_compare_tests")
            .join(".cubos_sql"),
    ];
    let conn_str = std::env::var("CUBOS_SQL_TEST_CONN").unwrap_or_else(|_| {
        for base in &search_dirs {
            if let Ok(entries) = std::fs::read_dir(base) {
                for entry in entries.flatten() {
                    let cj = entry.path().join("container.json");
                    if let Ok(content) = std::fs::read_to_string(&cj) {
                        if let Ok(info) = serde_json::from_str::<serde_json::Value>(&content) {
                            if info.get("ready").and_then(|v| v.as_bool()) == Some(true) {
                                let port = info["port"].as_u64().unwrap_or(5432);
                                return format!(
                                    "host=127.0.0.1 port={port} user=postgres password=postgres dbname=cubos_sql"
                                );
                            }
                        }
                    }
                }
            }
        }
        panic!(
            "No running cubos_sql test container found. \
             Run `cargo test -p cubos_sql_macros -- --ignored` first."
        );
    });

    postgres::Client::connect(&conn_str, postgres::NoTls).expect("Failed to connect")
}

/// Create a fresh database with a random name, run migrations, and return a
/// client connected to it. Each test gets its own isolated DB so there are no
/// conflicts from pre-existing types/tables.
pub fn connect() -> postgres::Client {
    let db_name = format!("test_{:016x}", rand::random::<u64>());

    // Connect to the maintenance DB to create the test database.
    let mut admin = base_connect();
    admin
        .batch_execute(&format!("CREATE DATABASE \"{db_name}\""))
        .expect("failed to create test database");
    drop(admin);

    // Build connection string for the new database.
    let mut base = base_connect();
    // Extract host/port from the admin connection by querying it.
    let row = base
        .query_one("SELECT inet_server_addr(), inet_server_port()", &[])
        .unwrap();
    let host: Option<std::net::IpAddr> = row.get(0);
    let port: Option<i32> = row.get(1);
    drop(base);

    let host = host
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let port = port.unwrap_or(5432);

    let conn_str =
        format!("host={host} port={port} user=postgres password=postgres dbname={db_name}");

    let mut client = postgres::Client::connect(&conn_str, postgres::NoTls)
        .expect("Failed to connect to test DB");
    client.batch_execute(MIGRATION).unwrap();
    client
}

pub fn setup() -> (SchemaSnapshot, postgres::Client) {
    let mut client = connect();
    let snapshot = export_schema(&mut client).unwrap();
    (snapshot, client)
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
// Live introspection (reuses cubos_sql_analyzer::introspect)
// ──────────────────────────────────────────────────────────────────────────────

pub fn live_introspect(client: &mut postgres::Client, sql: &str) -> QueryInfo {
    let empty = HashMap::new();
    introspect::introspect_query(client, sql, &empty, &empty, &empty).unwrap()
}

// ──────────────────────────────────────────────────────────────────────────────
// Assertion helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Run the static analyzer on a query (no live comparison).
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
pub fn assert_same_types(static_info: &QueryInfo, live_info: &QueryInfo, context: &str) {
    assert_eq!(
        static_info.columns.len(),
        live_info.columns.len(),
        "{context}: column count mismatch"
    );
    for (s, l) in static_info.columns.iter().zip(live_info.columns.iter()) {
        assert_eq!(s.name, l.name, "{context}: column name mismatch");
        assert_eq!(
            s.rust_type, l.rust_type,
            "{context}: type mismatch for column '{}'",
            s.name
        );
    }
    assert_eq!(
        static_info.params.len(),
        live_info.params.len(),
        "{context}: param count mismatch"
    );
    for (i, (s, l)) in static_info
        .params
        .iter()
        .zip(live_info.params.iter())
        .enumerate()
    {
        assert_eq!(
            s.rust_type, l.rust_type,
            "{context}: param {i} type mismatch"
        );
    }
}

/// Assert two QueryInfos are completely identical (types + nullability).
pub fn assert_identical(static_info: &QueryInfo, live_info: &QueryInfo, context: &str) {
    assert_same_types(static_info, live_info, context);
    for (s, l) in static_info.columns.iter().zip(live_info.columns.iter()) {
        assert_eq!(
            s.nullable, l.nullable,
            "{context}: nullability mismatch for column '{}' (static={}, live={})",
            s.name, s.nullable, l.nullable
        );
    }
}
