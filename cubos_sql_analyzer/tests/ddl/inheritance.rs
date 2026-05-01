//! Table inheritance — `CREATE TABLE child INHERITS (parent)`.
//!
//! `pg_inherits` records each (child, parent) edge. Columns from each
//! parent are copied onto the child's `pg_attribute` rows so that a
//! `SELECT FROM child` resolves the same way as PostgreSQL's. DROP
//! COLUMN on a parent is propagated to descendants by walking
//! `pg_inherits`.

use crate::common::*;

#[test]
fn create_table_inherits_copies_columns_into_child() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE animals (
            name  TEXT NOT NULL,
            sound TEXT NOT NULL
         );
         CREATE TABLE dogs (
            breed TEXT NOT NULL
         ) INHERITS (animals);",
    )]);

    // PG: child column order is local-first, then inherited; we follow the
    // same convention by appending parent attrs after the child's locals.
    let table = snap.resolve_table(Some("public"), "dogs").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let names: Vec<&str> = attrs.iter().map(|a| a.attname.as_str()).collect();
    assert_eq!(names, vec!["breed", "name", "sound"]);
}

#[test]
fn drop_column_on_parent_cascades_to_children() {
    // DROP COLUMN sound CASCADE on the parent removes the inherited
    // column from `dogs` too.
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE animals (
            name  TEXT NOT NULL,
            sound TEXT NOT NULL
         );
         CREATE TABLE dogs (
            breed TEXT NOT NULL
         ) INHERITS (animals);",
    )
    .unwrap();
    db.apply_sql("ALTER TABLE animals DROP COLUMN sound CASCADE;")
        .unwrap();

    let dogs = db.resolve_table(Some("public"), "dogs").unwrap();
    let names: Vec<String> = db
        .attributes_of(dogs.oid)
        .iter()
        .map(|a| a.attname.clone())
        .collect();
    assert_eq!(names, vec!["breed".to_string(), "name".to_string()]);
}

#[test]
fn inherits_propagates_not_null_and_default_flags() {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE base (
            id   BIGINT NOT NULL,
            tag  TEXT
         );
         CREATE TABLE leaf () INHERITS (base);",
    )
    .unwrap();
    let leaf = db.resolve_table(Some("public"), "leaf").unwrap();
    let attrs = db.attributes_of(leaf.oid);
    let id = attrs.iter().find(|a| a.attname == "id").unwrap();
    let tag = attrs.iter().find(|a| a.attname == "tag").unwrap();
    assert!(id.attnotnull, "inherited NOT NULL must propagate");
    assert!(!tag.attnotnull, "inherited nullable must propagate");
}

#[test]
fn inherits_multiple_parents_appends_columns_in_order() {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE a (x INT NOT NULL);
         CREATE TABLE b (y INT NOT NULL);
         CREATE TABLE c (z INT NOT NULL) INHERITS (a, b);",
    )
    .unwrap();
    let c = db.resolve_table(Some("public"), "c").unwrap();
    let names: Vec<String> = db
        .attributes_of(c.oid)
        .iter()
        .map(|a| a.attname.clone())
        .collect();
    assert_eq!(names, vec!["z", "x", "y"]);
}

#[test]
fn inherits_dedupes_same_named_column() {
    // PG: when the child already declares a column with the same name, the
    // parent's copy is merged into it (types must match) — no duplicate row.
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE base (id BIGINT NOT NULL, tag TEXT);
         CREATE TABLE leaf (id BIGINT NOT NULL) INHERITS (base);",
    )
    .unwrap();
    let leaf = db.resolve_table(Some("public"), "leaf").unwrap();
    let names: Vec<String> = db
        .attributes_of(leaf.oid)
        .iter()
        .map(|a| a.attname.clone())
        .collect();
    assert_eq!(names, vec!["id", "tag"]);
}

#[test]
fn drop_column_cascade_descends_two_levels_of_inheritance() {
    // grand <- parent <- child. DROP on grand must also remove the column
    // from `parent` AND `child`.
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE grand (id BIGINT NOT NULL, label TEXT);
         CREATE TABLE parent () INHERITS (grand);
         CREATE TABLE child () INHERITS (parent);
         ALTER TABLE grand DROP COLUMN label CASCADE;",
    )
    .unwrap();
    for relname in ["grand", "parent", "child"] {
        let r = db.resolve_table(Some("public"), relname).unwrap();
        let names: Vec<String> = db
            .attributes_of(r.oid)
            .iter()
            .map(|a| a.attname.clone())
            .collect();
        assert_eq!(
            names,
            vec!["id".to_string()],
            "label should be gone from {relname}"
        );
    }
}

#[test]
fn pg_inherits_records_one_row_per_parent() {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE a (x INT NOT NULL);
         CREATE TABLE b (y INT NOT NULL);
         CREATE TABLE c () INHERITS (a, b);",
    )
    .unwrap();
    let c = db.resolve_table(Some("public"), "c").unwrap();
    let edges: Vec<i32> = db
        .pg_inherits()
        .iter()
        .filter(|i| i.inhrelid == c.oid)
        .map(|i| i.inhseqno)
        .collect();
    assert_eq!(edges, vec![1, 2], "two parents → seqnos 1 and 2");
}
