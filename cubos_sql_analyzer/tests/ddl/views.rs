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
    assert_eq!(view.relkind, RelKind::View);
    let attrs = snap.attributes_of(view.oid);
    assert_eq!(attrs.len(), 2);
    assert_eq!(attrs[0].attname, "id");
    assert_eq!(attrs[1].attname, "name");
}

#[test]
fn view_star_expanded_at_creation_time() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
             CREATE VIEW v AS SELECT * FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t ADD COLUMN age INT;"),
    ]);

    let view = snap.resolve_table(None, "v").unwrap();
    let attrs = snap.attributes_of(view.oid);
    assert_eq!(
        attrs.len(),
        2,
        "SELECT * should be expanded at creation time"
    );
    assert_eq!(attrs[0].attname, "id");
    assert_eq!(attrs[1].attname, "name");

    let table = snap.resolve_table(None, "t").unwrap();
    assert_eq!(snap.attributes_of(table.oid).len(), 3);
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
    let attrs = snap.attributes_of(view.oid);
    assert_eq!(attrs.len(), 1);
    assert_eq!(attrs[0].attname, "id");
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
    let attrs = snap.attributes_of(view.oid);
    assert_eq!(attrs.len(), 2);
    assert_eq!(attrs[0].attname, "name");
    assert_eq!(attrs[1].attname, "title");
}

#[test]
fn view_with_aliases() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
         CREATE VIEW v (the_id, the_name) AS SELECT id, name FROM t;",
    )]);

    let view = snap.resolve_table(None, "v").unwrap();
    let attrs = snap.attributes_of(view.oid);
    assert_eq!(attrs[0].attname, "the_id");
    assert_eq!(attrs[1].attname, "the_name");
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
    let attrs = snap.attributes_of(table.oid);
    assert_eq!(attrs.len(), 1);
    assert_eq!(attrs[0].attname, "id");
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
    assert_eq!(snap.attributes_of(view.oid).len(), 2);
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
    let attrs = snap.attributes_of(view.oid);
    assert_eq!(attrs.len(), 2);
    let amount = attrs.iter().find(|c| c.attname == "amount").unwrap();
    let int8_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int8")
        .unwrap()
        .oid;
    assert_eq!(amount.atttypid, int8_oid);
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
    assert_eq!(snap.attributes_of(view.oid).len(), 2);
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
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL);
         CREATE TABLE orders (id INT NOT NULL, user_id INT NOT NULL);
         CREATE VIEW v AS
             SELECT u.id FROM users u JOIN orders o ON o.user_id = u.id;",
    )]);

    let view_oid = class_oid(&snap, None, "v");
    let table_deps = view_table_deps(&snap, view_oid);
    let col_deps = view_column_deps(&snap, view_oid);

    let users = QualifiedName::new("public", "users");
    let orders = QualifiedName::new("public", "orders");

    assert!(table_deps.contains(&users));
    assert!(table_deps.contains(&orders));

    assert!(
        col_deps.iter().any(|(k, c)| k == &users && c == "id"),
        "users.id must be tracked: {col_deps:?}",
    );
    assert!(
        col_deps.iter().any(|(k, c)| k == &orders && c == "user_id"),
        "orders.user_id must be tracked: {col_deps:?}",
    );
    assert!(
        !col_deps.iter().any(|(k, c)| k == &orders && c == "id"),
        "orders.id must NOT be tracked: {col_deps:?}",
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

    let view_oid = class_oid(&snap, None, "v");
    let table_deps = view_table_deps(&snap, view_oid);
    let col_deps = view_column_deps(&snap, view_oid);
    let users = QualifiedName::new("app", "users");

    assert!(table_deps.contains(&users));
    assert!(
        col_deps.iter().any(|(k, c)| k == &users && c == "id"),
        "schema-qualified column ref must resolve to (app.users, id): {col_deps:?}",
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

    let view_oid = class_oid(&snap, None, "v");
    let table_deps = view_table_deps(&snap, view_oid);
    let col_deps = view_column_deps(&snap, view_oid);
    let t = QualifiedName::new("public", "t");

    let t_count = table_deps.iter().filter(|k| *k == &t).count();
    assert_eq!(
        t_count, 1,
        "self-joined table must be dedup'd: {table_deps:?}"
    );

    let id_count = col_deps
        .iter()
        .filter(|(k, c)| k == &t && c == "id")
        .count();
    let parent_count = col_deps
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

    let view_oid = class_oid(&snap, None, "v");
    let table_deps = view_table_deps(&snap, view_oid);
    let col_deps = view_column_deps(&snap, view_oid);

    let users = QualifiedName::new("public", "users");
    assert!(table_deps.contains(&users));
    assert_eq!(table_deps.len(), 1);
    assert!(
        col_deps.iter().all(|(k, _)| k == &users),
        "no CTE-qualified entries should appear: {col_deps:?}",
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

    let view_oid = class_oid(&snap, None, "v");
    let table_deps = view_table_deps(&snap, view_oid);
    let col_deps = view_column_deps(&snap, view_oid);
    let new = QualifiedName::new("public", "t2");

    // Dep references are by OID — rename doesn't change OIDs, but the
    // resolved name follows the new relname.
    assert!(table_deps.contains(&new));
    assert!(col_deps.iter().any(|(k, c)| k == &new && c == "id"));
    assert!(col_deps.iter().any(|(k, c)| k == &new && c == "name"));
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

    let view_oid = class_oid(&snap, None, "v");
    let col_deps = view_column_deps(&snap, view_oid);
    let t = QualifiedName::new("public", "t");

    assert!(
        col_deps.iter().any(|(k, c)| k == &t && c == "full_name"),
        "renamed column must appear in deps: {col_deps:?}",
    );
    assert!(
        col_deps.iter().all(|(_, c)| c != "name"),
        "old column name must be gone from deps: {col_deps:?}",
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

    let view_oid = class_oid(&snap, None, "v");
    let table_deps = view_table_deps(&snap, view_oid);
    let col_deps = view_column_deps(&snap, view_oid);
    let new = QualifiedName::new("app", "t");

    assert!(table_deps.contains(&new));
    assert!(col_deps.iter().any(|(k, c)| k == &new && c == "id"));
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

    let view_oid = class_oid(&snap, Some("core"), "v");
    let table_deps = view_table_deps(&snap, view_oid);
    let col_deps = view_column_deps(&snap, view_oid);
    let new = QualifiedName::new("core", "users");

    assert!(table_deps.contains(&new));
    assert!(col_deps.iter().any(|(k, _)| k == &new));
    assert!(col_deps.iter().all(|(k, _)| k.schema != "app"));
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

    let view_oid = class_oid(&snap, None, "v");
    let table_deps = view_table_deps(&snap, view_oid);
    let col_deps = view_column_deps(&snap, view_oid);
    let t = QualifiedName::new("public", "t");

    assert_eq!(table_deps, vec![t.clone()]);
    assert!(col_deps.iter().all(|(k, c)| k == &t && c == "id"));
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

    let view_oid = class_oid(&snap, None, "v");
    let col_deps = view_column_deps(&snap, view_oid);
    let t = QualifiedName::new("public", "t");

    let names: Vec<&str> = col_deps
        .iter()
        .filter(|(k, _)| k == &t)
        .map(|(_, c)| c.as_str())
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

    let view_oid = class_oid(&snap, None, "v");
    let table_deps = view_table_deps(&snap, view_oid);
    let nodes = QualifiedName::new("public", "nodes");

    let count = table_deps.iter().filter(|k| *k == &nodes).count();
    assert_eq!(
        count, 1,
        "self-join dedup must survive rename: {table_deps:?}"
    );
    assert!(!table_deps.iter().any(|k| k.name == "t"));
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
    assert!(
        view.relviewdef.is_some(),
        "relviewdef should be populated for freshly-created views",
    );
}

#[test]
fn view_def_serde_roundtrip_preserves_ast() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (id INT NOT NULL, name TEXT);
         CREATE VIEW v AS SELECT id, name FROM t;",
    )]);

    let json = serde_json::to_string(&snap.to_seed()).unwrap();
    let back: PgCatalog = PgCatalog::from_seed(serde_json::from_str(&json).unwrap());

    let original = snap.resolve_table(None, "v").unwrap();
    let restored = back.resolve_table(None, "v").unwrap();
    assert_eq!(original.relviewdef, restored.relviewdef);

    let original_table_deps = view_table_deps(&snap, original.oid);
    let restored_table_deps = view_table_deps(&back, restored.oid);
    assert_eq!(original_table_deps, restored_table_deps);

    let original_col_deps = view_column_deps(&snap, original.oid);
    let restored_col_deps = view_column_deps(&back, restored.oid);
    assert_eq!(original_col_deps, restored_col_deps);
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
    let attrs = snap.attributes_of(view.oid);
    assert_eq!(attrs.len(), 3);
    assert_eq!(attrs[2].attname, "age");
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
    assert_eq!(view.relkind, RelKind::MaterializedView);
    let attrs = snap.attributes_of(view.oid);
    assert_eq!(attrs.len(), 2);
    assert_eq!(attrs[0].attname, "id");
    assert_eq!(attrs[1].attname, "name");
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
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL, age INT);
             CREATE VIEW v AS SELECT count(*) AS age FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t DROP COLUMN age;"),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    assert!(attrs.iter().all(|c| c.attname != "age"));
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
    assert!(snap.resolve_table(None, "t").is_some());
}

// ── Function/type bindings flow through pg_depend ───────────────────────────
//
// Views that name a function (`lower(x)`) or a type (`x::my_domain`) record
// pg_depend rows so DROP FUNCTION / DROP TYPE without CASCADE fails. Mirrors
// PG: every name slot in a view's query becomes a dep.

#[test]
fn drop_function_referenced_by_view_fails_without_cascade() {
    // PG: `cannot drop function ... because other objects depend on it`
    assert_ddl_err!(
        try_apply(&[
            (
                "0001.sql",
                "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
                 CREATE FUNCTION shout(s TEXT) RETURNS TEXT \
                   LANGUAGE SQL IMMUTABLE AS $$ SELECT upper(s) $$;
                 CREATE VIEW v AS SELECT id, shout(name) AS yelled FROM t;",
            ),
            ("0002.sql", "DROP FUNCTION shout(text);"),
        ]),
        DdlError::DependencyError(_),
        "depend",
    );
}

#[test]
fn drop_function_cascade_removes_dependent_view() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
             CREATE FUNCTION shout(s TEXT) RETURNS TEXT \
               LANGUAGE SQL IMMUTABLE AS $$ SELECT upper(s) $$;
             CREATE VIEW v AS SELECT id, shout(name) AS yelled FROM t;",
        ),
        ("0002.sql", "DROP FUNCTION shout(text) CASCADE;"),
    ]);

    assert!(snap.resolve_table(None, "v").is_none());
    assert!(snap.resolve_table(None, "t").is_some());
}

#[test]
fn drop_aggregate_referenced_by_view_fails_without_cascade() {
    // Aggregates share pg_proc + the same function-binding edge, so the
    // same protection must apply.
    assert_ddl_err!(
        try_apply(&[
            (
                "0001.sql",
                "CREATE TABLE t (id INT NOT NULL, score INT NOT NULL);
                 CREATE AGGREGATE custom_sum(int) (sfunc = int4pl, stype = int);
                 CREATE VIEW v AS SELECT custom_sum(score) AS total FROM t;",
            ),
            ("0002.sql", "DROP AGGREGATE custom_sum(int);"),
        ]),
        DdlError::DependencyError(_),
        "depend",
    );
}

#[test]
fn drop_type_referenced_by_view_cast_fails_without_cascade() {
    // PG: a view that casts to a domain registers a pg_depend row to the
    // domain — DROP DOMAIN without CASCADE has to surface that.
    assert_ddl_err!(
        try_apply(&[
            (
                "0001.sql",
                "CREATE DOMAIN positive_int AS INT CHECK (VALUE > 0);
                 CREATE TABLE t (id INT NOT NULL);
                 CREATE VIEW v AS SELECT id::positive_int AS pos FROM t;",
            ),
            ("0002.sql", "DROP DOMAIN positive_int;"),
        ]),
        DdlError::DependencyError(_),
        "depend",
    );
}

#[test]
fn drop_type_cascade_removes_dependent_view() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE DOMAIN positive_int AS INT CHECK (VALUE > 0);
             CREATE TABLE t (id INT NOT NULL);
             CREATE VIEW v AS SELECT id::positive_int AS pos FROM t;",
        ),
        ("0002.sql", "DROP DOMAIN positive_int CASCADE;"),
    ]);

    assert!(snap.resolve_table(None, "v").is_none());
    assert!(snap.resolve_table(None, "t").is_some());
}

#[test]
fn drop_function_unused_by_view_succeeds() {
    // Sanity: dropping a function that no view depends on must still go
    // through. The new pg_depend edges only block when a view references
    // the callee.
    let snap = build(&[
        (
            "0001.sql",
            "CREATE FUNCTION shout(s TEXT) RETURNS TEXT \
               LANGUAGE SQL IMMUTABLE AS $$ SELECT upper(s) $$;",
        ),
        ("0002.sql", "DROP FUNCTION shout(text);"),
    ]);
    assert!(snap.find_functions(None, "shout").is_empty());
}
