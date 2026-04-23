//! Unit tests for the DDL interpreter.

use cubos_sql_analyzer::schema::{RelationKind, SchemaSnapshot, TypeKind};
use cubos_sql_analyzer::{AnalyzerConfig, Database, DdlError, QualifiedName};

fn build_db(migrations: &[(&str, &str)]) -> Database {
    let mut db = Database::new();
    for (_, sql) in migrations {
        db.apply_sql(sql).unwrap();
    }
    db
}

fn build(migrations: &[(&str, &str)]) -> SchemaSnapshot {
    build_db(migrations).into_snapshot()
}

fn try_apply(migrations: &[(&str, &str)]) -> Result<(), DdlError> {
    let mut db = Database::new();
    for (_, sql) in migrations {
        db.apply_sql(sql)?;
    }
    Ok(())
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
fn create_extension_vector_registers_operators() {
    // pgvector's CREATE OPERATOR statements (e.g. `<=>`, `<->`) must be
    // registered in the snapshot so expressions using them type-check.
    let snap = build(&[("0001.sql", "CREATE EXTENSION vector;")]);

    let vector_oid = snap
        .resolve_type_by_name(None, "vector")
        .expect("vector type should be registered")
        .oid;
    let float8_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "float8")
        .unwrap()
        .oid;

    // Cosine distance `<=>` should resolve with (vector, vector) operands.
    let op = snap
        .find_operator("<=>", Some(vector_oid), vector_oid)
        .expect("'<=>' operator should be registered for (vector, vector)");
    assert_eq!(op.left_type_oid, Some(vector_oid));
    assert_eq!(op.right_type_oid, vector_oid);
    assert_eq!(
        op.result_type_oid, float8_oid,
        "cosine_distance returns float8"
    );

    // L2 distance `<->` too.
    assert!(
        snap.find_operator("<->", Some(vector_oid), vector_oid)
            .is_some()
    );
}

#[test]
fn analyze_vector_query_maps_to_pgvector_rust_type() {
    // End-to-end: a SELECT on a vector column and a `<=>` expression should
    // produce Rust types routed to the `pgvector` crate. `cubos_sql` itself
    // does not depend on pgvector — the type path is emitted as a string.
    let db = build_db(&[(
        "0001.sql",
        "CREATE EXTENSION vector;
         CREATE TABLE items (
             id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
             embedding vector NOT NULL
         );",
    )]);
    let config = AnalyzerConfig::default();

    // Column of type `vector` is mapped to `pgvector::Vector`.
    let info = db
        .analyze("SELECT id, embedding FROM items", &config)
        .unwrap();
    let embedding = info.columns.iter().find(|c| c.name == "embedding").unwrap();
    assert_eq!(embedding.rust_type, "pgvector::Vector");
    assert!(!embedding.nullable);

    // A query using the `<=>` operator infers the parameter's type as vector
    // (via the operator's left operand), and the parameter's Rust type is
    // mapped to `pgvector::Vector`.
    let info = db
        .analyze(
            "SELECT id, (embedding <=> $q) AS dist FROM items ORDER BY embedding <=> $q",
            &config,
        )
        .unwrap();
    let dist = info.columns.iter().find(|c| c.name == "dist").unwrap();
    assert_eq!(dist.rust_type, "f64", "cosine_distance returns float8");
    assert_eq!(info.params.len(), 1);
    assert_eq!(info.params[0].rust_type, "pgvector::Vector");
    assert_eq!(
        info.params[0].cast_type.as_deref(),
        Some("vector"),
        "cast_type should fall through to the snapshot type name for extension types"
    );
}

#[test]
fn create_extension_tags_types_with_extension_name() {
    // Types created by `CREATE EXTENSION` must carry the extension name so
    // the Rust type mapper can route them to crate-specific Rust types.
    let snap = build(&[("0001.sql", "CREATE EXTENSION vector;")]);

    let vector = snap
        .resolve_type_by_name(None, "vector")
        .expect("vector type should be registered");
    assert_eq!(vector.extension.as_deref(), Some("vector"));

    let halfvec = snap
        .resolve_type_by_name(None, "halfvec")
        .expect("halfvec type should be registered");
    assert_eq!(halfvec.extension.as_deref(), Some("vector"));

    let sparsevec = snap
        .resolve_type_by_name(None, "sparsevec")
        .expect("sparsevec type should be registered");
    assert_eq!(sparsevec.extension.as_deref(), Some("vector"));

    // User-defined types (from the same migration) must NOT be tagged.
    let snap = build(&[(
        "0001.sql",
        "CREATE EXTENSION vector;
         CREATE TYPE my_thing AS ENUM ('a', 'b');",
    )]);
    let my_thing = snap
        .resolve_type_by_name(None, "my_thing")
        .expect("my_thing should exist");
    assert_eq!(my_thing.extension, None);
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
    let result = try_apply(&[
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
    let result = try_apply(&[
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
    let result = try_apply(&[
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
fn extension_unknown_is_error() {
    // Unknown extensions are rejected: the analyzer has no way to know what
    // types or functions they register, so silently accepting them would
    // hide type errors downstream.
    let err = try_apply(&[(
        "0001.sql",
        "CREATE EXTENSION IF NOT EXISTS some_unknown_ext;",
    )])
    .unwrap_err();
    assert!(
        matches!(err, DdlError::ExtensionError(_)),
        "expected ExtensionError, got: {err:?}"
    );
}

// ─── CREATE / DROP AGGREGATE ────────────────────────────────────────────────

#[test]
fn create_aggregate_registers_function() {
    // CREATE AGGREGATE should register the aggregate in functions_by_name
    // with is_aggregate = true and the STYPE as the return type when no
    // FINALFUNC is declared.
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION my_sum_sfunc(int8, int4) RETURNS int8
             AS 'SELECT $1 + $2::int8' LANGUAGE SQL;
         CREATE AGGREGATE my_sum(int4) (
             SFUNC = my_sum_sfunc,
             STYPE = int8,
             INITCOND = '0'
         );",
    )]);

    let fns = snap.find_functions(None, "my_sum");
    let agg = fns
        .iter()
        .find(|f| f.is_aggregate)
        .expect("my_sum should be registered as an aggregate");
    let int4_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int4")
        .unwrap()
        .oid;
    let int8_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int8")
        .unwrap()
        .oid;
    assert_eq!(agg.arg_types, vec![int4_oid]);
    assert_eq!(
        agg.return_type_oid, int8_oid,
        "no FINALFUNC ⇒ return type equals STYPE"
    );
}

#[test]
fn create_aggregate_with_finalfunc_uses_final_return_type() {
    // When FINALFUNC is declared, the aggregate's effective return type is
    // the final function's return type, not the STYPE.
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION my_avg_sfunc(int8, int4) RETURNS int8
             AS 'SELECT $1' LANGUAGE SQL;
         CREATE FUNCTION my_avg_finalfunc(int8) RETURNS float8
             AS 'SELECT $1::float8' LANGUAGE SQL;
         CREATE AGGREGATE my_avg(int4) (
             SFUNC = my_avg_sfunc,
             STYPE = int8,
             FINALFUNC = my_avg_finalfunc
         );",
    )]);

    let float8_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "float8")
        .unwrap()
        .oid;
    let agg = snap
        .find_functions(None, "my_avg")
        .into_iter()
        .find(|f| f.is_aggregate)
        .expect("my_avg aggregate should exist");
    assert_eq!(agg.return_type_oid, float8_oid);
    assert_eq!(agg.agg_final_type_oid, Some(float8_oid));
}

#[test]
fn drop_aggregate_removes_only_aggregate() {
    // DROP AGGREGATE must match on (name, arg_types, is_aggregate) — a
    // scalar function with the same name/signature must survive.
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION dup(int4) RETURNS int4 AS 'SELECT $1' LANGUAGE SQL;
         CREATE FUNCTION dup_sfunc(int4, int4) RETURNS int4 AS 'SELECT $1 + $2' LANGUAGE SQL;
         CREATE AGGREGATE dup(int4) (
             SFUNC = dup_sfunc,
             STYPE = int4
         );
         DROP AGGREGATE dup(int4);",
    )]);

    let fns = snap.find_functions(None, "dup");
    assert_eq!(fns.len(), 1, "scalar dup(int4) should remain");
    assert!(!fns[0].is_aggregate);
}

#[test]
fn drop_aggregate_missing_errors_without_if_exists() {
    let err = try_apply(&[("0001.sql", "DROP AGGREGATE nonexistent(int4);")])
        .expect_err("dropping a missing aggregate without IF EXISTS must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("aggregate nonexistent"),
        "error should mention the aggregate name, got: {msg}"
    );
}

#[test]
fn drop_aggregate_if_exists_no_error() {
    let _snap = build(&[("0001.sql", "DROP AGGREGATE IF EXISTS nonexistent(int4);")]);
}

// ─── DROP OPERATOR ──────────────────────────────────────────────────────────

#[test]
fn drop_operator_removes_only_matching_signature() {
    // pgvector registers multiple `<=>` overloads (vector/halfvec/sparsevec).
    // Dropping the (vector, vector) overload must leave the others alone.
    //
    // We inspect the raw registry directly because pgvector also defines
    // implicit casts between vector, halfvec and sparsevec — so
    // `find_operator` would still succeed via cast-based resolution after
    // the exact (vector, vector) entry is removed.
    let snap = build(&[(
        "0001.sql",
        "CREATE EXTENSION vector;
         DROP OPERATOR <=> (vector, vector);",
    )]);

    let vector_oid = snap.resolve_type_by_name(None, "vector").unwrap().oid;
    let halfvec_oid = snap.resolve_type_by_name(None, "halfvec").unwrap().oid;

    let ops = snap
        .operators_by_name
        .get(&QualifiedName::new("public", "<=>"))
        .expect("other overloads should still be registered");

    assert!(
        !ops.iter()
            .any(|o| o.left_type_oid == Some(vector_oid) && o.right_type_oid == vector_oid),
        "(vector, vector) overload should have been dropped"
    );
    assert!(
        ops.iter()
            .any(|o| o.left_type_oid == Some(halfvec_oid) && o.right_type_oid == halfvec_oid),
        "(halfvec, halfvec) overload should still be registered"
    );
}

#[test]
fn drop_operator_if_exists_no_error() {
    let _snap = build(&[("0001.sql", "DROP OPERATOR IF EXISTS <=> (int4, int4);")]);
}

#[test]
fn drop_operator_missing_errors_without_if_exists() {
    let err = try_apply(&[("0001.sql", "DROP OPERATOR <=> (int4, int4);")])
        .expect_err("dropping a missing operator without IF EXISTS must fail");
    assert!(err.to_string().contains("operator <=>"));
}

// ─── CREATE / DROP CAST ─────────────────────────────────────────────────────

#[test]
fn create_cast_registers_implicit_cast() {
    // A basic CREATE CAST between two built-in types with WITH INOUT.
    let snap = build(&[(
        "0001.sql",
        "CREATE CAST (int4 AS text) WITH INOUT AS IMPLICIT;",
    )]);

    let int4_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int4")
        .unwrap()
        .oid;
    let text_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "text")
        .unwrap()
        .oid;
    assert!(snap.has_implicit_cast(int4_oid, text_oid));
}

#[test]
fn drop_cast_removes_cast() {
    let snap = build(&[(
        "0001.sql",
        "CREATE CAST (int4 AS text) WITH INOUT AS IMPLICIT;
         DROP CAST (int4 AS text);",
    )]);

    let int4_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int4")
        .unwrap()
        .oid;
    let text_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "text")
        .unwrap()
        .oid;
    // The user-defined cast is gone. (Built-in int4→text cast is explicit,
    // not implicit, so this check still holds.)
    assert!(!snap.has_implicit_cast(int4_oid, text_oid));
}

#[test]
fn drop_cast_if_exists_no_error() {
    // There's no user-defined int2 → uuid cast; IF EXISTS must silence it.
    let _snap = build(&[("0001.sql", "DROP CAST IF EXISTS (int2 AS uuid);")]);
}

#[test]
fn drop_cast_missing_errors_without_if_exists() {
    let err = try_apply(&[("0001.sql", "DROP CAST (int2 AS uuid);")])
        .expect_err("dropping a missing cast without IF EXISTS must fail");
    assert!(err.to_string().contains("cast from"));
}

// ─── CREATE PROCEDURE ──────────────────────────────────────────────────────

#[test]
fn create_procedure_registers_with_is_procedure_flag() {
    // Procedures land in functions_by_name but must be flagged so the
    // resolver does not surface them as callable expressions.
    let snap = build(&[(
        "0001.sql",
        "CREATE PROCEDURE do_thing(x int) LANGUAGE SQL AS $$ SELECT $1 $$;",
    )]);

    let fns = snap
        .functions_by_name
        .get(&QualifiedName::new("public", "do_thing"))
        .expect("do_thing should be registered");
    assert_eq!(fns.len(), 1);
    assert!(fns[0].is_procedure);
    assert!(!fns[0].is_aggregate);
}

#[test]
fn procedure_is_not_callable_in_expressions() {
    // A `CREATE PROCEDURE` must not be considered a valid expression-level
    // function. Calling it inside a SELECT should fail analysis.
    let db = build_db(&[(
        "0001.sql",
        "CREATE PROCEDURE do_thing(x int) LANGUAGE SQL AS $$ SELECT $1 $$;",
    )]);

    let result = db.analyze("SELECT do_thing(1)", &AnalyzerConfig::default());
    assert!(
        result.is_err(),
        "procedures must not resolve as expression functions"
    );
}

// ─── CREATE / DROP MATERIALIZED VIEW ───────────────────────────────────────

#[test]
fn create_materialized_view_is_registered() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE items (id INT PRIMARY KEY, name TEXT NOT NULL);
         CREATE MATERIALIZED VIEW item_names AS SELECT id, name FROM items;",
    )]);

    let view = snap
        .resolve_table(None, "item_names")
        .expect("materialized view should be registered as a relation");
    assert_eq!(view.kind, RelationKind::MaterializedView);
    assert_eq!(view.columns.len(), 2);
    assert_eq!(view.columns[0].name, "id");
    assert_eq!(view.columns[1].name, "name");
}

#[test]
fn drop_materialized_view_removes_it() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE items (id INT PRIMARY KEY, name TEXT NOT NULL);
         CREATE MATERIALIZED VIEW item_names AS SELECT id, name FROM items;
         DROP MATERIALIZED VIEW item_names;",
    )]);

    assert!(snap.resolve_table(None, "item_names").is_none());
}

// ─── ALTER FUNCTION / AGGREGATE / OPERATOR ─────────────────────────────────

#[test]
fn alter_function_rename_moves_overload() {
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION add_one(x int) RETURNS int AS 'SELECT $1 + 1' LANGUAGE SQL;
         ALTER FUNCTION add_one(int) RENAME TO plus_one;",
    )]);

    assert!(
        !snap
            .functions_by_name
            .contains_key(&QualifiedName::new("public", "add_one")),
        "old name should be gone"
    );
    let fns = snap
        .functions_by_name
        .get(&QualifiedName::new("public", "plus_one"))
        .expect("renamed function should exist");
    assert_eq!(fns.len(), 1);
    let int4_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int4")
        .unwrap()
        .oid;
    assert_eq!(fns[0].arg_types, vec![int4_oid]);
}

#[test]
fn alter_function_set_schema_moves_it() {
    let snap = build(&[(
        "0001.sql",
        "CREATE SCHEMA utils;
         CREATE FUNCTION add_one(x int) RETURNS int AS 'SELECT $1 + 1' LANGUAGE SQL;
         ALTER FUNCTION add_one(int) SET SCHEMA utils;",
    )]);

    let fns = snap
        .functions_by_name
        .get(&QualifiedName::new("utils", "add_one"))
        .unwrap();
    assert_eq!(fns[0].schema, "utils");
}

#[test]
fn alter_function_rename_with_overloads_only_moves_matching_signature() {
    // Two overloads of the same function; the rename must only move the
    // one whose arg_types match.
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION do_it(x int) RETURNS int AS 'SELECT $1' LANGUAGE SQL;
         CREATE FUNCTION do_it(x text) RETURNS text AS 'SELECT $1' LANGUAGE SQL;
         ALTER FUNCTION do_it(int) RENAME TO do_it_int;",
    )]);

    let int4_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int4")
        .unwrap()
        .oid;
    let text_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "text")
        .unwrap()
        .oid;

    let renamed = snap
        .functions_by_name
        .get(&QualifiedName::new("public", "do_it_int"))
        .unwrap();
    assert_eq!(renamed.len(), 1);
    assert_eq!(renamed[0].arg_types, vec![int4_oid]);

    let remaining = snap
        .functions_by_name
        .get(&QualifiedName::new("public", "do_it"))
        .unwrap();
    assert_eq!(
        remaining.len(),
        1,
        "text overload should still be under do_it"
    );
    assert_eq!(remaining[0].arg_types, vec![text_oid]);
}

#[test]
fn alter_aggregate_rename_only_touches_aggregate() {
    // A scalar function and an aggregate share the same name. ALTER
    // AGGREGATE must only rename the aggregate.
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION ag(x int) RETURNS int AS 'SELECT $1' LANGUAGE SQL;
         CREATE FUNCTION ag_sfunc(state int, val int) RETURNS int AS 'SELECT $1 + $2' LANGUAGE SQL;
         CREATE AGGREGATE ag(int) (SFUNC = ag_sfunc, STYPE = int);
         ALTER AGGREGATE ag(int) RENAME TO ag_total;",
    )]);

    // Scalar survives under original name.
    let scalar = snap
        .functions_by_name
        .get(&QualifiedName::new("public", "ag"))
        .unwrap();
    assert_eq!(scalar.len(), 1);
    assert!(!scalar[0].is_aggregate);

    // Aggregate moved.
    let moved = snap
        .functions_by_name
        .get(&QualifiedName::new("public", "ag_total"))
        .unwrap();
    assert_eq!(moved.len(), 1);
    assert!(moved[0].is_aggregate);
}

#[test]
fn alter_operator_is_noop_but_does_not_crash() {
    // ALTER OPERATOR currently only changes attributes (join selectivity,
    // restriction selectivity). None of those affect type analysis, so this
    // must be a successful no-op.
    let _snap = build(&[(
        "0001.sql",
        "CREATE EXTENSION vector;
         ALTER OPERATOR <=> (vector, vector) SET (RESTRICT = scalarlesel);",
    )]);
}

// ─── DROP SCHEMA ───────────────────────────────────────────────────────────

#[test]
fn drop_schema_empty_succeeds() {
    let snap = build(&[(
        "0001.sql",
        "CREATE SCHEMA temp_stuff;
         DROP SCHEMA temp_stuff;",
    )]);
    assert!(!snap.search_path.contains(&"temp_stuff".to_string()));
}

#[test]
fn drop_schema_with_objects_fails_without_cascade() {
    let err = try_apply(&[(
        "0001.sql",
        "CREATE SCHEMA foo;
         CREATE TABLE foo.bar (id INT PRIMARY KEY);
         DROP SCHEMA foo;",
    )])
    .expect_err("DROP SCHEMA without CASCADE must fail when schema has objects");
    assert!(err.to_string().contains("cannot drop schema"));
}

#[test]
fn drop_schema_cascade_removes_all_contents() {
    let snap = build(&[(
        "0001.sql",
        "CREATE SCHEMA foo;
         CREATE TABLE foo.bar (id INT PRIMARY KEY, name TEXT NOT NULL);
         CREATE TYPE foo.my_enum AS ENUM ('a', 'b');
         CREATE FUNCTION foo.do_it(x int) RETURNS int AS 'SELECT $1' LANGUAGE SQL;
         DROP SCHEMA foo CASCADE;",
    )]);

    assert!(snap.resolve_table(Some("foo"), "bar").is_none());
    assert!(snap.resolve_type_by_name(Some("foo"), "my_enum").is_none());
    assert!(snap.find_functions(Some("foo"), "do_it").is_empty());
}

#[test]
fn drop_schema_cascade_transitively_drops_views_in_other_schemas() {
    // A view in `public` depends on a table in `foo`. DROP SCHEMA foo
    // CASCADE must take the view down too.
    let snap = build(&[(
        "0001.sql",
        "CREATE SCHEMA foo;
         CREATE TABLE foo.items (id INT PRIMARY KEY, name TEXT NOT NULL);
         CREATE VIEW public.item_names AS SELECT id, name FROM foo.items;
         DROP SCHEMA foo CASCADE;",
    )]);

    assert!(snap.resolve_table(Some("public"), "item_names").is_none());
}

#[test]
fn drop_schema_if_exists_no_error() {
    let _snap = build(&[("0001.sql", "DROP SCHEMA IF EXISTS nonexistent;")]);
}

#[test]
fn drop_schema_missing_errors_without_if_exists() {
    let err = try_apply(&[("0001.sql", "DROP SCHEMA nonexistent;")])
        .expect_err("dropping a missing schema without IF EXISTS must fail");
    assert!(err.to_string().contains("nonexistent"));
}

// ─── CASCADE for function/aggregate drops ──────────────────────────────────

#[test]
fn drop_function_cascade_accepted() {
    // DROP FUNCTION ... CASCADE is a syntactic valid form. It must parse
    // and execute without erroring.
    let _snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION add_one(x int) RETURNS int AS 'SELECT $1 + 1' LANGUAGE SQL;
         DROP FUNCTION add_one(int) CASCADE;",
    )]);
}

#[test]
fn drop_aggregate_cascade_accepted() {
    let _snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION sum_sfunc(state int, val int) RETURNS int AS 'SELECT $1 + $2' LANGUAGE SQL;
         CREATE AGGREGATE my_total(int) (SFUNC = sum_sfunc, STYPE = int);
         DROP AGGREGATE my_total(int) CASCADE;",
    )]);
}

#[test]
fn drop_function_does_not_touch_procedure_of_same_name() {
    // DROP FUNCTION must not remove a procedure with the same name+sig,
    // mirroring PostgreSQL's asymmetry between the two object kinds.
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION f(x int) RETURNS int AS 'SELECT $1' LANGUAGE SQL;
         CREATE PROCEDURE f(x int) LANGUAGE SQL AS $$ SELECT $1 $$;
         DROP FUNCTION f(int);",
    )]);

    let fns = snap
        .functions_by_name
        .get(&QualifiedName::new("public", "f"))
        .unwrap();
    assert_eq!(fns.len(), 1, "procedure must survive DROP FUNCTION");
    assert!(fns[0].is_procedure);
}

#[test]
fn param_only_in_order_by_is_inferred() {
    // A parameter that appears exclusively in the ORDER BY clause must
    // still get its type inferred from the expression context. Regression:
    // previously ORDER BY was skipped entirely, so `$embedding` below was
    // reported as UNKNOWN.
    let db = build_db(&[(
        "0001.sql",
        "CREATE EXTENSION vector;
         CREATE TABLE items (
             id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
             embedding vector NOT NULL
         );",
    )]);

    let info = db
        .analyze(
            "SELECT id FROM items ORDER BY embedding <=> $q LIMIT 10",
            &AnalyzerConfig::default(),
        )
        .expect("ORDER BY param should be resolvable");
    assert_eq!(info.params.len(), 1);
    assert_eq!(info.params[0].rust_type, "pgvector::Vector");
}

#[test]
fn param_in_group_by_and_having_is_inferred() {
    // Params in GROUP BY / HAVING clauses must also be walked. This ensures
    // the analyzer collects and types them.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE orders (id BIGINT, total INT NOT NULL);",
    )]);

    let info = db
        .analyze(
            "SELECT total, COUNT(*) AS c
             FROM orders
             GROUP BY total
             HAVING COUNT(*) > $min",
            &AnalyzerConfig::default(),
        )
        .expect("HAVING param should be resolvable");
    assert_eq!(info.params.len(), 1);
    assert_eq!(info.params[0].rust_type, "i64");
}

#[test]
fn drop_procedure_does_not_touch_function_of_same_name() {
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION f(x int) RETURNS int AS 'SELECT $1' LANGUAGE SQL;
         CREATE PROCEDURE f(x int) LANGUAGE SQL AS $$ SELECT $1 $$;
         DROP PROCEDURE f(int);",
    )]);

    let fns = snap
        .functions_by_name
        .get(&QualifiedName::new("public", "f"))
        .unwrap();
    assert_eq!(fns.len(), 1);
    assert!(!fns[0].is_procedure);
}
