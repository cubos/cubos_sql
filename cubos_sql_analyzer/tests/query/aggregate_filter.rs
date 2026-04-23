//! Aggregate modifiers: `FILTER (WHERE cond)`, `DISTINCT`, and
//! per-aggregate `ORDER BY`. These all sit on top of an ordinary aggregate
//! call and must not change its return type — only the set of rows the
//! aggregate sees.
//!
//! Not exercised yet (intentionally, because the analyzer doesn't handle
//! them today): `WITHIN GROUP (ORDER BY …)` ordered-set aggregates like
//! `percentile_cont`/`percentile_disc`/`mode`, which currently fail
//! overload resolution (`cannot resolve function percentile_cont with 1
//! args (found 4 candidates)`).

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE posts (
            id      BIGINT PRIMARY KEY,
            user_id BIGINT NOT NULL,
            title   TEXT NOT NULL,
            views   INT NOT NULL,
            body    TEXT
         );",
    )
    .unwrap();
    db
}

// ── FILTER (WHERE …) ────────────────────────────────────────────────────────

#[test]
fn count_filter_where_literal() {
    let db = setup();
    // COUNT is always NOT NULL, regardless of the FILTER predicate.
    let s = db
        .analyze("SELECT COUNT(*) FILTER (WHERE views > 10) AS popular FROM posts")
        .unwrap();
    assert_cols(&s, vec![c("popular", int8())]);
}

#[test]
fn sum_filter_where_with_param() {
    let db = setup();
    // SUM with FILTER stays nullable (the FILTER predicate may eliminate
    // every row in the group, leaving SUM with an empty set → NULL).
    // The param inside FILTER is resolved from the column type.
    let s = db
        .analyze(
            "SELECT user_id, SUM(views) FILTER (WHERE views > $p1) AS big_views \
             FROM posts GROUP BY user_id",
        )
        .unwrap();
    assert_cols(&s, vec![c("user_id", int8()), cn("big_views", int8())]);
    assert_params(&s, vec![p(int4())]);
}

#[test]
fn count_filter_always_false_is_still_non_null() {
    let db = setup();
    // Even when FILTER excludes every row, COUNT still returns 0 — never NULL.
    let s = db
        .analyze("SELECT COUNT(*) FILTER (WHERE false) AS c FROM posts")
        .unwrap();
    assert_cols(&s, vec![c("c", int8())]);
}

#[test]
fn multiple_filters_side_by_side() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT \
               COUNT(*) FILTER (WHERE views > 100) AS many, \
               COUNT(*) FILTER (WHERE views = 0)   AS none \
             FROM posts",
        )
        .unwrap();
    assert_cols(&s, vec![c("many", int8()), c("none", int8())]);
}

// ── DISTINCT inside an aggregate ─────────────────────────────────────────────

#[test]
fn count_distinct_int_column() {
    let db = setup();
    // COUNT(DISTINCT x) → int8, still NOT NULL.
    let s = db
        .analyze("SELECT COUNT(DISTINCT user_id) AS authors FROM posts")
        .unwrap();
    assert_cols(&s, vec![c("authors", int8())]);
}

#[test]
fn count_distinct_nullable_column() {
    let db = setup();
    // COUNT(DISTINCT col) ignores NULLs, so still NOT NULL.
    let s = db
        .analyze("SELECT COUNT(DISTINCT body) AS bodies FROM posts")
        .unwrap();
    assert_cols(&s, vec![c("bodies", int8())]);
}

// ── ORDER BY inside an aggregate ─────────────────────────────────────────────

#[test]
fn array_agg_with_order_by() {
    let db = setup();
    // array_agg over a NOT NULL source stays nullable without GROUP BY
    // (empty input → NULL result), and the inner sort doesn't affect the
    // element type.
    let s = db
        .analyze("SELECT array_agg(title ORDER BY id) AS titles FROM posts")
        .unwrap();
    assert_cols(&s, vec![cn("titles", array_of(text()))]);
}

#[test]
fn string_agg_with_order_by_desc() {
    let db = setup();
    let s = db
        .analyze("SELECT string_agg(title, ', ' ORDER BY id DESC) AS joined FROM posts")
        .unwrap();
    assert_cols(&s, vec![cn("joined", text())]);
}

#[test]
fn array_agg_with_filter_and_order_by() {
    let db = setup();
    // FILTER + ORDER BY stack cleanly on top of array_agg.
    let s = db
        .analyze(
            "SELECT array_agg(title ORDER BY views DESC) FILTER (WHERE views > 0) AS top \
             FROM posts",
        )
        .unwrap();
    assert_cols(&s, vec![cn("top", array_of(text()))]);
}
