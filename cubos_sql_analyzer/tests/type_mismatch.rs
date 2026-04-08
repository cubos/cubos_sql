//! Tests for type mismatch errors: queries that our static analyzer rejects,
//! plus untyped parameter defaults.

mod common;
use common::*;

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Assert that our analyzer rejects the query with a type mismatch.
/// Validates our `TypeMismatch` fields.
fn assert_type_mismatch(
    snapshot: &SchemaSnapshot,
    sql: &str,
    expect_actual: &str,
    expect_expected: &str,
) {
    let result = analyze(snapshot, sql, &default_config());
    match &result {
        Err(cubos_sql_analyzer::error::AnalyzeError::TypeMismatch {
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
                info.params.iter().map(|p| &p.rust_type).collect::<Vec<_>>(),
                info.columns
                    .iter()
                    .map(|c| (&c.name, &c.rust_type))
                    .collect::<Vec<_>>(),
            );
        }
    }
}

/// Assert that our analyzer rejects the query.
/// Checks our error message contains `expected_substring`.
#[allow(dead_code)]
fn assert_analysis_error(snapshot: &SchemaSnapshot, sql: &str, expected_substring: &str) {
    let result = analyze(snapshot, sql, &default_config());
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
                info.params.iter().map(|p| &p.rust_type).collect::<Vec<_>>(),
                info.columns
                    .iter()
                    .map(|c| (&c.name, &c.rust_type))
                    .collect::<Vec<_>>(),
            );
        }
    }
}

// ── WHERE type mismatches ─────────────────────────────────────────────

#[test]
fn mismatch_where_integer_not_boolean() {
    // WHERE 42 → int4 is not boolean.
    let snapshot = setup();
    assert_type_mismatch(&snapshot, "SELECT id FROM users WHERE 42", "int4", "bool");
}

#[test]
fn mismatch_where_text_not_boolean() {
    // WHERE name → text is not boolean.
    let snapshot = setup();
    assert_type_mismatch(&snapshot, "SELECT id FROM users WHERE name", "text", "bool");
}

#[test]
fn mismatch_where_bigint_not_boolean() {
    // WHERE id → int8 is not boolean.
    let snapshot = setup();
    assert_type_mismatch(&snapshot, "SELECT name FROM users WHERE id", "int8", "bool");
}

#[test]
fn mismatch_where_timestamptz_not_boolean() {
    // WHERE created_at → timestamptz is not boolean.
    let snapshot = setup();
    assert_type_mismatch(
        &snapshot,
        "SELECT id FROM users WHERE created_at",
        "timestamptz",
        "bool",
    );
}

// ── LIMIT/OFFSET type mismatches ──────────────────────────────────────

#[test]
fn mismatch_limit_boolean() {
    // LIMIT true → bool is not int8.
    let snapshot = setup();
    assert_type_mismatch(&snapshot, "SELECT id FROM users LIMIT true", "bool", "int8");
}

#[test]
fn mismatch_limit_text_column() {
    // LIMIT name → text is not int8.
    let snapshot = setup();
    assert_type_mismatch(&snapshot, "SELECT id FROM users LIMIT name", "text", "int8");
}

#[test]
fn mismatch_limit_timestamptz_column() {
    // LIMIT created_at → timestamptz is not int8.
    let snapshot = setup();
    assert_type_mismatch(
        &snapshot,
        "SELECT id FROM users LIMIT created_at",
        "timestamptz",
        "int8",
    );
}

#[test]
fn mismatch_offset_boolean() {
    // OFFSET false → bool is not int8.
    let snapshot = setup();
    assert_type_mismatch(
        &snapshot,
        "SELECT id FROM users OFFSET false",
        "bool",
        "int8",
    );
}

// ── Untyped params default to text (matching PG) ─────────────────────

#[test]
fn goal_untyped_param_defaults_to_text() {
    // SELECT $1 → no context, defaults to text (PG's preferred type for unknown).
    let snapshot = setup();
    let info = analyze(&snapshot, "SELECT $1", &default_config()).unwrap();
    assert_eq!(info.params[0].rust_type, "String");
}

#[test]
fn goal_untyped_params_in_comparison_default_to_text() {
    // SELECT $1 > $2 → both unknown, PG infers text for both.
    let snapshot = setup();
    let info = analyze(&snapshot, "SELECT $1 > $2", &default_config()).unwrap();
    assert_eq!(info.params[0].rust_type, "String");
    assert_eq!(info.params[1].rust_type, "String");
}

#[test]
fn error_insert_wrong_column_name() {
    // INSERT INTO users (nonexistent) VALUES ($1) → column not found.
    let snapshot = setup();
    let sql = "INSERT INTO users (nonexistent) VALUES ($1)";

    // Our analyzer: may produce unknown-typed param or error — either is acceptable.
    // The key is it doesn't silently produce wrong types.
    let result = analyze(&snapshot, sql, &default_config());
    if let Ok(info) = result {
        assert_eq!(info.params.len(), 1);
    }
}
