//! Table inheritance — `pg_inherits` is not modeled.
//!
//! `CREATE TABLE child () INHERITS (parent)` makes `child` carry every
//! column of `parent`, and `pg_inherits` records the parent/child link so
//! `SELECT FROM parent` can transparently scan the children. None of that
//! lives in the catalog mirror today, so the migrations parse but the
//! parent/child relationship is silently dropped.

use crate::common::*;

#[test]
#[ignore = "pg_inherits not modeled — child does not pick up parent's columns"]
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

    // PG: `dogs` has columns name, sound, breed (in that order).
    let table = snap.resolve_table(Some("public"), "dogs").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let names: Vec<&str> = attrs.iter().map(|a| a.attname.as_str()).collect();
    assert_eq!(names, vec!["name", "sound", "breed"]);
}

#[test]
#[ignore = "pg_inherits not modeled — DROP COLUMN on parent does not cascade to children"]
fn drop_column_on_parent_cascades_to_children() {
    // PG behavior: DROP COLUMN sound CASCADE on the parent removes the
    // inherited column from `dogs` too. Without `pg_inherits` we don't
    // even know `dogs` is a child, so the cascade is a no-op.
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
    assert_eq!(names, vec!["name".to_string(), "breed".to_string()]);
}
