//! CREATE VIEW: column resolution at creation time, `*` expansion,
//! dependency tracking (columns in SELECT, WHERE, JOIN ON, ORDER BY),
//! CASCADE semantics, view invalidation on column type change,
//! view AST rewrite on table/column rename.

use crate::common::*;

// ── PostgreSQL-compatible column resolution at CREATE time ─────────────────

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

// ── DROP COLUMN with view dependency ───────────────────────────────────────

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

    assert_ddl_err!(result, DdlError::DependencyError(_), "depend");
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

// ── ALTER COLUMN TYPE with view dependency ─────────────────────────────────

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

    assert_ddl_err!(result, DdlError::DependencyError(_), "binary coercible");
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

// ── DROP TABLE with view dependency ────────────────────────────────────────

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

    assert_ddl_err!(result, DdlError::DependencyError(_), "depend");
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

// ── Structured dependency tracking ─────────────────────────────────────────

#[test]
fn view_with_invalid_column_fails_migration() {
    let result = try_apply(&[(
        "0001.sql",
        "CREATE TABLE users (id INT NOT NULL);
         CREATE VIEW bad AS SELECT nao_existe FROM users;",
    )]);

    assert_ddl_err!(result, DdlError::ViewAnalysis { .. }, "bad");
}

#[test]
fn view_deps_only_track_referenced_columns() {
    // Both users and orders have a column named `id`; the view only uses
    // users.id. The structured walker must not list (orders, id) as a dep.
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL);
         CREATE TABLE orders (id INT NOT NULL, user_id INT NOT NULL);
         CREATE VIEW v AS
             SELECT u.id FROM users u JOIN orders o ON o.user_id = u.id;",
    )]);

    let view = snap.resolve_table(None, "v").unwrap();
    let vd = view.view_def.as_ref().expect("view_def must be present");

    let users = QualifiedName::new("public", "users");
    let orders = QualifiedName::new("public", "orders");

    assert!(vd.depends_on_tables.contains(&users));
    assert!(vd.depends_on_tables.contains(&orders));

    // The SELECT list references users.id only; join predicate touches
    // orders.user_id and users.id. orders.id must NOT be in the dep list.
    assert!(
        vd.depends_on_columns
            .iter()
            .any(|(k, c)| k == &users && c == "id"),
        "users.id must be tracked: {:?}",
        vd.depends_on_columns
    );
    assert!(
        vd.depends_on_columns
            .iter()
            .any(|(k, c)| k == &orders && c == "user_id"),
        "orders.user_id must be tracked: {:?}",
        vd.depends_on_columns
    );
    assert!(
        !vd.depends_on_columns
            .iter()
            .any(|(k, c)| k == &orders && c == "id"),
        "orders.id must NOT be tracked — it was never referenced: {:?}",
        vd.depends_on_columns
    );
}

#[test]
fn view_deps_track_schema_qualified_columns() {
    let snap = build(&[(
        "0001.sql",
        "CREATE SCHEMA app;
         CREATE TABLE app.users (id INT NOT NULL, name TEXT NOT NULL);
         CREATE VIEW v AS SELECT app.users.id FROM app.users;",
    )]);

    let view = snap.resolve_table(None, "v").unwrap();
    let vd = view.view_def.as_ref().unwrap();
    let users = QualifiedName::new("app", "users");

    assert!(vd.depends_on_tables.contains(&users));
    assert!(
        vd.depends_on_columns
            .iter()
            .any(|(k, c)| k == &users && c == "id"),
        "schema-qualified column ref must resolve to (app.users, id): {:?}",
        vd.depends_on_columns
    );
}

#[test]
fn view_deps_dedup_on_self_join() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (id INT NOT NULL, parent_id INT);
         CREATE VIEW v AS
             SELECT a.id, b.id AS parent_id
             FROM t a JOIN t b ON b.id = a.parent_id;",
    )]);

    let view = snap.resolve_table(None, "v").unwrap();
    let vd = view.view_def.as_ref().unwrap();
    let t = QualifiedName::new("public", "t");

    // After dedup, the self-joined table appears only once in the table list.
    let t_count = vd.depends_on_tables.iter().filter(|k| *k == &t).count();
    assert_eq!(
        t_count, 1,
        "self-joined table must be dedup'd: {:?}",
        vd.depends_on_tables
    );

    // Column deps for id and parent_id are present exactly once.
    let id_count = vd
        .depends_on_columns
        .iter()
        .filter(|(k, c)| k == &t && c == "id")
        .count();
    let parent_count = vd
        .depends_on_columns
        .iter()
        .filter(|(k, c)| k == &t && c == "parent_id")
        .count();
    assert_eq!(id_count, 1);
    assert_eq!(parent_count, 1);
}

#[test]
fn view_with_cte_does_not_treat_cte_as_table_dep() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL);
         CREATE VIEW v AS
             WITH u AS (SELECT id, name FROM users)
             SELECT id, name FROM u;",
    )]);

    let view = snap.resolve_table(None, "v").unwrap();
    let vd = view.view_def.as_ref().unwrap();

    // The underlying users table is the only real dep.
    let users = QualifiedName::new("public", "users");
    assert!(vd.depends_on_tables.contains(&users));
    assert_eq!(vd.depends_on_tables.len(), 1);

    // And users.id / users.name are the columns — the CTE alias `u` must not
    // sneak in as a qualified name key.
    assert!(
        vd.depends_on_columns.iter().all(|(k, _)| k == &users),
        "no CTE-qualified entries should appear: {:?}",
        vd.depends_on_columns,
    );
}

// ── RENAME / SET SCHEMA propagation into view deps ─────────────────────────

#[test]
fn view_deps_updated_after_rename_table() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
             CREATE VIEW v AS SELECT id, name FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t RENAME TO t2;"),
    ]);

    let view = snap.resolve_table(None, "v").unwrap();
    let vd = view.view_def.as_ref().unwrap();
    let old = QualifiedName::new("public", "t");
    let new = QualifiedName::new("public", "t2");

    assert!(!vd.depends_on_tables.contains(&old));
    assert!(vd.depends_on_tables.contains(&new));
    assert!(
        vd.depends_on_columns.iter().all(|(k, _)| k != &old),
        "no dep should still point at old key: {:?}",
        vd.depends_on_columns,
    );
    assert!(
        vd.depends_on_columns
            .iter()
            .any(|(k, c)| k == &new && c == "id"),
    );
    assert!(
        vd.depends_on_columns
            .iter()
            .any(|(k, c)| k == &new && c == "name"),
    );
}

#[test]
fn view_deps_updated_after_rename_column() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
             CREATE VIEW v AS SELECT id, name FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t RENAME COLUMN name TO full_name;"),
    ]);

    let view = snap.resolve_table(None, "v").unwrap();
    let vd = view.view_def.as_ref().unwrap();
    let t = QualifiedName::new("public", "t");

    assert!(
        vd.depends_on_columns
            .iter()
            .any(|(k, c)| k == &t && c == "full_name"),
        "renamed column must appear in deps: {:?}",
        vd.depends_on_columns,
    );
    assert!(
        vd.depends_on_columns.iter().all(|(_, c)| c != "name"),
        "old column name must be gone from deps: {:?}",
        vd.depends_on_columns,
    );
}

#[test]
fn view_deps_updated_after_set_schema() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE SCHEMA app;
             CREATE TABLE t (id INT NOT NULL);
             CREATE VIEW v AS SELECT id FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t SET SCHEMA app;"),
    ]);

    let view = snap.resolve_table(None, "v").unwrap();
    let vd = view.view_def.as_ref().unwrap();
    let old = QualifiedName::new("public", "t");
    let new = QualifiedName::new("app", "t");

    assert!(!vd.depends_on_tables.contains(&old));
    assert!(vd.depends_on_tables.contains(&new));
    assert!(
        vd.depends_on_columns
            .iter()
            .any(|(k, c)| k == &new && c == "id"),
    );
    assert!(vd.depends_on_columns.iter().all(|(k, _)| k != &old));
}

#[test]
fn view_deps_updated_after_rename_schema() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE SCHEMA app;
             CREATE TABLE app.users (id INT NOT NULL, name TEXT NOT NULL);
             CREATE VIEW app.v AS SELECT id, name FROM app.users;",
        ),
        ("0002.sql", "ALTER SCHEMA app RENAME TO core;"),
    ]);

    let view = snap.resolve_table(Some("core"), "v").unwrap();
    let vd = view.view_def.as_ref().unwrap();
    let old = QualifiedName::new("app", "users");
    let new = QualifiedName::new("core", "users");

    assert!(!vd.depends_on_tables.contains(&old));
    assert!(vd.depends_on_tables.contains(&new));
    assert!(vd.depends_on_columns.iter().all(|(k, _)| k.schema != "app"));
    assert!(
        vd.depends_on_columns.iter().any(|(k, _)| k == &new),
        "at least one column dep should now point at core.users: {:?}",
        vd.depends_on_columns,
    );
}

#[test]
fn view_deps_unchanged_on_unrelated_rename() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL);
             CREATE TABLE other (id INT NOT NULL);
             CREATE VIEW v AS SELECT id FROM t;",
        ),
        ("0002.sql", "ALTER TABLE other RENAME TO other2;"),
    ]);

    let view = snap.resolve_table(None, "v").unwrap();
    let vd = view.view_def.as_ref().unwrap();
    let t = QualifiedName::new("public", "t");

    assert_eq!(vd.depends_on_tables, vec![t.clone()]);
    assert!(
        vd.depends_on_columns
            .iter()
            .all(|(k, c)| k == &t && c == "id"),
    );
}

#[test]
fn view_deps_rename_column_unrelated_is_noop() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL, age INT);
             CREATE VIEW v AS SELECT id, name FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t RENAME COLUMN age TO years;"),
    ]);

    let view = snap.resolve_table(None, "v").unwrap();
    let vd = view.view_def.as_ref().unwrap();
    let t = QualifiedName::new("public", "t");

    // Only id and name are deps — age/years was never referenced.
    let names: Vec<&str> = vd
        .depends_on_columns
        .iter()
        .filter(|(k, _)| k == &t)
        .map(|(_, c): &(QualifiedName, String)| c.as_str())
        .collect();
    assert!(names.contains(&"id"));
    assert!(names.contains(&"name"));
    assert!(!names.contains(&"age"));
    assert!(!names.contains(&"years"));
}

#[test]
fn view_deps_rename_preserves_self_join_dedup() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, parent_id INT);
             CREATE VIEW v AS
                 SELECT a.id, b.id AS parent_id
                 FROM t a JOIN t b ON b.id = a.parent_id;",
        ),
        ("0002.sql", "ALTER TABLE t RENAME TO nodes;"),
    ]);

    let view = snap.resolve_table(None, "v").unwrap();
    let vd = view.view_def.as_ref().unwrap();
    let nodes = QualifiedName::new("public", "nodes");

    let count = vd.depends_on_tables.iter().filter(|k| *k == &nodes).count();
    assert_eq!(
        count, 1,
        "self-join dedup must survive rename: {:?}",
        vd.depends_on_tables
    );

    assert!(!vd.depends_on_tables.iter().any(|k| k.name == "t"));
}

// ── Stored AST + serde roundtrip ───────────────────────────────────────────

#[test]
fn view_def_stores_resolved_ast() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (id INT NOT NULL);
         CREATE VIEW v AS SELECT id FROM t;",
    )]);

    let view = snap.resolve_table(None, "v").unwrap();
    let vd = view.view_def.as_ref().unwrap();
    assert!(
        !vd.resolved_ast.is_empty(),
        "resolved_ast should be populated for freshly-created views",
    );
}

#[test]
fn view_def_serde_roundtrip_preserves_ast() {
    // Serializing the catalog to JSON and back must reproduce the AST
    // byte-for-byte. Base64 is load-bearing: without it the JSON blows
    // up to one int per byte.
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (id INT NOT NULL, name TEXT);
         CREATE VIEW v AS SELECT id, name FROM t;",
    )]);

    let json = serde_json::to_string(&snap.to_seed()).unwrap();
    let back: PgCatalog = PgCatalog::from_seed(serde_json::from_str(&json).unwrap());

    let original = snap
        .resolve_table(None, "v")
        .unwrap()
        .view_def
        .as_ref()
        .unwrap();
    let restored = back
        .resolve_table(None, "v")
        .unwrap()
        .view_def
        .as_ref()
        .unwrap();
    assert_eq!(original.resolved_ast, restored.resolved_ast);
    assert_eq!(original.depends_on_tables, restored.depends_on_tables);
    assert_eq!(original.depends_on_columns, restored.depends_on_columns);
}

#[test]
fn view_def_accepts_legacy_json_without_resolved_ast() {
    // Older snapshots predate `resolved_ast`. Loading them must still work;
    // the absent field just defaults to empty, disabling rename propagation
    // into the AST (the deps arrays still cover CASCADE detection).
    let legacy = r#"{
        "types": {},
        "type_by_name": {},
        "tables": {
            "public.t": {
                "name": "t",
                "schema": "public",
                "kind": "Table",
                "columns": [
                    {"name": "id", "type_oid": 23, "not_null": true, "has_default": false}
                ]
            },
            "public.v": {
                "name": "v",
                "schema": "public",
                "kind": "View",
                "columns": [
                    {"name": "id", "type_oid": 23, "not_null": true, "has_default": false}
                ],
                "view_def": {
                    "depends_on_tables": ["public.t"],
                    "depends_on_columns": [["public.t", "id"]]
                }
            }
        },
        "functions_by_name": {},
        "operators_by_name": {},
        "casts": {},
        "search_path": ["public"]
    }"#;

    let snap: PgCatalog = PgCatalog::from_seed(serde_json::from_str(legacy).unwrap());
    let vd = snap
        .resolve_table(None, "v")
        .unwrap()
        .view_def
        .as_ref()
        .unwrap();
    assert!(vd.resolved_ast.is_empty());
    assert_eq!(vd.depends_on_tables.len(), 1);
}

// ── CREATE OR REPLACE VIEW ─────────────────────────────────────────────────

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

// ── CREATE / DROP MATERIALIZED VIEW ────────────────────────────────────────

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

// ── Column-level view dependencies (expression / WHERE / JOIN / ORDER BY) ──

#[test]
fn drop_column_referenced_by_view_expression_fails() {
    // The view's output column is "next_id" but the expression references
    // "id". The structured walker must find the ColumnRef and block the drop.
    let result = try_apply(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
             CREATE VIEW v AS SELECT id + 1 AS next_id FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t DROP COLUMN id;"),
    ]);

    assert_ddl_err!(result, DdlError::DependencyError(_), "depend");
}

#[test]
fn drop_column_referenced_in_view_where_fails() {
    let result = try_apply(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
             CREATE VIEW v AS SELECT name FROM t WHERE id > 10;",
        ),
        ("0002.sql", "ALTER TABLE t DROP COLUMN id;"),
    ]);

    assert_ddl_err!(result, DdlError::DependencyError(_), "depend");
}

#[test]
fn drop_column_referenced_in_view_join_on_fails() {
    let result = try_apply(&[
        (
            "0001.sql",
            "CREATE TABLE t1 (id INT NOT NULL, name TEXT NOT NULL);
             CREATE TABLE t2 (id INT NOT NULL, status TEXT NOT NULL);
             CREATE VIEW v AS SELECT t1.name FROM t1 JOIN t2 ON t1.id = t2.id;",
        ),
        ("0002.sql", "ALTER TABLE t2 DROP COLUMN id;"),
    ]);

    assert_ddl_err!(result, DdlError::DependencyError(_), "depend");
}

#[test]
fn drop_column_referenced_in_view_order_by_fails() {
    let result = try_apply(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL, score INT);
             CREATE VIEW v AS SELECT name FROM t ORDER BY score;",
        ),
        ("0002.sql", "ALTER TABLE t DROP COLUMN score;"),
    ]);

    assert_ddl_err!(result, DdlError::DependencyError(_), "depend");
}

#[test]
fn drop_column_aliased_by_view_fails() {
    // PG tracks the underlying column through the alias: even though the view
    // exposes the column as "n", the dependency is on t.name.
    let result = try_apply(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
             CREATE VIEW v AS SELECT id, name AS n FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t DROP COLUMN name;"),
    ]);

    assert_ddl_err!(result, DdlError::DependencyError(_), "depend");
}

#[test]
fn drop_column_not_referenced_by_view_despite_name_match_succeeds() {
    // The view's "age" is count(*), not t.age — no real dependency.
    // The structured walker must not be fooled by output column names.
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL, age INT);
             CREATE VIEW v AS SELECT count(*) AS age FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t DROP COLUMN age;"),
    ]);

    // Table loses the column; the view survives because count(*) doesn't
    // actually reference t.age.
    let table = snap.resolve_table(None, "t").unwrap();
    assert!(table.columns.iter().all(|c| c.name != "age"));
    assert!(snap.resolve_table(None, "v").is_some());
}

// ── View-on-view CASCADE chain ────────────────────────────────────────────

#[test]
fn drop_view_fails_when_another_view_depends_on_it() {
    let result = try_apply(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL);
             CREATE VIEW v1 AS SELECT id FROM t;
             CREATE VIEW v2 AS SELECT id FROM v1;",
        ),
        ("0002.sql", "DROP VIEW v1;"),
    ]);

    assert_ddl_err!(result, DdlError::DependencyError(_), "depend");
}

#[test]
fn drop_view_cascade_removes_dependent_views() {
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
    // Underlying table is untouched.
    assert!(snap.resolve_table(None, "t").is_some());
}
