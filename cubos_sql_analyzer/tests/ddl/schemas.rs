//! CREATE / DROP SCHEMA, RENAME SCHEMA, schema-qualified object handling,
//! CASCADE drops that remove all objects in a schema.

use crate::common::*;

// ── DROP SCHEMA ─────────────────────────────────────────────────────────────

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
    let result = try_apply(&[(
        "0001.sql",
        "CREATE SCHEMA foo;
         CREATE TABLE foo.bar (id INT PRIMARY KEY);
         DROP SCHEMA foo;",
    )]);
    assert_ddl_err!(result, DdlError::DependencyError(_), "cannot drop schema");
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
    let result = try_apply(&[("0001.sql", "DROP SCHEMA nonexistent;")]);
    assert_ddl_err!(result, DdlError::DependencyError(_), "nonexistent");
}
