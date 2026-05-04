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
fn numeric_plus_int_returns_numeric() {
    let db = setup();
    // `numeric + int4` must resolve to `numeric + numeric → numeric`
    // (PG §10.2 step 3c — most exact matches wins). The alternative
    // `float4 + float4` is reachable via implicit casts but scores lower
    // because neither side matches exactly, so it would silently narrow
    // money-style computations.
    let s = db
        .analyze("SELECT SUM(id)::numeric + 1 AS r FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("r", numeric())]);
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

// ── NULLIF ───────────────────────────────────────────────────────────────────

#[test]
fn nullif_returns_first_arg_type() {
    let db = setup();
    // Result type is the first arg's type (NOT bool — NULLIF wraps the `=`
    // operator but projects the first operand back on the non-match branch).
    // Always nullable (NULL when args are equal).
    let s = db
        .analyze("SELECT NULLIF(age, 0) AS maybe_age FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("maybe_age", int4())]);
}

#[test]
fn nullif_on_not_null_column_is_nullable() {
    let db = setup();
    // Even on a NOT NULL column, NULLIF can produce NULL (when args match).
    let s = db
        .analyze("SELECT NULLIF(name, 'admin') AS maybe_name FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("maybe_name", text())]);
}

#[test]
fn nullif_with_param_inherits_type() {
    let db = setup();
    // `$p1` gets typed from the first arg via the implicit goal.
    let s = db
        .analyze("SELECT NULLIF(age, $p1) AS maybe_age FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("maybe_age", int4())]);
    assert_params(&s, vec![p(int4())]);
}

#[test]
fn nullif_incompatible_concrete_types_rejected() {
    // `int = text` has no operator — PG dispatches to the `=` operator
    // resolver and errors with `operator does not exist: integer = text`.
    // The analyzer rejects via the generic coerce check first, so the
    // wording diverges from PG. Opt out of the mirror.
    let mut db = setup();
    db.skip_pg_sanity();
    assert_analyze_err!(
        db.analyze("SELECT NULLIF(age, 'x'::text) FROM users"),
        AnalyzeError::TypeMismatch { .. },
        "text",
    );
}

#[test]
fn nullif_int_with_string_literal_rejected() {
    // Bare string literal — PG raises a runtime cast error here
    // (`invalid input syntax for type integer`), the analyzer catches it
    // statically with a NULLIF-types message.
    let mut db = setup();
    db.skip_pg_sanity();
    assert_analyze_err!(
        db.analyze("SELECT NULLIF(age, 'x') FROM users"),
        AnalyzeError::TypeMismatch { .. },
        "NULLIF",
    );
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
    // CASE with one NOT NULL branch and one nullable branch. Qualify `id`
    // explicitly — both `users.id` and `posts.id` exist, so a bare `id` is
    // ambiguous (PG SQLSTATE 42702). The point of this test is the
    // nullability merge, not column resolution.
    let sql = "SELECT CASE WHEN u.id > 0 THEN name ELSE body END as val \
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

// ── NULL literals ───────────────────────────────────────────────────────────

#[test]
fn null_at_top_level_coerces_to_text() {
    let db = setup();
    // PG coerces unresolved UNKNOWN output columns to text before sending
    // them over the wire. A bare `NULL` surfaces as `text` nullable.
    let s = db.analyze("SELECT NULL AS x").unwrap();
    assert_cols(&s, vec![cn("x", text())]);
}

#[test]
fn null_eq_null_is_nullable_bool() {
    let db = setup();
    // `NULL = NULL` is NULL (not TRUE) — result is `bool` nullable.
    let s = db.analyze("SELECT NULL = NULL AS x").unwrap();
    assert_cols(&s, vec![cn("x", bool_ty())]);
}

#[test]
fn null_text_concat_propagates_null() {
    let db = setup();
    // Strict `||`: NULL on the left makes the whole concat nullable.
    let s = db.analyze("SELECT NULL::text || 'x' AS y").unwrap();
    assert_cols(&s, vec![cn("y", text())]);
}

// ── Schema-qualified function call ──────────────────────────────────────────

#[test]
fn schema_qualified_function_call() {
    let db = setup();
    // `pg_catalog.now()` should resolve the same as `now()`.
    let s = db.analyze("SELECT pg_catalog.now() AS ts").unwrap();
    assert_cols(&s, vec![c("ts", timestamptz())]);
}

// ── Nullability of strict comparisons ────────────────────────────────────────
//
// Comparison operators (`=`, `<`, `<>`, …) are strict — any NULL operand
// makes the result NULL. The analyzer tracks this through the usual
// `any_arg_nullable` path.

#[test]
fn comparison_both_sides_not_null() {
    let db = setup();
    let s = db
        .analyze("SELECT id = user_id AS same FROM posts")
        .unwrap();
    assert_cols(&s, vec![c("same", bool_ty())]);
}

#[test]
fn comparison_with_nullable_column() {
    let db = setup();
    // `age` (nullable) `= 18` → bool but nullable.
    let s = db.analyze("SELECT age = 18 AS adult FROM users").unwrap();
    assert_cols(&s, vec![cn("adult", bool_ty())]);
}

#[test]
fn comparison_with_nullable_both_sides() {
    let db = setup();
    let s = db
        .analyze("SELECT p.body = u.name AS match FROM posts p JOIN users u ON u.id = p.user_id")
        .unwrap();
    // `body` is nullable, `name` is NOT NULL — result nullable (any-nullable).
    assert_cols(&s, vec![cn("match", bool_ty())]);
}

// ── CASE / COALESCE branch-type validation ──────────────────────────────────

#[test]
fn case_with_incompatible_concrete_arms_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT CASE WHEN true THEN 1 ELSE 'x'::text END"),
        AnalyzeError::Invalid(_),
        "CASE types text and integer cannot be matched",
    );
}

#[test]
fn case_with_incompatible_unknown_literal_rejected() {
    // Bare string literal vs int — PG resolves the unknown literal toward
    // the int4 candidate and tries a runtime cast (`invalid input syntax
    // for type integer`), which is a different error from our compile-time
    // CASE mismatch. Opt out of the pglite mirror.
    let mut db = setup();
    db.skip_pg_sanity();
    assert_analyze_err!(
        db.analyze("SELECT CASE WHEN true THEN 1 ELSE 'x' END"),
        AnalyzeError::Invalid(_),
        "CASE types text and integer cannot be matched",
    );
}

#[test]
fn coalesce_with_incompatible_concrete_arms_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT COALESCE(1, 'x'::text)"),
        AnalyzeError::Invalid(_),
        "COALESCE types integer and text cannot be matched",
    );
}

#[test]
fn coalesce_with_incompatible_unknown_literal_rejected() {
    // Bare string literal vs int — PG raises a runtime cast error
    // (`invalid input syntax for integer`) rather than the analyzer's
    // compile-time COALESCE-types mismatch. Opt out of the pglite mirror.
    let mut db = setup();
    db.skip_pg_sanity();
    assert_analyze_err!(
        db.analyze("SELECT COALESCE(1, 'x')"),
        AnalyzeError::Invalid(_),
        "COALESCE types integer and text cannot be matched",
    );
}

// ── GREATEST / LEAST (non-strict minmax) ────────────────────────────────────

#[test]
fn greatest_of_all_null_typed_args() {
    let db = setup();
    // `GREATEST(NULL::int4, NULL::int4)` returns NULL typed int4 in PG —
    // the analyzer resolves the common arg type and marks the result
    // nullable because every arg is nullable.
    let s = db
        .analyze("SELECT GREATEST(NULL::int4, NULL::int4) AS g")
        .unwrap();
    assert_cols(&s, vec![cn("g", int4())]);
}

#[test]
fn least_of_mixed_nullable_args_keeps_not_null() {
    let db = setup();
    // `LEAST(nullable, non-null)` — GREATEST/LEAST skip NULLs at runtime,
    // so with at least one NOT NULL arg the result is NOT NULL (stricter
    // than PG's statement-level nullability, which just tracks types).
    let s = db.analyze("SELECT LEAST(age, id) AS m FROM users").unwrap();
    assert_cols(&s, vec![c("m", int8())]);
}

#[test]
fn greatest_over_int4_and_int8_promotes_to_int8() {
    let db = setup();
    // Common-type resolution promotes int4 + int8 → int8. `age` is nullable
    // but `id` is NOT NULL, so GREATEST's "skip NULLs" semantics guarantee
    // a non-null result.
    let s = db
        .analyze("SELECT GREATEST(age, id) AS g FROM users")
        .unwrap();
    assert_cols(&s, vec![c("g", int8())]);
}

#[test]
fn greatest_all_not_null_args() {
    let db = setup();
    let s = db
        .analyze("SELECT GREATEST(id, 1::int8) AS g FROM users")
        .unwrap();
    assert_cols(&s, vec![c("g", int8())]);
}

// ── ROW constructor ─────────────────────────────────────────────────────────

#[test]
fn row_comparison_returns_bool() {
    let db = setup();
    // `ROW(...)` builds an anonymous composite; the `record = record`
    // operator compares element-wise and returns bool.
    let s = db.analyze("SELECT ROW(1, 2) = ROW(1, 2) AS e").unwrap();
    assert_cols(&s, vec![c("e", bool_ty())]);
}

#[test]
fn row_from_columns_comparison() {
    let db = setup();
    // ROW wrappers are never NULL themselves, and `record = record` is
    // strict — since `id`/`name` are NOT NULL and the RHS uses literals,
    // the result is NOT NULL.
    let s = db
        .analyze("SELECT ROW(id, name) = ROW(1::int8, 'x') AS e FROM users")
        .unwrap();
    assert_cols(&s, vec![c("e", bool_ty())]);
}

// ── Interval / date arithmetic ───────────────────────────────────────────────

#[test]
fn timestamptz_plus_interval() {
    let db = setup();
    // `now()` is NOT NULL and `INTERVAL 'n'` is a constant, so the sum
    // stays NOT NULL.
    let s = db
        .analyze("SELECT now() + INTERVAL '1 day' AS later")
        .unwrap();
    assert_cols(&s, vec![c("later", timestamptz())]);
}

#[test]
fn age_between_two_timestamps() {
    let db = setup();
    let s = db.analyze("SELECT age(now(), now()) AS delta").unwrap();
    assert_cols(&s, vec![c("delta", interval())]);
}

#[test]
fn extract_year_from_now() {
    let db = setup();
    // EXTRACT returns numeric (PG14+) — independent of whether the source
    // field is nullable.
    let s = db.analyze("SELECT EXTRACT(YEAR FROM now()) AS y").unwrap();
    assert_cols(&s, vec![c("y", numeric())]);
}

#[test]
fn date_trunc_on_timestamptz() {
    let db = setup();
    let s = db.analyze("SELECT date_trunc('day', now()) AS d").unwrap();
    assert_cols(&s, vec![c("d", timestamptz())]);
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
fn concat_ws_with_null_separator_is_nullable() {
    let db = setup();
    // `concat_ws(sep, …)` skips NULL items, but a NULL separator makes the
    // entire result NULL — the variadic part is non-strict, the separator
    // arg is not.
    let sql = "SELECT concat_ws(NULL::text, 'a', 'b') AS c";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![cn("c", text())]);
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
