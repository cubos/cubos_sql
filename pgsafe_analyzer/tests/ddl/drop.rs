//! DROP TABLE / COLUMN / TYPE / SCHEMA / EXTENSION / FUNCTION / OPERATOR,
//! with and without CASCADE. Transitive dependency cascades, dependency
//! errors without CASCADE, IF EXISTS semantics.

use crate::common::*;

// ── DROP TABLE ──────────────────────────────────────────────────────────────

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

// ── DROP TYPE ───────────────────────────────────────────────────────────────

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

// ── DROP FUNCTION / AGGREGATE with CASCADE ─────────────────────────────────

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

// ── DROP TABLE ... CASCADE through transitive views ────────────────────────

#[test]
fn drop_table_cascade_transitive_views() {
    // Table t → view v1 → view v2. CASCADE must chase the whole chain.
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

// ── DROP TYPE with dependent table column ──────────────────────────────────

#[test]
fn drop_type_with_dependent_column_errors() {
    let result = try_apply(&[
        (
            "0001.sql",
            "CREATE TYPE status AS ENUM ('a', 'b');
             CREATE TABLE t (id INT NOT NULL, s status NOT NULL);",
        ),
        ("0002.sql", "DROP TYPE status;"),
    ]);

    assert_ddl_err!(result, DdlError::DependencyError(_), "depend");
}

// ── DROP FUNCTION: overload-safe ──────────────────────────────────────────

#[test]
fn drop_function_removes_only_matching_overload() {
    // DROP FUNCTION foo(INT) must leave foo(TEXT) intact. Historically we
    // removed the whole name bucket — this guards against that regression.
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
        "only the INT overload should be dropped, foo(TEXT) must survive",
    );
    let text_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "text")
        .unwrap()
        .oid;
    assert_eq!(
        fns[0].prorettype, text_oid,
        "remaining overload should be foo(TEXT)",
    );
}

// ── ALTER TABLE DROP COLUMN ───────────────────────────────────────────────

#[test]
fn drop_column_nonexistent_without_if_exists_errors() {
    let result = try_apply(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL, name TEXT);"),
        ("0002.sql", "ALTER TABLE t DROP COLUMN ghost;"),
    ]);

    assert_ddl_err!(result, DdlError::Parse(_), "does not exist");
}

#[test]
fn drop_column_if_exists_on_nonexistent_is_noop() {
    let snap = build(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL, name TEXT);"),
        (
            "0002.sql",
            "ALTER TABLE t DROP COLUMN IF EXISTS nonexistent;",
        ),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    assert_eq!(snap.attributes_of(table.oid).len(), 2, "columns unchanged");
}

// ── DROP EXTENSION removes its provided objects ────────────────────────────

#[test]
fn drop_extension_removes_its_functions() {
    // After DROP EXTENSION, the functions registered by that extension must
    // be gone from the snapshot so downstream queries that reference them
    // surface as unresolved.
    let snap = build(&[
        ("0001.sql", "CREATE EXTENSION \"uuid-ossp\";"),
        ("0002.sql", "DROP EXTENSION \"uuid-ossp\";"),
    ]);

    let fns = snap.find_functions(None, "uuid_generate_v4");
    assert!(
        fns.is_empty(),
        "uuid_generate_v4 must not exist after DROP EXTENSION",
    );
}
