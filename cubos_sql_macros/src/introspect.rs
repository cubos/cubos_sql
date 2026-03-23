//! Re-exports live introspection from `cubos_sql_analyzer`.
//!
//! The actual implementation lives in `cubos_sql_analyzer::introspect`.
//! This module re-exports the public API and keeps the tests that use
//! the Docker container managed by `cubos_sql_macros::docker`.

pub use cubos_sql_analyzer::introspect::*;

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use cubos_sql_core::query_info::{ColumnInfo, QueryInfo};

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

    fn types_insert_returning() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "INSERT INTO users (name, email) VALUES ($1, $2) RETURNING id, created_at",
        );

        assert_eq!(info.params.len(), 2);
        assert_eq!(col(&info, "id").rust_type, "i64");
        assert_eq!(
            col(&info, "created_at").rust_type,
            "chrono::DateTime<chrono::Utc>"
        );
    }

    // ── Nullability ─────────────────────────────────────────────────────

    #[test]

    fn null_basic() {
        let mut client = setup_pg();
        let info = query(&mut client, "SELECT id, name, age, created_at FROM users");

        assert!(!col(&info, "id").nullable);
        assert!(!col(&info, "name").nullable);
        assert!(col(&info, "age").nullable);
        assert!(!col(&info, "created_at").nullable);
    }

    #[test]

    fn null_inner_join() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "SELECT u.name, p.title FROM users u INNER JOIN posts p ON p.user_id = u.id",
        );

        assert!(!col(&info, "name").nullable);
        assert!(!col(&info, "title").nullable);
    }

    #[test]

    fn null_dml_returning() {
        let mut client = setup_pg();
        let info = query(
            &mut client,
            "INSERT INTO users (name, email) VALUES ($1, $2) RETURNING id, name, age",
        );

        assert!(!col(&info, "id").nullable);
        assert!(!col(&info, "name").nullable);
        assert!(col(&info, "age").nullable);
    }

    // ── Annotations ─────────────────────────────────────────────────────

    #[test]

    fn annotation_bang() {
        let mut client = setup_pg();
        let info = query(&mut client, "SELECT COUNT(*) as \"cnt!\" FROM users");
        assert!(!col(&info, "cnt").nullable);
    }

    #[test]

    fn annotation_question() {
        let mut client = setup_pg();
        let info = query(&mut client, "SELECT id as \"id?\" FROM users");
        assert!(col(&info, "id").nullable);
    }
}
