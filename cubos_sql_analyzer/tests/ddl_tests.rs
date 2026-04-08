//! Unit tests for the DDL interpreter.

use cubos_sql_analyzer::schema::{RelationKind, TypeKind};
use cubos_sql_analyzer::seed::build_schema_from_migrations;

fn build(migrations: &[(&str, &str)]) -> cubos_sql_analyzer::schema::SchemaSnapshot {
    let m: Vec<(String, String)> = migrations
        .iter()
        .map(|(f, s)| (f.to_string(), s.to_string()))
        .collect();
    let (snapshot, warnings) = build_schema_from_migrations(&m).unwrap();
    for w in &warnings {
        eprintln!("warning: {w}");
    }
    snapshot
}

fn try_build(
    migrations: &[(&str, &str)],
) -> Result<cubos_sql_analyzer::schema::SchemaSnapshot, cubos_sql_analyzer::ddl::DdlError> {
    let m: Vec<(String, String)> = migrations
        .iter()
        .map(|(f, s)| (f.to_string(), s.to_string()))
        .collect();
    let (snapshot, _) = build_schema_from_migrations(&m)?;
    Ok(snapshot)
}

// ─── CREATE TABLE ───────────────────────────────────────────────────────────

#[test]
fn create_table_basic() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE users (
            id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            name TEXT NOT NULL,
            email TEXT NOT NULL,
            age INT
        );",
    )]);

    let table = snap.resolve_table(Some("public"), "users").unwrap();
    assert_eq!(table.name, "users");
    assert_eq!(table.kind, RelationKind::Table);
    assert_eq!(table.columns.len(), 4);

    let id_col = &table.columns[0];
    assert_eq!(id_col.name, "id");
    assert!(id_col.not_null);
    assert!(id_col.has_default); // IDENTITY

    let name_col = &table.columns[1];
    assert_eq!(name_col.name, "name");
    assert!(name_col.not_null);
    assert!(!name_col.has_default);

    let age_col = &table.columns[3];
    assert_eq!(age_col.name, "age");
    assert!(!age_col.not_null);
    assert!(!age_col.has_default);
}

#[test]
fn create_table_with_default() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (
            id SERIAL PRIMARY KEY,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            name TEXT NOT NULL
        );",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let id_col = &table.columns[0];
    assert!(id_col.has_default); // SERIAL

    let created_col = &table.columns[1];
    assert!(created_col.has_default); // DEFAULT now()
    assert!(created_col.not_null);

    let name_col = &table.columns[2];
    assert!(!name_col.has_default);
}

#[test]
fn create_table_registers_composite_and_array_types() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE items (id INT NOT NULL, name TEXT);",
    )]);

    // Composite type for the table.
    let ct = snap.resolve_type_by_name(Some("public"), "items").unwrap();
    assert!(matches!(ct.kind, TypeKind::Composite { .. }));

    // Array type.
    let at = snap.resolve_type_by_name(Some("public"), "_items").unwrap();
    assert!(matches!(at.kind, TypeKind::Array { .. }));
}

#[test]
fn create_table_if_not_exists() {
    let snap = build(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL);"),
        (
            "0002.sql",
            "CREATE TABLE IF NOT EXISTS t (id INT, name TEXT);",
        ),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    // Should still have original schema (1 column), not the second one.
    assert_eq!(table.columns.len(), 1);
}

// ─── ALTER TABLE ────────────────────────────────────────────────────────────

#[test]
fn alter_table_add_column() {
    let snap = build(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL);"),
        ("0002.sql", "ALTER TABLE t ADD COLUMN name TEXT;"),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    assert_eq!(table.columns.len(), 2);
    assert_eq!(table.columns[1].name, "name");
    assert!(!table.columns[1].not_null);
}

#[test]
fn alter_table_drop_column() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT, age INT);",
        ),
        ("0002.sql", "ALTER TABLE t DROP COLUMN name;"),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    assert_eq!(table.columns.len(), 2);
    assert_eq!(table.columns[0].name, "id");
    assert_eq!(table.columns[1].name, "age");
}

#[test]
fn alter_table_set_not_null() {
    let snap = build(&[
        ("0001.sql", "CREATE TABLE t (id INT, name TEXT);"),
        ("0002.sql", "ALTER TABLE t ALTER COLUMN name SET NOT NULL;"),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    let name_col = table.columns.iter().find(|c| c.name == "name").unwrap();
    assert!(name_col.not_null);
}

#[test]
fn alter_table_drop_not_null() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);",
        ),
        ("0002.sql", "ALTER TABLE t ALTER COLUMN name DROP NOT NULL;"),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    let name_col = table.columns.iter().find(|c| c.name == "name").unwrap();
    assert!(!name_col.not_null);
}

#[test]
fn alter_table_set_default() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, status TEXT NOT NULL);",
        ),
        (
            "0002.sql",
            "ALTER TABLE t ALTER COLUMN status SET DEFAULT 'active';",
        ),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    let status_col = table.columns.iter().find(|c| c.name == "status").unwrap();
    assert!(status_col.has_default);
}

#[test]
fn alter_table_alter_column_type() {
    let snap = build(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL, amount INT);"),
        ("0002.sql", "ALTER TABLE t ALTER COLUMN amount TYPE BIGINT;"),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    let amount_col = table.columns.iter().find(|c| c.name == "amount").unwrap();
    // Should resolve to int8 OID.
    let int8_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int8")
        .unwrap()
        .oid;
    assert_eq!(amount_col.type_oid, int8_oid);
}

// ─── CREATE DOMAIN ──────────────────────────────────────────────────────────

#[test]
fn create_domain() {
    let snap = build(&[("0001.sql", "CREATE DOMAIN email AS TEXT;")]);

    let te = snap.resolve_type_by_name(None, "email").unwrap();
    match &te.kind {
        TypeKind::Domain { base_type_oid } => {
            let base = snap.get_type(*base_type_oid).unwrap();
            assert_eq!(base.name, "text");
        }
        _ => panic!("expected Domain, got {:?}", te.kind),
    }

    // Array type.
    assert!(
        snap.resolve_type_by_name(Some("public"), "_email")
            .is_some()
    );
}

// ─── CREATE TYPE AS ENUM ────────────────────────────────────────────────────

#[test]
fn create_enum() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');",
    )]);

    let te = snap.resolve_type_by_name(None, "mood").unwrap();
    match &te.kind {
        TypeKind::Enum { labels } => {
            assert_eq!(labels, &["sad", "ok", "happy"]);
        }
        _ => panic!("expected Enum, got {:?}", te.kind),
    }
}

#[test]
fn alter_enum_add_value() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');",
        ),
        (
            "0002.sql",
            "ALTER TYPE mood ADD VALUE 'ecstatic' AFTER 'happy';",
        ),
    ]);

    let te = snap.resolve_type_by_name(None, "mood").unwrap();
    match &te.kind {
        TypeKind::Enum { labels } => {
            assert_eq!(labels, &["sad", "ok", "happy", "ecstatic"]);
        }
        _ => panic!("expected Enum"),
    }
}

#[test]
fn alter_enum_add_value_before() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');",
        ),
        (
            "0002.sql",
            "ALTER TYPE mood ADD VALUE 'anxious' BEFORE 'sad';",
        ),
    ]);

    let te = snap.resolve_type_by_name(None, "mood").unwrap();
    match &te.kind {
        TypeKind::Enum { labels } => {
            assert_eq!(labels, &["anxious", "sad", "ok", "happy"]);
        }
        _ => panic!("expected Enum"),
    }
}

// ─── CREATE TYPE AS (composite) ─────────────────────────────────────────────

#[test]
fn create_composite_type() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TYPE address AS (
            street TEXT,
            city TEXT,
            zip TEXT
        );",
    )]);

    let te = snap.resolve_type_by_name(None, "address").unwrap();
    match &te.kind {
        TypeKind::Composite { fields } => {
            assert_eq!(fields.len(), 3);
            assert_eq!(fields[0].name, "street");
            assert_eq!(fields[1].name, "city");
            assert_eq!(fields[2].name, "zip");
        }
        _ => panic!("expected Composite"),
    }
}

// ─── DROP ───────────────────────────────────────────────────────────────────

#[test]
fn drop_table() {
    let snap = build(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL);"),
        ("0002.sql", "DROP TABLE t;"),
    ]);

    assert!(snap.resolve_table(None, "t").is_none());
    // Composite and array types should also be removed.
    assert!(snap.resolve_type_by_name(Some("public"), "t").is_none());
    assert!(snap.resolve_type_by_name(Some("public"), "_t").is_none());
}

#[test]
fn drop_table_if_exists_no_error() {
    let snap = build(&[("0001.sql", "DROP TABLE IF EXISTS nonexistent;")]);
    assert!(snap.resolve_table(None, "nonexistent").is_none());
}

#[test]
fn drop_type() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');",
        ),
        ("0002.sql", "DROP TYPE mood;"),
    ]);

    assert!(snap.resolve_type_by_name(None, "mood").is_none());
    assert!(snap.resolve_type_by_name(Some("public"), "_mood").is_none());
}

// ─── CREATE FUNCTION ────────────────────────────────────────────────────────

#[test]
fn create_function_basic() {
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION add_one(x INT) RETURNS INT AS $$ SELECT x + 1 $$ LANGUAGE sql;",
    )]);

    let fns = snap.find_functions(None, "add_one");
    assert_eq!(fns.len(), 1);
    let f = fns[0];
    assert_eq!(f.arg_types.len(), 1);
    let int4_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int4")
        .unwrap()
        .oid;
    assert_eq!(f.arg_types[0], int4_oid);
    assert_eq!(f.return_type_oid, int4_oid);
}

// ─── CREATE EXTENSION ───────────────────────────────────────────────────────

#[test]
fn create_extension_uuid_ossp() {
    let snap = build(&[("0001.sql", "CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\";")]);

    let fns = snap.find_functions(None, "uuid_generate_v4");
    assert!(!fns.is_empty());
    let uuid_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "uuid")
        .unwrap()
        .oid;
    assert_eq!(fns[0].return_type_oid, uuid_oid);
}

#[test]
fn create_extension_citext() {
    let snap = build(&[("0001.sql", "CREATE EXTENSION citext;")]);

    let te = snap.resolve_type_by_name(None, "citext").unwrap();
    assert!(matches!(te.kind, TypeKind::Base));
    assert!(
        snap.resolve_type_by_name(Some("public"), "_citext")
            .is_some()
    );

    // Should have implicit cast citext -> text.
    let text_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "text")
        .unwrap()
        .oid;
    assert!(snap.has_implicit_cast(te.oid, text_oid));
}

#[test]
fn create_extension_with_schema() {
    let snap = build(&[(
        "0001.sql",
        "CREATE SCHEMA extensions;
         CREATE EXTENSION \"uuid-ossp\" SCHEMA extensions;",
    )]);

    // Functions should be in the 'extensions' schema.
    let fns = snap.find_functions(Some("extensions"), "uuid_generate_v4");
    assert!(
        !fns.is_empty(),
        "uuid_generate_v4 should be in 'extensions' schema"
    );
}

// ─── Multi-migration scenario ───────────────────────────────────────────────

#[test]
fn full_blog_schema() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE users (
                id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                name TEXT NOT NULL,
                email TEXT NOT NULL UNIQUE,
                age INT,
                created_at TIMESTAMPTZ NOT NULL DEFAULT now()
            );",
        ),
        (
            "0002.sql",
            "CREATE TYPE post_status AS ENUM ('draft', 'published', 'archived');
             CREATE TABLE posts (
                id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                user_id BIGINT NOT NULL REFERENCES users(id),
                title TEXT NOT NULL,
                body TEXT,
                status post_status NOT NULL DEFAULT 'draft',
                published_at TIMESTAMPTZ,
                created_at TIMESTAMPTZ NOT NULL DEFAULT now()
             );
             CREATE INDEX idx_posts_user_id ON posts (user_id);",
        ),
        (
            "0003.sql",
            "ALTER TABLE users ADD COLUMN bio TEXT;
             ALTER TYPE post_status ADD VALUE 'deleted' AFTER 'archived';",
        ),
    ]);

    // Users table has 6 columns now (id, name, email, age, created_at, bio).
    let users = snap.resolve_table(None, "users").unwrap();
    assert_eq!(users.columns.len(), 6);
    assert_eq!(users.columns[5].name, "bio");
    assert!(!users.columns[5].not_null);

    // Posts table.
    let posts = snap.resolve_table(None, "posts").unwrap();
    assert_eq!(posts.columns.len(), 7);

    // post_status enum has 4 values.
    let ps = snap.resolve_type_by_name(None, "post_status").unwrap();
    match &ps.kind {
        TypeKind::Enum { labels } => {
            assert_eq!(labels, &["draft", "published", "archived", "deleted"]);
        }
        _ => panic!("expected Enum"),
    }
}

// ─── CREATE SCHEMA ──────────────────────────────────────────────────────────

#[test]
fn create_schema_with_table() {
    let snap = build(&[(
        "0001.sql",
        "CREATE SCHEMA myapp;
         CREATE TABLE myapp.items (id INT NOT NULL, name TEXT NOT NULL);",
    )]);

    let table = snap.resolve_table(Some("myapp"), "items").unwrap();
    assert_eq!(table.columns.len(), 2);
    assert_eq!(table.schema, "myapp");
}

// ─── No-op DDL doesn't fail ────────────────────────────────────────────────

#[test]
fn noops_dont_fail() {
    let _snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (id INT NOT NULL);
         CREATE INDEX idx_t ON t (id);
         CREATE SEQUENCE my_seq;
         GRANT SELECT ON t TO PUBLIC;
         COMMENT ON TABLE t IS 'test table';",
    )]);
}

// ─── Views — PostgreSQL-compatible behavior ─────────────────────────────────

#[test]
fn view_basic_columns_resolved_at_creation() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
         CREATE VIEW v AS SELECT id, name FROM t;",
    )]);

    let view = snap.resolve_table(None, "v").unwrap();
    assert_eq!(view.kind, RelationKind::View);
    assert_eq!(view.columns.len(), 2);
    assert_eq!(view.columns[0].name, "id");
    assert_eq!(view.columns[1].name, "name");
}

#[test]
fn view_star_expanded_at_creation_time() {
    // PG expands SELECT * at CREATE VIEW time.
    // Adding a column AFTER does NOT change the view.
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
             CREATE VIEW v AS SELECT * FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t ADD COLUMN age INT;"),
    ]);

    let view = snap.resolve_table(None, "v").unwrap();
    // View should have 2 columns (expanded at creation), NOT 3.
    assert_eq!(
        view.columns.len(),
        2,
        "SELECT * should be expanded at creation time"
    );
    assert_eq!(view.columns[0].name, "id");
    assert_eq!(view.columns[1].name, "name");

    // The table itself has 3 columns.
    let table = snap.resolve_table(None, "t").unwrap();
    assert_eq!(table.columns.len(), 3);
}

#[test]
fn view_add_unrelated_column_succeeds() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
             CREATE VIEW v AS SELECT id FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t ADD COLUMN age INT;"),
    ]);

    let view = snap.resolve_table(None, "v").unwrap();
    assert_eq!(view.columns.len(), 1);
    assert_eq!(view.columns[0].name, "id");
}

#[test]
fn view_with_join() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL);
         CREATE TABLE posts (id INT NOT NULL, user_id INT NOT NULL, title TEXT NOT NULL);
         CREATE VIEW user_posts AS
             SELECT u.name, p.title FROM users u JOIN posts p ON p.user_id = u.id;",
    )]);

    let view = snap.resolve_table(None, "user_posts").unwrap();
    assert_eq!(view.columns.len(), 2);
    assert_eq!(view.columns[0].name, "name");
    assert_eq!(view.columns[1].name, "title");
}

#[test]
fn view_with_aliases() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
         CREATE VIEW v (the_id, the_name) AS SELECT id, name FROM t;",
    )]);

    let view = snap.resolve_table(None, "v").unwrap();
    assert_eq!(view.columns[0].name, "the_id");
    assert_eq!(view.columns[1].name, "the_name");
}

// ─── DROP COLUMN with view dependency ───────────────────────────────────────

#[test]
fn view_drop_referenced_column_fails_without_cascade() {
    let result = try_build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
             CREATE VIEW v AS SELECT id, name FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t DROP COLUMN name;"),
    ]);

    assert!(
        result.is_err(),
        "DROP COLUMN should fail when view depends on it"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("depend"),
        "error should mention dependency: {err}"
    );
}

#[test]
fn view_drop_referenced_column_cascade_drops_view() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
             CREATE VIEW v AS SELECT id, name FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t DROP COLUMN name CASCADE;"),
    ]);

    assert!(
        snap.resolve_table(None, "v").is_none(),
        "view should be dropped by CASCADE"
    );
    let table = snap.resolve_table(None, "t").unwrap();
    assert_eq!(table.columns.len(), 1);
    assert_eq!(table.columns[0].name, "id");
}

#[test]
fn view_drop_unrelated_column_succeeds() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL, age INT);
             CREATE VIEW v AS SELECT id, name FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t DROP COLUMN age;"),
    ]);

    let view = snap.resolve_table(None, "v").unwrap();
    assert_eq!(view.columns.len(), 2);
}

// ─── ALTER COLUMN TYPE with view dependency ─────────────────────────────────

#[test]
fn view_alter_type_fails_without_cascade() {
    let result = try_build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, amount INT);
             CREATE VIEW v AS SELECT id, amount FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t ALTER COLUMN amount TYPE BIGINT;"),
    ]);

    assert!(
        result.is_err(),
        "ALTER TYPE should fail when view depends on column"
    );
}

#[test]
fn view_alter_type_drop_view_then_alter_then_recreate() {
    // In PG, ALTER COLUMN TYPE always fails with dependent views.
    // The correct pattern is: DROP VIEW, ALTER TYPE, CREATE VIEW.
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, amount INT);
             CREATE VIEW v AS SELECT id, amount FROM t;",
        ),
        (
            "0002.sql",
            "DROP VIEW v;
             ALTER TABLE t ALTER COLUMN amount TYPE BIGINT;
             CREATE VIEW v AS SELECT id, amount FROM t;",
        ),
    ]);

    let view = snap.resolve_table(None, "v").unwrap();
    assert_eq!(view.columns.len(), 2);
    let amount = view.columns.iter().find(|c| c.name == "amount").unwrap();
    let int8_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int8")
        .unwrap()
        .oid;
    assert_eq!(amount.type_oid, int8_oid);
}

#[test]
fn view_alter_unrelated_column_succeeds() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT, age INT);
             CREATE VIEW v AS SELECT id, name FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t ALTER COLUMN age TYPE BIGINT;"),
    ]);

    let view = snap.resolve_table(None, "v").unwrap();
    assert_eq!(view.columns.len(), 2);
}

// ─── DROP TABLE with view dependency ────────────────────────────────────────

#[test]
fn view_drop_table_fails_without_cascade() {
    let result = try_build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL);
             CREATE VIEW v AS SELECT id FROM t;",
        ),
        ("0002.sql", "DROP TABLE t;"),
    ]);

    assert!(
        result.is_err(),
        "DROP TABLE should fail when view depends on it"
    );
}

#[test]
fn view_drop_table_cascade_drops_view() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL);
             CREATE VIEW v AS SELECT id FROM t;",
        ),
        ("0002.sql", "DROP TABLE t CASCADE;"),
    ]);

    assert!(snap.resolve_table(None, "t").is_none());
    assert!(snap.resolve_table(None, "v").is_none());
}

// ─── CREATE OR REPLACE VIEW ─────────────────────────────────────────────────

#[test]
fn create_or_replace_view() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL, age INT);
             CREATE VIEW v AS SELECT id, name FROM t;",
        ),
        (
            "0002.sql",
            "CREATE OR REPLACE VIEW v AS SELECT id, name, age FROM t;",
        ),
    ]);

    let view = snap.resolve_table(None, "v").unwrap();
    assert_eq!(view.columns.len(), 3);
    assert_eq!(view.columns[2].name, "age");
}

// ─── Extension versioning ───────────────────────────────────────────────────

#[test]
fn extension_version_creates_at_default() {
    let snap = build(&[("0001.sql", "CREATE EXTENSION citext;")]);

    // citext should be installed — check the type exists.
    let te = snap.resolve_type_by_name(None, "citext").unwrap();
    assert!(matches!(te.kind, TypeKind::Base));

    // Functions from upgrade scripts should also be present.
    let fns = snap.find_functions(None, "citext_hash_extended");
    assert!(
        !fns.is_empty(),
        "citext_hash_extended should exist (from 1.5->1.6 upgrade)"
    );
}

#[test]
fn extension_version_specific() {
    // Install citext at version 1.4 (base only, no upgrades).
    let snap = build(&[("0001.sql", "CREATE EXTENSION citext VERSION '1.4';")]);

    let te = snap.resolve_type_by_name(None, "citext").unwrap();
    assert!(matches!(te.kind, TypeKind::Base));

    // citext_hash_extended is from 1.5->1.6 upgrade, should NOT exist.
    let fns = snap.find_functions(None, "citext_hash_extended");
    assert!(
        fns.is_empty(),
        "citext_hash_extended should NOT exist at version 1.4"
    );
}

#[test]
fn extension_alter_update() {
    // Install at 1.4, then upgrade to 1.6.
    let snap = build(&[
        ("0001.sql", "CREATE EXTENSION citext VERSION '1.4';"),
        ("0002.sql", "ALTER EXTENSION citext UPDATE TO '1.6';"),
    ]);

    // citext_hash_extended is from 1.5->1.6, should now exist.
    let fns = snap.find_functions(None, "citext_hash_extended");
    assert!(
        !fns.is_empty(),
        "citext_hash_extended should exist after upgrade to 1.6"
    );

    // citext_pattern_lt is from 1.4->1.5, should also exist.
    let fns = snap.find_functions(None, "citext_pattern_lt");
    assert!(
        !fns.is_empty(),
        "citext_pattern_lt should exist after upgrade through 1.5"
    );
}

#[test]
fn extension_if_not_exists() {
    let snap = build(&[
        ("0001.sql", "CREATE EXTENSION citext;"),
        ("0002.sql", "CREATE EXTENSION IF NOT EXISTS citext;"),
    ]);

    // Should not error, just skip.
    let te = snap.resolve_type_by_name(None, "citext").unwrap();
    assert!(matches!(te.kind, TypeKind::Base));
}

#[test]
fn extension_unknown_warns_no_error() {
    // Unknown extension should produce a warning, not an error.
    let _snap = build(&[(
        "0001.sql",
        "CREATE EXTENSION IF NOT EXISTS some_unknown_ext;",
    )]);
}
