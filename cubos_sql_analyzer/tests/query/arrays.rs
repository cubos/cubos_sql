//! Array operations: constructor, element access, length/cardinality,
//! concatenation, `ANY`/`ALL` on arrays, `unnest` in the projection,
//! `array_position` / `array_length` return types.
//!
//! Known gaps (not covered here — produce `Unsupported`/`UnknownColumn`
//! today): array slicing `arr[1:2]`, `unnest(arr)` used as a relation in
//! `FROM` (the resulting column is not bound by name). These should move
//! out of this file's ignore list as the analyzer learns them.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE users (
            id    BIGINT PRIMARY KEY,
            tags  TEXT[],
            nums  INT[] NOT NULL
         );",
    )
    .unwrap();
    db
}

// ── ARRAY[...] constructor ───────────────────────────────────────────────────

#[test]
fn array_constructor_int() {
    let db = setup();
    let s = db.analyze("SELECT ARRAY[1, 2, 3] AS a").unwrap();
    assert_cols(&s, vec![c("a", array_of(int4()))]);
}

#[test]
fn array_constructor_text() {
    let db = setup();
    let s = db
        .analyze("SELECT ARRAY['a', 'b', 'c']::text[] AS a")
        .unwrap();
    assert_cols(&s, vec![c("a", array_of(text()))]);
}

// ── Element access `arr[i]` ──────────────────────────────────────────────────

#[test]
fn array_element_access() {
    let db = setup();
    // Subscripting is always nullable (index may be out of bounds).
    let s = db
        .analyze("SELECT tags[1] AS first_tag FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("first_tag", text())]);
}

#[test]
fn array_element_access_not_null_array() {
    let db = setup();
    // Even when the array column itself is NOT NULL, the element is nullable.
    let s = db.analyze("SELECT nums[1] AS first FROM users").unwrap();
    assert_cols(&s, vec![cn("first", int4())]);
}

// ── ANY / = ANY on an array ──────────────────────────────────────────────────

#[test]
fn any_array_in_where() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE 'x' = ANY(tags)")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![]);
}

#[test]
fn eq_any_param_array() {
    let db = setup();
    // `col = ANY($p1)` — param is inferred as `int4[]` from the int4 column.
    let s = db
        .analyze("SELECT id FROM users WHERE id = ANY($p1)")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(array_of(int8()))]);
}

// ── Array concatenation `||` ─────────────────────────────────────────────────

#[test]
fn array_concat_text_arrays() {
    let db = setup();
    // `tags` is nullable → concat is nullable (|| is strict).
    let s = db
        .analyze("SELECT tags || ARRAY['z']::text[] AS combined FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("combined", array_of(text()))]);
}

#[test]
fn array_concat_int_arrays_preserves_not_null() {
    let db = setup();
    // `nums` is NOT NULL → concat stays NOT NULL.
    let s = db
        .analyze("SELECT nums || ARRAY[0] AS combined FROM users")
        .unwrap();
    assert_cols(&s, vec![c("combined", array_of(int4()))]);
}

#[test]
fn array_concat_element_to_array() {
    let db = setup();
    // `element || array` also resolves to the array type.
    let s = db
        .analyze("SELECT 0 || nums AS combined FROM users")
        .unwrap();
    assert_cols(&s, vec![c("combined", array_of(int4()))]);
}

// ── array_length / cardinality / array_position ──────────────────────────────

#[test]
fn array_length_returns_int4() {
    let db = setup();
    // `array_length` returns NULL for empty arrays, so it is always nullable.
    let s = db
        .analyze("SELECT array_length(nums, 1) AS n FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("n", int4())]);
}

#[test]
fn cardinality_returns_int4() {
    let db = setup();
    // `cardinality(nums)` — nums is NOT NULL and strict, so the result is
    // NOT NULL (unlike `array_length`, which is defined to return NULL for
    // empty arrays and is therefore always nullable).
    let s = db
        .analyze("SELECT cardinality(nums) AS n FROM users")
        .unwrap();
    assert_cols(&s, vec![c("n", int4())]);
}

#[test]
fn array_position_returns_int4() {
    let db = setup();
    let s = db
        .analyze("SELECT array_position(tags, 'x') AS pos FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("pos", int4())]);
}

// ── unnest in the projection ─────────────────────────────────────────────────

#[test]
fn unnest_in_select_list_text() {
    let db = setup();
    let s = db.analyze("SELECT unnest(tags) AS t FROM users").unwrap();
    assert_cols(&s, vec![cn("t", text())]);
}

#[test]
fn unnest_in_select_list_int() {
    let db = setup();
    let s = db.analyze("SELECT unnest(nums) AS n FROM users").unwrap();
    // `nums` is NOT NULL, so the per-element projection is NOT NULL too.
    assert_cols(&s, vec![c("n", int4())]);
}
