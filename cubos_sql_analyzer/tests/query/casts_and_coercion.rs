//! Type casts and coercion: explicit `CAST(…)` / `::type`, implicit
//! promotion (int4 → int8, numeric tower), common-type resolution in
//! CASE/COALESCE/UNION branches.

use crate::common::*;

fn setup() -> Database {
    let mut db = Database::new();
    db.apply_sql(
        "CREATE TABLE users (
            id    BIGINT PRIMARY KEY,
            name  TEXT NOT NULL,
            age   INT
         );",
    )
    .unwrap();
    db
}

// ── Explicit casts ───────────────────────────────────────────────────────────

#[test]
fn types_match_cast_int_to_text() {
    let db = setup();
    let s = db
        .analyze("SELECT age::text AS age_text FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("age_text", text())]);
}

#[test]
fn types_match_cast_bigint_to_int() {
    let db = setup();
    let s = db
        .analyze("SELECT id::int4 AS short_id FROM users")
        .unwrap();
    assert_cols(&s, vec![c("short_id", int4())]);
}

#[test]
fn types_match_cast_literal() {
    let db = setup();
    let s = db.analyze("SELECT '123'::int4 AS val").unwrap();
    assert_cols(&s, vec![c("val", int4())]);
}

// ── Cast preserves nullability ───────────────────────────────────────────────

#[test]
fn complex_cast_preserves_nullability() {
    let db = setup();
    // Casting a nullable column preserves nullability.
    let sql = "SELECT age::text as age_text, id::text as id_text FROM users";
    let info = db.analyze(sql).unwrap();
    // age is nullable → age::text is nullable.
    assert!(col(&info, "age_text").nullable);
    // id is NOT NULL → id::text is NOT NULL.
    assert!(!col(&info, "id_text").nullable);
}
