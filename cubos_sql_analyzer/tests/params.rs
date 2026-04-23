//! Tests for parameter type inference and nullability: annotations, auto-inferred
//! nullability from INSERT/UPDATE target columns, and goal-type-driven inference.

mod common;
use common::*;

// ──────────────────────────────────────────────────────────────────────────────
// Tests: parameter nullability annotations
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn param_not_null_by_default() {
    let db = setup();
    // $p1 has no nullable annotation → NOT NULL.
    // COALESCE(nullable_age, not_null_param) → NOT NULL.
    let sql = "SELECT COALESCE(age, $p1) as val FROM users";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "val").nullable);
}

#[test]
fn param_nullable_annotation() {
    let db = setup();
    // $p? has nullable annotation → nullable.
    // COALESCE(nullable_age, nullable_param) → nullable.
    let sql = "SELECT COALESCE(age, $p?) as val FROM users";
    let info = db.analyze(sql).unwrap();
    assert!(col(&info, "val").nullable);
}

#[test]
fn param_nullable_propagates_to_param_info() {
    let db = setup();
    // $id not nullable (default), $age nullable (?).
    let sql = "SELECT * FROM users WHERE id = $id AND age = $age?";
    let info = db.analyze(sql).unwrap();
    assert!(!info.params[0].nullable);
    assert!(info.params[1].nullable);
}

#[test]
fn param_not_null_in_where_comparison() {
    let db = setup();
    // $p1 not null by default → comparison `id = $p1` has both sides NOT NULL → NOT NULL.
    let sql = "SELECT id = $p1 as is_match FROM users";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "is_match").nullable);
}

#[test]
fn param_nullable_in_where_comparison() {
    let db = setup();
    // $p? is nullable → comparison `id = $p?` has one nullable side → nullable.
    let sql = "SELECT id = $p? as is_match FROM users";
    let info = db.analyze(sql).unwrap();
    assert!(col(&info, "is_match").nullable);
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: auto-inferred param nullability from INSERT/UPDATE target columns
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn param_nullable_inferred_from_update_nullable_column() {
    let db = setup();
    // age is nullable → $p1 should be auto-inferred as nullable.
    let sql = "UPDATE users SET age = $p1 WHERE id = $p2";
    let info = db.analyze(sql).unwrap();
    assert!(
        info.params[0].nullable,
        "age is nullable → $p1 inferred nullable"
    );
    assert!(
        !info.params[1].nullable,
        "id is NOT NULL → $p2 stays non-nullable"
    );
}

#[test]
fn param_not_null_inferred_from_update_not_null_column() {
    let db = setup();
    // name is NOT NULL → $p1 should stay non-nullable.
    let sql = "UPDATE users SET name = $p1 WHERE id = $p2";
    let info = db.analyze(sql).unwrap();
    assert!(
        !info.params[0].nullable,
        "name is NOT NULL → $p1 stays non-nullable"
    );
    assert!(
        !info.params[1].nullable,
        "id is NOT NULL → $p2 stays non-nullable"
    );
}

#[test]
fn param_nullable_inferred_from_update_multiple_columns() {
    let db = setup();
    // SET name (NOT NULL), age (nullable), email (NOT NULL)
    let sql = "UPDATE users SET name = $p1, age = $p2, email = $p3 WHERE id = $p4";
    let info = db.analyze(sql).unwrap();
    assert!(!info.params[0].nullable, "name NOT NULL");
    assert!(info.params[1].nullable, "age nullable");
    assert!(!info.params[2].nullable, "email NOT NULL");
    assert!(!info.params[3].nullable, "id NOT NULL (WHERE)");
}

#[test]
fn param_nullable_inferred_from_insert_values() {
    let db = setup();
    // body is nullable, title is NOT NULL, user_id is NOT NULL
    let sql = "INSERT INTO posts (user_id, title, body) VALUES ($p1, $p2, $p3) RETURNING id";
    let info = db.analyze(sql).unwrap();
    assert!(!info.params[0].nullable, "user_id NOT NULL");
    assert!(!info.params[1].nullable, "title NOT NULL");
    assert!(info.params[2].nullable, "body nullable");
}

#[test]
fn param_nullable_inferred_from_insert_all_nullable() {
    let db = setup();
    // age is nullable
    let sql = "INSERT INTO users (name, email, age) VALUES ($p1, $p2, $p3) RETURNING id";
    let info = db.analyze(sql).unwrap();
    assert!(!info.params[0].nullable, "name NOT NULL");
    assert!(!info.params[1].nullable, "email NOT NULL");
    assert!(info.params[2].nullable, "age nullable");
}

#[test]
fn param_explicit_annotation_overrides_inferred_nullable() {
    let db = setup();
    // age is nullable → auto-inferred as nullable.
    // But explicit annotation `$age!` (not nullable) should override.
    let sql = "UPDATE users SET age = $age! WHERE id = $id";
    let info = db.analyze(sql).unwrap();
    assert!(
        !info.params[0].nullable,
        "explicit non-nullable overrides inferred"
    );
    assert!(!info.params[1].nullable);
}

#[test]
fn param_explicit_nullable_annotation_on_not_null_column() {
    let db = setup();
    // name is NOT NULL, but explicit $name? annotation forces nullable.
    let sql = "UPDATE users SET name = $name? WHERE id = $id";
    let info = db.analyze(sql).unwrap();
    assert!(
        info.params[0].nullable,
        "explicit nullable overrides column NOT NULL"
    );
}

#[test]
fn param_nullable_inferred_from_insert_select() {
    let db = setup();
    // INSERT ... SELECT with params mapped to columns.
    // body is nullable, title is NOT NULL.
    let sql =
        "INSERT INTO posts (user_id, title, body) SELECT $p1, $p2, $p3 FROM users WHERE id = $p4";
    let info = db.analyze(sql).unwrap();
    assert!(!info.params[0].nullable, "user_id NOT NULL");
    assert!(!info.params[1].nullable, "title NOT NULL");
    assert!(info.params[2].nullable, "body nullable");
}

#[test]
fn param_nullable_inferred_update_with_returning() {
    let db = setup();
    // Verify that param inference works alongside RETURNING.
    let sql = "UPDATE posts SET body = $p1, title = $p2 WHERE id = $p3 RETURNING id, body, title";
    let info = db.analyze(sql).unwrap();
    assert!(
        info.params[0].nullable,
        "body nullable → $p1 inferred nullable"
    );
    assert!(
        !info.params[1].nullable,
        "title NOT NULL → $p2 stays non-nullable"
    );
    assert!(!info.params[2].nullable, "id NOT NULL (WHERE)");
    // RETURNING columns
    assert!(!col(&info, "id").nullable);
    assert!(col(&info, "body").nullable);
    assert!(!col(&info, "title").nullable);
}

#[test]
fn param_bang_overrides_inferred_nullable_on_update() {
    let db = setup();
    // age is nullable → auto-inferred as nullable.
    // But $age! (force non-null) should override the inference.
    let sql = "UPDATE users SET age = $age! WHERE id = $id";
    let info = db.analyze(sql).unwrap();
    assert!(
        !info.params[0].nullable,
        "$age! forces non-nullable even though column is nullable"
    );
    assert!(!info.params[1].nullable);
}

#[test]
fn param_bang_overrides_inferred_nullable_on_insert() {
    let db = setup();
    // body is nullable → auto-inferred as nullable.
    // But $body! should override.
    let sql =
        "INSERT INTO posts (user_id, title, body) VALUES ($user_id, $title, $body!) RETURNING id";
    let info = db.analyze(sql).unwrap();
    assert!(!info.params[0].nullable, "user_id NOT NULL, no annotation");
    assert!(!info.params[1].nullable, "title NOT NULL, no annotation");
    assert!(
        !info.params[2].nullable,
        "$body! forces non-nullable even though column is nullable"
    );
}

#[test]
fn param_bang_and_question_mixed_in_update() {
    let db = setup();
    // name is NOT NULL, age is nullable, email is NOT NULL.
    // $name? forces nullable on NOT NULL col, $age! forces non-null on nullable col, $email auto.
    let sql = "UPDATE users SET name = $name?, age = $age!, email = $email WHERE id = $id";
    let info = db.analyze(sql).unwrap();
    assert!(info.params[0].nullable, "$name? forces nullable");
    assert!(!info.params[1].nullable, "$age! forces non-nullable");
    assert!(
        !info.params[2].nullable,
        "$email auto → NOT NULL from column"
    );
    assert!(
        !info.params[3].nullable,
        "id NOT NULL (WHERE, no annotation)"
    );
}

#[test]
fn param_bang_on_insert_select() {
    let db = setup();
    // body is nullable, but $body! forces non-null.
    let sql = "INSERT INTO posts (user_id, title, body) \
               SELECT $user_id, $title, $body! FROM users WHERE id = $id";
    let info = db.analyze(sql).unwrap();
    assert!(!info.params[0].nullable, "user_id NOT NULL, auto");
    assert!(!info.params[1].nullable, "title NOT NULL, auto");
    assert!(!info.params[2].nullable, "$body! forces non-nullable");
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: goal-type-driven parameter inference
// ──────────────────────────────────────────────────────────────────────────────
// These tests validate that parameter types are inferred exactly as PostgreSQL
// infers them (via PREPARE), plus nullability inference from column definitions.

/// Helper: assert that the analyzer produces the expected param types.
fn assert_param_types(db: &Database, sql: &str, expected: &[Type]) {
    let info = db.analyze(sql).unwrap();
    assert_eq!(
        info.params.len(),
        expected.len(),
        "param count mismatch for: {sql}"
    );
    for (i, (p, exp)) in info.params.iter().zip(expected.iter()).enumerate() {
        assert_eq!(&p.pg_type, exp, "param ${} type mismatch for: {sql}", i + 1);
    }
}

// ── LIMIT with bare param ─────────────────────────────────────────────

#[test]
fn goal_limit_bare_param() {
    // LIMIT $p1  →  $p1 must be int8 (bigint), matching PG's transformLimitClause.
    let db = setup();
    let sql = "SELECT id FROM users LIMIT $p1";
    assert_param_types(&db, sql, &[int8()]);
}

#[test]
fn goal_offset_bare_param() {
    // OFFSET $p1  →  $p1 must be int8.
    let db = setup();
    let sql = "SELECT id FROM users OFFSET $p1";
    assert_param_types(&db, sql, &[int8()]);
}

#[test]
fn goal_limit_and_offset_params() {
    // Both LIMIT and OFFSET as params.
    let db = setup();
    let sql = "SELECT id FROM users ORDER BY id LIMIT $p1 OFFSET $p2";
    assert_param_types(&db, sql, &[int8(), int8()]);
}

// ── LIMIT with expression containing param ────────────────────────────

#[test]
fn goal_limit_param_plus_literal() {
    // LIMIT $p1 + 1  →  operator +(unknown, int4) resolves $p1 as int4.
    // The overall result int4 is coerced to int8 (assignment cast exists).
    let db = setup();
    let sql = "SELECT id FROM users LIMIT $p1 + 1";
    assert_param_types(&db, sql, &[int4()]);
}

#[test]
fn goal_limit_function_of_param() {
    // LIMIT length($p1)  →  $p1 must be text (from length(text) signature).
    // length() returns int4, which is coerced to int8 for LIMIT.
    // This is the key test: $p1 should NOT be int8.
    let db = setup();
    let sql = "SELECT id FROM users LIMIT length($p1)";
    assert_param_types(&db, sql, &[text()]);
}

#[test]
fn goal_limit_cast_param() {
    // LIMIT $p1::int4  →  $p1 receives int4 from the cast target.
    let db = setup();
    let sql = "SELECT id FROM users LIMIT $p1::int4";
    assert_param_types(&db, sql, &[int4()]);
}

// ── WHERE with bare param ─────────────────────────────────────────────

#[test]
fn goal_where_bool_param() {
    // WHERE $p1  →  $p1 must be bool (PG's coerce_to_boolean with COERCION_ASSIGNMENT).
    let db = setup();
    let sql = "SELECT id FROM users WHERE $p1";
    assert_param_types(&db, sql, &[bool_ty()]);
}

#[test]
fn goal_where_comparison_infers_column_type() {
    // WHERE id = $p1  →  $p1 gets type of id (int8).
    // WHERE name = $p2  →  $p2 gets type of name (text).
    // WHERE age > $p3  →  $p3 gets type of age (int4).
    let db = setup();
    let sql = "SELECT id FROM users WHERE id = $p1 AND name = $p2 AND age > $p3";
    assert_param_types(&db, sql, &[int8(), text(), int4()]);
}

#[test]
fn goal_where_function_of_param() {
    // WHERE length($p1) > 5  →  $p1 gets text from length(text), not bool from WHERE.
    let db = setup();
    let sql = "SELECT id FROM users WHERE length($p1) > 5";
    assert_param_types(&db, sql, &[text()]);
}

#[test]
fn goal_where_and_or_bool_propagation() {
    // WHERE $p1 AND $p2 OR $p3  →  all must be bool.
    let db = setup();
    let sql = "SELECT id FROM users WHERE $p1 AND $p2 OR $p3";
    assert_param_types(&db, sql, &[bool_ty(), bool_ty(), bool_ty()]);
}

// ── INSERT VALUES with param ──────────────────────────────────────────

#[test]
fn goal_insert_values_params() {
    // INSERT INTO users (name, email, age) VALUES ($p1, $p2, $p3)
    // $p1 → text (name), $p2 → text (email), $p3 → int4 (age).
    let db = setup();
    let sql = "INSERT INTO users (name, email, age) VALUES ($p1, $p2, $p3)";
    assert_param_types(&db, sql, &[text(), text(), int4()]);
}

#[test]
fn goal_insert_values_expression() {
    // INSERT INTO users (name, email, age) VALUES ($p1, $p2, $p3 + 1)
    // $p3 is inferred from operator +(unknown, int4) as int4.
    let db = setup();
    let sql = "INSERT INTO users (name, email, age) VALUES ($p1, $p2, $p3 + 1)";
    assert_param_types(&db, sql, &[text(), text(), int4()]);
}

#[test]
fn goal_insert_nullable_inference() {
    // age is nullable → $p3 should be nullable.
    // name/email are NOT NULL → $p1, $p2 should not be nullable.
    let db = setup();
    let sql = "INSERT INTO users (name, email, age) VALUES ($p1, $p2, $p3)";
    let info = db.analyze(sql).unwrap();
    assert!(!info.params[0].nullable, "name is NOT NULL");
    assert!(!info.params[1].nullable, "email is NOT NULL");
    assert!(info.params[2].nullable, "age is nullable");
}

// ── UPDATE SET with param ─────────────────────────────────────────────

#[test]
fn goal_update_set_params() {
    // UPDATE users SET name = $p1, age = $p2 WHERE id = $p3
    // $p1 → text, $p2 → int4, $p3 → int8.
    let db = setup();
    let sql = "UPDATE users SET name = $p1, age = $p2 WHERE id = $p3";
    assert_param_types(&db, sql, &[text(), int4(), int8()]);
}

#[test]
fn goal_update_set_nullable_inference() {
    // age is nullable → $p2 should be nullable.
    let db = setup();
    let sql = "UPDATE users SET name = $p1, age = $p2 WHERE id = $p3";
    let info = db.analyze(sql).unwrap();
    assert!(!info.params[0].nullable, "name is NOT NULL");
    assert!(info.params[1].nullable, "age is nullable");
    assert!(!info.params[2].nullable, "WHERE id = $p3, not nullable");
}

// ── TypeCast propagation ──────────────────────────────────────────────

#[test]
fn goal_cast_types_param() {
    // $p1::text → $p1 gets text.
    // $p2::int4 → $p2 gets int4.
    let db = setup();
    let sql = "SELECT $p1::text, $p2::int4";
    assert_param_types(&db, sql, &[text(), int4()]);
}

// ── COALESCE param inference ──────────────────────────────────────────

#[test]
fn goal_coalesce_param_with_typed_arg() {
    // COALESCE(age, $p1) → common type is int4, $p1 gets int4.
    let db = setup();
    let sql = "SELECT COALESCE(age, $p1) FROM users";
    assert_param_types(&db, sql, &[int4()]);
}

#[test]
fn goal_coalesce_all_params() {
    // COALESCE($p1, $p2) → both unknown → resolve as text (PG chapter 10.5 rule 3).
    let db = setup();
    let sql = "SELECT COALESCE($p1, $p2)";
    assert_param_types(&db, sql, &[text(), text()]);
}

// ── CASE param inference ──────────────────────────────────────────────

#[test]
fn goal_case_result_param() {
    // CASE WHEN true THEN age ELSE $p1 END → $p1 gets int4 (common type with age).
    let db = setup();
    let sql = "SELECT CASE WHEN true THEN age ELSE $p1 END FROM users";
    assert_param_types(&db, sql, &[int4()]);
}

#[test]
fn goal_case_condition_bool() {
    // CASE WHEN $p1 THEN 1 ELSE 0 END → $p1 must be bool.
    let db = setup();
    let sql = "SELECT CASE WHEN $p1 THEN 1 ELSE 0 END";
    assert_param_types(&db, sql, &[bool_ty()]);
}

// ── Mixed contexts: param used in multiple clauses ────────────────────

#[test]
fn goal_param_in_where_and_limit() {
    // $p1 used in WHERE (via comparison with int8 id), $p2 in LIMIT.
    let db = setup();
    let sql = "SELECT id FROM users WHERE id > $p1 LIMIT $p2";
    assert_param_types(&db, sql, &[int8(), int8()]);
}

#[test]
fn goal_param_insert_with_returning() {
    // Full INSERT ... RETURNING with various param contexts.
    let db = setup();
    let sql = "INSERT INTO posts (user_id, title, body) VALUES ($p1, $p2, $p3) RETURNING id, title";
    assert_param_types(&db, sql, &[int8(), text(), text()]);
}

// ── Operator two-pass: concrete on one side ───────────────────────────

#[test]
fn goal_operator_param_equals_column() {
    // $p1 = name → $p1 gets text.
    let db = setup();
    let sql = "SELECT id FROM users WHERE $p1 = name";
    assert_param_types(&db, sql, &[text()]);
}

#[test]
fn goal_operator_param_both_sides() {
    // $p1 > $p2 with no context — PG defaults both to text.
    let db = setup();
    assert_param_types(&db, "SELECT $p1 > $p2", &[text(), text()]);
}

// ── Nested function/operator param inference ──────────────────────────

#[test]
fn goal_nested_function_in_where() {
    // WHERE upper($p1) = name → $p1 gets text from upper(text).
    let db = setup();
    let sql = "SELECT id FROM users WHERE upper($p1) = name";
    assert_param_types(&db, sql, &[text()]);
}

#[test]
fn goal_concat_param_in_where() {
    // WHERE name = $p1 || $p2 → both get text from || operator.
    let db = setup();
    let sql = "SELECT id FROM users WHERE name = $p1 || $p2";
    assert_param_types(&db, sql, &[text(), text()]);
}

// ── DELETE with params ────────────────────────────────────────────────

#[test]
fn goal_delete_where_param() {
    let db = setup();
    let sql = "DELETE FROM users WHERE id = $p1 RETURNING name";
    assert_param_types(&db, sql, &[int8()]);
}

// ── Multiple value rows in INSERT ─────────────────────────────────────

#[test]
fn goal_insert_multiple_value_rows() {
    // INSERT INTO users (name, email) VALUES ($p1, $p2), ($p3, $p4)
    let db = setup();
    let sql = "INSERT INTO users (name, email) VALUES ($p1, $p2), ($p3, $p4)";
    assert_param_types(&db, sql, &[text(), text(), text(), text()]);
}

// ── Subquery param inference ──────────────────────────────────────────

#[test]
fn goal_subquery_param_in_where() {
    // WHERE id = (SELECT user_id FROM posts WHERE title = $p1 LIMIT 1)
    // $p1 gets text from title column.
    let db = setup();
    let sql =
        "SELECT name FROM users WHERE id = (SELECT user_id FROM posts WHERE title = $p1 LIMIT 1)";
    assert_param_types(&db, sql, &[text()]);
}

// ── Numeric promotion in operator context ─────────────────────────────

#[test]
fn goal_numeric_promotion_int4_with_int8() {
    // WHERE id > $p1 → id is int8, $p1 gets int8.
    // WHERE age > $p2 → age is int4, $p2 gets int4.
    let db = setup();
    let sql = "SELECT id FROM users WHERE id > $p1 AND age > $p2";
    assert_param_types(&db, sql, &[int8(), int4()]);
}

// ── Literal LIMIT/OFFSET (no params, just ensure no error) ───────────

#[test]
fn goal_limit_offset_literals_no_error() {
    let db = setup();
    let sql = "SELECT id FROM users ORDER BY id LIMIT 10 OFFSET 5";
    let info = db.analyze(sql).unwrap();
    assert!(!info.columns.is_empty(), "should have columns");
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: preferred-type tie-break when resolving functions with UNKNOWN args
// ──────────────────────────────────────────────────────────────────────────────
//
// When a call like `fn($param)` has an untyped parameter, PG §10.3 step 4e
// disambiguates overloads by:
//   1. Keeping candidates whose parameter at the UNKNOWN position is in the
//      string category ('S') — untyped literals are assumed to be strings.
//   2. Among those, preferring candidates whose parameter has
//      `typispreferred = true` (e.g. `text` over `bpchar`/`varchar`).
//
// Without this logic the analyzer would depend on the order of overloads in
// the `Vec` — which can shift when pg_proc is re-exported, extensions are
// installed, or the user defines functions in a different order. Each test
// below is constructed to fail if the tie-break is removed: they either
// register overloads in an order that favors the wrong candidate, or assert
// on `pg_type` to distinguish types that share the same Rust mapping.

/// Assert the single param of `sql` resolves to the expected PG type.
fn assert_single_param_ty(db: &Database, sql: &str, expected: Type) {
    let info = db.analyze(sql).unwrap();
    assert_eq!(info.params.len(), 1, "expected exactly one param in: {sql}");
    assert_eq!(
        info.params[0].pg_type, expected,
        "param PG type mismatch for: {sql}"
    );
}

#[test]
fn preferred_type_text_wins_over_bytea_in_reverse_registration_order() {
    // bytea is category 'U' and text is the preferred type of category 'S'.
    // Registering bytea first puts it at Vec[0]; without the tie-break,
    // `pick($p)` would choose bytea and `$p` would become `Vec<u8>`.
    let mut db = Database::new();
    db.apply_sql(
        "CREATE FUNCTION public.pick(bytea) RETURNS int
             AS 'SELECT 1' LANGUAGE sql;
         CREATE FUNCTION public.pick(text) RETURNS int
             AS 'SELECT 1' LANGUAGE sql;",
    )
    .unwrap();
    assert_single_param_ty(&db, "SELECT pick($p)", text());
}

#[test]
fn preferred_type_text_wins_over_bpchar_in_reverse_registration_order() {
    // Both `bpchar` and `text` are in category 'S', but only `text` has
    // `typispreferred = true`. Registering bpchar first puts it at Vec[0];
    // without the tie-break we'd return bpchar (OID 1042) — even though
    // both map to `String` in Rust, the chosen PG OID differs and drives
    // downstream behavior (e.g. generated `::text` casts).
    let mut db = Database::new();
    db.apply_sql(
        "CREATE FUNCTION public.pick(bpchar) RETURNS int
             AS 'SELECT 1' LANGUAGE sql;
         CREATE FUNCTION public.pick(text) RETURNS int
             AS 'SELECT 1' LANGUAGE sql;",
    )
    .unwrap();
    assert_single_param_ty(&db, "SELECT pick($p)", text());
}

#[test]
fn preferred_type_text_wins_over_varchar_in_reverse_registration_order() {
    // `varchar` and `text` share category 'S' and Rust type `String`; only
    // `text` is preferred. Without the tie-break, varchar would win since
    // it is registered first.
    let mut db = Database::new();
    db.apply_sql(
        "CREATE FUNCTION public.pick(varchar) RETURNS int
             AS 'SELECT 1' LANGUAGE sql;
         CREATE FUNCTION public.pick(text) RETURNS int
             AS 'SELECT 1' LANGUAGE sql;",
    )
    .unwrap();
    assert_single_param_ty(&db, "SELECT pick($p)", text());
}

#[test]
fn preferred_type_ignores_non_string_category_when_only_non_preferred_in_string() {
    // When bytea and bpchar are the only candidates, bpchar wins because
    // it is the only string-category (even though it is NOT preferred).
    // This exercises step 1 of the tie-break (category filter) independent
    // of step 2 (preferred-type filter).
    let mut db = Database::new();
    db.apply_sql(
        "CREATE FUNCTION public.pick(bytea) RETURNS int
             AS 'SELECT 1' LANGUAGE sql;
         CREATE FUNCTION public.pick(bpchar) RETURNS int
             AS 'SELECT 1' LANGUAGE sql;",
    )
    .unwrap();
    // With bytea at Vec[0] and no string-category filtering, a naive pick
    // would return bytea. The filter keeps only bpchar.
    assert_single_param_ty(&db, "SELECT pick($p)", bpchar());
}
