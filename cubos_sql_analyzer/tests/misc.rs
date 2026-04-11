//! Miscellaneous tests: UNKNOWN type resolution in function calls and
//! snapshot serialization roundtrip.

mod common;
use common::*;

// ──────────────────────────────────────────────────────────────────────────────
// Tests: UNKNOWN type resolution in function calls
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn unknown_literal_in_function_call() {
    let snapshot = setup();
    // ', ' is UNKNOWN type — should resolve string_agg(text, text) unambiguously.
    let sql = "SELECT post_id, string_agg(author_name, ', ') as authors \
               FROM comments GROUP BY post_id";
    let info = static_analyze(&snapshot, sql);
    assert_eq!(col(&info, "authors").rust_type, "String");
}

#[test]
fn unknown_literal_in_replace() {
    let snapshot = setup();
    // replace(text, text, text) — two UNKNOWN literals.
    let sql = "SELECT replace(name, 'foo', 'bar') as replaced FROM users";
    let info = static_analyze(&snapshot, sql);
    assert_eq!(col(&info, "replaced").rust_type, "String");
    assert!(!col(&info, "replaced").nullable);
}

#[test]
fn unknown_literal_in_position() {
    let snapshot = setup();
    // position(text in text) — UNKNOWN in first arg.
    let sql = "SELECT position('x' in name) as pos FROM users";
    let info = static_analyze(&snapshot, sql);
    assert!(!col(&info, "pos").nullable);
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: UNKNOWN type resolution in operators
// ──────────────────────────────────────────────────────────────────────────────

/// jsonb `?` operator with both sides UNKNOWN resolves to `jsonb ? text → bool`
/// (unique candidate), which then types the param as jsonb.
#[test]
fn unknown_operator_jsonb_exists() {
    let snapshot = setup();
    let sql = "SELECT id FROM users WHERE preferences ? 'theme'";
    let info = static_analyze(&snapshot, sql);
    assert_eq!(col(&info, "id").rust_type, "i64");
}

/// jsonb `->>` with a typed left side and UNKNOWN right resolves to
/// `jsonb ->> text → text` (text-fallback disambiguation).
#[test]
fn unknown_operator_jsonb_arrow_text() {
    let snapshot = setup();
    let sql = "SELECT preferences->>'theme' as theme FROM users";
    let info = static_analyze(&snapshot, sql);
    assert_eq!(col(&info, "theme").rust_type, "String");
    assert!(col(&info, "theme").nullable);
}

/// Param used with `?` first (infers jsonb), then with `->>` — the second
/// usage should see the already-inferred type.
#[test]
fn unknown_param_jsonb_exists_then_arrow() {
    let snapshot = setup();
    let sql = "UPDATE whatsapp.contacts SET \
               name = CASE WHEN $1 ? 'name' THEN $1->>'name' ELSE name END \
               WHERE channel_id = $2 AND id = $3";
    let info = analyze(&snapshot, sql, &default_config()).unwrap();
    // $1 should be inferred as jsonb via the `?` operator
    assert_eq!(info.params[0].rust_type, "::serde_json::Value");
    assert_eq!(info.params[1].rust_type, "i64");
    assert_eq!(info.params[2].rust_type, "String");
}

/// Multiple CASE WHEN branches using `?` and `->>` with the same param.
#[test]
fn unknown_param_jsonb_multiple_case_branches() {
    let snapshot = setup();
    let sql = "UPDATE whatsapp.contacts SET \
               name = CASE WHEN $1 ? 'name' THEN $1->>'name' ELSE name END, \
               pushname = CASE WHEN $1 ? 'pushname' THEN $1->>'pushname' ELSE pushname END, \
               is_business = CASE WHEN $1 ? 'is_business' THEN ($1->>'is_business')::boolean ELSE is_business END \
               WHERE channel_id = $2 AND id = $3";
    let info = analyze(&snapshot, sql, &default_config()).unwrap();
    assert_eq!(info.params[0].rust_type, "::serde_json::Value");
    assert_eq!(info.params[1].rust_type, "i64");
    assert_eq!(info.params[2].rust_type, "String");
}

/// Operator `->` (returns jsonb) with UNKNOWN right side should resolve.
#[test]
fn unknown_operator_jsonb_arrow() {
    let snapshot = setup();
    let sql = "SELECT preferences->'theme' as theme FROM users";
    let info = static_analyze(&snapshot, sql);
    // -> returns jsonb (when left is jsonb)
    assert_eq!(col(&info, "theme").rust_type, "::serde_json::Value");
}

/// Query against pg_catalog table with obj_description function.
#[test]
fn pg_catalog_obj_description_with_param() {
    let snapshot = setup();
    let sql = "SELECT obj_description(oid) as comment FROM pg_namespace WHERE nspname = $1";
    let info = analyze(&snapshot, sql, &default_config()).unwrap();
    // obj_description returns nullable text → Option<String>
    assert_eq!(col(&info, "comment").rust_type, "String");
    assert!(col(&info, "comment").nullable);
    // $1 compared with nspname (type name) → String
    assert_eq!(info.params[0].rust_type, "String");
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: snapshot serialization roundtrip
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn snapshot_roundtrip() {
    let snapshot = setup();

    let json = serde_json::to_string(&snapshot).unwrap();
    let restored: SchemaSnapshot = serde_json::from_str(&json).unwrap();

    assert_eq!(snapshot.types.len(), restored.types.len());
    assert_eq!(snapshot.tables.len(), restored.tables.len());
    assert_eq!(
        snapshot.functions_by_name.len(),
        restored.functions_by_name.len()
    );
    assert_eq!(
        snapshot.operators_by_name.len(),
        restored.operators_by_name.len()
    );
    assert_eq!(snapshot.casts.len(), restored.casts.len());

    // Analyze with restored snapshot gives same results.
    let config = default_config();
    let sql = "SELECT id, name FROM users";
    let info1 = analyze(&snapshot, sql, &config).unwrap();
    let info2 = analyze(&restored, sql, &config).unwrap();
    assert_identical(&info1, &info2, "snapshot roundtrip");
}
