//! Aggregates and GROUP BY / HAVING: COUNT, SUM, MIN, MAX, AVG,
//! string_agg, array_agg. Nullability rules for empty sets and grouped
//! columns. Strict vs non-strict builtin functions.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE users (
            id   BIGINT PRIMARY KEY,
            name TEXT NOT NULL,
            age  INT
         );
         CREATE TABLE posts (
            id           BIGINT PRIMARY KEY,
            user_id      BIGINT NOT NULL,
            title        TEXT NOT NULL,
            body         TEXT,
            published_at TIMESTAMPTZ
         );
         CREATE TABLE comments (
            id          BIGINT PRIMARY KEY,
            post_id     BIGINT NOT NULL,
            author_name TEXT NOT NULL,
            content     TEXT NOT NULL,
            rating      INT
         );",
    )
    .unwrap();
    db
}

// ── COUNT / GROUP BY basic shapes ────────────────────────────────────────────

#[test]
fn types_match_count_star() {
    let db = setup();
    let s = db.analyze("SELECT count(*) AS total FROM users").unwrap();
    assert_cols(&s, vec![c("total", int8())]);
}

#[test]
fn types_match_group_by_count() {
    let db = setup();
    let s = db
        .analyze("SELECT user_id, count(*) AS post_count FROM posts GROUP BY user_id")
        .unwrap();
    assert_cols(&s, vec![c("user_id", int8()), c("post_count", int8())]);
}

#[test]
fn types_match_group_by_multiple_aggregates() {
    let db = setup();
    // max() is nullable — returns NULL for empty groups.
    let s = db
        .analyze(
            "SELECT user_id, count(*) AS cnt, max(published_at) AS latest \
             FROM posts GROUP BY user_id",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("user_id", int8()),
            c("cnt", int8()),
            cn("latest", timestamptz()),
        ],
    );
}

#[test]
fn count_not_null() {
    let db = setup();
    let sql = "SELECT COUNT(*) as cnt FROM users";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "cnt").nullable);
}

// ── GROUP BY + aggregates: SUM/MIN/MAX/AVG/string_agg ────────────────────────

#[test]
fn agg_sum_with_group_by_not_null_input() {
    let db = setup();
    // user_id is NOT NULL + GROUP BY → SUM guaranteed non-null.
    // SUM(int8) → numeric.
    let sql = "SELECT user_id, SUM(user_id) as total FROM posts GROUP BY user_id";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("user_id", int8()), c("total", numeric())]);
}

#[test]
fn agg_sum_with_group_by_nullable_input() {
    let db = setup();
    // rating is nullable + GROUP BY → SUM still nullable (all rows in group could be NULL).
    // SUM(int4) → int8.
    let sql = "SELECT post_id, SUM(rating) as total FROM comments GROUP BY post_id";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("post_id", int8()), cn("total", int8())]);
}

#[test]
fn agg_min_max_with_group_by_not_null() {
    let db = setup();
    // title is NOT NULL + GROUP BY → MIN/MAX are NOT NULL.
    // MIN/MAX(text) → text.
    let sql = "SELECT user_id, MIN(title) as first_title, MAX(title) as last_title \
               FROM posts GROUP BY user_id";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
        vec![
            c("user_id", int8()),
            c("first_title", text()),
            c("last_title", text()),
        ],
    );
}

#[test]
fn agg_avg_with_group_by_not_null() {
    let db = setup();
    // id is NOT NULL + GROUP BY → AVG is NOT NULL.
    // AVG(int8) → numeric.
    let sql = "SELECT user_id, AVG(id) as avg_id FROM posts GROUP BY user_id";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("user_id", int8()), c("avg_id", numeric())]);
}

#[test]
fn agg_count_with_group_by() {
    let db = setup();
    // COUNT is always NOT NULL, with or without GROUP BY.
    // COUNT(*) → int8.
    let sql = "SELECT user_id, COUNT(*) as cnt FROM posts GROUP BY user_id";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("user_id", int8()), c("cnt", int8())]);
}

#[test]
fn agg_count_without_group_by() {
    let db = setup();
    // COUNT without GROUP BY: still NOT NULL (returns 0).
    // COUNT(*) → int8.
    let sql = "SELECT COUNT(*) as cnt FROM posts";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("cnt", int8())]);
}

// ── aggfinalfn-driven return types ──────────────────────────────────────────
//
// These exercise the codepath that walks pg_aggregate.aggfinalfn → pg_proc
// → prorettype. Without a properly populated `aggfinalfn` in the seed,
// the returned type would silently fall back to the aggregate's transition
// type (e.g., AVG(int) would return its internal accumulator instead of
// numeric). Each variant of AVG we test below has its own finalfn.

#[test]
fn avg_int4_returns_numeric_via_finalfn() {
    let db = setup();
    // PG: avg(int4) → numeric. Driven by `int8_avg` finalfn — `aggfinalfn`
    // points at it; the analyzer walks that to recover the type.
    let sql = "SELECT AVG(rating) AS avg_rating FROM comments";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![cn("avg_rating", numeric())]);
}

#[test]
fn avg_int8_returns_numeric_via_finalfn() {
    let db = setup();
    let sql = "SELECT AVG(id) AS avg_id FROM posts";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![cn("avg_id", numeric())]);
}

#[test]
fn avg_numeric_returns_numeric_via_finalfn() {
    // Cover NUMERIC explicitly — its accumulator is `_numeric` (an array)
    // but the finalfn collapses to numeric. If `aggfinalfn` weren't
    // resolved, we'd surface the array intermediate.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (price NUMERIC NOT NULL);")
        .unwrap();
    let info = db.analyze("SELECT AVG(price) AS avg_price FROM t").unwrap();
    assert_cols(&info, vec![cn("avg_price", numeric())]);
}

#[test]
fn variance_int4_returns_numeric_via_finalfn() {
    // VARIANCE / STDDEV also have finalfns. Different finalfn than AVG —
    // makes sure we're not just lucky on one binding.
    let db = setup();
    let info = db
        .analyze("SELECT VARIANCE(rating) AS v FROM comments")
        .unwrap();
    assert_cols(&info, vec![cn("v", numeric())]);
}

#[test]
fn agg_sum_without_group_by_always_nullable() {
    let db = setup();
    // SUM without GROUP BY: table could be empty → NULL.
    // Even with NOT NULL input.
    // SUM(int8) → numeric.
    let sql = "SELECT SUM(id) as total FROM posts";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![cn("total", numeric())]);
}

#[test]
fn agg_mixed_nullability_with_group_by() {
    let db = setup();
    // Mix of NOT NULL and nullable aggregates in same GROUP BY query.
    // COUNT(*) → int8, SUM(int4) → int8, MIN(text) → text, MAX(int4) → int4.
    let sql = "SELECT post_id, \
                      COUNT(*) as cnt, \
                      SUM(rating) as sum_rating, \
                      MIN(author_name) as first_author, \
                      MAX(rating) as max_rating \
               FROM comments GROUP BY post_id";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
        vec![
            c("post_id", int8()),
            c("cnt", int8()),
            cn("sum_rating", int8()),
            c("first_author", text()),
            cn("max_rating", int4()),
        ],
    );
}

#[test]
fn agg_with_group_by_and_left_join() {
    let db = setup();
    // LEFT JOIN + GROUP BY: right-side columns are nullable from JOIN,
    // so aggregate on them is nullable even with GROUP BY.
    // COUNT(x) → int8, MAX(text) → text.
    let sql = "SELECT u.id, COUNT(p.id) as post_count, MAX(p.title) as last_title \
               FROM users u \
               LEFT JOIN posts p ON p.user_id = u.id \
               GROUP BY u.id";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
        vec![
            c("id", int8()),
            c("post_count", int8()),
            cn("last_title", text()),
        ],
    );
}

#[test]
fn agg_string_agg_with_group_by_not_null() {
    let db = setup();
    // string_agg(NOT NULL, delimiter) with GROUP BY → NOT NULL.
    // The literal ', ' has type UNKNOWN — resolved via UNKNOWN-compatible matching.
    // string_agg(text, text) → text.
    let sql = "SELECT post_id, string_agg(author_name, ', ') as authors \
               FROM comments GROUP BY post_id";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("post_id", int8()), c("authors", text())]);
}

// ── Stress / torture ─────────────────────────────────────────────────────────

#[test]
fn stress_aggregates_no_group_by() {
    let db = setup();
    // COUNT(*) → int8, SUM(int4) → int8, MAX(text) → text.
    // SUM and MAX are nullable (empty table → NULL).
    let sql = "SELECT COUNT(*) as cnt, SUM(age) as total_age, MAX(name) as last_name FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
        vec![
            c("cnt", int8()),
            cn("total_age", int8()),
            cn("last_name", text()),
        ],
    );
}

#[test]
fn torture_count_with_group_by() {
    let db = setup();
    // COUNT in GROUP BY context is still NOT NULL.
    // COUNT(x) → int8.
    let sql = "SELECT u.name, COUNT(p.id) as post_count \
               FROM users u \
               LEFT JOIN posts p ON p.user_id = u.id \
               GROUP BY u.name";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("name", text()), c("post_count", int8())]);
}

// ── Placement rules ──────────────────────────────────────────────────────────

#[test]
fn aggregate_in_where_rejected() {
    let db = setup();
    // PG: `aggregate functions are not allowed in WHERE`.
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE COUNT(*) > 0"),
        AnalyzeError::Invalid(_),
        concat!(
            "aggregate functions are not allowed in WHERE\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users WHERE COUNT(*) > 0\n",
            "  ·                            ─────\n",
            "  ╰────\n",
            "  help: to filter on an aggregate, use a HAVING clause instead of WHERE\n",
        ),
    );
}

#[test]
fn aggregate_in_group_by_rejected() {
    let db = setup();
    // PG: `aggregate functions are not allowed in GROUP BY`.
    assert_analyze_err!(
        db.analyze("SELECT age FROM users GROUP BY COUNT(*)"),
        AnalyzeError::Invalid(_),
        concat!(
            "aggregate functions are not allowed in GROUP BY\n",
            "  ╭────\n",
            "1 │ SELECT age FROM users GROUP BY COUNT(*)\n",
            "  ·                                ─────\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn nested_aggregate_rejected() {
    let db = setup();
    // PG: `aggregate function calls cannot be nested`.
    assert_analyze_err!(
        db.analyze("SELECT SUM(COUNT(*)) FROM posts GROUP BY user_id"),
        AnalyzeError::Invalid(_),
        "aggregate function calls cannot be nested",
    );
}

#[test]
fn window_function_in_aggregate_argument_rejected() {
    let db = setup();
    // PG (SQLSTATE 42803): `aggregate function calls cannot contain window
    // function calls`. Mirror PG's wording verbatim so the sanity check
    // passes.
    assert_analyze_err!(
        db.analyze("SELECT SUM(ROW_NUMBER() OVER ()) FROM posts"),
        AnalyzeError::Invalid(_),
        "aggregate function calls cannot contain window function calls",
    );
}

// ── GROUP BY column validation + select-alias fallback ───────────────────────

#[test]
fn group_by_unknown_column_rejected() {
    // A typo in GROUP BY used to be silently accepted — the walker
    // discarded `infer_expr`'s error to preserve "param coverage". Now
    // we propagate the error, but with a fallback for select aliases.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT count(*) FROM users GROUP BY ghost"),
        AnalyzeError::UndefinedColumn(_),
        concat!(
            "column \"ghost\" does not exist\n",
            "  ╭────\n",
            "1 │ SELECT count(*) FROM users GROUP BY ghost\n",
            "  ·                                     ──┬──\n",
            "  ·                                       ╰─ column does not exist\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn group_by_resolves_select_alias() {
    // PG accepts `GROUP BY <select_alias>` even though the alias isn't
    // in the FROM scope. The fallback in walk_group_clause_node makes
    // this still work after the propagation fix.
    let db = setup();
    let s = db
        .analyze("SELECT name AS author, count(*) AS posts FROM users GROUP BY author")
        .unwrap();
    assert_cols(&s, vec![c("author", text()), c("posts", int8())]);
}

// ── Ungrouped column in a grouped query (PG SQLSTATE 42803) ──────────────────

#[test]
fn ungrouped_column_with_aggregate_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT count(*), age FROM users"),
        AnalyzeError::Invalid(_),
        concat!(
            "column \"users.age\" must appear in the GROUP BY clause or be used in an aggregate function\n",
            "  ╭────\n",
            "1 │ SELECT count(*), age FROM users\n",
            "  ·                  ───\n",
            "  ╰────\n",
            "  help: add `users.age` to the GROUP BY clause, or wrap it in an aggregate like max(age)\n",
        ),
    );
}

#[test]
fn ungrouped_column_inside_expression_rejected() {
    let db = setup();
    // The offending column can be nested in an expression, not just bare.
    assert_analyze_err!(
        db.analyze("SELECT count(*), age + 1 FROM users"),
        AnalyzeError::Invalid(_),
        concat!(
            "column \"users.age\" must appear in the GROUP BY clause or be used in an aggregate function\n",
            "  ╭────\n",
            "1 │ SELECT count(*), age + 1 FROM users\n",
            "  ·                  ───\n",
            "  ╰────\n",
            "  help: add `users.age` to the GROUP BY clause, or wrap it in an aggregate like max(age)\n",
        ),
    );
}

#[test]
fn ungrouped_column_with_non_pk_group_by_rejected() {
    let db = setup();
    // Grouping by `name` doesn't cover `age`.
    assert_analyze_err!(
        db.analyze("SELECT name, age FROM users GROUP BY name"),
        AnalyzeError::Invalid(_),
        concat!(
            "column \"users.age\" must appear in the GROUP BY clause or be used in an aggregate function\n",
            "  ╭────\n",
            "1 │ SELECT name, age FROM users GROUP BY name\n",
            "  ·              ───\n",
            "  ╰────\n",
            "  help: add `users.age` to the GROUP BY clause, or wrap it in an aggregate like max(age)\n",
        ),
    );
}

#[test]
fn ungrouped_column_in_having_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT count(*) FROM users HAVING age > 0"),
        AnalyzeError::Invalid(_),
        concat!(
            "column \"users.age\" must appear in the GROUP BY clause or be used in an aggregate function\n",
            "  ╭────\n",
            "1 │ SELECT count(*) FROM users HAVING age > 0\n",
            "  ·                                   ───\n",
            "  ╰────\n",
            "  help: add `users.age` to the GROUP BY clause, or wrap it in an aggregate like max(age)\n",
        ),
    );
}

#[test]
fn ungrouped_column_in_order_by_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT count(*) FROM users ORDER BY age"),
        AnalyzeError::Invalid(_),
        concat!(
            "column \"users.age\" must appear in the GROUP BY clause or be used in an aggregate function\n",
            "  ╭────\n",
            "1 │ SELECT count(*) FROM users ORDER BY age\n",
            "  ·                                     ───\n",
            "  ╰────\n",
            "  help: add `users.age` to the GROUP BY clause, or wrap it in an aggregate like max(age)\n",
        ),
    );
}

#[test]
fn grouped_column_accepted() {
    let db = setup();
    let s = db
        .analyze("SELECT age, count(*) AS n FROM users GROUP BY age")
        .unwrap();
    assert_cols(&s, vec![cn("age", int4()), c("n", int8())]);
}

#[test]
fn non_grouped_columns_ok_when_primary_key_is_grouped() {
    // Functional dependency: grouping by the PK (`id`) functionally determines
    // every other column, so `name` and `age` need not be listed. PG accepts.
    let db = setup();
    let s = db
        .analyze("SELECT id, name, age FROM users GROUP BY id")
        .unwrap();
    assert_cols(
        &s,
        vec![c("id", int8()), c("name", text()), cn("age", int4())],
    );
}

#[test]
fn window_function_does_not_require_grouping() {
    // A window function is not an aggregate — the query isn't grouped, so a
    // plain column alongside it is fine.
    let db = setup();
    let s = db
        .analyze("SELECT age, row_number() OVER () AS rn FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("age", int4()), c("rn", int8())]);
}

#[test]
fn non_boolean_having_rejected() {
    // PG: a non-boolean HAVING is `argument of HAVING must be type boolean…`.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT count(*) FROM users HAVING age"),
        AnalyzeError::Invalid(_),
        concat!(
            "argument of HAVING must be type boolean, not type integer\n",
            "  ╭────\n",
            "1 │ SELECT count(*) FROM users HAVING age\n",
            "  ·                                   ───\n",
            "  ╰────\n",
        ),
    );
}

// ── Aggregate overload resolution must check arg types and arity ─────────────

#[test]
fn single_aggregate_overload_rejects_wrong_arg_type() {
    // `bool_or(boolean)` is the lone `bool_or` overload, but a single
    // aggregate candidate must still type-check: `integer` is not coercible to
    // `boolean`, so the call does not resolve.
    // PG: `function bool_or(integer) does not exist`.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT bool_or(age) FROM users"),
        AnalyzeError::UndefinedFunction(_),
        concat!(
            "function bool_or(integer) does not exist (found 1 candidate(s))\n",
            "  ╭────\n",
            "1 │ SELECT bool_or(age) FROM users\n",
            "  ·        ───┬───\n",
            "  ·           ╰─ function does not exist\n",
            "  ╰────\n",
            "  help: did you mean \"bool_or\"?\n",
        ),
    );
}

#[test]
fn single_aggregate_overload_rejects_wrong_arity() {
    // A lone aggregate overload must also reject the wrong argument count.
    // PG: `function bool_or(text, integer) does not exist`.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT bool_or(name, age) FROM users"),
        AnalyzeError::UndefinedFunction(_),
        concat!(
            "function bool_or(text, integer) does not exist (found 1 candidate(s))\n",
            "  ╭────\n",
            "1 │ SELECT bool_or(name, age) FROM users\n",
            "  ·        ───┬───\n",
            "  ·           ╰─ function does not exist\n",
            "  ╰────\n",
            "  help: did you mean \"bool_or\"?\n",
        ),
    );
}

// ── max/min over the `record` pseudo-type only accepts composite actuals ─────

#[test]
fn max_over_scalar_without_overload_rejected() {
    // `max(record)` exists, but its `record` parameter accepts only an actual
    // composite/record value — not an arbitrary scalar. `max(boolean)` must be
    // rejected (it used to resolve to `record` via the pseudo-type fallback).
    // PG: `function max(boolean) does not exist`.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (active BOOLEAN, prefs JSONB);")
        .unwrap();
    assert_analyze_err!(
        db.analyze("SELECT max(active) FROM t"),
        AnalyzeError::UndefinedFunction(_),
        concat!(
            "function max(boolean) does not exist (found 24 candidate(s))\n",
            "  ╭────\n",
            "1 │ SELECT max(active) FROM t\n",
            "  ·        ─┬─\n",
            "  ·         ╰─ function does not exist\n",
            "  ╰────\n",
            "  help: did you mean \"max\"?\n",
        ),
    );
    assert_analyze_err!(
        db.analyze("SELECT max(prefs) FROM t"),
        AnalyzeError::UndefinedFunction(_),
        concat!(
            "function max(jsonb) does not exist (found 24 candidate(s))\n",
            "  ╭────\n",
            "1 │ SELECT max(prefs) FROM t\n",
            "  ·        ─┬─\n",
            "  ·         ╰─ function does not exist\n",
            "  ╰────\n",
            "  help: did you mean \"max\"?\n",
        ),
    );
}

#[test]
fn max_over_composite_column_accepted() {
    // The `max(record)` overload still resolves for an actual composite value —
    // guards against over-rejecting after the scalar tightening.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TYPE pt AS (x FLOAT8, y FLOAT8);
         CREATE TABLE shapes (id BIGINT PRIMARY KEY, p pt NOT NULL);",
    )
    .unwrap();
    let r = db.analyze("SELECT max(p) FROM shapes");
    assert!(r.is_ok(), "expected max(composite) to resolve, got: {r:?}");
}
