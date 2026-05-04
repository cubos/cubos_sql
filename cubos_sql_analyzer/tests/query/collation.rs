//! COLLATE clauses — `expr COLLATE "en_US"` in ORDER BY / WHERE /
//! expressions. The analyzer doesn't model collations explicitly, but the
//! `COLLATE` clause must not change the result type or nullability of the
//! underlying expression — so SELECT/WHERE/ORDER BY queries with a COLLATE
//! decoration should analyze as if the collation were absent.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE users (
            id   BIGINT PRIMARY KEY,
            name TEXT NOT NULL,
            nick TEXT
         );",
    )
    .unwrap();
    db
}

// ── COLLATE in projections ──────────────────────────────────────────────────

#[test]
fn collate_in_select_preserves_text_type_and_nullability() {
    let db = setup();
    // `name COLLATE "C"` is still text, NOT NULL, with the collation
    // surfaced on the output column (PG does the same in the row
    // description).
    let s = db
        .analyze("SELECT name COLLATE \"C\" AS n FROM users")
        .unwrap();
    assert_cols(
        &s,
        vec![c("n", basic_with_collation("pg_catalog", "text", "C"))],
    );
}

#[test]
fn collate_in_select_keeps_nullable() {
    let db = setup();
    let s = db
        .analyze("SELECT nick COLLATE \"C\" AS n FROM users")
        .unwrap();
    assert_cols(
        &s,
        vec![cn("n", basic_with_collation("pg_catalog", "text", "C"))],
    );
}

// ── COLLATE in ORDER BY ─────────────────────────────────────────────────────

#[test]
fn collate_in_order_by_does_not_affect_columns() {
    let db = setup();
    // ORDER BY decorations are invisible at the projection level.
    let s = db
        .analyze("SELECT id, name FROM users ORDER BY name COLLATE \"C\"")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

// ── COLLATE in WHERE ────────────────────────────────────────────────────────

#[test]
fn collate_in_where_against_param() {
    let db = setup();
    // `name COLLATE "C" = $p1` — the comparison is still text=text, so the
    // param should be inferred as text.
    let s = db
        .analyze("SELECT id FROM users WHERE name COLLATE \"C\" = $p1")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text())]);
}

// ── COLLATE on a non-string type must error (PG: collations are not
// supported by type X). Marked ignored: analyzer does not track collation
// applicability today. ──────────────────────────────────────────────────────

#[test]
fn collate_on_int_column_is_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id COLLATE \"C\" FROM users"),
        AnalyzeError::Invalid(_),
        "collations are not supported by type bigint",
    );
}

#[test]
fn collate_on_jsonb_column_is_rejected() {
    let mut db = PgCatalog::new();
    db.apply_sql("CREATE TABLE t (id BIGINT PRIMARY KEY, meta JSONB NOT NULL);")
        .unwrap();
    assert_analyze_err!(
        db.analyze("SELECT meta COLLATE \"C\" FROM t"),
        AnalyzeError::Invalid(_),
        "collations are not supported by type jsonb",
    );
}

#[test]
fn collate_on_text_domain_accepted() {
    // A domain over text should still inherit the string category — the
    // analyzer must unwrap the domain before checking applicability.
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE DOMAIN slug AS TEXT;
         CREATE TABLE t (id BIGINT PRIMARY KEY, name slug NOT NULL);",
    )
    .unwrap();
    let s = db.analyze("SELECT name COLLATE \"C\" AS n FROM t").unwrap();
    assert_cols(
        &s,
        vec![c("n", domain_with_collation("public", "slug", text(), "C"))],
    );
}

// ── Stacked / nested COLLATE ────────────────────────────────────────────────

#[test]
fn collate_in_concat_expression() {
    let db = setup();
    // `name || (nick COLLATE "C")` — collate on the nullable side; the
    // concat is strict so the result is nullable.
    let s = db
        .analyze("SELECT name || (nick COLLATE \"C\") AS combined FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("combined", text())]);
}

#[test]
fn collate_in_case_branch() {
    let db = setup();
    let s = db
        .analyze("SELECT CASE WHEN id > 0 THEN name COLLATE \"C\" ELSE 'x' END AS v FROM users")
        .unwrap();
    assert_cols(&s, vec![c("v", text())]);
}

// ── Collation registry — `pg_collation` / `attcollation` not modeled ────────
//
// The analyzer accepts any collation name in a `COLLATE "x"` decoration —
// there's no `pg_collation` to validate against, and no `attcollation`
// recording the column's default collation. PG rejects unknown collations
// up front and propagates a column's collation through expressions.

#[test]
fn collate_unknown_collation_should_error() {
    let mut db = PgCatalog::new();
    db.apply_sql("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT NOT NULL);")
        .unwrap();
    // PG: `collation "definitely_not_a_real_collation" for encoding "UTF8"
    // does not exist`. CREATE TABLE with a bogus column-level COLLATE
    // raises this at apply time; we mirror it.
    let err = db
        .apply_sql(
            "CREATE TABLE t (
                id BIGINT PRIMARY KEY,
                name TEXT COLLATE \"definitely_not_a_real_collation\" NOT NULL
             );",
        )
        .unwrap_err();
    assert!(
        format!("{err}").contains("does not exist"),
        "expected collation-not-found error, got: {err}"
    );
}

#[test]
fn create_collation_then_use_it() {
    // PG: `CREATE COLLATION my_coll (LOCALE = 'C')` registers a new
    // collation in pg_collation. Subsequent `COLLATE "my_coll"` resolves.
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE COLLATION my_coll (LOCALE = 'C');
         CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT NOT NULL);",
    )
    .unwrap();
    db.analyze("SELECT name COLLATE \"my_coll\" AS n FROM t")
        .unwrap();
}

#[test]
fn create_collation_from_existing() {
    // PG: `CREATE COLLATION new FROM existing` clones an existing entry.
    // The new row resolves with the same encoding semantics as its source.
    let mut db = PgCatalog::new();
    db.apply_sql("CREATE COLLATION my_c FROM \"C\";").unwrap();
    let resolved = db
        .resolve_collation(None, "my_c")
        .expect("clone should be registered");
    let source = db.resolve_collation(None, "C").unwrap();
    assert_eq!(resolved.collencoding, source.collencoding);
}

#[test]
fn create_collation_from_unknown_errors() {
    let mut db = PgCatalog::new();
    let err = db
        .apply_sql("CREATE COLLATION my_c FROM \"definitely_not_a_real_one\";")
        .unwrap_err();
    assert!(
        format!("{err}").contains("does not exist"),
        "expected source-not-found error, got: {err}"
    );
}

#[test]
fn create_collation_duplicate_name_errors() {
    let mut db = PgCatalog::new();
    db.apply_sql("CREATE COLLATION my_c (LOCALE = 'C');")
        .unwrap();
    let err = db
        .apply_sql("CREATE COLLATION my_c (LOCALE = 'C');")
        .unwrap_err();
    assert!(
        format!("{err}").contains("already exists"),
        "expected duplicate error, got: {err}"
    );
}

#[test]
fn create_collation_if_not_exists_swallows_duplicate() {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE COLLATION my_c (LOCALE = 'C');
         CREATE COLLATION IF NOT EXISTS my_c (LOCALE = 'C');",
    )
    .unwrap();
}

#[test]
fn column_level_collate_in_create_table_is_preserved() {
    // PG: `name TEXT COLLATE "C"` pins the column's default collation in
    // pg_attribute.attcollation. The analyzer now records it, so two
    // tables with different declared collations no longer look identical.
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE t (
            id   BIGINT PRIMARY KEY,
            name TEXT COLLATE \"C\" NOT NULL
         );",
    )
    .unwrap();
    let table = db.resolve_table(None, "t").unwrap();
    let attrs = db.attributes_of(table.oid);
    let name = attrs.iter().find(|a| a.attname == "name").unwrap();
    let c_oid = db
        .resolve_collation(None, "C")
        .expect("\"C\" collation must be in the seed")
        .oid;
    assert_eq!(name.attcollation, Some(c_oid));
}
