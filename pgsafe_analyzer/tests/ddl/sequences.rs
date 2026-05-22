//! CREATE / ALTER / DROP SEQUENCE (standalone, not via SERIAL) and the
//! sequence-manipulation functions (`nextval` / `currval` / `lastval` /
//! `setval`). Sequences are registered as `pg_class` rows with
//! `relkind = Sequence` — see `pgsafe_analyzer/src/ddl/sequences.rs`.

use crate::common::*;

// ── CREATE SEQUENCE ────────────────────────────────────────────────────────

#[test]
fn create_sequence_basic_registers_pg_class() {
    let snap = build(&[("0001.sql", "CREATE SEQUENCE my_seq;")]);

    let seq = snap.resolve_table(None, "my_seq").unwrap();
    assert_eq!(seq.relname, "my_seq");
    assert_eq!(seq.relkind, RelKind::Sequence);
    assert_eq!(snap.namespace_name(seq.relnamespace), Some("public"));
}

#[test]
fn create_sequence_with_options_is_accepted() {
    let snap = build(&[(
        "0001.sql",
        "CREATE SEQUENCE s START WITH 100 INCREMENT BY 5 \
         MINVALUE 0 MAXVALUE 1000 CACHE 10 CYCLE;",
    )]);

    let seq = snap.resolve_table(None, "s").unwrap();
    assert_eq!(seq.relkind, RelKind::Sequence);
}

#[test]
fn create_sequence_in_explicit_schema() {
    let snap = build(&[("0001.sql", "CREATE SCHEMA app; CREATE SEQUENCE app.s;")]);

    let seq = snap.resolve_table(Some("app"), "s").unwrap();
    assert_eq!(seq.relkind, RelKind::Sequence);
    assert_eq!(snap.namespace_name(seq.relnamespace), Some("app"));
    assert!(snap.resolve_table(Some("public"), "s").is_none());
}

#[test]
fn create_sequence_if_not_exists_is_silent_on_duplicate() {
    let snap = build(&[
        ("0001.sql", "CREATE SEQUENCE s;"),
        ("0002.sql", "CREATE SEQUENCE IF NOT EXISTS s;"),
    ]);

    let seq = snap.resolve_table(None, "s").unwrap();
    assert_eq!(seq.relkind, RelKind::Sequence);
    let public_oid = snap.namespace_oid("public").unwrap();
    let count = snap
        .pg_class()
        .values()
        .filter(|c| {
            c.relnamespace == public_oid && c.relname == "s" && c.relkind == RelKind::Sequence
        })
        .count();
    assert_eq!(count, 1, "IF NOT EXISTS must not register a duplicate row");
}

#[test]
fn create_sequence_duplicate_without_if_not_exists_errors() {
    let result = try_apply(&[
        ("0001.sql", "CREATE SEQUENCE seq;"),
        ("0002.sql", "CREATE SEQUENCE seq;"),
    ]);

    assert_ddl_err!(result, DdlError::DuplicateObject(_), "already exists");
}

// ── DROP SEQUENCE ──────────────────────────────────────────────────────────

#[test]
fn drop_sequence_existing_removes_it() {
    let snap = build(&[
        ("0001.sql", "CREATE SEQUENCE s;"),
        ("0002.sql", "DROP SEQUENCE s;"),
    ]);

    assert!(snap.resolve_table(None, "s").is_none());
}

#[test]
fn drop_sequence_missing_errors() {
    let result = try_apply(&[("0001.sql", "DROP SEQUENCE missing;")]);

    assert_ddl_err!(result, DdlError::TableNotFound(_), "does not exist");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("missing"),
        "error must reference the missing sequence name; got: {err}"
    );
    assert!(
        err.contains("sequence"),
        "error must say sequence (PG: \"sequence \\\"missing\\\" does not exist\"); got: {err}"
    );
}

#[test]
fn drop_sequence_if_exists_missing_is_silent() {
    let snap = build(&[("0001.sql", "DROP SEQUENCE IF EXISTS missing;")]);
    assert!(snap.resolve_table(None, "missing").is_none());
}

#[test]
fn drop_sequence_against_table_errors() {
    let result = try_apply(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL);"),
        ("0002.sql", "DROP SEQUENCE t;"),
    ]);

    assert_ddl_err!(result, DdlError::TableNotFound(_), "is not a sequence");
}

// ── ALTER SEQUENCE ─────────────────────────────────────────────────────────

#[test]
fn alter_sequence_with_options_on_existing() {
    let snap = build(&[(
        "0001.sql",
        "CREATE SEQUENCE s; ALTER SEQUENCE s RESTART WITH 100 INCREMENT BY 2;",
    )]);

    let seq = snap.resolve_table(None, "s").unwrap();
    assert_eq!(seq.relkind, RelKind::Sequence);
}

#[test]
fn alter_sequence_missing_errors() {
    let result = try_apply(&[("0001.sql", "ALTER SEQUENCE missing RESTART WITH 1;")]);

    assert_ddl_err!(result, DdlError::TableNotFound(_), "does not exist");
}

#[test]
fn alter_sequence_if_exists_missing_is_silent() {
    let _snap = build(&[(
        "0001.sql",
        "ALTER SEQUENCE IF EXISTS missing RESTART WITH 1;",
    )]);
}

#[test]
fn alter_sequence_rename_to() {
    let snap = build(&[(
        "0001.sql",
        "CREATE SEQUENCE s; ALTER SEQUENCE s RENAME TO s2;",
    )]);

    assert!(snap.resolve_table(None, "s").is_none());
    let renamed = snap.resolve_table(None, "s2").unwrap();
    assert_eq!(renamed.relkind, RelKind::Sequence);
}

#[test]
fn alter_sequence_set_schema() {
    let snap = build(&[(
        "0001.sql",
        "CREATE SCHEMA app;
         CREATE SEQUENCE s;
         ALTER SEQUENCE s SET SCHEMA app;",
    )]);

    assert!(snap.resolve_table(Some("public"), "s").is_none());
    let moved = snap.resolve_table(Some("app"), "s").unwrap();
    assert_eq!(moved.relkind, RelKind::Sequence);
    assert_eq!(snap.namespace_name(moved.relnamespace), Some("app"));
}

// ── Sequence functions: nextval / currval / lastval / setval ───────────────

#[test]
fn nextval_returns_int8_not_null() {
    let db = build_db(&[("0001.sql", "CREATE SEQUENCE s;")]);

    let info = db.analyze("SELECT nextval('s')").unwrap();
    assert_cols(&info, vec![c("nextval", int8())]);
}

#[test]
fn currval_returns_int8_not_null() {
    let db = build_db(&[("0001.sql", "CREATE SEQUENCE s;")]);

    let info = db.analyze("SELECT currval('s')").unwrap();
    assert_cols(&info, vec![c("currval", int8())]);
}

#[test]
fn lastval_returns_int8_not_null() {
    let db = build_db(&[("0001.sql", "CREATE SEQUENCE s;")]);

    let info = db.analyze("SELECT lastval()").unwrap();
    assert_cols(&info, vec![c("lastval", int8())]);
}

#[test]
fn setval_two_args_returns_int8_not_null() {
    let db = build_db(&[("0001.sql", "CREATE SEQUENCE s;")]);

    let info = db.analyze("SELECT setval('s', 1)").unwrap();
    assert_cols(&info, vec![c("setval", int8())]);
}

#[test]
fn setval_three_args_returns_int8_not_null() {
    let db = build_db(&[("0001.sql", "CREATE SEQUENCE s;")]);

    let info = db.analyze("SELECT setval('s', 1, true)").unwrap();
    assert_cols(&info, vec![c("setval", int8())]);
}
