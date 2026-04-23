//! WHERE clause: AND/OR, IN, LIKE, IS NULL, NOT, comparison operators.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE users (
            id         BIGINT PRIMARY KEY,
            name       TEXT NOT NULL,
            email      TEXT NOT NULL,
            age        INT,
            created_at TIMESTAMPTZ NOT NULL
        );",
    )
    .unwrap();
    db
}

// ── Non-boolean expression in WHERE is a type mismatch ───────────────────────

#[test]
fn where_int4_not_boolean() {
    let db = setup();
    assert_type_mismatch(&db, "SELECT id FROM users WHERE 42", "int4", "bool");
}

#[test]
fn where_text_column_not_boolean() {
    let db = setup();
    assert_type_mismatch(&db, "SELECT id FROM users WHERE name", "text", "bool");
}

#[test]
fn where_int8_column_not_boolean() {
    let db = setup();
    assert_type_mismatch(&db, "SELECT name FROM users WHERE id", "int8", "bool");
}

#[test]
fn where_timestamptz_column_not_boolean() {
    let db = setup();
    assert_type_mismatch(
        &db,
        "SELECT id FROM users WHERE created_at",
        "timestamptz",
        "bool",
    );
}

// ── IS NULL / IS NOT NULL ────────────────────────────────────────────────────

#[test]
fn where_is_not_null() {
    let db = setup();
    // The analyzer doesn't narrow nullability through WHERE clauses.
    let s = db
        .analyze("SELECT id, age FROM users WHERE age IS NOT NULL")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), cn("age", int4())]);
}

#[test]
fn where_is_null() {
    let db = setup();
    let s = db
        .analyze("SELECT id, name FROM users WHERE age IS NULL")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

// ── AND / OR / NOT ───────────────────────────────────────────────────────────

#[test]
fn where_and() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE name = $p1 AND email = $p2")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text()), p(text())]);
}

#[test]
fn where_or() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE name = $p1 OR email = $p2")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text()), p(text())]);
}

#[test]
fn where_not() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE NOT (age > $p1)")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(int4())]);
}

// ── IN / LIKE / comparison ───────────────────────────────────────────────────

#[test]
fn where_in_list() {
    let db = setup();
    // Literal form: PG promotes list elements to the column's type (int4), but
    // since they are constants nothing surfaces in the param list.
    let s = db
        .analyze("SELECT id FROM users WHERE age IN (1, 2, 3)")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![]);
}

#[test]
fn where_in_list_with_params() {
    let db = setup();
    // Param form: each `$pN` inside the IN list must be inferred with the
    // left-hand column's type as the goal, so all three params surface as int4.
    let s = db
        .analyze("SELECT id FROM users WHERE age IN ($p1, $p2, $p3)")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(int4()), p(int4()), p(int4())]);
}

#[test]
fn where_like() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE name LIKE $p1")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text())]);
}

#[test]
fn where_comparison_operators() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE age >= $p1 AND age <= $p2 AND name <> $p3")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(int4()), p(int4()), p(text())]);
}

// ── BETWEEN ──────────────────────────────────────────────────────────────────

#[test]
fn where_between_literals() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE age BETWEEN 18 AND 65")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![]);
}

#[test]
fn where_between_params() {
    let db = setup();
    // Both bounds are walked with the lhs column type as the inference goal,
    // so `$p1`/`$p2` resolve to `int4` (the type of `age`).
    let s = db
        .analyze("SELECT id FROM users WHERE age BETWEEN $p1 AND $p2")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(int4()), p(int4())]);
}

#[test]
fn where_not_between_params() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE age NOT BETWEEN $p1 AND $p2")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(int4()), p(int4())]);
}

#[test]
fn where_between_symmetric_with_params() {
    let db = setup();
    // BETWEEN SYMMETRIC should route through the same walker as plain BETWEEN.
    let s = db
        .analyze("SELECT id FROM users WHERE age BETWEEN SYMMETRIC $p1 AND $p2")
        .unwrap();
    assert_params(&s, vec![p(int4()), p(int4())]);
}

// ── IS [NOT] DISTINCT FROM ──────────────────────────────────────────────────

#[test]
fn where_is_distinct_from_literal() {
    let db = setup();
    // PG defines `IS DISTINCT FROM` to be NULL-aware and to always produce
    // a definite boolean, so `d` is NOT NULL even though `age` is nullable.
    let s = db
        .analyze("SELECT id, age IS DISTINCT FROM 10 AS d FROM users")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("d", bool_ty())]);
}

#[test]
fn where_is_not_distinct_from_null() {
    let db = setup();
    // `col IS NOT DISTINCT FROM NULL` is effectively `col IS NULL` — still
    // always-definite, so the result column is NOT NULL.
    let s = db
        .analyze("SELECT id, age IS NOT DISTINCT FROM NULL AS d FROM users")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("d", bool_ty())]);
}

#[test]
fn where_is_distinct_from_with_param() {
    let db = setup();
    // The rhs param is walked and inferred from the lhs column type.
    // Matches the convention used by other WHERE-clause param tests: params
    // default to NOT NULL unless the caller annotates them with `$p1?`.
    let s = db
        .analyze("SELECT id FROM users WHERE age IS DISTINCT FROM $p1")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(int4())]);
}

// ── POSIX regex + SIMILAR TO ─────────────────────────────────────────────────

#[test]
fn where_regex_match() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE name ~ '^foo'")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn where_regex_imatch_with_param() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE name ~* $p1")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text())]);
}

#[test]
fn where_regex_not_match() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE name !~ 'bar'")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn where_similar_to() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE name SIMILAR TO 'ab%'")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

// ── Stress ───────────────────────────────────────────────────────────────────

#[test]
fn stress_complex_where_params() {
    let db = setup();
    let sql = "SELECT id FROM users \
               WHERE (name = $p1 OR email = $p2) AND age > $p3";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("id", int8())]);
    assert_params(&info, vec![p(text()), p(text()), p(int4())]);
}
