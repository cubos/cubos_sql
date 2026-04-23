//! ALTER TABLE: ADD/DROP/SET/ALTER column, RENAME COLUMN, DROP CONSTRAINT,
//! ADD PRIMARY KEY, ALTER COLUMN TYPE, SET DEFAULT / DROP DEFAULT.

use crate::common::*;

// ── ADD / DROP / SET / ALTER COLUMN ─────────────────────────────────────────

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

// ── ALTER COLUMN TYPE with dependent views ─────────────────────────────────

#[test]
fn alter_column_type_binary_coercible_with_view_succeeds() {
    // Domain user_id over int4 → changing the column to plain int4 is
    // binary coercible (PG's IsBinaryCoercible rule), so the dependent view
    // must survive the ALTER.
    let snap = build(&[
        (
            "0001.sql",
            "CREATE DOMAIN user_id AS INT;
             CREATE TABLE t (id user_id NOT NULL);
             CREATE VIEW v AS SELECT id FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t ALTER COLUMN id TYPE INT;"),
    ]);

    let view = snap.resolve_table(None, "v").unwrap();
    assert_eq!(view.columns.len(), 1, "view must survive the ALTER");
    let int4 = snap
        .resolve_type_by_name(Some("pg_catalog"), "int4")
        .unwrap()
        .oid;
    assert_eq!(
        view.columns[0].type_oid, int4,
        "view column OID should be updated to the new base type",
    );
}

#[test]
fn alter_column_type_non_binary_coercible_with_view_fails_with_hint() {
    // int → bigint is a Function cast, not Binary — must fail with a hint
    // that mentions binary coercibility so the user knows why PG rejects it.
    let result = try_apply(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, amount INT);
             CREATE VIEW v AS SELECT amount FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t ALTER COLUMN amount TYPE BIGINT;"),
    ]);

    assert_ddl_err!(result, DdlError::DependencyError(_), "binary coercible");
    // The message should also hint at drop-and-recreate.
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("drop"),
        "error should hint at drop-and-recreate: {err}",
    );
}

// ── ALTER COLUMN TYPE triggers view AST reanalyze ──────────────────────────

#[test]
fn alter_column_type_reanalyzes_view_column_oid() {
    // View exposes the column with its domain OID. The ALTER collapses the
    // domain to int4 (binary coercible), and reanalyze must refresh the
    // view's column OID to the new base type.
    let snap = build(&[
        (
            "0001.sql",
            "CREATE DOMAIN user_id AS INT;
             CREATE TABLE t (id user_id NOT NULL);
             CREATE VIEW v AS SELECT id FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t ALTER COLUMN id TYPE INT;"),
    ]);

    let int4 = snap
        .resolve_type_by_name(Some("pg_catalog"), "int4")
        .unwrap()
        .oid;
    let view = snap.resolve_table(None, "v").unwrap();
    assert_eq!(view.columns[0].type_oid, int4);
}

// ── ADD COLUMN and IF NOT EXISTS ──────────────────────────────────────────

#[test]
fn alter_table_add_column_duplicate_errors() {
    let result = try_apply(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);",
        ),
        ("0002.sql", "ALTER TABLE t ADD COLUMN name TEXT;"),
    ]);

    assert_ddl_err!(result, DdlError::DuplicateObject(_), "already exists");
}

#[test]
fn alter_table_add_column_if_not_exists_on_existing_is_noop() {
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

// ── ADD CONSTRAINT PRIMARY KEY ────────────────────────────────────────────

#[test]
fn alter_table_add_primary_key_sets_not_null() {
    // PG: a PRIMARY KEY constraint added via ALTER TABLE propagates NOT NULL
    // to the indexed columns.
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
        "PRIMARY KEY constraint must make the column NOT NULL"
    );
}

// ── DROP CONSTRAINT ───────────────────────────────────────────────────────

#[test]
fn alter_table_drop_constraint_if_exists_is_noop() {
    // DROP CONSTRAINT IF EXISTS on a real constraint (name ignored by our
    // interpreter, which does not track constraint names) must not error.
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

// ── Multiple ALTER commands in one statement ──────────────────────────────

#[test]
fn alter_table_multiple_commands_in_one_statement() {
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

// ── Errors on nonexistent column ──────────────────────────────────────────

#[test]
fn alter_column_set_not_null_on_nonexistent_column_errors() {
    let result = try_apply(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL);"),
        (
            "0002.sql",
            "ALTER TABLE t ALTER COLUMN nonexistent SET NOT NULL;",
        ),
    ]);

    assert_ddl_err!(result, DdlError::Parse(_), "does not exist");
}

#[test]
fn alter_column_set_default_on_nonexistent_column_errors() {
    let result = try_apply(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL);"),
        (
            "0002.sql",
            "ALTER TABLE t ALTER COLUMN ghost SET DEFAULT 42;",
        ),
    ]);

    assert_ddl_err!(result, DdlError::Parse(_), "does not exist");
}

#[test]
fn alter_column_type_on_nonexistent_column_errors() {
    let result = try_apply(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL);"),
        ("0002.sql", "ALTER TABLE t ALTER COLUMN ghost TYPE BIGINT;"),
    ]);

    assert_ddl_err!(result, DdlError::Parse(_), "does not exist");
}

#[test]
fn alter_column_type_reanalyze_is_noop_for_legacy_view() {
    // Simulate a legacy snapshot where resolved_ast was never populated:
    // the ALTER path must still succeed (best-effort), not blow up.
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE DOMAIN user_id AS INT;
         CREATE TABLE t (id user_id NOT NULL);
         CREATE VIEW v AS SELECT id FROM t;",
    )]);

    let key = QualifiedName::new("public", "v");
    db.snapshot_mut()
        .tables
        .get_mut(&key)
        .unwrap()
        .view_def
        .as_mut()
        .unwrap()
        .resolved_ast
        .clear();

    db.apply_sql("ALTER TABLE t ALTER COLUMN id TYPE INT;")
        .unwrap();
}
