//! Miscellaneous tests: UNKNOWN type resolution in function calls and
//! snapshot serialization roundtrip.

mod common;
use common::*;

// ──────────────────────────────────────────────────────────────────────────────
// Tests: UNKNOWN type resolution in function calls
// ──────────────────────────────────────────────────────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn unknown_literal_in_function_call() {
    let (snapshot, mut client) = setup();
    // ', ' is UNKNOWN type — should resolve string_agg(text, text) unambiguously.
    let sql = "SELECT post_id, string_agg(author_name, ', ') as authors \
               FROM comments GROUP BY post_id";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);
    assert_same_types(&static_info, &live_info, "string_agg with UNKNOWN literal");
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn unknown_literal_in_replace() {
    let (snapshot, _) = setup();
    // replace(text, text, text) — two UNKNOWN literals.
    let sql = "SELECT replace(name, 'foo', 'bar') as replaced FROM users";
    let info = static_analyze(&snapshot, sql);
    assert_eq!(col(&info, "replaced").rust_type, "String");
    assert!(!col(&info, "replaced").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn unknown_literal_in_position() {
    let (snapshot, _) = setup();
    // position(text in text) — UNKNOWN in first arg.
    let sql = "SELECT position('x' in name) as pos FROM users";
    let info = static_analyze(&snapshot, sql);
    assert!(!col(&info, "pos").nullable);
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: snapshot serialization roundtrip
// ──────────────────────────────────────────────────────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn snapshot_roundtrip() {
    let (snapshot, _client) = setup();

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
