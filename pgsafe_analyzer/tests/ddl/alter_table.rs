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
    let attrs = snap.attributes_of(table.oid);
    assert_eq!(attrs.len(), 2);
    assert_eq!(attrs[1].attname, "name");
    assert!(!attrs[1].attnotnull);
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
    let attrs = snap.attributes_of(table.oid);
    assert_eq!(attrs.len(), 2);
    assert_eq!(attrs[0].attname, "id");
    assert_eq!(attrs[1].attname, "age");
}

#[test]
fn alter_table_set_not_null() {
    let snap = build(&[
        ("0001.sql", "CREATE TABLE t (id INT, name TEXT);"),
        ("0002.sql", "ALTER TABLE t ALTER COLUMN name SET NOT NULL;"),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let name_col = attrs.iter().find(|c| c.attname == "name").unwrap();
    assert!(name_col.attnotnull);
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
    let attrs = snap.attributes_of(table.oid);
    let name_col = attrs.iter().find(|c| c.attname == "name").unwrap();
    assert!(!name_col.attnotnull);
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
    let attrs = snap.attributes_of(table.oid);
    let status_col = attrs.iter().find(|c| c.attname == "status").unwrap();
    assert!(status_col.atthasdef);
}

#[test]
fn alter_table_alter_column_type() {
    let snap = build(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL, amount INT);"),
        ("0002.sql", "ALTER TABLE t ALTER COLUMN amount TYPE BIGINT;"),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let amount_col = attrs.iter().find(|c| c.attname == "amount").unwrap();
    let int8_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int8")
        .unwrap()
        .oid;
    assert_eq!(amount_col.atttypid, int8_oid);
}

// ── ALTER COLUMN TYPE with dependent views ─────────────────────────────────
//
// PG (SQLSTATE 0A000) blocks `ALTER COLUMN TYPE` on any column referenced by a
// view, even when the change would be binary-coercible. We mirror that — the
// only safe migration is DROP VIEW → ALTER → CREATE VIEW.

#[test]
fn alter_column_type_with_view_fails_even_when_binary_coercible() {
    let result = try_apply(&[
        (
            "0001.sql",
            "CREATE DOMAIN user_id AS INT;
             CREATE TABLE t (id user_id NOT NULL);
             CREATE VIEW v AS SELECT id FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t ALTER COLUMN id TYPE INT;"),
    ]);

    assert_ddl_err!(
        result,
        DdlError::DependencyError(_),
        "cannot alter type of a column used by a view or rule",
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("drop"),
        "error should hint at drop-and-recreate: {err}",
    );
}

#[test]
fn alter_column_type_with_view_fails_when_not_binary_coercible() {
    let result = try_apply(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, amount INT);
             CREATE VIEW v AS SELECT amount FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t ALTER COLUMN amount TYPE BIGINT;"),
    ]);

    assert_ddl_err!(
        result,
        DdlError::DependencyError(_),
        "cannot alter type of a column used by a view or rule",
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("drop"),
        "error should hint at drop-and-recreate: {err}",
    );
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
    assert_eq!(
        snap.attributes_of(table.oid).len(),
        2,
        "should still have 2 columns"
    );
}

// ── ADD CONSTRAINT PRIMARY KEY ────────────────────────────────────────────

#[test]
fn alter_table_add_primary_key_sets_not_null() {
    let snap = build(&[
        ("0001.sql", "CREATE TABLE t (id INT, name TEXT);"),
        (
            "0002.sql",
            "ALTER TABLE t ADD CONSTRAINT t_pkey PRIMARY KEY (id);",
        ),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let id_col = attrs.iter().find(|c| c.attname == "id").unwrap();
    assert!(
        id_col.attnotnull,
        "PRIMARY KEY constraint must make the column NOT NULL"
    );
}

// ── DROP CONSTRAINT ───────────────────────────────────────────────────────

#[test]
fn alter_table_drop_constraint_if_exists_is_noop() {
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
    assert_eq!(snap.attributes_of(table.oid).len(), 2);
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
    let attrs = snap.attributes_of(table.oid);
    let name = attrs.iter().find(|c| c.attname == "name").unwrap();
    let age = attrs.iter().find(|c| c.attname == "age").unwrap();
    assert!(name.attnotnull);
    assert!(age.atthasdef);
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
