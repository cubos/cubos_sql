//! Less-common query constructs: LATERAL, VALUES, set-returning functions
//! (SRFs), `(expr).field` indirection, `arr[i]`, `row_to_json`,
//! UNKNOWN-literal resolution in operators/functions, subquery alias
//! overrides.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE users (
            id          BIGINT PRIMARY KEY,
            name        TEXT NOT NULL,
            age         INT,
            preferences JSONB
        );
         CREATE TABLE comments (
            id          BIGINT PRIMARY KEY,
            post_id     BIGINT NOT NULL,
            author_name TEXT NOT NULL
         );
         CREATE SCHEMA whatsapp;
         CREATE TABLE whatsapp.contacts (
            channel_id  BIGINT NOT NULL,
            id          TEXT NOT NULL,
            name        TEXT,
            pushname    TEXT,
            is_business BOOLEAN,
            PRIMARY KEY (channel_id, id)
         );",
    )
    .unwrap();
    db
}

// ── UNKNOWN literals in function calls ───────────────────────────────────────

#[test]
fn unknown_literal_in_function_call() {
    let db = setup();
    // ', ' is UNKNOWN — should resolve `string_agg(text, text)` unambiguously.
    let sql = "SELECT post_id, string_agg(author_name, ', ') as authors \
               FROM comments GROUP BY post_id";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "authors").pg_type, text());
}

#[test]
fn unknown_literal_in_replace() {
    let db = setup();
    // replace(text, text, text) — two UNKNOWN literals.
    let sql = "SELECT replace(name, 'foo', 'bar') as replaced FROM users";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "replaced").pg_type, text());
    assert!(!col(&info, "replaced").nullable);
}

#[test]
fn unknown_literal_in_position() {
    let db = setup();
    let sql = "SELECT position('x' in name) as pos FROM users";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "pos").nullable);
}

// ── UNKNOWN type resolution in operators ─────────────────────────────────────

/// jsonb `?` operator with both sides UNKNOWN resolves to `jsonb ? text → bool`
/// (unique candidate), which then types the param as jsonb.
#[test]
fn unknown_operator_jsonb_exists() {
    let db = setup();
    let sql = "SELECT id FROM users WHERE preferences ? 'theme'";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "id").pg_type, int8());
}

/// jsonb `->>` with a typed left side and UNKNOWN right resolves to
/// `jsonb ->> text → text` (text-fallback disambiguation).
#[test]
fn unknown_operator_jsonb_arrow_text() {
    let db = setup();
    let sql = "SELECT preferences->>'theme' as theme FROM users";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "theme").pg_type, text());
    assert!(col(&info, "theme").nullable);
}

/// Param used with `?` first (infers jsonb), then with `->>` — the second
/// usage should see the already-inferred type.
#[test]
fn unknown_param_jsonb_exists_then_arrow() {
    let db = setup();
    let sql = "UPDATE whatsapp.contacts SET \
               name = CASE WHEN $p1 ? 'name' THEN $p1->>'name' ELSE name END \
               WHERE channel_id = $p2 AND id = $p3";
    let info = db.analyze(sql).unwrap();
    assert_eq!(info.params[0].pg_type, jsonb());
    assert_eq!(info.params[1].pg_type, int8());
    assert_eq!(info.params[2].pg_type, text());
}

/// Multiple CASE WHEN branches using `?` and `->>` with the same param.
#[test]
fn unknown_param_jsonb_multiple_case_branches() {
    let db = setup();
    let sql = "UPDATE whatsapp.contacts SET \
               name = CASE WHEN $p1 ? 'name' THEN $p1->>'name' ELSE name END, \
               pushname = CASE WHEN $p1 ? 'pushname' THEN $p1->>'pushname' ELSE pushname END, \
               is_business = CASE WHEN $p1 ? 'is_business' THEN ($p1->>'is_business')::boolean ELSE is_business END \
               WHERE channel_id = $p2 AND id = $p3";
    let info = db.analyze(sql).unwrap();
    assert_eq!(info.params[0].pg_type, jsonb());
    assert_eq!(info.params[1].pg_type, int8());
    assert_eq!(info.params[2].pg_type, text());
}

/// Operator `->` (returns jsonb) with UNKNOWN right side should resolve.
#[test]
fn unknown_operator_jsonb_arrow() {
    let db = setup();
    let sql = "SELECT preferences->'theme' as theme FROM users";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "theme").pg_type, jsonb());
}

/// Query against pg_catalog table: obj_description function + name column.
#[test]
fn pg_catalog_obj_description_with_param() {
    let db = setup();
    let sql = "SELECT obj_description(oid) as comment FROM pg_namespace WHERE nspname = $p1";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "comment").pg_type, text());
    assert!(col(&info, "comment").nullable);
    assert_eq!(info.params[0].pg_type, name_ty());
}

// ── AIndirection — `(expr).field` ────────────────────────────────────────────

#[test]
fn indirection_field_on_composite_column() {
    // A table's composite type is registered under the table's QN.
    // `(u).name` takes the composite row and pulls a named field out.
    let db = setup();
    let sql = "SELECT (u).name AS the_name FROM users u";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "the_name").pg_type, text());
}

#[test]
fn indirection_field_nullability_honors_field_not_null() {
    let db = setup();
    // `users.age` is nullable, so `(u).age` must be nullable too.
    let sql = "SELECT (u).age AS the_age FROM users u";
    let info = db.analyze(sql).unwrap();
    assert!(col(&info, "the_age").nullable);
}

#[test]
fn indirection_field_unknown_errors() {
    let db = setup();
    let sql = "SELECT (u).nao_existe FROM users u";
    assert!(db.analyze(sql).is_err());
}

// ── ARRAY(SELECT …) sublink ──────────────────────────────────────────────────

#[test]
fn array_sublink_of_text_returns_text_array() {
    let db = setup();
    let sql = "SELECT ARRAY(SELECT name FROM users) AS names";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "names").pg_type, array_of(text()));
    // ARRAY() always returns a non-null array (empty if no rows).
    assert!(!col(&info, "names").nullable);
}

#[test]
fn array_sublink_of_int4_returns_int4_array() {
    let db = setup();
    let sql = "SELECT ARRAY(SELECT age FROM users WHERE age IS NOT NULL) AS ages";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "ages").pg_type, array_of(int4()));
    assert!(!col(&info, "ages").nullable);
}

// ── `FROM func(...)` — RangeFunction ─────────────────────────────────────────

#[test]
fn range_function_scalar_srf_exposes_single_column() {
    let db = setup();
    let sql = "SELECT generate_series FROM generate_series(1, 10)";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "generate_series").pg_type, int4());
}

#[test]
fn range_function_with_out_args_exposes_named_columns() {
    let db = setup();
    // pg_options_to_table(text[]) is declared with
    // TABLE(option_name text, option_value text).
    let sql = "SELECT option_name, option_value \
               FROM pg_options_to_table(ARRAY['a=b', 'c=d']::text[])";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "option_name").pg_type, text());
    assert_eq!(col(&info, "option_value").pg_type, text());
}

#[test]
fn range_function_column_alias_list_overrides_names() {
    let db = setup();
    let sql = "SELECT n FROM generate_series(1, 5) AS t(n)";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "n").pg_type, int4());
}

#[test]
fn range_function_with_ordinality_appends_bigint_column() {
    let db = setup();
    let sql = "SELECT n, ord FROM generate_series(1, 3) WITH ORDINALITY AS t(n, ord)";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "n").pg_type, int4());
    assert_eq!(col(&info, "ord").pg_type, int8());
    assert!(!col(&info, "ord").nullable);
}

// ── (func(...)).field — indirection over a SRF with out_args ─────────────────

#[test]
fn indirection_on_srf_with_out_args_resolves_named_field() {
    let db = setup();
    let sql = "SELECT (pg_options_to_table(ARRAY['a=b']::text[])).option_name AS opt";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "opt").pg_type, text());
}

#[test]
fn indirection_on_srf_unknown_field_errors() {
    let db = setup();
    let sql = "SELECT (pg_options_to_table(ARRAY['a=b']::text[])).nao_existe";
    assert!(db.analyze(sql).is_err());
}

// ── Record field via subquery column (record_fields propagation) ─────────────

#[test]
fn indirection_on_subquery_record_column() {
    // _pg_expandarray returns a record with named out_args. When held in a
    // subquery column and referenced via `(ta.x).n`, the analyzer must resolve
    // through the record_fields carried on the scope column.
    let db = setup();
    let sql = "SELECT (ta.x).n AS idx \
               FROM (SELECT information_schema._pg_expandarray(ARRAY[1, 2]) AS x) ta";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "idx").pg_type, int4());
}

// ── VALUES (…) in FROM with column alias list ────────────────────────────────

#[test]
fn values_in_from_with_column_alias_list() {
    let db = setup();
    let sql = "SELECT em.num, em.text \
               FROM (VALUES (4, 'INSERT'::text), (8, 'DELETE'::text)) AS em(num, text)";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "num").pg_type, int4());
    assert_eq!(col(&info, "text").pg_type, text());
}

// ── LATERAL — inner subquery sees outer FROM scope ───────────────────────────

#[test]
fn lateral_subquery_sees_outer_scope() {
    let db = setup();
    let sql = "SELECT u.name, s.double_id \
               FROM users u, LATERAL (SELECT u.id * 2 AS double_id) s";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "name").pg_type, text());
    assert_eq!(col(&info, "double_id").pg_type, int8());
}

// ── Subquery column alias list overrides inner names ─────────────────────────

#[test]
fn subquery_column_alias_list_overrides_inner_names() {
    let db = setup();
    let sql = "SELECT x.user_id, x.user_name \
               FROM (SELECT id, name FROM users) AS x(user_id, user_name)";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "user_id").pg_type, int8());
    assert_eq!(col(&info, "user_name").pg_type, text());
}

// ── EXISTS without FROM ──────────────────────────────────────────────────────

#[test]
fn exists_without_from() {
    let db = setup();
    // SELECT EXISTS(...) without FROM — always returns exactly 1 row, never NULL.
    let sql = "SELECT EXISTS(SELECT 1 FROM users WHERE id = $p1)";
    let info = db.analyze(sql).unwrap();
    assert_eq!(info.columns.len(), 1);
    assert!(!info.columns[0].nullable, "EXISTS should be NOT NULL");
    assert_eq!(info.columns[0].pg_type, bool_ty());
}

#[test]
fn exists_constant_without_from() {
    let db = setup();
    // SELECT EXISTS(SELECT 1) — pure constant, no table reference at all.
    let sql = "SELECT EXISTS(SELECT 1)";
    let info = db.analyze(sql).unwrap();
    assert_eq!(info.columns.len(), 1);
    assert!(!info.columns[0].nullable, "EXISTS should be NOT NULL");
    assert_eq!(info.columns[0].pg_type, bool_ty());
}

// ── Annotations on LEFT JOIN ─────────────────────────────────────────────────

#[test]
fn stress_annotation_on_left_join_star() {
    // Force nullable LEFT JOIN column to NOT NULL via annotation.
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE users (
            id   BIGINT PRIMARY KEY,
            name TEXT NOT NULL
         );
         CREATE TABLE posts (
            id      BIGINT PRIMARY KEY,
            user_id BIGINT NOT NULL,
            title   TEXT NOT NULL
         );",
    )
    .unwrap();
    let sql = "SELECT u.name, p.title as \"title!\" \
               FROM users u LEFT JOIN posts p ON p.user_id = u.id";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "title").nullable);
}
