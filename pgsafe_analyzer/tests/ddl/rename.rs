//! RENAME TABLE / COLUMN / SCHEMA and their downstream effects: dependent
//! views get their AST rewritten, function references get re-bound.

use crate::common::*;

// ── RENAME TABLE and CASCADE detection ──────────────────────────────────────

#[test]
fn rename_table_preserves_cascade_detection() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL);
             CREATE VIEW v AS SELECT id FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t RENAME TO t2;"),
        ("0003.sql", "DROP TABLE t2 CASCADE;"),
    ]);

    assert!(snap.resolve_table(None, "t2").is_none());
    assert!(
        snap.resolve_table(None, "v").is_none(),
        "view should have been dropped via CASCADE through the renamed dep",
    );
}

#[test]
fn rename_table_without_cascade_still_blocks_drop() {
    let result = try_apply(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL);
             CREATE VIEW v AS SELECT id FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t RENAME TO t2;"),
        ("0003.sql", "DROP TABLE t2;"),
    ]);

    assert_ddl_err!(
        result,
        DdlError::DependencyError(_),
        "cannot drop table t2 because other objects depend on it (view(s) public.v depend on this)"
    );
}

// ── AST rewriting on RENAME propagates into dependent view definitions ─────

#[test]
fn rename_table_rewrites_view_ast() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL);
             CREATE VIEW v AS SELECT id FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t RENAME TO nodes;"),
    ]);

    let view = snap.resolve_table(None, "v").unwrap();
    assert!(snap.view_body(view.oid).is_some());

    let table_deps = view_table_deps(&snap, view.oid);
    assert!(
        !table_deps
            .iter()
            .any(|k| k.name == "t" && k.schema == "public")
    );
    assert!(
        table_deps
            .iter()
            .any(|k| k.name == "nodes" && k.schema == "public")
    );
}

#[test]
fn rename_column_rewrites_deps_with_self_join() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, parent_id INT);
             CREATE VIEW v AS
                 SELECT a.id, b.id AS parent_id
                 FROM t a JOIN t b ON b.id = a.parent_id;",
        ),
        ("0002.sql", "ALTER TABLE t RENAME COLUMN id TO node_id;"),
    ]);

    let view = snap.resolve_table(None, "v").unwrap();
    let col_deps = view_column_deps(&snap, view.oid);
    let t = QualifiedName::new("public", "t");
    assert!(col_deps.iter().any(|(k, c)| k == &t && c == "node_id"));
    assert!(col_deps.iter().all(|(_, c)| c != "id"));
}

#[test]
fn rename_schema_rewrites_view_ast() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE SCHEMA app;
             CREATE TABLE app.t (id INT NOT NULL);
             CREATE VIEW app.v AS SELECT id FROM app.t;",
        ),
        ("0002.sql", "ALTER SCHEMA app RENAME TO core;"),
    ]);

    let view = snap.resolve_table(Some("core"), "v").unwrap();
    let table_deps = view_table_deps(&snap, view.oid);
    let new = QualifiedName::new("core", "t");
    assert!(table_deps.contains(&new));
    assert!(!table_deps.iter().any(|k| k.schema == "app"));
}
