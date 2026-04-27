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
    // `name COLLATE "C"` is still text, NOT NULL — collation is metadata.
    let s = db
        .analyze("SELECT name COLLATE \"C\" AS n FROM users")
        .unwrap();
    assert_cols(&s, vec![c("n", text())]);
}

#[test]
fn collate_in_select_keeps_nullable() {
    let db = setup();
    let s = db
        .analyze("SELECT nick COLLATE \"C\" AS n FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("n", text())]);
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
        "collations are not supported",
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
        "collations are not supported",
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
    assert_cols(&s, vec![c("n", domain("public", "slug", text()))]);
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
