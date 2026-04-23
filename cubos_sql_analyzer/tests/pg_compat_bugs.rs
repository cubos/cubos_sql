//! Tests that expose behavioral differences from PostgreSQL.
//!
//! Each test documents the expected PostgreSQL behavior and the bug in our
//! DDL interpreter. Tests are marked `#[should_panic]` where the bug is
//! confirmed — flip to normal `#[test]` after fixing.

use cubos_sql_analyzer::schema::{SchemaSnapshot, TypeKind};
use cubos_sql_analyzer::{Database, DdlError};

fn build(migrations: &[(&str, &str)]) -> SchemaSnapshot {
    let mut db = Database::new();
    for (_, sql) in migrations {
        db.apply_sql(sql).unwrap();
    }
    db.into_snapshot()
}

fn try_apply(migrations: &[(&str, &str)]) -> Result<(), DdlError> {
    let mut db = Database::new();
    for (_, sql) in migrations {
        db.apply_sql(sql)?;
    }
    Ok(())
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 1: Column alias hides dependency — DROP COLUMN succeeds when it shouldn't
// ═════════════════════════════════════════════════════════════════════════════
//
// PostgreSQL behavior:
//   CREATE TABLE t (id INT, name TEXT);
//   CREATE VIEW v AS SELECT id, name AS n FROM t;
//   ALTER TABLE t DROP COLUMN name;
//   → ERROR: cannot drop column name because view v depends on it
//
// Our bug: The view's output column is "n" (aliased). The dependency tracker
// matches output column names against table column names. "n" doesn't match
// "name" in table t, so no dependency is recorded. DROP COLUMN succeeds.

#[test]
fn bug_alias_hides_column_dependency() {
    let result = try_apply(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
             CREATE VIEW v AS SELECT id, name AS n FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t DROP COLUMN name;"),
    ]);
    // BUG: Should fail with dependency error (PG would fail here),
    // but currently succeeds because the alias "n" doesn't match "name".
    assert!(
        result.is_err(),
        "BUG: DROP COLUMN should fail when view references this column via alias"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 2: Computed column creates false dependency — DROP COLUMN fails when it shouldn't
// ═════════════════════════════════════════════════════════════════════════════
//
// PostgreSQL behavior:
//   CREATE TABLE t (id INT, name TEXT, age INT);
//   CREATE VIEW v AS SELECT count(*) AS age FROM t;
//   ALTER TABLE t DROP COLUMN age;
//   → SUCCESS (the view's "age" is count(*), not t.age)
//
// Our bug: The view's output column "age" matches table column "age" by name,
// so a false dependency is recorded. DROP COLUMN fails.

#[test]
fn bug_computed_column_false_dependency() {
    let result = try_apply(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL, age INT);
             CREATE VIEW v AS SELECT count(*) AS age FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t DROP COLUMN age;"),
    ]);
    // BUG: Should succeed — view's "age" is count(*), not t.age.
    // But our code falsely detects a dependency because the output column
    // name "age" matches table column name "age".
    assert!(
        result.is_ok(),
        "BUG: DROP COLUMN should succeed — view uses count(*) AS age, not t.age"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 3: SERIAL columns — PG treats as NOT NULL, we don't
// ═════════════════════════════════════════════════════════════════════════════
//
// PostgreSQL behavior:
//   CREATE TABLE t (id SERIAL PRIMARY KEY);
//   → id is NOT NULL (implied by PRIMARY KEY, which is a constraint on the column)
//
// But SERIAL alone (without PRIMARY KEY) is NOT implicitly NOT NULL in PG.
// Only the PRIMARY KEY constraint adds NOT NULL.
//
// Test: SERIAL without PRIMARY KEY should NOT be NOT NULL.

#[test]
fn serial_without_pk_is_nullable() {
    let snap = build(&[("0001.sql", "CREATE TABLE t (id SERIAL, name TEXT);")]);

    let table = snap.resolve_table(None, "t").unwrap();
    let id_col = table.columns.iter().find(|c| c.name == "id").unwrap();
    // SERIAL without PRIMARY KEY is NOT implicitly NOT NULL in PostgreSQL.
    assert!(
        !id_col.not_null,
        "SERIAL without PRIMARY KEY should be nullable"
    );
    assert!(id_col.has_default, "SERIAL should have a default");
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 4: CREATE TABLE IF NOT EXISTS with different schema — should be separate tables
// ═════════════════════════════════════════════════════════════════════════════
//
// PostgreSQL behavior:
//   CREATE TABLE public.t (id INT);
//   CREATE TABLE IF NOT EXISTS myschema.t (id INT, name TEXT);
//   → Two separate tables: public.t and myschema.t
//
// Our code checks `table_by_name` with the key for the NEW table. If the
// schemas differ, the keys differ, so both tables should exist.

#[test]
fn if_not_exists_different_schema_creates_both() {
    let snap = build(&[(
        "0001.sql",
        "CREATE SCHEMA other;
         CREATE TABLE public.t (id INT NOT NULL);
         CREATE TABLE IF NOT EXISTS other.t (id INT NOT NULL, name TEXT NOT NULL);",
    )]);

    let t1 = snap.resolve_table(Some("public"), "t").unwrap();
    assert_eq!(t1.columns.len(), 1);

    let t2 = snap.resolve_table(Some("other"), "t").unwrap();
    assert_eq!(t2.columns.len(), 2);
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 5: CREATE CAST with user-defined types
// ═════════════════════════════════════════════════════════════════════════════
//
// Test: Create a domain, create a cast, verify the cast exists in the snapshot.

#[test]
fn create_cast_between_user_types() {
    let snap = build(&[(
        "0001.sql",
        "CREATE DOMAIN email AS TEXT;
         CREATE CAST (email AS text) WITHOUT FUNCTION AS IMPLICIT;",
    )]);

    let email_oid = snap.resolve_type_by_name(None, "email").unwrap().oid;
    let text_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "text")
        .unwrap()
        .oid;
    assert!(
        snap.has_implicit_cast(email_oid, text_oid),
        "should have implicit cast email -> text"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 6: Type aliases — "integer" vs "int4", "boolean" vs "bool"
// ═════════════════════════════════════════════════════════════════════════════
//
// PostgreSQL allows various spellings. Our resolve_type_name normalizes them.
// Test that all common spellings resolve to the same OID.

#[test]
fn type_alias_resolution() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (
            a integer NOT NULL,
            b int NOT NULL,
            c bigint NOT NULL,
            d smallint NOT NULL,
            e boolean NOT NULL,
            f real NOT NULL,
            g text NOT NULL
        );",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let int4_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int4")
        .unwrap()
        .oid;
    let int8_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int8")
        .unwrap()
        .oid;
    let int2_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int2")
        .unwrap()
        .oid;
    let bool_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "bool")
        .unwrap()
        .oid;
    let float4_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "float4")
        .unwrap()
        .oid;
    let text_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "text")
        .unwrap()
        .oid;

    assert_eq!(
        table.columns[0].type_oid, int4_oid,
        "integer should resolve to int4"
    );
    assert_eq!(
        table.columns[1].type_oid, int4_oid,
        "int should resolve to int4"
    );
    assert_eq!(
        table.columns[2].type_oid, int8_oid,
        "bigint should resolve to int8"
    );
    assert_eq!(
        table.columns[3].type_oid, int2_oid,
        "smallint should resolve to int2"
    );
    assert_eq!(
        table.columns[4].type_oid, bool_oid,
        "boolean should resolve to bool"
    );
    assert_eq!(
        table.columns[5].type_oid, float4_oid,
        "real should resolve to float4"
    );
    assert_eq!(
        table.columns[6].type_oid, text_oid,
        "text should resolve to text"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 7: Array column types — TEXT[] should resolve to _text array type
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn array_column_type() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (id INT NOT NULL, tags TEXT[] NOT NULL);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let tags_col = table.columns.iter().find(|c| c.name == "tags").unwrap();
    // Should be the _text array type, not 0.
    assert_ne!(
        tags_col.type_oid, 0,
        "TEXT[] should resolve to a valid type OID"
    );

    // Verify it's actually an array type.
    let type_entry = snap.get_type(tags_col.type_oid).unwrap();
    assert!(
        matches!(type_entry.kind, TypeKind::Array { .. }),
        "TEXT[] should be an Array type, got {:?}",
        type_entry.kind
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 8: BIGSERIAL should resolve to int8, not int4
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn bigserial_resolves_to_int8() {
    let snap = build(&[("0001.sql", "CREATE TABLE t (id BIGSERIAL PRIMARY KEY);")]);

    let table = snap.resolve_table(None, "t").unwrap();
    let id_col = table.columns.iter().find(|c| c.name == "id").unwrap();
    let int8_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int8")
        .unwrap()
        .oid;
    assert_eq!(
        id_col.type_oid, int8_oid,
        "BIGSERIAL should resolve to int8"
    );
    assert!(id_col.has_default, "BIGSERIAL should have a default");
    assert!(id_col.not_null, "BIGSERIAL PRIMARY KEY should be NOT NULL");
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 9: SMALLSERIAL should resolve to int2
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn smallserial_resolves_to_int2() {
    let snap = build(&[("0001.sql", "CREATE TABLE t (id SMALLSERIAL NOT NULL);")]);

    let table = snap.resolve_table(None, "t").unwrap();
    let id_col = table.columns.iter().find(|c| c.name == "id").unwrap();
    let int2_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int2")
        .unwrap()
        .oid;
    assert_eq!(
        id_col.type_oid, int2_oid,
        "SMALLSERIAL should resolve to int2"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 10: View on view — CASCADE should propagate
// ═════════════════════════════════════════════════════════════════════════════
//
// PostgreSQL behavior:
//   CREATE TABLE t (id INT);
//   CREATE VIEW v1 AS SELECT id FROM t;
//   CREATE VIEW v2 AS SELECT id FROM v1;
//   DROP VIEW v1;
//   → ERROR: cannot drop view v1 because view v2 depends on it
//   DROP VIEW v1 CASCADE;
//   → Drops both v1 and v2

#[test]
fn view_on_view_drop_fails_without_cascade() {
    let result = try_apply(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL);
             CREATE VIEW v1 AS SELECT id FROM t;
             CREATE VIEW v2 AS SELECT id FROM v1;",
        ),
        ("0002.sql", "DROP VIEW v1;"),
    ]);

    assert!(
        result.is_err(),
        "DROP VIEW should fail when another view depends on it"
    );
}

#[test]
fn view_on_view_drop_cascade() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL);
             CREATE VIEW v1 AS SELECT id FROM t;
             CREATE VIEW v2 AS SELECT id FROM v1;",
        ),
        ("0002.sql", "DROP VIEW v1 CASCADE;"),
    ]);

    assert!(snap.resolve_table(None, "v1").is_none());
    assert!(snap.resolve_table(None, "v2").is_none());
    // Table should still exist.
    assert!(snap.resolve_table(None, "t").is_some());
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 11: DROP TABLE CASCADE should transitively drop view chains
// ═════════════════════════════════════════════════════════════════════════════
//
// Table t → view v1 → view v2. DROP TABLE t CASCADE should drop all.

#[test]
fn drop_table_cascade_transitive_views() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL);
             CREATE VIEW v1 AS SELECT id FROM t;
             CREATE VIEW v2 AS SELECT id FROM v1;",
        ),
        ("0002.sql", "DROP TABLE t CASCADE;"),
    ]);

    assert!(snap.resolve_table(None, "t").is_none());
    assert!(snap.resolve_table(None, "v1").is_none());
    assert!(
        snap.resolve_table(None, "v2").is_none(),
        "transitive view v2 should also be dropped by CASCADE"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 12: ALTER TABLE ADD CONSTRAINT PRIMARY KEY — should set NOT NULL
// ═════════════════════════════════════════════════════════════════════════════
//
// PostgreSQL behavior:
//   CREATE TABLE t (id INT, name TEXT);
//   ALTER TABLE t ADD CONSTRAINT t_pkey PRIMARY KEY (id);
//   → id becomes NOT NULL

#[test]
fn alter_table_add_pk_sets_not_null() {
    let snap = build(&[
        ("0001.sql", "CREATE TABLE t (id INT, name TEXT);"),
        (
            "0002.sql",
            "ALTER TABLE t ADD CONSTRAINT t_pkey PRIMARY KEY (id);",
        ),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    let id_col = table.columns.iter().find(|c| c.name == "id").unwrap();
    assert!(
        id_col.not_null,
        "PRIMARY KEY constraint should make column NOT NULL"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 13: CREATE TABLE with FOREIGN KEY referencing another table
// ═════════════════════════════════════════════════════════════════════════════
//
// FOREIGN KEY should NOT affect type or nullability — just a constraint.
// But it should parse without error.

#[test]
fn foreign_key_constraint_parses() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE users (id SERIAL PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE posts (
             id SERIAL PRIMARY KEY,
             user_id INT NOT NULL REFERENCES users(id),
             title TEXT NOT NULL
         );",
    )]);

    let posts = snap.resolve_table(None, "posts").unwrap();
    assert_eq!(posts.columns.len(), 3);
    let user_id = posts.columns.iter().find(|c| c.name == "user_id").unwrap();
    assert!(user_id.not_null);
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 14: Enum used as column type
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn enum_as_column_type() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TYPE status AS ENUM ('active', 'inactive');
         CREATE TABLE t (id INT NOT NULL, s status NOT NULL);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let s_col = table.columns.iter().find(|c| c.name == "s").unwrap();
    let status_oid = snap.resolve_type_by_name(None, "status").unwrap().oid;
    assert_eq!(s_col.type_oid, status_oid);
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 15: Domain used as column type
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn domain_as_column_type() {
    let snap = build(&[(
        "0001.sql",
        "CREATE DOMAIN email AS TEXT;
         CREATE TABLE t (id INT NOT NULL, contact email NOT NULL);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let contact = table.columns.iter().find(|c| c.name == "contact").unwrap();
    let email_oid = snap.resolve_type_by_name(None, "email").unwrap().oid;
    assert_eq!(contact.type_oid, email_oid);
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 16: Multiple ALTER TABLE commands in one statement
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn multiple_alter_commands() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT, age INT);",
        ),
        (
            "0002.sql",
            "ALTER TABLE t
                ALTER COLUMN name SET NOT NULL,
                ALTER COLUMN age SET DEFAULT 0;",
        ),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    let name = table.columns.iter().find(|c| c.name == "name").unwrap();
    let age = table.columns.iter().find(|c| c.name == "age").unwrap();
    assert!(name.not_null);
    assert!(age.has_default);
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 17: DROP COLUMN IF EXISTS on nonexistent column
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn drop_column_if_exists_nonexistent() {
    let snap = build(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL, name TEXT);"),
        (
            "0002.sql",
            "ALTER TABLE t DROP COLUMN IF EXISTS nonexistent;",
        ),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    assert_eq!(table.columns.len(), 2); // Unchanged.
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 18: CREATE TABLE with UNIQUE constraint
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn unique_constraint_parses() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (
            id SERIAL PRIMARY KEY,
            email TEXT NOT NULL UNIQUE,
            name TEXT NOT NULL
        );",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    assert_eq!(table.columns.len(), 3);
    let email = table.columns.iter().find(|c| c.name == "email").unwrap();
    assert!(email.not_null);
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 19: Composite type fields — verify they have correct types
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn composite_type_field_types() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TYPE address AS (
            street TEXT,
            city TEXT,
            zip INT
        );",
    )]);

    let te = snap.resolve_type_by_name(None, "address").unwrap();
    if let TypeKind::Composite { fields } = &te.kind {
        let text_oid = snap
            .resolve_type_by_name(Some("pg_catalog"), "text")
            .unwrap()
            .oid;
        let int4_oid = snap
            .resolve_type_by_name(Some("pg_catalog"), "int4")
            .unwrap()
            .oid;
        assert_eq!(fields[0].type_oid, text_oid);
        assert_eq!(fields[1].type_oid, text_oid);
        assert_eq!(fields[2].type_oid, int4_oid);
    } else {
        panic!("expected Composite");
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 20: Range type — verify subtype
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn range_type_subtype() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TYPE floatrange AS RANGE (subtype = float8);",
    )]);

    let te = snap.resolve_type_by_name(None, "floatrange").unwrap();
    if let TypeKind::Range { subtype_oid } = &te.kind {
        let float8_oid = snap
            .resolve_type_by_name(Some("pg_catalog"), "float8")
            .unwrap()
            .oid;
        assert_eq!(*subtype_oid, float8_oid);
    } else {
        panic!("expected Range");
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 21: DROP FUNCTION nukes all overloads instead of specific signature
// ═════════════════════════════════════════════════════════════════════════════
//
// PG: CREATE FUNCTION foo(INT) RETURNS INT ...;
//     CREATE FUNCTION foo(TEXT) RETURNS TEXT ...;
//     DROP FUNCTION foo(INT);
//     → only the INT overload is removed; foo(TEXT) still exists
//
// Our bug: drop_function does `functions_by_name.remove(&name)` which
// removes the entire Vec of overloads, not just the matching signature.

#[test]
fn bug_drop_function_keeps_other_overloads() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE FUNCTION foo(x INT) RETURNS INT AS $$ SELECT x $$ LANGUAGE sql;
             CREATE FUNCTION foo(x TEXT) RETURNS TEXT AS $$ SELECT x $$ LANGUAGE sql;",
        ),
        ("0002.sql", "DROP FUNCTION foo(INT);"),
    ]);

    let fns = snap.find_functions(None, "foo");
    assert_eq!(
        fns.len(),
        1,
        "BUG: DROP FUNCTION foo(INT) should only remove the INT overload, not foo(TEXT)"
    );
    let text_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "text")
        .unwrap()
        .oid;
    assert_eq!(
        fns[0].return_type_oid, text_oid,
        "remaining overload should be foo(TEXT)"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 22: ALTER ENUM ADD VALUE duplicate without IF NOT EXISTS silently adds it
// ═════════════════════════════════════════════════════════════════════════════
//
// PG: CREATE TYPE mood AS ENUM ('happy', 'sad');
//     ALTER TYPE mood ADD VALUE 'happy';
//     → ERROR: enum label "happy" already exists
//
// Our bug: when skip_if_new_val_exists is false (no IF NOT EXISTS),
// alter_enum pushes the value without checking for duplicates.

#[test]
fn bug_alter_enum_duplicate_value_errors() {
    let result = try_apply(&[
        ("0001.sql", "CREATE TYPE mood AS ENUM ('happy', 'sad');"),
        ("0002.sql", "ALTER TYPE mood ADD VALUE 'happy';"),
    ]);

    assert!(
        result.is_err(),
        "BUG: ALTER TYPE ADD VALUE should fail when value already exists (no IF NOT EXISTS)"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 23: View depends on column used in expression — DROP should fail
// ═════════════════════════════════════════════════════════════════════════════
//
// PG: CREATE VIEW v AS SELECT id + 1 AS next_id FROM t;
//     ALTER TABLE t DROP COLUMN id; → ERROR (view depends on t.id)
//
// The view output column is "next_id" but the expression references "id".
// Dependency tracking must find "id" in the ColumnRef AST, not just output names.

#[test]
fn view_depends_on_column_in_expression() {
    let result = try_apply(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
             CREATE VIEW v AS SELECT id + 1 AS next_id FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t DROP COLUMN id;"),
    ]);

    assert!(
        result.is_err(),
        "DROP COLUMN should fail — view expression references t.id"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 22: View depends on column in WHERE clause — DROP should fail
// ═════════════════════════════════════════════════════════════════════════════
//
// PG: CREATE VIEW v AS SELECT name FROM t WHERE id > 10;
//     ALTER TABLE t DROP COLUMN id; → ERROR (view depends on t.id in WHERE)

#[test]
fn view_depends_on_column_in_where() {
    let result = try_apply(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
             CREATE VIEW v AS SELECT name FROM t WHERE id > 10;",
        ),
        ("0002.sql", "ALTER TABLE t DROP COLUMN id;"),
    ]);

    assert!(
        result.is_err(),
        "DROP COLUMN should fail — view WHERE clause references t.id"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 23: View depends on column in JOIN ON — DROP should fail
// ═════════════════════════════════════════════════════════════════════════════
//
// PG: CREATE VIEW v AS SELECT t1.name FROM t1 JOIN t2 ON t1.id = t2.id;
//     ALTER TABLE t2 DROP COLUMN id; → ERROR (view depends on t2.id in JOIN)

#[test]
fn view_depends_on_column_in_join_on() {
    let result = try_apply(&[
        (
            "0001.sql",
            "CREATE TABLE t1 (id INT NOT NULL, name TEXT NOT NULL);
             CREATE TABLE t2 (id INT NOT NULL, status TEXT NOT NULL);
             CREATE VIEW v AS SELECT t1.name FROM t1 JOIN t2 ON t1.id = t2.id;",
        ),
        ("0002.sql", "ALTER TABLE t2 DROP COLUMN id;"),
    ]);

    assert!(
        result.is_err(),
        "DROP COLUMN should fail — view JOIN ON references t2.id"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 24: View depends on column in ORDER BY — DROP should fail
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn view_depends_on_column_in_order_by() {
    let result = try_apply(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL, score INT);
             CREATE VIEW v AS SELECT name FROM t ORDER BY score;",
        ),
        ("0002.sql", "ALTER TABLE t DROP COLUMN score;"),
    ]);

    assert!(
        result.is_err(),
        "DROP COLUMN should fail — view ORDER BY references t.score"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 25: ALTER TABLE ADD COLUMN IF NOT EXISTS — should skip if exists
// ═════════════════════════════════════════════════════════════════════════════
//
// PG: ALTER TABLE t ADD COLUMN IF NOT EXISTS name TEXT;
//     → skips silently if column "name" already exists

#[test]
fn add_column_if_not_exists() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);",
        ),
        (
            "0002.sql",
            "ALTER TABLE t ADD COLUMN IF NOT EXISTS name TEXT;",
        ),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    assert_eq!(table.columns.len(), 2, "should still have 2 columns");
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 26: CREATE OR REPLACE FUNCTION should update existing
// ═════════════════════════════════════════════════════════════════════════════
//
// PG: CREATE FUNCTION foo(INT) RETURNS INT ...;
//     CREATE OR REPLACE FUNCTION foo(INT) RETURNS BIGINT ...;
//     → replaces the function, return type changes

#[test]
fn create_or_replace_function() {
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION foo(x INT) RETURNS INT AS $$ SELECT x $$ LANGUAGE sql;
         CREATE OR REPLACE FUNCTION foo(x INT) RETURNS BIGINT AS $$ SELECT x::bigint $$ LANGUAGE sql;",
    )]);

    let fns = snap.find_functions(None, "foo");
    assert_eq!(fns.len(), 1, "should have exactly 1 overload");
    let int8_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int8")
        .unwrap()
        .oid;
    assert_eq!(
        fns[0].return_type_oid, int8_oid,
        "return type should be updated to int8"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 27: Function overloading — different arg types should coexist
// ═════════════════════════════════════════════════════════════════════════════
//
// PG: CREATE FUNCTION foo(INT) RETURNS INT ...;
//     CREATE FUNCTION foo(TEXT) RETURNS TEXT ...;
//     → two separate overloads

#[test]
fn function_overloading() {
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION foo(x INT) RETURNS INT AS $$ SELECT x $$ LANGUAGE sql;
         CREATE FUNCTION foo(x TEXT) RETURNS TEXT AS $$ SELECT x $$ LANGUAGE sql;",
    )]);

    let fns = snap.find_functions(None, "foo");
    assert_eq!(fns.len(), 2, "should have 2 overloads");
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 28: GENERATED ALWAYS AS (stored) should have has_default = true
// ═════════════════════════════════════════════════════════════════════════════
//
// PG: CREATE TABLE t (a INT, b INT GENERATED ALWAYS AS (a * 2) STORED);
//     → b has a default (computed), can't be set in INSERT

#[test]
fn generated_stored_column() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (a INT NOT NULL, b INT GENERATED ALWAYS AS (a * 2) STORED);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let b = table.columns.iter().find(|c| c.name == "b").unwrap();
    assert!(
        b.has_default,
        "GENERATED ALWAYS AS (stored) should have has_default = true"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 29: VARCHAR(n) and NUMERIC(p,s) should resolve to valid type OIDs
// ═════════════════════════════════════════════════════════════════════════════
//
// PG uses the base type OID (varchar=1043, numeric=1700) with typemod for the
// precision. We should resolve these correctly.

#[test]
fn varchar_with_length() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (name VARCHAR(100) NOT NULL, code CHAR(5) NOT NULL);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let name = table.columns.iter().find(|c| c.name == "name").unwrap();
    let code = table.columns.iter().find(|c| c.name == "code").unwrap();

    assert_ne!(
        name.type_oid, 0,
        "VARCHAR(100) should resolve to a valid type"
    );
    assert_ne!(code.type_oid, 0, "CHAR(5) should resolve to a valid type");

    // VARCHAR should resolve to varchar (OID 1043 in pg_catalog).
    let varchar_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "varchar")
        .unwrap()
        .oid;
    assert_eq!(name.type_oid, varchar_oid);
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 30: NUMERIC(p,s) should resolve
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn numeric_with_precision() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (amount NUMERIC(10,2) NOT NULL, factor DECIMAL NOT NULL);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let amount = table.columns.iter().find(|c| c.name == "amount").unwrap();
    let factor = table.columns.iter().find(|c| c.name == "factor").unwrap();
    assert_ne!(amount.type_oid, 0, "NUMERIC(10,2) should resolve");

    let numeric_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "numeric")
        .unwrap()
        .oid;
    assert_eq!(amount.type_oid, numeric_oid);
    assert_eq!(
        factor.type_oid, numeric_oid,
        "DECIMAL should resolve to numeric"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 31: TIMESTAMPTZ and common datetime types
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn datetime_types() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (
            a TIMESTAMP NOT NULL,
            b TIMESTAMPTZ NOT NULL,
            c DATE NOT NULL,
            d TIME NOT NULL,
            e INTERVAL NOT NULL
        );",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    for col in &table.columns {
        assert_ne!(
            col.type_oid, 0,
            "column '{}' should have a valid type OID",
            col.name
        );
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 32: JSONB and JSON types
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn json_types() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (data JSONB NOT NULL, meta JSON);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let data = table.columns.iter().find(|c| c.name == "data").unwrap();
    let meta = table.columns.iter().find(|c| c.name == "meta").unwrap();

    let jsonb_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "jsonb")
        .unwrap()
        .oid;
    let json_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "json")
        .unwrap()
        .oid;

    assert_eq!(data.type_oid, jsonb_oid);
    assert_eq!(meta.type_oid, json_oid);
    assert!(!meta.not_null, "JSON without NOT NULL should be nullable");
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 33: UUID type should resolve
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn uuid_type() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (id UUID NOT NULL DEFAULT gen_random_uuid(), name TEXT NOT NULL);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let id = table.columns.iter().find(|c| c.name == "id").unwrap();
    let uuid_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "uuid")
        .unwrap()
        .oid;
    assert_eq!(id.type_oid, uuid_oid);
    assert!(
        id.has_default,
        "DEFAULT gen_random_uuid() should set has_default"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 34: User-defined array types — column of type my_enum[]
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn user_enum_array_column() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TYPE role AS ENUM ('admin', 'user', 'guest');
         CREATE TABLE t (id INT NOT NULL, roles role[] NOT NULL);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let roles = table.columns.iter().find(|c| c.name == "roles").unwrap();
    assert_ne!(roles.type_oid, 0, "role[] should resolve to a valid type");

    let type_entry = snap.get_type(roles.type_oid).unwrap();
    assert!(
        matches!(type_entry.kind, TypeKind::Array { .. }),
        "role[] should be an Array type, got {:?}",
        type_entry.kind
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 35: DROP TYPE CASCADE should drop tables using that type
// ═════════════════════════════════════════════════════════════════════════════
//
// PG: CREATE TYPE status AS ENUM ('a', 'b');
//     CREATE TABLE t (id INT, s status);
//     DROP TYPE status;
//     → ERROR: cannot drop type because table t column s depends on it
//     DROP TYPE status CASCADE;
//     → drops the column from the table (or drops the table)

#[test]
fn drop_type_fails_with_dependent_column() {
    let result = try_apply(&[
        (
            "0001.sql",
            "CREATE TYPE status AS ENUM ('a', 'b');
             CREATE TABLE t (id INT NOT NULL, s status NOT NULL);",
        ),
        ("0002.sql", "DROP TYPE status;"),
    ]);

    // PG would fail here because table t has a column of type status.
    // Our implementation currently succeeds (drops the type without checking).
    assert!(
        result.is_err(),
        "BUG: DROP TYPE should fail when a table column uses that type"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 36: Multi-statement migration with CREATE TABLE + INSERT
// ═════════════════════════════════════════════════════════════════════════════
//
// Migrations often have DML mixed with DDL. DML should be silently ignored.

#[test]
fn dml_in_migration_ignored() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (id SERIAL PRIMARY KEY, name TEXT NOT NULL);
         INSERT INTO t (name) VALUES ('seed');
         UPDATE t SET name = 'updated' WHERE id = 1;
         DELETE FROM t WHERE id = 999;",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    assert_eq!(table.columns.len(), 2);
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 37: ALTER TABLE DROP CONSTRAINT — should be silently accepted
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn alter_table_drop_constraint() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL UNIQUE);",
        ),
        (
            "0002.sql",
            "ALTER TABLE t DROP CONSTRAINT IF EXISTS t_name_key;",
        ),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    assert_eq!(table.columns.len(), 2);
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 38: CREATE TABLE with CHECK constraint should parse
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn check_constraint_parses() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (
            id INT NOT NULL,
            age INT CHECK (age >= 0 AND age <= 200),
            status TEXT NOT NULL CHECK (status IN ('active', 'inactive'))
        );",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    assert_eq!(table.columns.len(), 3);
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 39: Complex real-world migration scenario
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn complex_real_world_migration() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\";

             CREATE TYPE user_role AS ENUM ('admin', 'editor', 'viewer');

             CREATE TABLE organizations (
                 id UUID NOT NULL DEFAULT uuid_generate_v4() PRIMARY KEY,
                 name TEXT NOT NULL,
                 slug TEXT NOT NULL UNIQUE,
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now()
             );

             CREATE TABLE users (
                 id UUID NOT NULL DEFAULT uuid_generate_v4() PRIMARY KEY,
                 org_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
                 email TEXT NOT NULL,
                 name TEXT NOT NULL,
                 role user_role NOT NULL DEFAULT 'viewer',
                 active BOOLEAN NOT NULL DEFAULT true,
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                 UNIQUE (org_id, email)
             );

             CREATE INDEX idx_users_org_id ON users (org_id);
             CREATE INDEX idx_users_email ON users (email);",
        ),
        (
            "0002.sql",
            "CREATE TABLE projects (
                 id UUID NOT NULL DEFAULT uuid_generate_v4() PRIMARY KEY,
                 org_id UUID NOT NULL REFERENCES organizations(id),
                 name TEXT NOT NULL,
                 description TEXT,
                 archived BOOLEAN NOT NULL DEFAULT false,
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now()
             );

             CREATE TABLE tasks (
                 id UUID NOT NULL DEFAULT uuid_generate_v4() PRIMARY KEY,
                 project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                 assigned_to UUID REFERENCES users(id),
                 title TEXT NOT NULL,
                 body TEXT,
                 priority INT NOT NULL DEFAULT 0 CHECK (priority >= 0),
                 completed_at TIMESTAMPTZ,
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now()
             );

             CREATE VIEW active_tasks AS
                 SELECT t.id, t.title, t.priority, p.name AS project_name,
                        u.name AS assignee_name
                 FROM tasks t
                 JOIN projects p ON p.id = t.project_id
                 LEFT JOIN users u ON u.id = t.assigned_to
                 WHERE t.completed_at IS NULL AND NOT p.archived;",
        ),
        (
            "0003.sql",
            "ALTER TYPE user_role ADD VALUE 'owner' BEFORE 'admin';
             ALTER TABLE users ADD COLUMN last_login_at TIMESTAMPTZ;
             ALTER TABLE projects ADD COLUMN owner_id UUID REFERENCES users(id);",
        ),
    ]);

    // Verify organizations.
    let orgs = snap.resolve_table(None, "organizations").unwrap();
    assert_eq!(orgs.columns.len(), 4);
    let org_id = orgs.columns.iter().find(|c| c.name == "id").unwrap();
    assert!(org_id.not_null);
    assert!(org_id.has_default);

    // Verify users (should have 8 columns after ALTER).
    let users = snap.resolve_table(None, "users").unwrap();
    assert_eq!(users.columns.len(), 8);
    let role_col = users.columns.iter().find(|c| c.name == "role").unwrap();
    let role_type = snap.get_type(role_col.type_oid).unwrap();
    assert!(matches!(role_type.kind, TypeKind::Enum { .. }));

    // Verify user_role has 4 values.
    if let TypeKind::Enum { labels } = &role_type.kind {
        assert_eq!(labels, &["owner", "admin", "editor", "viewer"]);
    }

    // Verify active_tasks view exists with correct columns.
    let view = snap.resolve_table(None, "active_tasks").unwrap();
    assert_eq!(view.columns.len(), 5);
    assert_eq!(view.columns[0].name, "id");
    assert_eq!(view.columns[1].name, "title");
    assert_eq!(view.columns[3].name, "project_name");
    assert_eq!(view.columns[4].name, "assignee_name");

    // Tasks table.
    let tasks = snap.resolve_table(None, "tasks").unwrap();
    assert_eq!(tasks.columns.len(), 8);
    let priority = tasks.columns.iter().find(|c| c.name == "priority").unwrap();
    assert!(priority.not_null);
    assert!(priority.has_default);

    // Projects with added column.
    let projects = snap.resolve_table(None, "projects").unwrap();
    assert_eq!(projects.columns.len(), 7);
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 41: ALTER TYPE ADD VALUE IF NOT EXISTS on nonexistent type swallowed
// ═════════════════════════════════════════════════════════════════════════════
//
// PG: ALTER TYPE nonexistent ADD VALUE IF NOT EXISTS 'x';
//     → ERROR: type "nonexistent" does not exist
//
// IF NOT EXISTS only applies to the VALUE, not the TYPE.
// Our bug: we use `skip_if_new_val_exists` to also skip the type-not-found
// error, incorrectly returning Ok(()).

#[test]
fn bug_alter_enum_if_not_exists_on_missing_type() {
    let result = try_apply(&[(
        "0001.sql",
        "ALTER TYPE nonexistent ADD VALUE IF NOT EXISTS 'x';",
    )]);

    assert!(
        result.is_err(),
        "BUG: should error because the TYPE doesn't exist, IF NOT EXISTS only applies to the VALUE"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 42: CREATE EXTENSION without IF NOT EXISTS on already-installed extension
// ═════════════════════════════════════════════════════════════════════════════
//
// PG: CREATE EXTENSION citext;
//     CREATE EXTENSION citext;  -- no IF NOT EXISTS
//     → ERROR: extension "citext" already exists
//
// Our bug: we don't check installed_extensions when if_not_exists is false,
// so we reinstall the extension (creating duplicate types/functions).

#[test]
fn bug_create_extension_duplicate_errors() {
    let result = try_apply(&[
        ("0001.sql", "CREATE EXTENSION \"uuid-ossp\";"),
        ("0002.sql", "CREATE EXTENSION \"uuid-ossp\";"),
    ]);

    assert!(
        result.is_err(),
        "BUG: CREATE EXTENSION without IF NOT EXISTS should fail if already installed"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 43: CREATE TABLE with duplicate column names
// ═════════════════════════════════════════════════════════════════════════════
//
// PG: CREATE TABLE t (id INT, id TEXT);
//     → ERROR: column "id" specified more than once
//
// Our bug: we silently accept duplicate column names.

#[test]
fn bug_create_table_duplicate_columns() {
    let result = try_apply(&[(
        "0001.sql",
        "CREATE TABLE t (id INT NOT NULL, name TEXT, id TEXT);",
    )]);

    assert!(
        result.is_err(),
        "BUG: CREATE TABLE should fail when column name is duplicated"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 44: DROP EXTENSION not handled — objects linger after drop
// ═════════════════════════════════════════════════════════════════════════════
//
// PG: CREATE EXTENSION "uuid-ossp";
//     DROP EXTENSION "uuid-ossp";
//     SELECT uuid_generate_v4();  → ERROR: function does not exist
//
// Our bug: DropStmt with ObjectExtension is silently ignored.
// After DROP EXTENSION, the functions remain in the snapshot.

#[test]
fn bug_drop_extension_removes_objects() {
    let snap = build(&[
        ("0001.sql", "CREATE EXTENSION \"uuid-ossp\";"),
        ("0002.sql", "DROP EXTENSION \"uuid-ossp\";"),
    ]);

    let fns = snap.find_functions(None, "uuid_generate_v4");
    assert!(
        fns.is_empty(),
        "BUG: uuid_generate_v4 should not exist after DROP EXTENSION"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 45: CREATE TABLE duplicate without IF NOT EXISTS — silently overwrites
// ═════════════════════════════════════════════════════════════════════════════
//
// PG: CREATE TABLE t (id INT);
//     CREATE TABLE t (name TEXT);
//     → ERROR: relation "t" already exists

#[test]
fn bug_create_table_duplicate_errors() {
    let result = try_apply(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL);"),
        ("0002.sql", "CREATE TABLE t (name TEXT NOT NULL);"),
    ]);

    assert!(
        result.is_err(),
        "BUG: CREATE TABLE should fail when table already exists (no IF NOT EXISTS)"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 46: CREATE TYPE (enum/domain) duplicate without error
// ═════════════════════════════════════════════════════════════════════════════
//
// PG: CREATE TYPE mood AS ENUM ('happy');
//     CREATE TYPE mood AS ENUM ('sad');
//     → ERROR: type "mood" already exists

#[test]
fn bug_create_type_duplicate_errors() {
    let result = try_apply(&[
        ("0001.sql", "CREATE TYPE mood AS ENUM ('happy', 'sad');"),
        ("0002.sql", "CREATE TYPE mood AS ENUM ('angry');"),
    ]);

    assert!(
        result.is_err(),
        "BUG: CREATE TYPE should fail when type already exists"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 47: ALTER TABLE ADD COLUMN duplicate without IF NOT EXISTS
// ═════════════════════════════════════════════════════════════════════════════
//
// PG: CREATE TABLE t (id INT, name TEXT);
//     ALTER TABLE t ADD COLUMN name TEXT;
//     → ERROR: column "name" of relation "t" already exists

#[test]
fn bug_add_column_duplicate_errors() {
    let result = try_apply(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);",
        ),
        ("0002.sql", "ALTER TABLE t ADD COLUMN name TEXT;"),
    ]);

    assert!(
        result.is_err(),
        "BUG: ADD COLUMN should fail when column already exists (no IF NOT EXISTS)"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 48: ALTER TABLE SET NOT NULL on nonexistent column — silently ignored
// ═════════════════════════════════════════════════════════════════════════════
//
// PG: ALTER TABLE t ALTER COLUMN nonexistent SET NOT NULL;
//     → ERROR: column "nonexistent" of relation "t" does not exist

#[test]
fn bug_alter_nonexistent_column_errors() {
    let result = try_apply(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL);"),
        (
            "0002.sql",
            "ALTER TABLE t ALTER COLUMN nonexistent SET NOT NULL;",
        ),
    ]);

    assert!(
        result.is_err(),
        "BUG: ALTER COLUMN on nonexistent column should fail"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 49: CREATE FUNCTION duplicate signature without OR REPLACE
// ═════════════════════════════════════════════════════════════════════════════
//
// PG: CREATE FUNCTION foo(INT) RETURNS INT AS $$ SELECT $1 $$ LANGUAGE sql;
//     CREATE FUNCTION foo(INT) RETURNS INT AS $$ SELECT $1 $$ LANGUAGE sql;
//     → ERROR: function "foo" already exists with same argument types

#[test]
fn bug_create_function_duplicate_errors() {
    let result = try_apply(&[(
        "0001.sql",
        "CREATE FUNCTION foo(x INT) RETURNS INT AS $$ SELECT x $$ LANGUAGE sql;
         CREATE FUNCTION foo(x INT) RETURNS INT AS $$ SELECT x + 1 $$ LANGUAGE sql;",
    )]);

    assert!(
        result.is_err(),
        "BUG: CREATE FUNCTION should fail when same signature exists (no OR REPLACE)"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 50: ALTER TABLE SET DEFAULT on nonexistent column — silently ignored
// ═════════════════════════════════════════════════════════════════════════════
//
// PG: ALTER TABLE t ALTER COLUMN ghost SET DEFAULT 42;
//     → ERROR: column "ghost" of relation "t" does not exist

#[test]
fn bug_set_default_nonexistent_column() {
    let result = try_apply(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL);"),
        (
            "0002.sql",
            "ALTER TABLE t ALTER COLUMN ghost SET DEFAULT 42;",
        ),
    ]);

    assert!(
        result.is_err(),
        "BUG: SET DEFAULT on nonexistent column should fail"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 51: ALTER TABLE ALTER COLUMN TYPE on nonexistent column — silently ignored
// ═════════════════════════════════════════════════════════════════════════════
//
// PG: ALTER TABLE t ALTER COLUMN ghost TYPE BIGINT;
//     → ERROR: column "ghost" of relation "t" does not exist

#[test]
fn bug_alter_type_nonexistent_column() {
    let result = try_apply(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL);"),
        ("0002.sql", "ALTER TABLE t ALTER COLUMN ghost TYPE BIGINT;"),
    ]);

    assert!(
        result.is_err(),
        "BUG: ALTER TYPE on nonexistent column should fail"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 52: DROP COLUMN on nonexistent column without IF EXISTS — silently ignored
// ═════════════════════════════════════════════════════════════════════════════
//
// PG: ALTER TABLE t DROP COLUMN ghost;
//     → ERROR: column "ghost" of relation "t" does not exist
//     (vs DROP COLUMN IF EXISTS ghost → OK)

#[test]
fn bug_drop_nonexistent_column_errors() {
    let result = try_apply(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL, name TEXT);"),
        ("0002.sql", "ALTER TABLE t DROP COLUMN ghost;"),
    ]);

    assert!(
        result.is_err(),
        "BUG: DROP COLUMN without IF EXISTS on nonexistent column should fail"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 53: CREATE COMPOSITE TYPE duplicate without error
// ═════════════════════════════════════════════════════════════════════════════
//
// PG: CREATE TYPE point2d AS (x float8, y float8);
//     CREATE TYPE point2d AS (x float8, y float8);
//     → ERROR: type "point2d" already exists

#[test]
fn bug_create_composite_duplicate_errors() {
    let result = try_apply(&[(
        "0001.sql",
        "CREATE TYPE point2d AS (x float8, y float8);
         CREATE TYPE point2d AS (a int, b int);",
    )]);

    assert!(
        result.is_err(),
        "BUG: CREATE TYPE (composite) should fail when type already exists"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// BUG 54: CREATE RANGE TYPE duplicate without error
// ═════════════════════════════════════════════════════════════════════════════
//
// PG: CREATE TYPE floatrange AS RANGE (subtype = float8);
//     CREATE TYPE floatrange AS RANGE (subtype = float8);
//     → ERROR: type "floatrange" already exists

#[test]
fn bug_create_range_duplicate_errors() {
    let result = try_apply(&[(
        "0001.sql",
        "CREATE TYPE floatrange AS RANGE (subtype = float8);
         CREATE TYPE floatrange AS RANGE (subtype = float8);",
    )]);

    assert!(
        result.is_err(),
        "BUG: CREATE TYPE (range) should fail when type already exists"
    );
}
