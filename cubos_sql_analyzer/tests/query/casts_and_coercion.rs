//! Type casts and coercion: explicit `CAST(…)` / `::type`, implicit
//! promotion (int4 → int8, numeric tower), common-type resolution in
//! CASE/COALESCE/UNION branches.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new();
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

// ── Numeric tower: int2 → int4 → int8 → numeric → float4 → float8 ────────────

#[test]
fn numeric_tower_int2_to_int8() {
    let db = setup();
    let s = db.analyze("SELECT 1::int2::int8 AS n").unwrap();
    assert_cols(&s, vec![c("n", int8())]);
}

#[test]
fn numeric_tower_int4_to_numeric() {
    let db = setup();
    let s = db.analyze("SELECT 1::int4::numeric AS n").unwrap();
    assert_cols(&s, vec![c("n", numeric())]);
}

#[test]
fn numeric_tower_numeric_to_float8() {
    let db = setup();
    let s = db.analyze("SELECT (1::numeric)::float8 AS n").unwrap();
    assert_cols(&s, vec![c("n", float8())]);
}

#[test]
fn numeric_tower_float4_to_float8() {
    let db = setup();
    let s = db.analyze("SELECT (1.0::float4)::float8 AS n").unwrap();
    assert_cols(&s, vec![c("n", float8())]);
}

// ── Array cast ───────────────────────────────────────────────────────────────

#[test]
fn cast_array_literal_to_int8_array() {
    let db = setup();
    let s = db.analyze("SELECT ARRAY[1,2]::int8[] AS xs").unwrap();
    assert_cols(&s, vec![c("xs", array_of(int8()))]);
}

// ── Cast inside a VALUES list projected from a subquery ──────────────────────

#[test]
fn cast_in_values_subquery() {
    let db = setup();
    // VALUES infers the column type from the first row's expressions; the
    // cast pins the element type so downstream consumers see int8.
    let s = db
        .analyze("SELECT x FROM (VALUES (1::int8), (2)) AS t(x)")
        .unwrap();
    assert_cols(&s, vec![c("x", int8())]);
}
