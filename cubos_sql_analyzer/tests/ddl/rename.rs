//! RENAME TABLE / COLUMN / SCHEMA and their downstream effects: dependent
//! views get their AST rewritten, function references get re-bound.

use crate::common::*;

// ── RENAME TABLE and CASCADE detection ──────────────────────────────────────

#[test]
fn rename_table_preserves_cascade_detection() {
    // End-to-end: rename underlying table, then DROP it — CASCADE must still
    // find the dependent view through the rewritten deps.
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

    assert_ddl_err!(result, DdlError::DependencyError(_), "depend");
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

    let vd = snap
        .resolve_table(None, "v")
        .unwrap()
        .view_def
        .as_ref()
        .unwrap();
    assert!(!vd.resolved_ast.is_empty());
    assert!(
        !vd.depends_on_tables
            .iter()
            .any(|k| k.name == "t" && k.schema == "public"),
    );
    assert!(
        vd.depends_on_tables
            .iter()
            .any(|k| k.name == "nodes" && k.schema == "public"),
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

    let vd = snap
        .resolve_table(None, "v")
        .unwrap()
        .view_def
        .as_ref()
        .unwrap();
    let t = QualifiedName::new("public", "t");
    assert!(
        vd.depends_on_columns
            .iter()
            .any(|(k, c)| k == &t && c == "node_id"),
    );
    assert!(vd.depends_on_columns.iter().all(|(_, c)| c != "id"));
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

    let vd = snap
        .resolve_table(Some("core"), "v")
        .unwrap()
        .view_def
        .as_ref()
        .unwrap();
    let old = QualifiedName::new("app", "t");
    let new = QualifiedName::new("core", "t");
    assert!(!vd.depends_on_tables.contains(&old));
    assert!(vd.depends_on_tables.contains(&new));
}

#[test]
fn rename_then_alter_type_reanalyzes_through_renamed_ast() {
    // End-to-end: rename the table, then ALTER a column's type binary-
    // coercibly. Without AST rewriting on rename, reanalyze would choke on
    // "unknown relation t" after step 2 and the ALTER would fail.
    let snap = build(&[
        (
            "0001.sql",
            "CREATE DOMAIN user_id AS INT;
             CREATE TABLE t (id user_id NOT NULL);
             CREATE VIEW v AS SELECT id FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t RENAME TO accounts;"),
        ("0003.sql", "ALTER TABLE accounts ALTER COLUMN id TYPE INT;"),
    ]);

    let int4 = snap
        .resolve_type_by_name(Some("pg_catalog"), "int4")
        .unwrap()
        .oid;
    let view = snap.resolve_table(None, "v").unwrap();
    assert_eq!(
        view.columns[0].type_oid, int4,
        "reanalyze must work after rename",
    );
}
