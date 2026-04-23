//! JSON / JSONB operators beyond `->>` and `?` (which are already covered
//! in [`special.rs`](special.rs)): containment `@>` / `<@`, key-exists
//! variants `?|` / `?&`, path operators `#>` / `#>>`, constructors
//! `jsonb_build_object` / `json_build_array`.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE users (
            id    BIGINT PRIMARY KEY,
            prefs JSONB,
            meta  JSONB NOT NULL
         );",
    )
    .unwrap();
    db
}

// ── Containment: @>, <@ ──────────────────────────────────────────────────────

#[test]
fn jsonb_contains_rhs_literal() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE prefs @> '{\"a\":1}'::jsonb")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn jsonb_contained_by_rhs_literal() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE '{\"a\":1}'::jsonb <@ meta")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn jsonb_contains_param() {
    let db = setup();
    // `meta @> $p1` — param is inferred as `jsonb` from the left-hand column.
    let s = db
        .analyze("SELECT id FROM users WHERE meta @> $p1")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(jsonb())]);
}

// ── Key-exists variants: ?|, ?& ──────────────────────────────────────────────

#[test]
fn jsonb_any_key_exists() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE prefs ?| ARRAY['a','b']")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn jsonb_all_keys_exist() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE meta ?& ARRAY['a','b']")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

// ── Path extraction: #>, #>> ─────────────────────────────────────────────────

#[test]
fn jsonb_path_get_text() {
    let db = setup();
    // `#>>` returns text. Nullable because the path may not exist.
    let s = db
        .analyze("SELECT prefs #>> '{a,b}' AS v FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("v", text())]);
}

#[test]
fn jsonb_path_get_jsonb() {
    let db = setup();
    // `#>` returns jsonb. Nullable: path may not exist even on NOT NULL meta.
    let s = db
        .analyze("SELECT meta #> '{a,b}' AS v FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("v", jsonb())]);
}

// ── Constructors ─────────────────────────────────────────────────────────────

#[test]
fn jsonb_build_object_returns_jsonb() {
    let db = setup();
    // Non-strict builder: NULL arguments produce a JSON `null` entry, so the
    // outer value is never SQL NULL.
    let s = db
        .analyze("SELECT jsonb_build_object('k', 1) AS x")
        .unwrap();
    assert_cols(&s, vec![c("x", jsonb())]);
}

#[test]
fn json_build_array_returns_json() {
    let db = setup();
    let s = db.analyze("SELECT json_build_array(1, 'a') AS x").unwrap();
    assert_cols(&s, vec![c("x", json_ty())]);
}
