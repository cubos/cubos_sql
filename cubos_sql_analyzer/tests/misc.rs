//! Miscellaneous tests: UNKNOWN type resolution in function calls and
//! snapshot serialization roundtrip.

mod common;
use common::*;

// ──────────────────────────────────────────────────────────────────────────────
// Tests: UNKNOWN type resolution in function calls
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn unknown_literal_in_function_call() {
    let db = setup();
    // ', ' is UNKNOWN type — should resolve string_agg(text, text) unambiguously.
    let sql = "SELECT post_id, string_agg(author_name, ', ') as authors \
               FROM comments GROUP BY post_id";
    let info = db.analyze(sql, &default_config()).unwrap();
    assert_eq!(col(&info, "authors").rust_type, "String");
}

#[test]
fn unknown_literal_in_replace() {
    let db = setup();
    // replace(text, text, text) — two UNKNOWN literals.
    let sql = "SELECT replace(name, 'foo', 'bar') as replaced FROM users";
    let info = db.analyze(sql, &default_config()).unwrap();
    assert_eq!(col(&info, "replaced").rust_type, "String");
    assert!(!col(&info, "replaced").nullable);
}

#[test]
fn unknown_literal_in_position() {
    let db = setup();
    // position(text in text) — UNKNOWN in first arg.
    let sql = "SELECT position('x' in name) as pos FROM users";
    let info = db.analyze(sql, &default_config()).unwrap();
    assert!(!col(&info, "pos").nullable);
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: UNKNOWN type resolution in operators
// ──────────────────────────────────────────────────────────────────────────────

/// jsonb `?` operator with both sides UNKNOWN resolves to `jsonb ? text → bool`
/// (unique candidate), which then types the param as jsonb.
#[test]
fn unknown_operator_jsonb_exists() {
    let db = setup();
    let sql = "SELECT id FROM users WHERE preferences ? 'theme'";
    let info = db.analyze(sql, &default_config()).unwrap();
    assert_eq!(col(&info, "id").rust_type, "i64");
}

/// jsonb `->>` with a typed left side and UNKNOWN right resolves to
/// `jsonb ->> text → text` (text-fallback disambiguation).
#[test]
fn unknown_operator_jsonb_arrow_text() {
    let db = setup();
    let sql = "SELECT preferences->>'theme' as theme FROM users";
    let info = db.analyze(sql, &default_config()).unwrap();
    assert_eq!(col(&info, "theme").rust_type, "String");
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
    let info = db.analyze(sql, &default_config()).unwrap();
    // $p1 should be inferred as jsonb via the `?` operator
    assert_eq!(info.params[0].rust_type, "::serde_json::Value");
    assert_eq!(info.params[1].rust_type, "i64");
    assert_eq!(info.params[2].rust_type, "String");
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
    let info = db.analyze(sql, &default_config()).unwrap();
    assert_eq!(info.params[0].rust_type, "::serde_json::Value");
    assert_eq!(info.params[1].rust_type, "i64");
    assert_eq!(info.params[2].rust_type, "String");
}

/// Operator `->` (returns jsonb) with UNKNOWN right side should resolve.
#[test]
fn unknown_operator_jsonb_arrow() {
    let db = setup();
    let sql = "SELECT preferences->'theme' as theme FROM users";
    let info = db.analyze(sql, &default_config()).unwrap();
    // -> returns jsonb (when left is jsonb)
    assert_eq!(col(&info, "theme").rust_type, "::serde_json::Value");
}

/// Query against pg_catalog table with obj_description function.
#[test]
fn pg_catalog_obj_description_with_param() {
    let db = setup();
    let sql = "SELECT obj_description(oid) as comment FROM pg_namespace WHERE nspname = $p1";
    let info = db.analyze(sql, &default_config()).unwrap();
    // obj_description returns nullable text → Option<String>
    assert_eq!(col(&info, "comment").rust_type, "String");
    assert!(col(&info, "comment").nullable);
    // $p1 compared with nspname (type name) → String
    assert_eq!(info.params[0].rust_type, "String");
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: `alias.*` in expression context (composite type)
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn star_expr_resolves_to_composite_type_via_row_to_json() {
    let db = setup();
    // `u.*` here feeds into row_to_json, which takes `record` / any composite.
    // The analyzer should recognize the composite and produce JSON.
    let sql = "SELECT row_to_json(u.*) AS payload FROM users u";
    let info = db.analyze(sql, &default_config()).unwrap();
    assert_eq!(col(&info, "payload").rust_type, "::serde_json::Value");
}

#[test]
fn star_expr_not_null_because_row_is_always_present() {
    let db = setup();
    let sql = "SELECT row_to_json(u.*) AS payload FROM users u";
    let info = db.analyze(sql, &default_config()).unwrap();
    // `alias.*` is a composite value that exists iff the row exists, which
    // by definition it does for every returned tuple → NOT NULL.
    assert!(!col(&info, "payload").nullable);
}

#[test]
fn star_expr_on_cte_is_unsupported() {
    let db = setup();
    // CTE rows don't have a registered composite type — analyzer should error.
    let sql = "WITH u AS (SELECT id, name FROM users) \
               SELECT row_to_json(u.*) FROM u";
    let err = db.analyze(sql, &default_config()).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("CTE") || msg.contains("subquery") || msg.contains("real relation"),
        "expected CTE/subquery error, got: {msg}",
    );
}

#[test]
fn star_expr_on_unknown_alias_fails() {
    let db = setup();
    let sql = "SELECT row_to_json(nope.*) FROM users u";
    assert!(db.analyze(sql, &default_config()).is_err());
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: AIndirection — `(expr).field` and `arr[i]`
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn indirection_field_on_composite_column() {
    // A table's composite type is registered under the table's QN. Selecting
    // a composite-typed column is rare in user code but well-defined: the
    // column's declared type IS the composite. `((SELECT u FROM users u …)).name`
    // stresses the subquery→composite→field chain.
    let db = setup();
    let sql = "SELECT (u).name AS the_name FROM users u";
    let info = db.analyze(sql, &default_config()).unwrap();
    assert_eq!(col(&info, "the_name").rust_type, "String");
}

#[test]
fn indirection_field_nullability_honors_field_not_null() {
    let db = setup();
    // `users.age` is nullable in the shared fixture; `.age` field of the
    // composite should preserve that.
    let sql = "SELECT (u).age AS the_age FROM users u";
    let info = db.analyze(sql, &default_config()).unwrap();
    assert!(col(&info, "the_age").nullable);
}

#[test]
fn indirection_field_unknown_errors() {
    let db = setup();
    let sql = "SELECT (u).nao_existe FROM users u";
    assert!(db.analyze(sql, &default_config()).is_err());
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: ARRAY(SELECT …) sublink
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn array_sublink_of_text_returns_text_array() {
    let db = setup();
    let sql = "SELECT ARRAY(SELECT name FROM users) AS names";
    let info = db.analyze(sql, &default_config()).unwrap();
    assert_eq!(col(&info, "names").rust_type, "Vec<String>");
    // An ARRAY() subquery always returns a non-null array (empty if no rows).
    assert!(!col(&info, "names").nullable);
}

#[test]
fn array_sublink_of_int4_returns_int4_array() {
    let db = setup();
    let sql = "SELECT ARRAY(SELECT age FROM users WHERE age IS NOT NULL) AS ages";
    let info = db.analyze(sql, &default_config()).unwrap();
    assert_eq!(col(&info, "ages").rust_type, "Vec<i32>");
    assert!(!col(&info, "ages").nullable);
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: `FROM func(...)` — RangeFunction
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn range_function_scalar_srf_exposes_single_column() {
    let db = setup();
    // `generate_series(int4, int4)` is a scalar SRF with no named out-args.
    // The FROM clause should expose a single column named after the function.
    let sql = "SELECT generate_series FROM generate_series(1, 10)";
    let info = db.analyze(sql, &default_config()).unwrap();
    assert_eq!(col(&info, "generate_series").rust_type, "i32");
}

#[test]
fn range_function_with_out_args_exposes_named_columns() {
    let db = setup();
    // `pg_options_to_table(text[])` is declared with TABLE(option_name text,
    // option_value text). Both columns must be visible from the FROM scope.
    let sql = "SELECT option_name, option_value \
               FROM pg_options_to_table(ARRAY['a=b', 'c=d']::text[])";
    let info = db.analyze(sql, &default_config()).unwrap();
    assert_eq!(col(&info, "option_name").rust_type, "String");
    assert_eq!(col(&info, "option_value").rust_type, "String");
}

#[test]
fn range_function_column_alias_list_overrides_names() {
    let db = setup();
    let sql = "SELECT n FROM generate_series(1, 5) AS t(n)";
    let info = db.analyze(sql, &default_config()).unwrap();
    assert_eq!(col(&info, "n").rust_type, "i32");
}

#[test]
fn range_function_with_ordinality_appends_bigint_column() {
    let db = setup();
    let sql = "SELECT n, ord FROM generate_series(1, 3) WITH ORDINALITY AS t(n, ord)";
    let info = db.analyze(sql, &default_config()).unwrap();
    assert_eq!(col(&info, "n").rust_type, "i32");
    assert_eq!(col(&info, "ord").rust_type, "i64");
    assert!(!col(&info, "ord").nullable);
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: (func(...)).field — AIndirection over a SRF with out_args
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn indirection_on_srf_with_out_args_resolves_named_field() {
    let db = setup();
    let sql = "SELECT (pg_options_to_table(ARRAY['a=b']::text[])).option_name AS opt";
    let info = db.analyze(sql, &default_config()).unwrap();
    assert_eq!(col(&info, "opt").rust_type, "String");
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: VALUES (…) in FROM with column alias list
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn values_in_from_with_column_alias_list() {
    let db = setup();
    // `VALUES` as a derived relation named via `AS em(num, text)`. The
    // analyzer must see `em.num` and `em.text` in scope.
    let sql = "SELECT em.num, em.text \
               FROM (VALUES (4, 'INSERT'::text), (8, 'DELETE'::text)) AS em(num, text)";
    let info = db.analyze(sql, &default_config()).unwrap();
    assert_eq!(col(&info, "num").rust_type, "i32");
    assert_eq!(col(&info, "text").rust_type, "String");
}

#[test]
fn lateral_subquery_sees_outer_scope() {
    let db = setup();
    // `LATERAL` lets the subquery reference `u.id` from the outer FROM
    // clause. Without LATERAL semantics wired up, the inner `u.id` would
    // fail to resolve.
    let sql = "SELECT u.name, s.double_id \
               FROM users u, LATERAL (SELECT u.id * 2 AS double_id) s";
    let info = db.analyze(sql, &default_config()).unwrap();
    assert_eq!(col(&info, "name").rust_type, "String");
    assert_eq!(col(&info, "double_id").rust_type, "i64");
}

#[test]
fn subquery_column_alias_list_overrides_inner_names() {
    let db = setup();
    // `FROM (SELECT id, name FROM users) x(user_id, user_name)` — outer
    // scope must see the aliases, not the subquery's own column names.
    let sql = "SELECT x.user_id, x.user_name \
               FROM (SELECT id, name FROM users) AS x(user_id, user_name)";
    let info = db.analyze(sql, &default_config()).unwrap();
    assert_eq!(col(&info, "user_id").rust_type, "i64");
    assert_eq!(col(&info, "user_name").rust_type, "String");
}

#[test]
fn indirection_on_subquery_record_column() {
    // `_pg_expandarray(…)` returns a record with named out_args. When its
    // result is held in a subquery column and referenced via `(ta.x).n`
    // from the outer SELECT, the analyzer must look up the field through
    // the record_fields carried on the scope column — not the opaque OID.
    let db = setup();
    let sql = "SELECT (ta.x).n AS idx \
               FROM (SELECT information_schema._pg_expandarray(ARRAY[1, 2]) AS x) ta";
    let info = db.analyze(sql, &default_config()).unwrap();
    assert_eq!(col(&info, "idx").rust_type, "i32");
}

#[test]
fn indirection_on_srf_unknown_field_errors() {
    let db = setup();
    let sql = "SELECT (pg_options_to_table(ARRAY['a=b']::text[])).nao_existe";
    assert!(db.analyze(sql, &default_config()).is_err());
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: snapshot serialization roundtrip
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn snapshot_roundtrip() {
    use cubos_sql_analyzer::schema::SchemaSnapshot;

    let db = setup();
    let snapshot = db.snapshot();

    let json = serde_json::to_string(snapshot).unwrap();
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

    // Analyze through a Database backed by the restored snapshot: same result.
    let restored_db = Database::from_snapshot(restored);
    let config = default_config();
    let sql = "SELECT id, name FROM users";
    let info1 = db.analyze(sql, &config).unwrap();
    let info2 = restored_db.analyze(sql, &config).unwrap();
    assert_identical(&info1, &info2, "snapshot roundtrip");
}
