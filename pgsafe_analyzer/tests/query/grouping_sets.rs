//! GROUPING SETS / CUBE / ROLLUP and the `GROUPING()` function.
//!
//! Key nullability rule: a non-NULL column referenced in a SELECT list
//! becomes nullable for grouping sets that omit it (PG fills those rows
//! with NULL for absent grouping columns). The analyzer must mirror that.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE sales (
            region   TEXT NOT NULL,
            product  TEXT NOT NULL,
            amount   INT  NOT NULL
         );",
    )
    .unwrap();
    db
}

// ── GROUPING SETS ───────────────────────────────────────────────────────────

#[test]
fn grouping_sets_promotes_omitted_columns_to_nullable() {
    let db = setup();
    // Two grouping sets: one groups by `region`, the other groups by
    // nothing. PG fills `region` with NULL on the second-set rows, so the
    // analyzer should report `region` as nullable.
    let s = db
        .analyze(
            "SELECT region, SUM(amount) AS total FROM sales \
             GROUP BY GROUPING SETS ((region), ())",
        )
        .unwrap();
    assert_cols(&s, vec![cn("region", text()), cn("total", int8())]);
}

#[test]
fn grouping_sets_explicit_two_sets() {
    let db = setup();
    // GROUP BY GROUPING SETS ((region), (product)) — both columns become
    // nullable.
    let s = db
        .analyze(
            "SELECT region, product, COUNT(*) AS n FROM sales \
             GROUP BY GROUPING SETS ((region), (product))",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![cn("region", text()), cn("product", text()), c("n", int8())],
    );
}

// ── ROLLUP / CUBE ───────────────────────────────────────────────────────────

#[test]
fn rollup_makes_grouped_columns_nullable() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT region, product, SUM(amount) AS total FROM sales \
             GROUP BY ROLLUP(region, product)",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            cn("region", text()),
            cn("product", text()),
            cn("total", int8()),
        ],
    );
}

#[test]
fn cube_makes_grouped_columns_nullable() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT region, product, COUNT(*) AS n FROM sales \
             GROUP BY CUBE(region, product)",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![cn("region", text()), cn("product", text()), c("n", int8())],
    );
}

// ── GROUPING() function ─────────────────────────────────────────────────────

#[test]
fn grouping_function_returns_int4_not_null() {
    let db = setup();
    // `GROUPING(col)` returns int4 marking whether `col` is part of the
    // current grouping set. Always defined → NOT NULL.
    let s = db
        .analyze(
            "SELECT region, GROUPING(region) AS g, COUNT(*) AS n FROM sales \
             GROUP BY ROLLUP(region)",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![cn("region", text()), c("g", int4()), c("n", int8())],
    );
}

// ── HAVING with GROUPING SETS ───────────────────────────────────────────────

#[test]
fn grouping_sets_with_having() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT region, SUM(amount) AS total FROM sales \
             GROUP BY GROUPING SETS ((region), ()) \
             HAVING SUM(amount) > 0",
        )
        .unwrap();
    assert_cols(&s, vec![cn("region", text()), cn("total", int8())]);
}
