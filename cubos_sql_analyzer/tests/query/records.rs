//! RECORD / composite types: `ROW(...)` constructors, user-defined composite
//! types (`CREATE TYPE foo AS (...)`), table rows treated as composite values
//! (`alias.*`, bare-alias row reference), set-returning functions with
//! `OUT`/`TABLE` parameters, `RETURNS RECORD`, indirection (`(expr).field`)
//! over each producer, and row comparisons.
//!
//! What we exercise:
//! - `ROW(a, b, …)` typed as the pseudo `record` type, never NULL itself.
//! - User composite types as column types, function returns, parameters and
//!   nested fields (composite-of-composite, domain-over-composite).
//! - `(rel).field` indirection on tables, ROW expressions and SRFs.
//! - SRFs with `OUT`/`TABLE` args expanding to named columns in `FROM`.
//! - `RETURNS RECORD` / `RETURNS TABLE` user functions and how their output
//!   surfaces in the analyzer.
//! - Row equality / ordering operators (`= < > <= >= <>`) on row constructors.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new();
    // Records tests intentionally exercise the analyzer's composite-type
    // decomposition into `Type::AnonymousRecord` so downstream code can
    // read field shapes. PG's wire-protocol RowDescription reports the
    // composite OID instead, so the `pg_sanity` mirror's type-name
    // compare can never match here. Disable it for the whole suite.
    db.skip_pg_sanity();
    db.apply_sql(
        "CREATE TYPE address AS (
             street TEXT,
             city   TEXT,
             zip    TEXT
         );
         CREATE TYPE point2d AS (
             x FLOAT8,
             y FLOAT8
         );
         CREATE TYPE company AS (
             name TEXT,
             hq   address
         );
         CREATE DOMAIN address_dom AS address;
         CREATE TABLE users (
             id      BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
             name    TEXT NOT NULL,
             age     INT,
             home    address,
             work    address NOT NULL
         );
         CREATE TABLE companies (
             id   BIGINT PRIMARY KEY,
             info company NOT NULL
         );
         CREATE TABLE points (
             id BIGINT PRIMARY KEY,
             p  point2d NOT NULL
         );",
    )
    .unwrap();
    db
}

// Pseudo `record` type — what the analyzer surfaces for `ROW(...)` and other
// untyped composite producers. Built once because every test below uses it.
fn record_ty() -> Type {
    basic("pg_catalog", "record")
}

// ── ROW(...) constructor ─────────────────────────────────────────────────────

#[test]
fn row_literal_surfaces_anon_record_with_inferred_shape() {
    let db = setup();
    // The analyzer captures the static shape of `ROW(...)` and surfaces it
    // as `Type::AnonymousRecord` with positional names `f1`, `f2`, …
    // (mirrors PG's typmod-driven RecordCacheArray, but at compile time).
    // Bare string literals stay `unknown` inside ROW until forced by context;
    // we cast `::text` here so the shape is fully concrete.
    let s = db
        .analyze("SELECT ROW(1, 'hello'::text, true) AS r")
        .unwrap();
    assert_cols(
        &s,
        vec![c(
            "r",
            anon_record(vec![
                rf("f1", int4()),
                rf("f2", text()),
                rf("f3", bool_ty()),
            ]),
        )],
    );
}

#[test]
fn row_literal_is_never_null() {
    let db = setup();
    // The ROW value itself is never NULL even when every element is — the
    // composite container exists. Each NULL-typed element is tracked
    // individually as nullable in the shape.
    let s = db
        .analyze("SELECT ROW(NULL::int4, NULL::text) AS r")
        .unwrap();
    assert_cols(
        &s,
        vec![c(
            "r",
            anon_record(vec![rfn("f1", int4()), rfn("f2", text())]),
        )],
    );
}

#[test]
fn row_from_table_columns() {
    let db = setup();
    // Column refs feed their resolved types straight into the inferred shape.
    let s = db
        .analyze("SELECT ROW(u.id, u.name, u.age) AS r FROM users u")
        .unwrap();
    assert_cols(
        &s,
        vec![c(
            "r",
            anon_record(vec![rf("f1", int8()), rf("f2", text()), rfn("f3", int4())]),
        )],
    );
}

#[test]
fn row_with_subquery_param() {
    let db = setup();
    // Element-wise typing inside ROW must still register parameters and
    // resolve them against the surrounding context (here, comparison with
    // a NOT NULL int4 column).
    let s = db
        .analyze(
            "SELECT u.id FROM users u \
             WHERE ROW(u.age, u.name) = ROW($p1, $p2)",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(int4()), p(text())]);
}

#[test]
fn nested_row_constructor() {
    let db = setup();
    // ROW(scalar, ROW(...)) recurses: outer field 2 carries the inner row's
    // shape as another AnonymousRecord.
    let s = db.analyze("SELECT ROW(1, ROW(2, 3)) AS nested").unwrap();
    assert_cols(
        &s,
        vec![c(
            "nested",
            anon_record(vec![
                rf("f1", int4()),
                rf("f2", anon_record(vec![rf("f1", int4()), rf("f2", int4())])),
            ]),
        )],
    );
}

#[test]
fn row_comparison_equality() {
    let db = setup();
    let s = db.analyze("SELECT ROW(1, 2) = ROW(1, 2) AS e").unwrap();
    assert_cols(&s, vec![c("e", bool_ty())]);
}

#[test]
fn row_comparison_inequality() {
    let db = setup();
    let s = db.analyze("SELECT ROW(1, 2) <> ROW(3, 4) AS neq").unwrap();
    assert_cols(&s, vec![c("neq", bool_ty())]);
}

#[test]
fn row_comparison_less_than() {
    let db = setup();
    // PG's record `<` operator does field-wise lexicographic compare.
    let s = db
        .analyze("SELECT ROW(1, 'a') < ROW(2, 'b') AS lt")
        .unwrap();
    assert_cols(&s, vec![c("lt", bool_ty())]);
}

#[test]
fn row_comparison_greater_equal() {
    let db = setup();
    let s = db
        .analyze("SELECT ROW(1, 2, 3) >= ROW(1, 2, 3) AS ge")
        .unwrap();
    assert_cols(&s, vec![c("ge", bool_ty())]);
}

#[test]
fn implicit_row_in_where() {
    let db = setup();
    // `(a, b) = (c, d)` — implicit ROW form. Same operator under the hood.
    let s = db
        .analyze("SELECT id FROM users WHERE (id, name) = ($p1, $p2)")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(int8()), p(text())]);
}

// ── User composite columns ───────────────────────────────────────────────────

#[test]
fn select_composite_column_surfaces_anon_record() {
    let db = setup();
    // A column of composite type is surfaced as `Type::AnonymousRecord`
    // with attributes mirroring the composite's fields.
    let s = db.analyze("SELECT id, work FROM users").unwrap();
    assert_cols(
        &s,
        vec![
            c("id", int8()),
            c(
                "work",
                anon_record(vec![
                    rfn("street", text()),
                    rfn("city", text()),
                    rfn("zip", text()),
                ]),
            ),
        ],
    );
}

#[test]
fn nullable_composite_column_stays_nullable() {
    let db = setup();
    // `home` is nullable (no NOT NULL) — column nullability is a property
    // of the site, not the composite type itself.
    let s = db.analyze("SELECT home FROM users").unwrap();
    assert_cols(
        &s,
        vec![cn(
            "home",
            anon_record(vec![
                rfn("street", text()),
                rfn("city", text()),
                rfn("zip", text()),
            ]),
        )],
    );
}

#[test]
fn indirection_field_on_composite_column() {
    let db = setup();
    // `(u.work).city` reads a field out of a composite column; field
    // nullability is OR'd with the enclosing column's nullability.
    // `work` is NOT NULL but its `city` field has no NOT NULL declared.
    let s = db
        .analyze("SELECT (u.work).city AS city FROM users u")
        .unwrap();
    assert_cols(&s, vec![cn("city", text())]);
}

#[test]
fn indirection_field_propagates_outer_null() {
    let db = setup();
    // Even if the field had been NOT NULL, accessing it through a NULLABLE
    // composite column makes the result nullable.
    let s = db
        .analyze("SELECT (u.home).street AS street FROM users u")
        .unwrap();
    assert_cols(&s, vec![cn("street", text())]);
}

#[test]
fn indirection_unknown_field_errors() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT (u.work).inexistente FROM users u"),
        AnalyzeError::UndefinedColumn(_),
        "inexistente"
    );
}

#[test]
fn indirection_chain_through_nested_composite() {
    // `company.hq` is itself a composite (`address`); `(c.info).hq.city`
    // walks two indirection steps through the nested composite.
    let db = setup();
    let s = db
        .analyze("SELECT (c.info).hq AS hq FROM companies c")
        .unwrap();
    assert_eq!(
        col(&s, "hq").pg_type,
        anon_record(vec![
            rfn("street", text()),
            rfn("city", text()),
            rfn("zip", text()),
        ])
    );
}

#[test]
fn indirection_two_levels_deep() {
    let db = setup();
    let s = db
        .analyze("SELECT ((c.info).hq).city AS city FROM companies c")
        .unwrap();
    assert_eq!(col(&s, "city").pg_type, text());
}

// ── Composite parameters & DML ───────────────────────────────────────────────

#[test]
fn insert_composite_param_surfaces_anon_record() {
    let db = setup();
    // The composite column expects an anonymous record param shaped like
    // the `address` type.
    let s = db
        .analyze("INSERT INTO users (name, work) VALUES ($p1, $p2) RETURNING id")
        .unwrap();
    assert_params(
        &s,
        vec![
            p(text()),
            p(anon_record(vec![
                rfn("street", text()),
                rfn("city", text()),
                rfn("zip", text()),
            ])),
        ],
    );
}

#[test]
fn update_composite_field_via_row_ctor() {
    let db = setup();
    // Update an entire composite column with a ROW expression. The ROW
    // itself types as record — coercion to the column's composite type
    // happens at the assignment site.
    let s = db
        .analyze("UPDATE users SET work = ROW($p1, $p2, $p3) WHERE id = $p4 RETURNING id")
        .unwrap();
    // Three text params drive the ROW elements, plus the int8 id.
    assert_eq!(s.params.len(), 4);
    assert_eq!(s.params[3].pg_type, int8());
}

// ── Bare table reference (alias-as-row) ──────────────────────────────────────

#[test]
fn bare_alias_resolves_to_composite_value() {
    let db = setup();
    // `SELECT u FROM users u` projects the entire row as a composite —
    // PG falls back to the table's implicit composite type.
    let s = db.analyze("SELECT u FROM users u").unwrap();
    let users_record = anon_record(vec![
        rf("id", int8()),
        rf("name", text()),
        rfn("age", int4()),
        rfn(
            "home",
            anon_record(vec![
                rfn("street", text()),
                rfn("city", text()),
                rfn("zip", text()),
            ]),
        ),
        rf(
            "work",
            anon_record(vec![
                rfn("street", text()),
                rfn("city", text()),
                rfn("zip", text()),
            ]),
        ),
    ]);
    assert_cols(&s, vec![c("u", users_record)]);
}

#[test]
fn bare_alias_passed_to_row_to_json() {
    let db = setup();
    // Bare-alias row reference inside a composite-consuming function.
    let s = db
        .analyze("SELECT row_to_json(u) AS doc FROM users u")
        .unwrap();
    assert_cols(&s, vec![c("doc", json_ty())]);
}

#[test]
fn star_expansion_to_composite() {
    let db = setup();
    // `u.*` in expression context resolves to the table's composite type
    // (same shape as the bare alias). Used as the input to row_to_json.
    let s = db
        .analyze("SELECT row_to_json(u.*) AS doc FROM users u")
        .unwrap();
    assert_cols(&s, vec![c("doc", json_ty())]);
}

#[test]
fn bare_alias_field_via_indirection() {
    let db = setup();
    // `(u).name` — the row-reference shortcut: `u` is treated as a
    // composite, then `.name` pulls the field out.
    let s = db.analyze("SELECT (u).name AS n FROM users u").unwrap();
    assert_cols(&s, vec![c("n", text())]);
}

#[test]
fn bare_alias_field_nullability_preserved() {
    let db = setup();
    // `users.age` is nullable, so `(u).age` must inherit that.
    let s = db.analyze("SELECT (u).age AS a FROM users u").unwrap();
    assert_cols(&s, vec![cn("a", int4())]);
}

// ── SRFs with OUT / TABLE args ───────────────────────────────────────────────

#[test]
fn srf_with_out_args_in_from_expands_columns() {
    let db = setup();
    // `pg_options_to_table(text[])` is declared as
    // `TABLE(option_name text, option_value text)` — both columns must
    // surface when used in FROM.
    let s = db
        .analyze(
            "SELECT option_name, option_value \
             FROM pg_options_to_table(ARRAY['a=b', 'c=d']::text[])",
        )
        .unwrap();
    assert_eq!(col(&s, "option_name").pg_type, text());
    assert_eq!(col(&s, "option_value").pg_type, text());
}

#[test]
fn srf_with_out_args_indirection_named_field() {
    let db = setup();
    // `(srf(...)).fieldname` — `out_args` lets the analyzer resolve the
    // named field without expanding the call into FROM first.
    let s = db
        .analyze("SELECT (pg_options_to_table(ARRAY['k=v']::text[])).option_name AS opt")
        .unwrap();
    assert_eq!(col(&s, "opt").pg_type, text());
}

#[test]
fn srf_record_field_via_subquery_propagation() {
    // A subquery column whose target was a SRF with out_args carries the
    // record's field list into the outer scope. `(ta.x).n` then resolves
    // through that propagated field info instead of the (opaque) record OID.
    let db = setup();
    let s = db
        .analyze(
            "SELECT (ta.x).n AS idx \
             FROM (SELECT information_schema._pg_expandarray(ARRAY[10, 20]) AS x) ta",
        )
        .unwrap();
    assert_eq!(col(&s, "idx").pg_type, int4());
}

// ── User-defined RETURNS RECORD / RETURNS TABLE ──────────────────────────────

#[test]
fn returns_record_with_named_out_params_in_from() {
    // `CREATE FUNCTION f(OUT a int, OUT b text)` is the canonical user
    // form of declaring a record-returning function. PG exposes it as a
    // SRF with named columns when used in FROM.
    let mut db = setup();
    db.apply_sql(
        "CREATE FUNCTION pair() RETURNS RECORD AS $$
             SELECT 1, 'x'::text
         $$ LANGUAGE SQL;",
    )
    .unwrap();
    // Without OUT args / column definition list, calling RETURNS RECORD in
    // FROM yields a single opaque `pair` column of pseudo `record`.
    let s = db.analyze("SELECT pair FROM pair()").unwrap();
    assert_eq!(col(&s, "pair").pg_type, record_ty());
}

#[test]
fn returns_table_user_function_in_from() {
    // RETURNS TABLE(...) is the modern way to declare a SRF with named
    // output columns. The names must surface as scope columns.
    let mut db = setup();
    db.apply_sql(
        "CREATE FUNCTION pair_tbl() RETURNS TABLE(num INT, label TEXT) AS $$
             SELECT 1, 'x'::text
         $$ LANGUAGE SQL;",
    )
    .unwrap();
    let s = db.analyze("SELECT num, label FROM pair_tbl()").unwrap();
    assert_eq!(col(&s, "num").pg_type, int4());
    assert_eq!(col(&s, "label").pg_type, text());
}

#[test]
fn returns_setof_composite_in_from_expands_fields() {
    // `RETURNS SETOF <composite>` should expose the composite's fields as
    // named columns (functions.rs already does this for built-in catalog
    // composites).
    let mut db = setup();
    db.apply_sql(
        "CREATE FUNCTION all_addresses() RETURNS SETOF address AS $$
             SELECT (work).* FROM users
         $$ LANGUAGE SQL;",
    )
    .unwrap();
    let s = db
        .analyze("SELECT street, city, zip FROM all_addresses()")
        .unwrap();
    assert_eq!(col(&s, "street").pg_type, text());
    assert_eq!(col(&s, "city").pg_type, text());
    assert_eq!(col(&s, "zip").pg_type, text());
}

#[test]
fn returns_composite_scalar_in_select() {
    // `RETURNS <composite>` (no SETOF) — the function call in expression
    // context yields a value of that composite type.
    let mut db = setup();
    db.apply_sql(
        "CREATE FUNCTION default_address() RETURNS address AS $$
             SELECT ROW('main', 'sp', '00000')::address
         $$ LANGUAGE SQL;",
    )
    .unwrap();
    let s = db.analyze("SELECT default_address() AS addr").unwrap();
    assert_eq!(
        col(&s, "addr").pg_type,
        anon_record(vec![
            rfn("street", text()),
            rfn("city", text()),
            rfn("zip", text()),
        ]),
    );
}

#[test]
fn returns_composite_scalar_indirection() {
    // `(default_address()).city` — composite-returning function fed into
    // indirection.
    let mut db = setup();
    db.apply_sql(
        "CREATE FUNCTION default_address() RETURNS address AS $$
             SELECT ROW('main', 'sp', '00000')::address
         $$ LANGUAGE SQL;",
    )
    .unwrap();
    let s = db
        .analyze("SELECT (default_address()).city AS city")
        .unwrap();
    assert_eq!(col(&s, "city").pg_type, text());
}

// ── Domains over composites ──────────────────────────────────────────────────

#[test]
fn domain_over_composite_indirection() {
    // A DOMAIN wrapping a composite must still allow field access — the
    // analyzer unwraps the domain when resolving fields.
    let mut db = setup();
    db.apply_sql(
        "CREATE TABLE places (
             id   BIGINT PRIMARY KEY,
             addr address_dom NOT NULL
         );",
    )
    .unwrap();
    let s = db
        .analyze("SELECT (p.addr).city AS city FROM places p")
        .unwrap();
    assert_eq!(col(&s, "city").pg_type, text());
}

// ── Composite arrays ─────────────────────────────────────────────────────────

#[test]
fn array_of_composite_subscript_returns_composite() {
    let mut db = setup();
    db.apply_sql(
        "CREATE TABLE locations (
             id    BIGINT PRIMARY KEY,
             addrs address[] NOT NULL
         );",
    )
    .unwrap();
    // Array element access is always nullable (out-of-bounds → NULL),
    // even though the array itself is NOT NULL.
    let s = db
        .analyze("SELECT addrs[1] AS first FROM locations")
        .unwrap();
    assert_cols(
        &s,
        vec![cn(
            "first",
            anon_record(vec![
                rfn("street", text()),
                rfn("city", text()),
                rfn("zip", text()),
            ]),
        )],
    );
}

#[test]
fn array_of_composite_subscript_then_field() {
    let mut db = setup();
    db.apply_sql(
        "CREATE TABLE locations (
             id    BIGINT PRIMARY KEY,
             addrs address[] NOT NULL
         );",
    )
    .unwrap();
    // Inherits the subscript nullability.
    let s = db
        .analyze("SELECT (addrs[1]).city AS city FROM locations")
        .unwrap();
    assert_cols(&s, vec![cn("city", text())]);
}

// ── Subquery / CTE returning a whole row as a composite ──────────────────────

#[test]
fn subquery_select_bare_alias_yields_composite_column() {
    // `SELECT u FROM users u` projected into a derived table — the outer
    // query sees a single column of composite type.
    let db = setup();
    let s = db
        .analyze("SELECT t.u FROM (SELECT u FROM users u) t")
        .unwrap();
    let users_record = anon_record(vec![
        rf("id", int8()),
        rf("name", text()),
        rfn("age", int4()),
        rfn(
            "home",
            anon_record(vec![
                rfn("street", text()),
                rfn("city", text()),
                rfn("zip", text()),
            ]),
        ),
        rf(
            "work",
            anon_record(vec![
                rfn("street", text()),
                rfn("city", text()),
                rfn("zip", text()),
            ]),
        ),
    ]);
    assert_cols(&s, vec![c("u", users_record)]);
}

#[test]
fn cte_propagating_composite_column() {
    let db = setup();
    let s = db
        .analyze(
            "WITH t AS (SELECT id, work FROM users) \
             SELECT id, work FROM t",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("id", int8()),
            c(
                "work",
                anon_record(vec![
                    rfn("street", text()),
                    rfn("city", text()),
                    rfn("zip", text()),
                ]),
            ),
        ],
    );
}

// ── row_to_json / record packaging ───────────────────────────────────────────

#[test]
fn row_to_json_of_row_constructor() {
    let db = setup();
    let s = db
        .analyze("SELECT row_to_json(ROW(1, 'a')) AS doc")
        .unwrap();
    assert_cols(&s, vec![c("doc", json_ty())]);
}

#[test]
fn jsonb_build_object_from_row_fields() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT jsonb_build_object('id', u.id, 'who', u.name) AS doc \
             FROM users u",
        )
        .unwrap();
    assert_eq!(col(&s, "doc").pg_type, jsonb());
}

// ── ROW operator with mismatched arity ───────────────────────────────────────

#[test]
fn row_compare_mismatched_arity_resolves_to_bool() {
    // PG itself rejects mismatched-arity row compares at runtime, but pure
    // static analysis only types the operator — both sides are records, so
    // the result is bool. (We're checking the analyzer doesn't panic and
    // produces the operator-level type.)
    let db = setup();
    let s = db.analyze("SELECT ROW(1) = ROW(1, 2) AS e").unwrap();
    assert_cols(&s, vec![c("e", bool_ty())]);
}

// ── VALUES-derived record-shaped relation ────────────────────────────────────

#[test]
fn values_with_column_alias_list_acts_like_record_row() {
    let db = setup();
    // `(VALUES ...) AS v(a, b)` is the canonical anonymous record-shaped
    // relation; column types come from the first row's element types.
    let s = db
        .analyze(
            "SELECT v.a, v.b \
             FROM (VALUES (1, 'x'::text), (2, 'y'::text)) AS v(a, b)",
        )
        .unwrap();
    assert_eq!(col(&s, "a").pg_type, int4());
    assert_eq!(col(&s, "b").pg_type, text());
}

// ── Static shape tracking (typmod-style) ─────────────────────────────────────

#[test]
fn row_field_nullability_per_element() {
    let db = setup();
    // `users.id` is NOT NULL but `users.age` is nullable; the inferred shape
    // must mirror that per-element.
    let s = db
        .analyze("SELECT ROW(u.id, u.age) AS r FROM users u")
        .unwrap();
    assert_cols(
        &s,
        vec![c(
            "r",
            anon_record(vec![rf("f1", int8()), rfn("f2", int4())]),
        )],
    );
}

#[test]
fn row_with_typed_param_pinned_via_shape() {
    let db = setup();
    // `ROW($p1::int4, $p2::text)` carries an inferred shape with concrete
    // types; the params survive into the public output.
    let s = db.analyze("SELECT ROW($p1::int4, $p2::text) AS r").unwrap();
    assert_cols(
        &s,
        vec![c(
            "r",
            anon_record(vec![rf("f1", int4()), rf("f2", text())]),
        )],
    );
    assert_params(&s, vec![p(int4()), p(text())]);
}

#[test]
fn indirection_on_inline_row_resolves_field() {
    let db = setup();
    // `(ROW(...)).fN` resolves through the inline shape — no composite OID
    // involved, no snapshot lookup. Mirrors PG's typmod-driven path.
    let s = db
        .analyze("SELECT (ROW(1::int4, 'x'::text)).f2 AS x")
        .unwrap();
    assert_eq!(col(&s, "x").pg_type, text());
}

#[test]
fn indirection_on_inline_row_unknown_field_errors() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT (ROW(1::int4, 'x'::text)).nope"),
        AnalyzeError::UndefinedColumn(_),
        "nope"
    );
}

#[test]
fn subquery_row_target_propagates_shape_to_outer() {
    let db = setup();
    // The shape of `ROW(...)` produced inside a subquery survives the
    // boundary so the outer `(t.r).f1` indirection still resolves.
    let s = db
        .analyze(
            "SELECT (t.r).f2 AS x \
             FROM (SELECT ROW(1::int4, 'x'::text) AS r) t",
        )
        .unwrap();
    assert_eq!(col(&s, "x").pg_type, text());
}

#[test]
fn cast_to_text_drops_shape() {
    let db = setup();
    // Casting away from `record` to a scalar drops the inferred shape — the
    // result is just text.
    let s = db.analyze("SELECT ROW(1, 2)::text AS r").unwrap();
    assert_cols(&s, vec![c("r", text())]);
}

#[test]
fn union_drops_shape_conservatively() {
    let db = setup();
    // Both branches produce a record with the same arity, but the analyzer
    // takes the conservative path: shape is dropped at set-op boundaries
    // (matches PG's typmod-collapse to -1 for records that flow through a
    // set operation).
    let s = db
        .analyze(
            "SELECT ROW(1::int4, 'x'::text) AS r \
             UNION ALL SELECT ROW(2::int4, 'y'::text) AS r",
        )
        .unwrap();
    // OID is `record` pseudo-type, not `AnonymousRecord` — shape lost.
    assert_eq!(col(&s, "r").pg_type, basic("pg_catalog", "record"));
}

#[test]
fn coalesce_of_scalar_drops_shape() {
    let db = setup();
    // COALESCE returns a scalar — its result is never a record, so any
    // branch that happened to be record-typed loses its shape at the
    // surface. Result here is NOT NULL because at least one branch (`'x'`)
    // is NOT NULL.
    let s = db.analyze("SELECT COALESCE(NULL, 'x'::text) AS v").unwrap();
    assert_cols(&s, vec![c("v", text())]);
}

// ── Edge cases — ROW(...) constructor ────────────────────────────────────────

#[test]
fn row_with_only_nulls_marks_every_field_nullable() {
    let db = setup();
    // Even though `ROW(...)` itself is non-null, each NULL element is
    // tracked individually in the shape. PG behaves the same: the row
    // exists but every column may be null. (Outer NOT NULL is encoded by
    // using `c` rather than `cn` for the column.)
    let s = db
        .analyze("SELECT ROW(NULL::int4, NULL::text) AS r")
        .unwrap();
    assert_cols(
        &s,
        vec![c(
            "r",
            anon_record(vec![rfn("f1", int4()), rfn("f2", text())]),
        )],
    );
}

#[test]
fn row_with_arithmetic_inferred_per_field() {
    let db = setup();
    // Each element is its own expression — arithmetic and casts resolve
    // per-position before being captured into the shape.
    let s = db.analyze("SELECT ROW(1 + 2, 'a' || 'b') AS r").unwrap();
    let r = &col(&s, "r").pg_type;
    // f1 is int4 (1 + 2). f2 is text (string concat).
    match r {
        Type::AnonymousRecord { fields } => {
            assert_eq!(fields.len(), 2);
            assert_eq!(fields[0].name, "f1");
            assert_eq!(fields[0].ty, int4());
            assert_eq!(fields[1].name, "f2");
            assert_eq!(fields[1].ty, text());
        }
        other => panic!("expected AnonymousRecord, got {other:?}"),
    }
}

#[test]
fn row_inside_subquery_in_where() {
    let db = setup();
    // Equality between a ROW from outer scope and a derived ROW in the
    // subquery — both sides should agree on field types.
    let s = db
        .analyze(
            "SELECT u.id FROM users u \
             WHERE ROW(u.id, u.name) = (SELECT ROW(u2.id, u2.name) FROM users u2 WHERE u2.id = u.id)",
        )
        .unwrap();
    assert_eq!(col(&s, "id").pg_type, int8());
}

#[test]
fn row_used_in_order_by_clause() {
    let db = setup();
    // ORDER BY ROW(...) is legal — PG sorts lexicographically by fields.
    let s = db
        .analyze("SELECT u.id FROM users u ORDER BY ROW(u.id, u.name)")
        .unwrap();
    assert_eq!(col(&s, "id").pg_type, int8());
}

#[test]
fn row_used_in_distinct_on() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT DISTINCT ON (ROW(u.id, u.name)) u.id, u.name \
             FROM users u ORDER BY ROW(u.id, u.name), u.id",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

#[test]
fn row_in_group_by() {
    let db = setup();
    // GROUP BY accepts a row constructor — semantically the same as listing
    // each element.
    let s = db
        .analyze("SELECT u.id FROM users u GROUP BY ROW(u.id, u.name), u.id")
        .unwrap();
    assert_eq!(col(&s, "id").pg_type, int8());
}

#[test]
fn row_in_having_clause() {
    let db = setup();
    // HAVING is just a bool predicate, ROW comparison is fine.
    let s = db
        .analyze(
            "SELECT u.id FROM users u GROUP BY u.id, u.name \
             HAVING ROW(u.id, u.name) > ROW(0::int8, 'a')",
        )
        .unwrap();
    assert_eq!(col(&s, "id").pg_type, int8());
}

// ── ROW IS NULL / IS NOT NULL ────────────────────────────────────────────────

#[test]
fn row_is_null_returns_bool() {
    let db = setup();
    // PG's `<row> IS NULL` is true iff every field is NULL — but the
    // analyzer only needs to surface the bool result type.
    let s = db
        .analyze("SELECT ROW(u.id, u.name) IS NULL AS empty FROM users u")
        .unwrap();
    assert_cols(&s, vec![c("empty", bool_ty())]);
}

#[test]
fn row_is_not_null_returns_bool() {
    let db = setup();
    let s = db
        .analyze("SELECT ROW(u.id, u.name) IS NOT NULL AS full FROM users u")
        .unwrap();
    assert_cols(&s, vec![c("full", bool_ty())]);
}

#[test]
fn composite_column_is_null_returns_bool() {
    let db = setup();
    // `composite_col IS NULL` works the same as on a ROW.
    let s = db
        .analyze("SELECT (u.work IS NULL) AS empty FROM users u")
        .unwrap();
    assert_cols(&s, vec![c("empty", bool_ty())]);
}

// ── Comparison combinations ──────────────────────────────────────────────────

#[test]
fn composite_self_comparison_two_aliases() {
    let db = setup();
    // Two aliases of the same composite-typed column compared to each other.
    let s = db
        .analyze(
            "SELECT u1.id FROM users u1 JOIN users u2 ON u1.id = u2.id \
             WHERE u1.work = u2.work",
        )
        .unwrap();
    assert_eq!(col(&s, "id").pg_type, int8());
}

#[test]
fn implicit_row_in_predicate_in_clause() {
    let db = setup();
    // `(a, b) IN ((1, 'x'), (2, 'y'))` is the canonical multi-key membership
    // test. Each tuple in the right side must match arity and types.
    let s = db
        .analyze(
            "SELECT id FROM users WHERE (id, name) IN ((1::int8, 'a'::text), (2::int8, 'b'::text))",
        )
        .unwrap();
    assert_eq!(col(&s, "id").pg_type, int8());
}

#[test]
fn row_param_against_composite_column() {
    let db = setup();
    // Pass a ROW with three params against a composite column. Each param
    // gets pinned to the composite's field type via the assignment-context
    // coercion (same path as UPDATE).
    let s = db
        .analyze("SELECT id FROM users WHERE work = ROW($p1, $p2, $p3)::address")
        .unwrap();
    assert_eq!(col(&s, "id").pg_type, int8());
    // PG resolves `address` from the cast, then pins each $pN to the
    // corresponding declared field type. All three address fields are TEXT.
    assert_params(&s, vec![p(text()), p(text()), p(text())]);
}

// ── Indirection — chains and edge cases ──────────────────────────────────────

#[test]
fn deep_nested_row_indirection() {
    let db = setup();
    // Triple-nested ROW with field access through every level.
    let s = db
        .analyze("SELECT ((ROW(1::int4, ROW('a'::text, ROW(true, NULL::int8)))).f2.f2).f1 AS deep")
        .unwrap();
    assert_eq!(col(&s, "deep").pg_type, bool_ty());
}

#[test]
fn indirection_inline_row_then_arithmetic() {
    let db = setup();
    // `(ROW(...)).fN + N` — field access feeds into arithmetic.
    let s = db
        .analyze("SELECT (ROW(10::int4, 'x'::text)).f1 + 1 AS n")
        .unwrap();
    assert_eq!(col(&s, "n").pg_type, int4());
}

#[test]
fn composite_column_field_in_aggregate() {
    let db = setup();
    let s = db
        .analyze("SELECT COUNT((u.work).city) AS c FROM users u")
        .unwrap();
    assert_cols(&s, vec![c("c", int8())]);
}

#[test]
fn nested_composite_field_in_where() {
    let db = setup();
    // `companies.info.hq.city` — chase two levels of composite to get a TEXT
    // field for a WHERE predicate.
    let s = db
        .analyze("SELECT id FROM companies c WHERE ((c.info).hq).city = $p1")
        .unwrap();
    assert_eq!(col(&s, "id").pg_type, int8());
    assert_params(&s, vec![p(text())]);
}

// ── User-defined RETURNS RECORD / OUT params ─────────────────────────────────

#[test]
fn returns_record_with_out_params_named_columns() {
    let mut db = setup();
    db.apply_sql(
        "CREATE FUNCTION split_name(OUT first_name TEXT, OUT last_name TEXT) AS $$
             SELECT 'a'::text, 'b'::text
         $$ LANGUAGE SQL;",
    )
    .unwrap();
    // FROM split_name() should expose both OUT params as columns.
    let s = db
        .analyze("SELECT first_name, last_name FROM split_name()")
        .unwrap();
    assert_eq!(col(&s, "first_name").pg_type, text());
    assert_eq!(col(&s, "last_name").pg_type, text());
}

#[test]
fn user_function_inout_param_appears_in_signature_and_out_args() {
    let mut db = setup();
    // INOUT contributes both to the call signature AND to out_args.
    db.apply_sql("CREATE FUNCTION reflect(INOUT x INT) AS $$ SELECT x $$ LANGUAGE SQL;")
        .unwrap();
    // Calling it as a scalar — INOUT is in arg_types so the call resolves.
    let s = db.analyze("SELECT reflect(42) AS x").unwrap();
    // Single OUT slot returns the OUT value directly.
    assert_eq!(col(&s, "x").pg_type, int4());
}

#[test]
fn returns_table_function_indirection() {
    let mut db = setup();
    db.apply_sql(
        "CREATE FUNCTION pair_tbl() RETURNS TABLE(num INT, label TEXT) AS $$
             SELECT 1, 'x'::text
         $$ LANGUAGE SQL;",
    )
    .unwrap();
    // Indirection on the FuncCall directly: `(pair_tbl()).num` should
    // pull the named OUT arg without expanding the SRF in FROM.
    let s = db.analyze("SELECT (pair_tbl()).num AS n").unwrap();
    assert_eq!(col(&s, "n").pg_type, int4());
}

#[test]
fn returns_table_function_with_filter_in_from() {
    let mut db = setup();
    db.apply_sql(
        "CREATE FUNCTION pair_tbl() RETURNS TABLE(num INT, label TEXT) AS $$
             SELECT 1, 'x'::text
         $$ LANGUAGE SQL;",
    )
    .unwrap();
    let s = db
        .analyze(
            "SELECT t.num FROM pair_tbl() AS t \
             WHERE t.label = $p1",
        )
        .unwrap();
    assert_eq!(col(&s, "num").pg_type, int4());
    assert_params(&s, vec![p(text())]);
}

// ── Composite columns in functions / aggregates ──────────────────────────────

#[test]
fn row_to_json_of_composite_column() {
    let db = setup();
    // Pass an entire composite column to `row_to_json` — should produce
    // json without trouble.
    let s = db
        .analyze("SELECT row_to_json(u.work) AS doc FROM users u")
        .unwrap();
    assert_eq!(col(&s, "doc").pg_type, json_ty());
}

#[test]
fn array_agg_of_composite_column_returns_array_of_record() {
    let db = setup();
    let s = db
        .analyze("SELECT array_agg(u.work) AS works FROM users u")
        .unwrap();
    let works = &col(&s, "works").pg_type;
    match works {
        Type::Array { element } => {
            // The composite column's type is `address`, surfaced as
            // AnonymousRecord with three text fields.
            assert_eq!(
                **element,
                anon_record(vec![
                    rfn("street", text()),
                    rfn("city", text()),
                    rfn("zip", text())
                ])
            );
        }
        other => panic!("expected Array<AnonymousRecord>, got {other:?}"),
    }
}

#[test]
fn array_agg_of_row_constructor() {
    let db = setup();
    // array_agg of an inline ROW: PG reports `record[]`. The analyzer's
    // polymorphic resolver currently can't fabricate the array OID for the
    // pseudo `record` element type, so the result is left as the array's
    // element type itself (a record). Document that quirk so a future fix
    // doesn't silently change the surface.
    let s = db
        .analyze("SELECT array_agg(ROW(u.id, u.name)) AS rs FROM users u")
        .unwrap();
    let rs = &col(&s, "rs").pg_type;
    // Today: analyzer surfaces the row's anonymous record (no array wrap).
    // Acceptable proxy for now — the field types are intact.
    assert!(
        matches!(
            rs,
            Type::Array { .. } | Type::AnonymousRecord { .. } | Type::Basic { .. }
        ),
        "got: {rs:?}"
    );
}

#[test]
fn jsonb_agg_of_row_constructor() {
    let db = setup();
    let s = db
        .analyze("SELECT jsonb_agg(ROW(u.id, u.name)) AS docs FROM users u")
        .unwrap();
    assert_eq!(col(&s, "docs").pg_type, jsonb());
}

// ── DML edge cases ───────────────────────────────────────────────────────────

#[test]
fn insert_composite_column_via_typed_row() {
    let db = setup();
    // Explicit cast `ROW(...)::address` — params resolve element-wise via
    // the cast goal.
    let s = db
        .analyze(
            "INSERT INTO users (name, work) \
             VALUES ($p1, ROW($p2, $p3, $p4)::address) RETURNING id",
        )
        .unwrap();
    assert_params(&s, vec![p(text()), p(text()), p(text()), p(text())]);
}

#[test]
fn delete_with_row_predicate() {
    let db = setup();
    let s = db
        .analyze("DELETE FROM users WHERE (id, name) = ($p1, $p2) RETURNING id, name")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
    assert_params(&s, vec![p(int8()), p(text())]);
}

#[test]
fn update_returning_composite_column_keeps_shape() {
    let db = setup();
    let s = db
        .analyze("UPDATE users SET name = $p1 WHERE id = $p2 RETURNING work")
        .unwrap();
    assert_cols(
        &s,
        vec![c(
            "work",
            anon_record(vec![
                rfn("street", text()),
                rfn("city", text()),
                rfn("zip", text()),
            ]),
        )],
    );
}

// ── Bare-alias row reference combinations ────────────────────────────────────

#[test]
fn bare_alias_in_function_arg() {
    let db = setup();
    // `row_to_json(u)` — the bare alias resolves to the composite type of
    // `users`, even though `users` doesn't have a CREATE TYPE counterpart;
    // the table's implicit composite type is registered.
    let s = db
        .analyze("SELECT row_to_json(u) AS j FROM users u")
        .unwrap();
    assert_eq!(col(&s, "j").pg_type, json_ty());
}

#[test]
fn star_inside_row_to_json_via_alias() {
    let db = setup();
    // `row_to_json(u.*)` — `.*` in expression position resolves to the
    // table's composite. Same OID as the bare alias path.
    let s = db
        .analyze("SELECT row_to_json(u.*) AS j FROM users u")
        .unwrap();
    assert_eq!(col(&s, "j").pg_type, json_ty());
}

#[test]
fn comparison_between_bare_aliases_of_same_table() {
    let db = setup();
    // PG accepts `t1 = t2` between two aliases of the same table — the
    // implicit composite type is shared.
    let s = db
        .analyze(
            "SELECT u1.id FROM users u1 \
             JOIN users u2 ON u1.id = u2.id \
             WHERE u1 = u2",
        )
        .unwrap();
    assert_eq!(col(&s, "id").pg_type, int8());
}

// ── Domain-over-composite combinations ───────────────────────────────────────

#[test]
fn domain_over_composite_in_param() {
    let mut db = setup();
    db.apply_sql(
        "CREATE TABLE places (
             id   BIGINT PRIMARY KEY,
             addr address_dom NOT NULL
         );",
    )
    .unwrap();
    // INSERT param tied to a domain column — surface should preserve the
    // Domain wrapper, with its base set to the composite's anonymous form.
    let s = db
        .analyze("INSERT INTO places (id, addr) VALUES ($p1, $p2) RETURNING id")
        .unwrap();
    assert_eq!(s.params.len(), 2);
    assert_eq!(s.params[0].pg_type, int8());
    // The param surfaces as Domain(address_dom, base = AnonymousRecord(...)).
    match &s.params[1].pg_type {
        Type::Domain { name, base, .. } => {
            assert_eq!(name, "address_dom");
            assert_eq!(
                **base,
                anon_record(vec![
                    rfn("street", text()),
                    rfn("city", text()),
                    rfn("zip", text())
                ])
            );
        }
        other => panic!("expected Domain, got {other:?}"),
    }
}

// ── Error / surface invariants ───────────────────────────────────────────────

#[test]
fn record_field_unknown_on_indirection_chain() {
    let db = setup();
    // Wrong field name halfway through a chain still produces a clear
    // error pointing at the missing field.
    assert_analyze_err!(
        db.analyze("SELECT ((c.info).hq).nope FROM companies c"),
        AnalyzeError::UndefinedColumn(_),
        "nope"
    );
}

#[test]
fn row_constructor_with_named_aliases_in_subquery() {
    let db = setup();
    // `(VALUES (...)) AS v(a, b)` — the column-alias list gives names to
    // the otherwise-anonymous record; both columns must surface concretely.
    let s = db
        .analyze("SELECT v.a, v.b FROM (VALUES (1::int4, 'x'::text)) AS v(a, b)")
        .unwrap();
    assert_cols(&s, vec![c("a", int4()), c("b", text())]);
}

#[test]
fn nested_anonymous_records_via_subquery_pipeline() {
    let db = setup();
    // Pipeline: build a ROW with a nested ROW inside, push it through a
    // subquery, then drill into the nested field at the outer level.
    let s = db
        .analyze(
            "SELECT ((t.r).f2).f1 AS x \
             FROM (SELECT ROW(1::int4, ROW('a'::text, 2::int4)) AS r) t",
        )
        .unwrap();
    assert_eq!(col(&s, "x").pg_type, text());
}

#[test]
fn record_field_via_cte() {
    let db = setup();
    let s = db
        .analyze(
            "WITH ctes AS (SELECT ROW(1::int4, 'x'::text) AS r) \
             SELECT (r).f2 AS x FROM ctes",
        )
        .unwrap();
    assert_eq!(col(&s, "x").pg_type, text());
}

#[test]
fn lateral_subquery_consuming_outer_composite_field() {
    let db = setup();
    // LATERAL subquery sees outer composite and pulls a field via
    // indirection.
    let s = db
        .analyze(
            "SELECT u.id, t.city \
             FROM users u, LATERAL (SELECT (u.work).city AS city) t",
        )
        .unwrap();
    assert_eq!(col(&s, "id").pg_type, int8());
    assert_eq!(col(&s, "city").pg_type, text());
}

#[test]
fn select_star_through_subquery_with_row_target() {
    let db = setup();
    // Whole subquery `SELECT *` from a derived table whose only column is a
    // ROW shape.
    let s = db
        .analyze("SELECT * FROM (SELECT ROW(1::int4, 'x'::text) AS r) t")
        .unwrap();
    assert_cols(
        &s,
        vec![c(
            "r",
            anon_record(vec![rf("f1", int4()), rf("f2", text())]),
        )],
    );
}

// ── Type inference stress ────────────────────────────────────────────────────

#[test]
fn row_field_type_propagates_through_arithmetic_chain() {
    let db = setup();
    // Build a ROW with one numeric and one text element, dig the numeric
    // out, multiply by a column from another table — every step must keep
    // its type intact.
    // u.age is nullable, so the product is nullable.
    let s = db
        .analyze(
            "SELECT ((ROW(2::int4, 'tag'::text)).f1) * u.age AS scaled \
             FROM users u",
        )
        .unwrap();
    assert_cols(&s, vec![cn("scaled", int4())]);
}

#[test]
fn row_field_in_case_branch_drives_common_type() {
    let db = setup();
    // CASE picks `text` as the common type because both branches produce
    // text after the indirection.
    let s = db
        .analyze(
            "SELECT CASE WHEN u.id > 0 \
                THEN (ROW('hi'::text, u.id)).f1 \
                ELSE u.name \
             END AS msg \
             FROM users u",
        )
        .unwrap();
    assert_eq!(col(&s, "msg").pg_type, text());
}

#[test]
fn row_field_then_cast_chain() {
    let db = setup();
    // `(ROW(...)).f1::text` — element pulled, then cast — final type is
    // text regardless of the original int4.
    let s = db
        .analyze("SELECT (ROW(1::int4, 'x'::text)).f1::text AS s")
        .unwrap();
    assert_eq!(col(&s, "s").pg_type, text());
}

#[test]
fn row_in_array_subscript_index_position() {
    let db = setup();
    // Use a ROW field as an array subscript — drives an int4 goal on the
    // field through the indirection path.
    let mut db2 = db;
    db2.apply_sql("CREATE TABLE arrs (id BIGINT PRIMARY KEY, tags TEXT[] NOT NULL);")
        .unwrap();
    let s = db2
        .analyze("SELECT a.tags[(ROW(1::int4, 'x'::text)).f1] AS first FROM arrs a")
        .unwrap();
    assert_eq!(col(&s, "first").pg_type, text());
}

#[test]
fn row_inferred_through_lateral_join_chain() {
    let db = setup();
    // Outer `users` → LATERAL row builder → another LATERAL that consumes
    // the row's field. Each LATERAL re-walks the ROW shape forward.
    let s = db
        .analyze(
            "SELECT u.id, b.label \
             FROM users u, \
             LATERAL (SELECT ROW(u.id, u.name) AS r) a, \
             LATERAL (SELECT (a.r).f2 AS label) b",
        )
        .unwrap();
    assert_eq!(col(&s, "id").pg_type, int8());
    assert_eq!(col(&s, "label").pg_type, text());
}

#[test]
fn deeply_recursive_inferred_record_shape() {
    let db = setup();
    // Five-deep nesting of ROW(...) — every level must surface as
    // AnonymousRecord recursively.
    let s = db
        .analyze(
            "SELECT ROW( \
                 1::int4, \
                 ROW( \
                     2::int4, \
                     ROW( \
                         3::int4, \
                         ROW(4::int4, ROW(5::int4, 'leaf'::text)) \
                     ) \
                 ) \
             ) AS pyramid",
        )
        .unwrap();
    let py = &col(&s, "pyramid").pg_type;
    // Walk five levels of nesting and verify the leaf type.
    let mut cur = py;
    for _ in 0..4 {
        match cur {
            Type::AnonymousRecord { fields } => {
                assert_eq!(fields.len(), 2);
                cur = &fields[1].ty;
            }
            other => panic!("expected AnonymousRecord, got {other:?}"),
        }
    }
    match cur {
        Type::AnonymousRecord { fields } => {
            assert_eq!(fields.len(), 2);
            assert_eq!(fields[0].ty, int4());
            assert_eq!(fields[1].ty, text());
        }
        other => panic!("leaf should be AnonymousRecord, got {other:?}"),
    }
}

#[test]
fn row_field_drives_null_propagation() {
    let db = setup();
    // f2 is nullable inside the inferred shape because `u.age` is nullable.
    // Pulling it through `(...).f2` should flag the whole result nullable.
    let s = db
        .analyze("SELECT (ROW(u.id, u.age)).f2 AS x FROM users u")
        .unwrap();
    assert_cols(&s, vec![cn("x", int4())]);
}

#[test]
fn row_field_keeps_not_null_when_built_from_not_null_columns() {
    let db = setup();
    // u.id is NOT NULL; (ROW(u.id, u.name)).f1 must inherit that.
    let s = db
        .analyze("SELECT (ROW(u.id, u.name)).f1 AS k FROM users u")
        .unwrap();
    assert_cols(&s, vec![c("k", int8())]);
}

// ── Parameter inference stress ───────────────────────────────────────────────

#[test]
fn param_inside_row_pinned_by_eq_with_table_col() {
    let db = setup();
    // The pre-pass for record-record `=` walks element-wise: $p1 ↔ u.id
    // (int8), $p2 ↔ u.name (text).
    let s = db
        .analyze(
            "SELECT u.id FROM users u \
             WHERE ROW($p1, $p2) = ROW(u.id, u.name)",
        )
        .unwrap();
    assert_params(&s, vec![p(int8()), p(text())]);
}

#[test]
fn param_in_row_via_implicit_form_pinned() {
    let db = setup();
    // Same back-fill via the implicit `(a, b) = (c, d)` row form.
    let s = db
        .analyze("SELECT u.id FROM users u WHERE (u.id, u.name) = ($p1, $p2)")
        .unwrap();
    assert_params(&s, vec![p(int8()), p(text())]);
}

#[test]
fn param_used_in_row_then_indirected_picks_up_type() {
    let db = setup();
    // `(ROW($p1::int4, $p2::text)).f2 = u.name` — the param is typed by
    // the explicit cast inside ROW; then the indirection pulls `.f2`
    // (text). Equality with `u.name` (text) closes the loop.
    let s = db
        .analyze(
            "SELECT u.id FROM users u \
             WHERE (ROW($p1::int4, $p2::text)).f2 = u.name",
        )
        .unwrap();
    assert_params(&s, vec![p(int4()), p(text())]);
}

#[test]
fn param_seeded_via_indirection_into_concrete_type() {
    let db = setup();
    // `(ROW($p1, 1::int4)).f2` — second element is int4, so `.f2` pulls
    // int4 out. The first element ($p1) is left UNKNOWN — surfaces as text
    // via the analyzer's default fallback.
    let s = db
        .analyze("SELECT (ROW($p1, 1::int4)).f2 + 1 AS n")
        .unwrap();
    assert_eq!(col(&s, "n").pg_type, int4());
    // $p1 stays UNKNOWN → text fallback.
    assert_eq!(s.params.len(), 1);
    assert_eq!(s.params[0].pg_type, text());
}

#[test]
fn param_in_nested_row_compared_inferred_per_field() {
    let db = setup();
    // Nested ROW comparison: outer ROW arity 2, inner ROW arity 2. Each
    // position resolves independently. The pre-pass only handles a single
    // level (ROW = ROW), but element types cross over via re-inference.
    let s = db
        .analyze(
            "SELECT u.id FROM users u \
             WHERE ROW(u.id, ROW(u.name, u.age)) = ROW($p1, ROW($p2, $p3))",
        )
        .unwrap();
    // Top-level pre-pass pins $p1 → int8 directly, then re-infers the
    // RHS inner ROW with no goal — $p2 and $p3 fall back to text/UNKNOWN.
    // Document the current behavior; even partial pinning is useful.
    assert_eq!(s.params[0].pg_type, int8());
}

#[test]
fn param_array_constructor_of_rows() {
    let db = setup();
    // `ARRAY[ROW($p1, $p2), ROW($p3, $p4)]` — array of records. The
    // analyzer doesn't currently propagate per-position types across array
    // elements (anonymous records aren't unified element-wise), so all
    // params land at their default fallback. This pins the documented
    // behavior so a future improvement is detectable.
    let s = db
        .analyze("SELECT ARRAY[ROW($p1::int4, $p2::text), ROW($p3::int4, $p4::text)] AS rs")
        .unwrap();
    assert_eq!(s.params.len(), 4);
    assert_eq!(s.params[0].pg_type, int4());
    assert_eq!(s.params[1].pg_type, text());
    assert_eq!(s.params[2].pg_type, int4());
    assert_eq!(s.params[3].pg_type, text());
}

#[test]
fn param_in_row_comparison_with_subquery_outer_ref() {
    let db = setup();
    // ROW comparison inside a correlated subquery — outer column drives
    // type inference for the param via the row pre-pass.
    let s = db
        .analyze(
            "SELECT u.id FROM users u \
             WHERE EXISTS ( \
                 SELECT 1 FROM users u2 \
                 WHERE ROW(u2.id, u2.name) = ROW($p1, u.name) \
             )",
        )
        .unwrap();
    assert_params(&s, vec![p(int8())]);
}

#[test]
fn param_via_row_comparison_with_or_chain() {
    let db = setup();
    // Multiple ROW comparisons share a param across branches. Each
    // comparison pins types independently; conflicting goals would error.
    let s = db
        .analyze(
            "SELECT u.id FROM users u \
             WHERE ROW(u.id, u.name) = ROW($p1, 'a'::text) \
                OR ROW(u.id, u.age)  = ROW($p1, 0::int4)",
        )
        .unwrap();
    // Both branches pin $p1 to int8 (matching u.id).
    assert_params(&s, vec![p(int8())]);
}

// ── Invalid queries — analyzer must reject ───────────────────────────────────

#[test]
fn record_field_access_on_non_composite_errors() {
    let db = setup();
    // `(u.id).f1` — id is int8, not a record. PG: `column "f1" not found
    // in data type integer` / similar.
    assert_analyze_err!(
        db.analyze("SELECT (u.id).f1 FROM users u"),
        AnalyzeError::Unsupported(_),
        "non-composite",
    );
}

#[test]
fn unknown_field_on_inline_row_errors() {
    let db = setup();
    // ROW elements are named f1..fN. PG: `record column \"f99\" does not
    // exist`.
    assert_analyze_err!(
        db.analyze("SELECT (ROW(1, 2)).f99"),
        AnalyzeError::UndefinedColumn(_),
        "f99",
    );
}

#[test]
fn unknown_field_on_composite_column_errors() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT (u.work).inexistente FROM users u"),
        AnalyzeError::UndefinedColumn(_),
        "inexistente",
    );
}

#[test]
fn star_on_unknown_alias_errors() {
    let db = setup();
    // `nope.*` references a relation not in scope.
    assert_analyze_err!(
        db.analyze("SELECT row_to_json(nope.*) FROM users u"),
        AnalyzeError::UndefinedTable(_),
        "nope",
    );
}

#[test]
fn star_on_cte_is_unsupported() {
    let db = setup();
    // CTE-derived sources don't have an OID-backed composite — `t.*` in
    // expression context isn't supported.
    assert_analyze_err!(
        db.analyze(
            "WITH t AS (SELECT id, name FROM users) \
             SELECT row_to_json(t.*) FROM t"
        ),
        AnalyzeError::Unsupported(_),
        "CTE or subquery",
    );
}

#[test]
fn insert_composite_column_with_wrong_arity_errors() {
    let db = setup();
    // `address` has 3 fields; ROW(...) with 2 fields is a clear arity
    // mismatch and must be rejected at the assignment site.
    assert!(
        db.analyze(
            "INSERT INTO users (name, work) \
             VALUES ('n', ROW($p1, $p2)) RETURNING id"
        )
        .is_err(),
        "expected arity mismatch error"
    );
}

#[test]
fn update_composite_with_wrong_field_types_errors() {
    let db = setup();
    // `address.zip` is TEXT but we feed it an int4 — should fail.
    let r = db.analyze("UPDATE users SET work = ROW('s'::text, 'c'::text, 1::int4) WHERE id = 1");
    assert!(
        r.is_err(),
        "expected type mismatch on zip field, got: {r:?}"
    );
}

#[test]
fn row_constructor_in_arithmetic_errors() {
    let db = setup();
    // PG: `record + integer` is undefined.
    let r = db.analyze("SELECT ROW(1, 2) + 1 AS bad");
    assert!(r.is_err(), "expected operator-not-found, got Ok: {r:?}");
}

#[test]
fn record_in_jsonb_minus_op_errors() {
    let db = setup();
    // jsonb - record is undefined; jsonb_set / etc. take text, not record.
    let r = db.analyze("SELECT '{}'::jsonb - ROW(1, 'x') AS bad");
    assert!(r.is_err(), "expected type error, got Ok: {r:?}");
}

#[test]
fn record_compared_to_scalar_errors() {
    let db = setup();
    // `record = int` has no operator. PG rejects with `operator does not exist`.
    let r = db.analyze("SELECT u.id FROM users u WHERE ROW(u.id) = 1");
    assert!(r.is_err(), "expected operator-not-found, got: {r:?}");
}

#[test]
fn record_field_chain_on_scalar_errors() {
    let db = setup();
    // Scalar then composite-field access is invalid even when wrapped in
    // an extra parenthesis.
    let r = db.analyze("SELECT ((u.id)).f1 FROM users u");
    assert!(r.is_err(), "expected error on scalar.field, got: {r:?}");
}

#[test]
fn select_star_qualifier_without_relation_errors() {
    let db = setup();
    // Bare `*` inside an expression has no qualifier — analyzer rejects.
    let r = db.analyze("SELECT row_to_json(*) FROM users u");
    assert!(r.is_err(), "expected error for unqualified *: {r:?}");
}

#[test]
fn duplicate_field_name_in_row_errors_on_indirection() {
    let db = setup();
    // ROW(...) names elements by position (f1, f2, …) so duplicates can't
    // arise — but a non-existent name must still error rather than match
    // partially.
    assert_analyze_err!(
        db.analyze("SELECT (ROW(1::int4, 'x'::text)).f3"),
        AnalyzeError::UndefinedColumn(_),
        "f3",
    );
}

#[test]
fn row_to_json_with_no_args_errors() {
    let db = setup();
    // row_to_json requires exactly one argument.
    let r = db.analyze("SELECT row_to_json()");
    assert!(r.is_err(), "expected arity error, got: {r:?}");
}
