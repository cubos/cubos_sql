//! Scalar expressions: literals, arithmetic, boolean, concat, CASE,
//! COALESCE, NULLIF, strict vs non-strict operators, IS [NOT] NULL,
//! BETWEEN.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE users (
            id    BIGINT PRIMARY KEY,
            name  TEXT NOT NULL,
            email TEXT NOT NULL,
            age   INT
         );
         CREATE TABLE posts (
            id      BIGINT PRIMARY KEY,
            user_id BIGINT NOT NULL,
            title   TEXT NOT NULL,
            body    TEXT
         );",
    )
    .unwrap();
    db
}

// ── Literals ─────────────────────────────────────────────────────────────────

#[test]
fn types_match_integer_literal() {
    let db = setup();
    let s = db.analyze("SELECT 42 AS val").unwrap();
    assert_cols(&s, vec![c("val", int4())]);
}

#[test]
fn types_match_boolean_literal() {
    let db = setup();
    let s = db.analyze("SELECT true AS flag, false AS other").unwrap();
    assert_cols(&s, vec![c("flag", bool_ty()), c("other", bool_ty())]);
}

#[test]
fn literal_not_null() {
    let db = setup();
    let sql = "SELECT id, 'constant' as label FROM users";
    let info = db.analyze(sql).unwrap();
    // PG coerces any `unknown`-typed output column to `text` before sending
    // it to the client, so the bare string literal surfaces as `text`.
    assert_cols(&info, vec![c("id", int8()), c("label", text())]);
}

// ── Arithmetic / operators ───────────────────────────────────────────────────

#[test]
fn types_match_arithmetic() {
    let db = setup();
    let s = db
        .analyze("SELECT id + 1 AS next_id, age * 2 AS double_age FROM users")
        .unwrap();
    assert_cols(&s, vec![c("next_id", int8()), cn("double_age", int4())]);
}

#[test]
fn types_match_string_concat() {
    let db = setup();
    let s = db
        .analyze("SELECT name || ' <' || email || '>' AS display FROM users")
        .unwrap();
    assert_cols(&s, vec![c("display", text())]);
}

#[test]
fn complex_arithmetic_on_nullable() {
    let db = setup();
    // age is nullable → age + 1 nullable.
    let sql = "SELECT id, age + 1 as age_plus_one FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("id", int8()), cn("age_plus_one", int4())]);
}

#[test]
fn complex_arithmetic_on_not_null() {
    let db = setup();
    // id is NOT NULL → id + 1 also NOT NULL.
    let sql = "SELECT id + 1 as next_id FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("next_id", int8())]);
}

#[test]
fn complex_coalesce_in_arithmetic() {
    let db = setup();
    // COALESCE(age, 0) is NOT NULL → adding 10 stays NOT NULL.
    let sql = "SELECT COALESCE(age, 0) + 10 as safe_age_plus FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("safe_age_plus", int4())]);
}

// ── Built-in strict / non-strict functions ───────────────────────────────────

#[test]
fn types_match_upper_lower() {
    let db = setup();
    let s = db
        .analyze("SELECT upper(name) AS up, lower(email) AS lo FROM users")
        .unwrap();
    assert_cols(&s, vec![c("up", text()), c("lo", text())]);
}

#[test]
fn types_match_length() {
    let db = setup();
    let s = db.analyze("SELECT length(name) AS len FROM users").unwrap();
    assert_cols(&s, vec![c("len", int4())]);
}

#[test]
fn types_match_coalesce_with_literal() {
    let db = setup();
    let s = db
        .analyze("SELECT COALESCE(age, 0) AS age_or_zero FROM users")
        .unwrap();
    assert_cols(&s, vec![c("age_or_zero", int4())]);
}

#[test]
fn types_match_now() {
    let db = setup();
    let s = db.analyze("SELECT now() AS ts").unwrap();
    assert_cols(&s, vec![c("ts", timestamptz())]);
}

#[test]
fn coalesce_not_null() {
    let db = setup();
    let sql = "SELECT COALESCE(age, 0) as safe_age FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("safe_age", int4())]);
}

// ── CASE ─────────────────────────────────────────────────────────────────────

#[test]
fn types_match_case_with_else() {
    let db = setup();
    let s = db
        .analyze("SELECT CASE WHEN age > 18 THEN 'adult' ELSE 'minor' END AS category FROM users")
        .unwrap();
    assert_cols(&s, vec![c("category", text())]);
}

#[test]
fn types_match_case_expression() {
    let db = setup();
    let s = db
        .analyze("SELECT CASE WHEN age IS NULL THEN 0 ELSE age END AS safe_age FROM users")
        .unwrap();
    // PG control-flow narrowing would make this NOT NULL (ELSE branch only
    // reached when age IS NOT NULL), but the analyzer does not currently
    // infer that — it takes the least-common nullability across branches.
    assert_cols(&s, vec![cn("safe_age", int4())]);
}

#[test]
fn case_with_else_not_null() {
    let db = setup();
    let sql = "SELECT CASE WHEN age > 18 THEN 'adult' ELSE 'minor' END as category FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("category", text())]);
}

#[test]
fn case_without_else_is_nullable() {
    let db = setup();
    let sql = "SELECT CASE WHEN age > 18 THEN 'adult' END as category FROM users";
    let info = db.analyze(sql).unwrap();
    // CASE without ELSE is nullable because there's no ELSE branch.
    assert_cols(&info, vec![cn("category", text())]);
}

// ── Boolean / NULL tests ─────────────────────────────────────────────────────

#[test]
fn types_match_null_test() {
    let db = setup();
    let s = db
        .analyze("SELECT id, age IS NULL AS is_null, age IS NOT NULL AS is_not_null FROM users")
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("id", int8()),
            c("is_null", bool_ty()),
            c("is_not_null", bool_ty()),
        ],
    );
}

#[test]
fn types_match_boolean_test() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT (age > 18) IS TRUE AS adult, (age > 18) IS NOT TRUE AS not_adult FROM users",
        )
        .unwrap();
    assert_cols(&s, vec![c("adult", bool_ty()), c("not_adult", bool_ty())]);
}

#[test]
fn complex_boolean_with_nullable_input() {
    let db = setup();
    // age IS NOT NULL → bool, NOT NULL. age > 18 → bool, nullable (age can be NULL).
    let sql = "SELECT age IS NOT NULL as has_age, age > 18 as is_adult FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
        vec![c("has_age", bool_ty()), cn("is_adult", bool_ty())],
    );
}

// ── Stress: nested COALESCE / CASE / expressions ─────────────────────────────

#[test]
fn stress_nested_coalesce() {
    let db = setup();
    // COALESCE(COALESCE(nullable, nullable), literal) → NOT NULL.
    let sql = "SELECT COALESCE(COALESCE(age, age), 0) as val FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("val", int4())]);
}

#[test]
fn stress_coalesce_all_nullable() {
    let db = setup();
    // COALESCE(nullable, nullable) → still nullable (no non-null fallback).
    let sql = "SELECT COALESCE(age, age) as val FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![cn("val", int4())]);
}

#[test]
fn stress_case_with_null_branch() {
    let db = setup();
    // CASE with one branch returning NULL explicitly.
    let sql = "SELECT CASE WHEN age > 18 THEN name ELSE NULL END as val FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![cn("val", text())]);
}

#[test]
fn stress_case_mixing_nullable_branches() {
    let db = setup();
    // CASE with one NOT NULL branch and one nullable branch.
    let sql = "SELECT CASE WHEN id > 0 THEN name ELSE body END as val \
               FROM users u INNER JOIN posts p ON p.user_id = u.id";
    let info = db.analyze(sql).unwrap();
    // name is NOT NULL but body is nullable → result is nullable.
    assert_cols(&info, vec![cn("val", text())]);
}

// ── Torture ──────────────────────────────────────────────────────────────────

#[test]
fn torture_nested_case_in_coalesce() {
    let db = setup();
    // COALESCE(CASE without ELSE, literal) → NOT NULL.
    let sql = "SELECT COALESCE( \
                   CASE WHEN age > 18 THEN age END, \
                   0 \
               ) as val FROM users";
    let info = db.analyze(sql).unwrap();
    // CASE without ELSE is nullable, but COALESCE with 0 fallback makes it NOT NULL.
    assert_cols(&info, vec![c("val", int4())]);
}

// ── Strict pg_catalog functions ──────────────────────────────────────────────

#[test]
fn strict_pg_catalog_function_not_null() {
    let db = setup();
    // length(text) is pg_catalog, strict, not in exceptions → NOT NULL with NOT NULL input.
    let sql = "SELECT length(name) as len FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("len", int4())]);
}

#[test]
fn strict_pg_catalog_function_nullable_with_nullable_arg() {
    let db = setup();
    // length(text) is strict: nullable input → nullable output.
    let sql = "SELECT length(body) as len FROM posts";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![cn("len", int4())]);
}

#[test]
fn strict_pg_catalog_upper_not_null() {
    let db = setup();
    // upper(text) is pg_catalog, strict → NOT NULL with NOT NULL input.
    let sql = "SELECT upper(name) as uname FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("uname", text())]);
}

// ── Operators: + / ‖ strictness ──────────────────────────────────────────────

#[test]
fn operator_plus_not_null() {
    let db = setup();
    // 1 + 1: both non-null, operator not in exceptions → NOT NULL.
    let sql = "SELECT 1 + 1 as result";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("result", int4())]);
}

#[test]
fn operator_plus_nullable_arg() {
    let db = setup();
    // age is nullable → result is nullable.
    let sql = "SELECT age + 1 as next_age FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![cn("next_age", int4())]);
}

#[test]
fn operator_concat_not_null() {
    let db = setup();
    // || with two NOT NULL → NOT NULL.
    let sql = "SELECT name || ' <' || email || '>' as display FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("display", text())]);
}

#[test]
fn operator_concat_nullable_arg() {
    let db = setup();
    // body is nullable → concat is nullable.
    let sql = "SELECT title || body as combined FROM posts";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![cn("combined", text())]);
}

// ── Non-strict pg_catalog functions that never return NULL ───────────────────

#[test]
fn nonstrict_concat_never_null() {
    let db = setup();
    // concat is non-strict but never returns NULL (treats NULLs as '').
    let sql = "SELECT concat(p.title, ' ', p.body) as full_text FROM posts p";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("full_text", text())]);
}

#[test]
fn nonstrict_concat_ws_never_null() {
    let db = setup();
    let sql = "SELECT concat_ws(', '::text, name, email) as combined FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("combined", text())]);
}

#[test]
fn nonstrict_now_never_null() {
    let db = setup();
    let sql = "SELECT now() as ts";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("ts", timestamptz())]);
}

#[test]
fn nonstrict_random_never_null() {
    let db = setup();
    let sql = "SELECT random() as r";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("r", float8())]);
}
