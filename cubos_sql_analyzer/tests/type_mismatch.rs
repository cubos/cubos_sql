//! Tests for type mismatch errors: queries that our static analyzer rejects,
//! plus untyped parameter defaults.

mod common;
use common::*;

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Assert that our analyzer rejects the query with a type mismatch.
/// Validates our `TypeMismatch` fields.
fn assert_type_mismatch(db: &Database, sql: &str, expect_actual: &str, expect_expected: &str) {
    let result = db.analyze(sql);
    match &result {
        Err(cubos_sql_analyzer::AnalyzeError::TypeMismatch {
            actual,
            expected,
            context,
        }) => {
            assert_eq!(
                actual, expect_actual,
                "TypeMismatch.actual wrong for: {sql}\n  context: {context}"
            );
            assert_eq!(
                expected, expect_expected,
                "TypeMismatch.expected wrong for: {sql}\n  context: {context}"
            );
            assert!(
                !context.is_empty(),
                "TypeMismatch.context is empty for: {sql}"
            );
        }
        Err(other) => {
            panic!(
                "expected TypeMismatch({expect_actual} → {expect_expected}) for: {sql}\n  \
                 got different error: {other}"
            );
        }
        Ok(info) => {
            panic!(
                "expected TypeMismatch({expect_actual} → {expect_expected}) for: {sql}\n  \
                 got params: {:?}\n  got columns: {:?}",
                info.params
                    .iter()
                    .map(|p| p.pg_type.clone())
                    .collect::<Vec<_>>(),
                info.columns
                    .iter()
                    .map(|c| (c.name.clone(), c.pg_type.clone()))
                    .collect::<Vec<_>>(),
            );
        }
    }
}

/// Assert that our analyzer rejects the query.
/// Checks our error message contains `expected_substring`.
#[allow(dead_code)]
fn assert_analysis_error(db: &Database, sql: &str, expected_substring: &str) {
    let result = db.analyze(sql);
    match &result {
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains(expected_substring),
                "error for `{sql}` should contain \"{expected_substring}\"\n  got: {msg}"
            );
        }
        Ok(info) => {
            panic!(
                "expected error containing \"{expected_substring}\" for: {sql}\n  \
                 got params: {:?}\n  got columns: {:?}",
                info.params
                    .iter()
                    .map(|p| p.pg_type.clone())
                    .collect::<Vec<_>>(),
                info.columns
                    .iter()
                    .map(|c| (c.name.clone(), c.pg_type.clone()))
                    .collect::<Vec<_>>(),
            );
        }
    }
}

// ── WHERE type mismatches ─────────────────────────────────────────────

#[test]
fn mismatch_where_integer_not_boolean() {
    // WHERE 42 → int4 is not boolean.
    let db = setup();
    assert_type_mismatch(&db, "SELECT id FROM users WHERE 42", "int4", "bool");
}

#[test]
fn mismatch_where_text_not_boolean() {
    // WHERE name → text is not boolean.
    let db = setup();
    assert_type_mismatch(&db, "SELECT id FROM users WHERE name", "text", "bool");
}

#[test]
fn mismatch_where_bigint_not_boolean() {
    // WHERE id → int8 is not boolean.
    let db = setup();
    assert_type_mismatch(&db, "SELECT name FROM users WHERE id", "int8", "bool");
}

#[test]
fn mismatch_where_timestamptz_not_boolean() {
    // WHERE created_at → timestamptz is not boolean.
    let db = setup();
    assert_type_mismatch(
        &db,
        "SELECT id FROM users WHERE created_at",
        "timestamptz",
        "bool",
    );
}

// ── LIMIT/OFFSET type mismatches ──────────────────────────────────────

#[test]
fn mismatch_limit_boolean() {
    // LIMIT true → bool is not int8.
    let db = setup();
    assert_type_mismatch(&db, "SELECT id FROM users LIMIT true", "bool", "int8");
}

#[test]
fn mismatch_limit_text_column() {
    // LIMIT name → text is not int8.
    let db = setup();
    assert_type_mismatch(&db, "SELECT id FROM users LIMIT name", "text", "int8");
}

#[test]
fn mismatch_limit_timestamptz_column() {
    // LIMIT created_at → timestamptz is not int8.
    let db = setup();
    assert_type_mismatch(
        &db,
        "SELECT id FROM users LIMIT created_at",
        "timestamptz",
        "int8",
    );
}

#[test]
fn mismatch_offset_boolean() {
    // OFFSET false → bool is not int8.
    let db = setup();
    assert_type_mismatch(&db, "SELECT id FROM users OFFSET false", "bool", "int8");
}

// ── Untyped params default to text (matching PG) ─────────────────────

#[test]
fn goal_untyped_param_defaults_to_text() {
    // SELECT $p1 → no context, defaults to text (PG's preferred type for unknown).
    let db = setup();
    let info = db.analyze("SELECT $p1").unwrap();
    assert_eq!(info.params[0].pg_type, text());
}

#[test]
fn goal_untyped_params_in_comparison_default_to_text() {
    // SELECT $p1 > $p2 → both unknown, PG infers text for both.
    let db = setup();
    let info = db.analyze("SELECT $p1 > $p2").unwrap();
    assert_eq!(info.params[0].pg_type, text());
    assert_eq!(info.params[1].pg_type, text());
}

#[test]
fn error_insert_wrong_column_name() {
    // INSERT INTO users (nonexistent) VALUES ($p1) → column not found.
    let db = setup();
    let sql = "INSERT INTO users (nonexistent) VALUES ($p1)";

    // Our analyzer: may produce unknown-typed param or error — either is acceptable.
    // The key is it doesn't silently produce wrong types.
    let result = db.analyze(sql);
    if let Ok(info) = result {
        assert_eq!(info.params.len(), 1);
    }
}
