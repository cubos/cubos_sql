//! Tests for parameter type inference and nullability: annotations, auto-inferred
//! nullability from INSERT/UPDATE target columns, and goal-type-driven inference.

mod common;
use common::*;

// ──────────────────────────────────────────────────────────────────────────────
// Tests: parameter nullability annotations
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn param_not_null_by_default() {
    let snapshot = setup();
    // $1 has no nullable annotation → NOT NULL.
    // COALESCE(nullable_age, not_null_param) → NOT NULL.
    let sql = "SELECT COALESCE(age, $1) as val FROM users";
    let info = analyze(&snapshot, sql, &default_config()).unwrap();
    assert!(!col(&info, "val").nullable);
}

#[test]
fn param_nullable_annotation() {
    let snapshot = setup();
    // $1 has nullable annotation → nullable.
    // COALESCE(nullable_age, nullable_param) → nullable.
    let sql = "SELECT COALESCE(age, $1) as val FROM users";
    let config = config_with_nullable(&[Some(true)]);
    let info = analyze(&snapshot, sql, &config).unwrap();
    assert!(col(&info, "val").nullable);
}

#[test]
fn param_nullable_propagates_to_param_info() {
    let snapshot = setup();
    let sql = "SELECT * FROM users WHERE id = $1 AND age = $2";
    // $1 not nullable, $2 nullable
    let config = config_with_nullable(&[Some(false), Some(true)]);
    let info = analyze(&snapshot, sql, &config).unwrap();
    assert!(!info.params[0].nullable);
    assert!(info.params[1].nullable);
}

#[test]
fn param_not_null_in_where_comparison() {
    let snapshot = setup();
    // $1 not null by default → comparison `id = $1` has both sides NOT NULL → NOT NULL.
    let sql = "SELECT id = $1 as is_match FROM users";
    let info = analyze(&snapshot, sql, &default_config()).unwrap();
    assert!(!col(&info, "is_match").nullable);
}

#[test]
fn param_nullable_in_where_comparison() {
    let snapshot = setup();
    // $1 nullable → comparison `id = $1?` has one nullable side → nullable.
    let sql = "SELECT id = $1 as is_match FROM users";
    let config = config_with_nullable(&[Some(true)]);
    let info = analyze(&snapshot, sql, &config).unwrap();
    assert!(col(&info, "is_match").nullable);
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: auto-inferred param nullability from INSERT/UPDATE target columns
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn param_nullable_inferred_from_update_nullable_column() {
    let snapshot = setup();
    // age is nullable → $1 should be auto-inferred as nullable.
    let sql = "UPDATE users SET age = $1 WHERE id = $2";
    let info = analyze(&snapshot, sql, &default_config()).unwrap();
    assert!(
        info.params[0].nullable,
        "age is nullable → $1 inferred nullable"
    );
    assert!(
        !info.params[1].nullable,
        "id is NOT NULL → $2 stays non-nullable"
    );
}

#[test]
fn param_not_null_inferred_from_update_not_null_column() {
    let snapshot = setup();
    // name is NOT NULL → $1 should stay non-nullable.
    let sql = "UPDATE users SET name = $1 WHERE id = $2";
    let info = analyze(&snapshot, sql, &default_config()).unwrap();
    assert!(
        !info.params[0].nullable,
        "name is NOT NULL → $1 stays non-nullable"
    );
    assert!(
        !info.params[1].nullable,
        "id is NOT NULL → $2 stays non-nullable"
    );
}

#[test]
fn param_nullable_inferred_from_update_multiple_columns() {
    let snapshot = setup();
    // SET name (NOT NULL), age (nullable), email (NOT NULL)
    let sql = "UPDATE users SET name = $1, age = $2, email = $3 WHERE id = $4";
    let info = analyze(&snapshot, sql, &default_config()).unwrap();
    assert!(!info.params[0].nullable, "name NOT NULL");
    assert!(info.params[1].nullable, "age nullable");
    assert!(!info.params[2].nullable, "email NOT NULL");
    assert!(!info.params[3].nullable, "id NOT NULL (WHERE)");
}

#[test]
fn param_nullable_inferred_from_insert_values() {
    let snapshot = setup();
    // body is nullable, title is NOT NULL, user_id is NOT NULL
    let sql = "INSERT INTO posts (user_id, title, body) VALUES ($1, $2, $3) RETURNING id";
    let info = analyze(&snapshot, sql, &default_config()).unwrap();
    assert!(!info.params[0].nullable, "user_id NOT NULL");
    assert!(!info.params[1].nullable, "title NOT NULL");
    assert!(info.params[2].nullable, "body nullable");
}

#[test]
fn param_nullable_inferred_from_insert_all_nullable() {
    let snapshot = setup();
    // age is nullable
    let sql = "INSERT INTO users (name, email, age) VALUES ($1, $2, $3) RETURNING id";
    let info = analyze(&snapshot, sql, &default_config()).unwrap();
    assert!(!info.params[0].nullable, "name NOT NULL");
    assert!(!info.params[1].nullable, "email NOT NULL");
    assert!(info.params[2].nullable, "age nullable");
}

#[test]
fn param_explicit_annotation_overrides_inferred_nullable() {
    let snapshot = setup();
    // age is nullable → auto-inferred as nullable.
    // But explicit annotation `$1!` (not nullable) from config should override.
    let sql = "UPDATE users SET age = $1 WHERE id = $2";
    let config = config_with_nullable(&[Some(false), Some(false)]);
    let info = analyze(&snapshot, sql, &config).unwrap();
    assert!(
        !info.params[0].nullable,
        "explicit non-nullable overrides inferred"
    );
    assert!(!info.params[1].nullable);
}

#[test]
fn param_explicit_nullable_annotation_on_not_null_column() {
    let snapshot = setup();
    // name is NOT NULL, but explicit $1? annotation forces nullable.
    let sql = "UPDATE users SET name = $1 WHERE id = $2";
    let config = config_with_nullable(&[Some(true)]);
    let info = analyze(&snapshot, sql, &config).unwrap();
    assert!(
        info.params[0].nullable,
        "explicit nullable overrides column NOT NULL"
    );
}

#[test]
fn param_nullable_inferred_from_insert_select() {
    let snapshot = setup();
    // INSERT ... SELECT with params mapped to columns.
    // body is nullable, title is NOT NULL.
    let sql = "INSERT INTO posts (user_id, title, body) SELECT $1, $2, $3 FROM users WHERE id = $4";
    let info = analyze(&snapshot, sql, &default_config()).unwrap();
    assert!(!info.params[0].nullable, "user_id NOT NULL");
    assert!(!info.params[1].nullable, "title NOT NULL");
    assert!(info.params[2].nullable, "body nullable");
}

#[test]
fn param_nullable_inferred_update_with_returning() {
    let snapshot = setup();
    // Verify that param inference works alongside RETURNING.
    let sql = "UPDATE posts SET body = $1, title = $2 WHERE id = $3 RETURNING id, body, title";
    let info = analyze(&snapshot, sql, &default_config()).unwrap();
    assert!(
        info.params[0].nullable,
        "body nullable → $1 inferred nullable"
    );
    assert!(
        !info.params[1].nullable,
        "title NOT NULL → $2 stays non-nullable"
    );
    assert!(!info.params[2].nullable, "id NOT NULL (WHERE)");
    // RETURNING columns
    assert!(!col(&info, "id").nullable);
    assert!(col(&info, "body").nullable);
    assert!(!col(&info, "title").nullable);
}

#[test]
fn param_bang_overrides_inferred_nullable_on_update() {
    let snapshot = setup();
    // age is nullable → auto-inferred as nullable.
    // But $1! (force non-null) should override the inference.
    let sql = "UPDATE users SET age = $1 WHERE id = $2";
    let config = config_with_nullable(&[Some(false)]); // $1!
    let info = analyze(&snapshot, sql, &config).unwrap();
    assert!(
        !info.params[0].nullable,
        "$age! forces non-nullable even though column is nullable"
    );
    assert!(!info.params[1].nullable);
}

#[test]
fn param_bang_overrides_inferred_nullable_on_insert() {
    let snapshot = setup();
    // body is nullable → auto-inferred as nullable.
    // But $3! should override.
    let sql = "INSERT INTO posts (user_id, title, body) VALUES ($1, $2, $3) RETURNING id";
    let config = config_with_nullable(&[None, None, Some(false)]); // only $3!
    let info = analyze(&snapshot, sql, &config).unwrap();
    assert!(!info.params[0].nullable, "user_id NOT NULL, no annotation");
    assert!(!info.params[1].nullable, "title NOT NULL, no annotation");
    assert!(
        !info.params[2].nullable,
        "$body! forces non-nullable even though column is nullable"
    );
}

#[test]
fn param_bang_and_question_mixed_in_update() {
    let snapshot = setup();
    // name is NOT NULL, age is nullable, email is NOT NULL.
    // $1? forces nullable on NOT NULL col, $2! forces non-null on nullable col, $3 auto.
    let sql = "UPDATE users SET name = $1, age = $2, email = $3 WHERE id = $4";
    let config = config_with_nullable(&[Some(true), Some(false), None]);
    let info = analyze(&snapshot, sql, &config).unwrap();
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
    let snapshot = setup();
    // body is nullable, but $3! forces non-null.
    let sql = "INSERT INTO posts (user_id, title, body) SELECT $1, $2, $3 FROM users WHERE id = $4";
    let config = config_with_nullable(&[None, None, Some(false)]);
    let info = analyze(&snapshot, sql, &config).unwrap();
    assert!(!info.params[0].nullable, "user_id NOT NULL, auto");
    assert!(!info.params[1].nullable, "title NOT NULL, auto");
    assert!(!info.params[2].nullable, "$body! forces non-nullable");
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: goal-type-driven parameter inference
// ──────────────────────────────────────────────────────────────────────────────
// These tests validate that parameter types are inferred exactly as PostgreSQL
// infers them (via PREPARE), plus nullability inference from column definitions.

/// Helper: assert that the analyzer produces the expected param Rust types.
fn assert_param_types(snapshot: &SchemaSnapshot, sql: &str, expected: &[&str]) {
    let info = analyze(snapshot, sql, &default_config()).unwrap();
    assert_eq!(
        info.params.len(),
        expected.len(),
        "param count mismatch for: {sql}"
    );
    for (i, (p, &exp)) in info.params.iter().zip(expected.iter()).enumerate() {
        assert_eq!(
            p.rust_type,
            exp,
            "param ${} type mismatch for: {sql}",
            i + 1
        );
    }
}

// ── LIMIT with bare param ─────────────────────────────────────────────

#[test]
fn goal_limit_bare_param() {
    // LIMIT $1  →  $1 must be int8 (bigint), matching PG's transformLimitClause.
    let snapshot = setup();
    let sql = "SELECT id FROM users LIMIT $1";
    assert_param_types(&snapshot, sql, &["i64"]);
}

#[test]
fn goal_offset_bare_param() {
    // OFFSET $1  →  $1 must be int8.
    let snapshot = setup();
    let sql = "SELECT id FROM users OFFSET $1";
    assert_param_types(&snapshot, sql, &["i64"]);
}

#[test]
fn goal_limit_and_offset_params() {
    // Both LIMIT and OFFSET as params.
    let snapshot = setup();
    let sql = "SELECT id FROM users ORDER BY id LIMIT $1 OFFSET $2";
    assert_param_types(&snapshot, sql, &["i64", "i64"]);
}

// ── LIMIT with expression containing param ────────────────────────────

#[test]
fn goal_limit_param_plus_literal() {
    // LIMIT $1 + 1  →  operator +(unknown, int4) resolves $1 as int4.
    // The overall result int4 is coerced to int8 (assignment cast exists).
    let snapshot = setup();
    let sql = "SELECT id FROM users LIMIT $1 + 1";
    assert_param_types(&snapshot, sql, &["i32"]);
}

#[test]
fn goal_limit_function_of_param() {
    // LIMIT length($1)  →  $1 must be text (from length(text) signature).
    // length() returns int4, which is coerced to int8 for LIMIT.
    // This is the key test: $1 should NOT be int8.
    let snapshot = setup();
    let sql = "SELECT id FROM users LIMIT length($1)";
    assert_param_types(&snapshot, sql, &["String"]);
}

#[test]
fn goal_limit_cast_param() {
    // LIMIT $1::int4  →  $1 receives int4 from the cast target.
    let snapshot = setup();
    let sql = "SELECT id FROM users LIMIT $1::int4";
    assert_param_types(&snapshot, sql, &["i32"]);
}

// ── WHERE with bare param ─────────────────────────────────────────────

#[test]
fn goal_where_bool_param() {
    // WHERE $1  →  $1 must be bool (PG's coerce_to_boolean with COERCION_ASSIGNMENT).
    let snapshot = setup();
    let sql = "SELECT id FROM users WHERE $1";
    assert_param_types(&snapshot, sql, &["bool"]);
}

#[test]
fn goal_where_comparison_infers_column_type() {
    // WHERE id = $1  →  $1 gets type of id (int8).
    // WHERE name = $2  →  $2 gets type of name (text).
    // WHERE age > $3  →  $3 gets type of age (int4).
    let snapshot = setup();
    let sql = "SELECT id FROM users WHERE id = $1 AND name = $2 AND age > $3";
    assert_param_types(&snapshot, sql, &["i64", "String", "i32"]);
}

#[test]
fn goal_where_function_of_param() {
    // WHERE length($1) > 5  →  $1 gets text from length(text), not bool from WHERE.
    let snapshot = setup();
    let sql = "SELECT id FROM users WHERE length($1) > 5";
    assert_param_types(&snapshot, sql, &["String"]);
}

#[test]
fn goal_where_and_or_bool_propagation() {
    // WHERE $1 AND $2 OR $3  →  all must be bool.
    let snapshot = setup();
    let sql = "SELECT id FROM users WHERE $1 AND $2 OR $3";
    assert_param_types(&snapshot, sql, &["bool", "bool", "bool"]);
}

// ── INSERT VALUES with param ──────────────────────────────────────────

#[test]
fn goal_insert_values_params() {
    // INSERT INTO users (name, email, age) VALUES ($1, $2, $3)
    // $1 → text (name), $2 → text (email), $3 → int4 (age).
    let snapshot = setup();
    let sql = "INSERT INTO users (name, email, age) VALUES ($1, $2, $3)";
    assert_param_types(&snapshot, sql, &["String", "String", "i32"]);
}

#[test]
fn goal_insert_values_expression() {
    // INSERT INTO users (name, email, age) VALUES ($1, $2, $3 + 1)
    // $3 is inferred from operator +(unknown, int4) as int4.
    let snapshot = setup();
    let sql = "INSERT INTO users (name, email, age) VALUES ($1, $2, $3 + 1)";
    assert_param_types(&snapshot, sql, &["String", "String", "i32"]);
}

#[test]
fn goal_insert_nullable_inference() {
    // age is nullable → $3 should be nullable.
    // name/email are NOT NULL → $1, $2 should not be nullable.
    let snapshot = setup();
    let sql = "INSERT INTO users (name, email, age) VALUES ($1, $2, $3)";
    let info = analyze(&snapshot, sql, &default_config()).unwrap();
    assert!(!info.params[0].nullable, "name is NOT NULL");
    assert!(!info.params[1].nullable, "email is NOT NULL");
    assert!(info.params[2].nullable, "age is nullable");
}

// ── UPDATE SET with param ─────────────────────────────────────────────

#[test]
fn goal_update_set_params() {
    // UPDATE users SET name = $1, age = $2 WHERE id = $3
    // $1 → text, $2 → int4, $3 → int8.
    let snapshot = setup();
    let sql = "UPDATE users SET name = $1, age = $2 WHERE id = $3";
    assert_param_types(&snapshot, sql, &["String", "i32", "i64"]);
}

#[test]
fn goal_update_set_nullable_inference() {
    // age is nullable → $2 should be nullable.
    let snapshot = setup();
    let sql = "UPDATE users SET name = $1, age = $2 WHERE id = $3";
    let info = analyze(&snapshot, sql, &default_config()).unwrap();
    assert!(!info.params[0].nullable, "name is NOT NULL");
    assert!(info.params[1].nullable, "age is nullable");
    assert!(!info.params[2].nullable, "WHERE id = $3, not nullable");
}

// ── TypeCast propagation ──────────────────────────────────────────────

#[test]
fn goal_cast_types_param() {
    // $1::text → $1 gets text.
    // $2::int4 → $2 gets int4.
    let snapshot = setup();
    let sql = "SELECT $1::text, $2::int4";
    assert_param_types(&snapshot, sql, &["String", "i32"]);
}

// ── COALESCE param inference ──────────────────────────────────────────

#[test]
fn goal_coalesce_param_with_typed_arg() {
    // COALESCE(age, $1) → common type is int4, $1 gets int4.
    let snapshot = setup();
    let sql = "SELECT COALESCE(age, $1) FROM users";
    assert_param_types(&snapshot, sql, &["i32"]);
}

#[test]
fn goal_coalesce_all_params() {
    // COALESCE($1, $2) → both unknown → resolve as text (PG chapter 10.5 rule 3).
    let snapshot = setup();
    let sql = "SELECT COALESCE($1, $2)";
    assert_param_types(&snapshot, sql, &["String", "String"]);
}

// ── CASE param inference ──────────────────────────────────────────────

#[test]
fn goal_case_result_param() {
    // CASE WHEN true THEN age ELSE $1 END → $1 gets int4 (common type with age).
    let snapshot = setup();
    let sql = "SELECT CASE WHEN true THEN age ELSE $1 END FROM users";
    assert_param_types(&snapshot, sql, &["i32"]);
}

#[test]
fn goal_case_condition_bool() {
    // CASE WHEN $1 THEN 1 ELSE 0 END → $1 must be bool.
    let snapshot = setup();
    let sql = "SELECT CASE WHEN $1 THEN 1 ELSE 0 END";
    assert_param_types(&snapshot, sql, &["bool"]);
}

// ── Mixed contexts: param used in multiple clauses ────────────────────

#[test]
fn goal_param_in_where_and_limit() {
    // $1 used in WHERE (via comparison with int8 id), $2 in LIMIT.
    let snapshot = setup();
    let sql = "SELECT id FROM users WHERE id > $1 LIMIT $2";
    assert_param_types(&snapshot, sql, &["i64", "i64"]);
}

#[test]
fn goal_param_insert_with_returning() {
    // Full INSERT ... RETURNING with various param contexts.
    let snapshot = setup();
    let sql = "INSERT INTO posts (user_id, title, body) VALUES ($1, $2, $3) RETURNING id, title";
    assert_param_types(&snapshot, sql, &["i64", "String", "String"]);
}

// ── Operator two-pass: concrete on one side ───────────────────────────

#[test]
fn goal_operator_param_equals_column() {
    // $1 = name → $1 gets text.
    let snapshot = setup();
    let sql = "SELECT id FROM users WHERE $1 = name";
    assert_param_types(&snapshot, sql, &["String"]);
}

#[test]
fn goal_operator_param_both_sides() {
    // $1 > $2 with no context — PG defaults both to text.
    let snapshot = setup();
    assert_param_types(&snapshot, "SELECT $1 > $2", &["String", "String"]);
}

// ── Nested function/operator param inference ──────────────────────────

#[test]
fn goal_nested_function_in_where() {
    // WHERE upper($1) = name → $1 gets text from upper(text).
    let snapshot = setup();
    let sql = "SELECT id FROM users WHERE upper($1) = name";
    assert_param_types(&snapshot, sql, &["String"]);
}

#[test]
fn goal_concat_param_in_where() {
    // WHERE name = $1 || $2 → both get text from || operator.
    let snapshot = setup();
    let sql = "SELECT id FROM users WHERE name = $1 || $2";
    assert_param_types(&snapshot, sql, &["String", "String"]);
}

// ── DELETE with params ────────────────────────────────────────────────

#[test]
fn goal_delete_where_param() {
    let snapshot = setup();
    let sql = "DELETE FROM users WHERE id = $1 RETURNING name";
    assert_param_types(&snapshot, sql, &["i64"]);
}

// ── Multiple value rows in INSERT ─────────────────────────────────────

#[test]
fn goal_insert_multiple_value_rows() {
    // INSERT INTO users (name, email) VALUES ($1, $2), ($3, $4)
    let snapshot = setup();
    let sql = "INSERT INTO users (name, email) VALUES ($1, $2), ($3, $4)";
    assert_param_types(&snapshot, sql, &["String", "String", "String", "String"]);
}

// ── Subquery param inference ──────────────────────────────────────────

#[test]
fn goal_subquery_param_in_where() {
    // WHERE id = (SELECT user_id FROM posts WHERE title = $1 LIMIT 1)
    // $1 gets text from title column.
    let snapshot = setup();
    let sql =
        "SELECT name FROM users WHERE id = (SELECT user_id FROM posts WHERE title = $1 LIMIT 1)";
    assert_param_types(&snapshot, sql, &["String"]);
}

// ── Numeric promotion in operator context ─────────────────────────────

#[test]
fn goal_numeric_promotion_int4_with_int8() {
    // WHERE id > $1 → id is int8, $1 gets int8.
    // WHERE age > $2 → age is int4, $2 gets int4.
    let snapshot = setup();
    let sql = "SELECT id FROM users WHERE id > $1 AND age > $2";
    assert_param_types(&snapshot, sql, &["i64", "i32"]);
}

// ── Literal LIMIT/OFFSET (no params, just ensure no error) ───────────

#[test]
fn goal_limit_offset_literals_no_error() {
    let snapshot = setup();
    let sql = "SELECT id FROM users ORDER BY id LIMIT 10 OFFSET 5";
    let info = analyze(&snapshot, sql, &default_config()).unwrap();
    assert!(!info.columns.is_empty(), "should have columns");
}
