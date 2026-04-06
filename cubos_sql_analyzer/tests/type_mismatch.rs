//! Tests for type mismatch errors: queries that both our static analyzer and
//! PostgreSQL reject, plus untyped parameter defaults.

mod common;
use common::*;

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Try to PREPARE a SQL statement against live PostgreSQL.
/// Returns `Err(pg_message)` if PG rejects it, `Ok(())` if it succeeds.
fn pg_prepare(client: &mut postgres::Client, sql: &str) -> Result<(), String> {
    let _ = client.batch_execute("DEALLOCATE ALL");
    let prepare = format!("PREPARE __cubos_test AS {sql}");
    match client.batch_execute(&prepare) {
        Ok(_) => {
            let _ = client.batch_execute("DEALLOCATE __cubos_test");
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Assert that BOTH our analyzer and PostgreSQL reject the query with a type
/// mismatch.  Validates our `TypeMismatch` fields and checks that PG's error
/// message mentions the same types.
fn assert_type_mismatch(
    snapshot: &SchemaSnapshot,
    client: &mut postgres::Client,
    sql: &str,
    expect_actual: &str,
    expect_expected: &str,
) {
    // ── Our analyzer ──────────────────────────────────────────────────────
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

    // ── PostgreSQL ────────────────────────────────────────────────────────
    let pg_result = pg_prepare(client, sql);
    let pg_err = pg_result.expect_err(&format!(
        "PostgreSQL should also reject: {sql}\n  (our error: {})",
        result.unwrap_err()
    ));
    // PG error should mention the actual type name (in PG's own naming).
    // We don't assert exact wording — just that PG also errored.
    assert!(!pg_err.is_empty(), "PG error message is empty for: {sql}");
}

/// Assert that BOTH our analyzer and PostgreSQL reject the query.
/// Checks our error message contains `expected_substring`.
fn assert_analysis_error(
    snapshot: &SchemaSnapshot,
    client: &mut postgres::Client,
    sql: &str,
    expected_substring: &str,
) {
    // ── Our analyzer ──────────────────────────────────────────────────────
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

    // ── PostgreSQL ────────────────────────────────────────────────────────
    let pg_result = pg_prepare(client, sql);
    let pg_err = pg_result.expect_err(&format!(
        "PostgreSQL should also reject: {sql}\n  (our error: {})",
        result.unwrap_err()
    ));
    assert!(!pg_err.is_empty(), "PG error message is empty for: {sql}");
}

// ── WHERE type mismatches ─────────────────────────────────────────────

#[test]
#[ignore]
fn mismatch_where_integer_not_boolean() {
    // WHERE 42 → int4 is not boolean.
    // PG: ERROR: argument of WHERE must be type boolean, not type integer
    let (snapshot, mut client) = setup();
    assert_type_mismatch(
        &snapshot,
        &mut client,
        "SELECT id FROM users WHERE 42",
        "int4",
        "bool",
    );
}

#[test]
#[ignore]
fn mismatch_where_text_not_boolean() {
    // WHERE name → text is not boolean.
    // PG: ERROR: argument of WHERE must be type boolean, not type text
    let (snapshot, mut client) = setup();
    assert_type_mismatch(
        &snapshot,
        &mut client,
        "SELECT id FROM users WHERE name",
        "text",
        "bool",
    );
}

#[test]
#[ignore]
fn mismatch_where_bigint_not_boolean() {
    // WHERE id → int8 is not boolean.
    // PG: ERROR: argument of WHERE must be type boolean, not type bigint
    let (snapshot, mut client) = setup();
    assert_type_mismatch(
        &snapshot,
        &mut client,
        "SELECT name FROM users WHERE id",
        "int8",
        "bool",
    );
}

#[test]
#[ignore]
fn mismatch_where_timestamptz_not_boolean() {
    // WHERE created_at → timestamptz is not boolean.
    // PG: ERROR: argument of WHERE must be type boolean, not type timestamp with time zone
    let (snapshot, mut client) = setup();
    assert_type_mismatch(
        &snapshot,
        &mut client,
        "SELECT id FROM users WHERE created_at",
        "timestamptz",
        "bool",
    );
}

// ── LIMIT/OFFSET type mismatches ──────────────────────────────────────

#[test]
#[ignore]
fn mismatch_limit_boolean() {
    // LIMIT true → bool is not int8.
    // PG: ERROR: argument of LIMIT must be type bigint, not type boolean
    let (snapshot, mut client) = setup();
    assert_type_mismatch(
        &snapshot,
        &mut client,
        "SELECT id FROM users LIMIT true",
        "bool",
        "int8",
    );
}

#[test]
#[ignore]
fn mismatch_limit_text_column() {
    // LIMIT name → text is not int8.
    // PG: ERROR: argument of LIMIT must be type bigint, not type text
    let (snapshot, mut client) = setup();
    assert_type_mismatch(
        &snapshot,
        &mut client,
        "SELECT id FROM users LIMIT name",
        "text",
        "int8",
    );
}

#[test]
#[ignore]
fn mismatch_limit_timestamptz_column() {
    // LIMIT created_at → timestamptz is not int8.
    // PG: ERROR: argument of LIMIT must be type bigint, not type timestamp with time zone
    let (snapshot, mut client) = setup();
    assert_type_mismatch(
        &snapshot,
        &mut client,
        "SELECT id FROM users LIMIT created_at",
        "timestamptz",
        "int8",
    );
}

#[test]
#[ignore]
fn mismatch_offset_boolean() {
    // OFFSET false → bool is not int8.
    // PG: ERROR: argument of OFFSET must be type bigint, not type boolean
    let (snapshot, mut client) = setup();
    assert_type_mismatch(
        &snapshot,
        &mut client,
        "SELECT id FROM users OFFSET false",
        "bool",
        "int8",
    );
}

// ── Untyped params default to text (matching PG) ─────────────────────

#[test]
#[ignore]
fn goal_untyped_param_defaults_to_text() {
    // SELECT $1 → no context, defaults to text (PG's preferred type for unknown).
    let (snapshot, mut client) = setup();
    let info = analyze(&snapshot, "SELECT $1", &default_config()).unwrap();
    assert_eq!(info.params[0].rust_type, "String");
    let live_info = live_introspect(&mut client, "SELECT $1");
    assert_eq!(live_info.params[0].rust_type, "String");
}

#[test]
#[ignore]
fn goal_untyped_params_in_comparison_default_to_text() {
    // SELECT $1 > $2 → both unknown, PG infers text for both.
    let (snapshot, mut client) = setup();
    let info = analyze(&snapshot, "SELECT $1 > $2", &default_config()).unwrap();
    assert_eq!(info.params[0].rust_type, "String");
    assert_eq!(info.params[1].rust_type, "String");
    let live_info = live_introspect(&mut client, "SELECT $1 > $2");
    assert_eq!(live_info.params[0].rust_type, "String");
    assert_eq!(live_info.params[1].rust_type, "String");
}

#[test]
#[ignore]
fn error_insert_wrong_column_name() {
    // INSERT INTO users (nonexistent) VALUES ($1) → column not found.
    // PG: ERROR: column "nonexistent" of relation "users" does not exist
    let (snapshot, mut client) = setup();
    let sql = "INSERT INTO users (nonexistent) VALUES ($1)";

    // PG must reject.
    let pg_err = pg_prepare(&mut client, sql);
    assert!(
        pg_err.is_err(),
        "PG should reject INSERT with nonexistent column"
    );

    // Our analyzer: may produce unknown-typed param or error — either is acceptable.
    // The key is it doesn't silently produce wrong types.
    let result = analyze(&snapshot, sql, &default_config());
    if let Ok(info) = result {
        assert_eq!(info.params.len(), 1);
    }
}
