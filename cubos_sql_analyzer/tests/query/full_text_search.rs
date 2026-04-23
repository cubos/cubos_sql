//! Full-text search: `tsvector`/`tsquery` column types, the `@@` match
//! operator, and the common builder/rank/headline functions. These cover
//! what most apps use; advanced operators (`@@@`, jsonpath `@?`) stay out
//! of scope until a consumer needs them.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE docs (
            id    BIGINT PRIMARY KEY,
            body  TEXT NOT NULL,
            tsv   TSVECTOR
         );",
    )
    .unwrap();
    db
}

fn tsvector() -> Type {
    basic("pg_catalog", "tsvector")
}

fn tsquery() -> Type {
    basic("pg_catalog", "tsquery")
}

// ── Builders ─────────────────────────────────────────────────────────────────

#[test]
fn to_tsvector_from_text() {
    let db = setup();
    let s = db
        .analyze("SELECT to_tsvector(body) AS v FROM docs")
        .unwrap();
    assert_cols(&s, vec![c("v", tsvector())]);
}

#[test]
fn to_tsquery_with_config() {
    let db = setup();
    let s = db
        .analyze("SELECT to_tsquery('english', 'foo & bar') AS q")
        .unwrap();
    assert_cols(&s, vec![c("q", tsquery())]);
}

#[test]
fn plainto_tsquery() {
    let db = setup();
    let s = db
        .analyze("SELECT plainto_tsquery('foo bar') AS q")
        .unwrap();
    assert_cols(&s, vec![c("q", tsquery())]);
}

// ── Match operator `@@` ─────────────────────────────────────────────────────

#[test]
fn tsvector_match_in_where() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM docs WHERE tsv @@ to_tsquery('foo')")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn text_match_auto_casts_to_tsvector() {
    let db = setup();
    // `text @@ tsquery` — PG uses an implicit cast from text to tsvector.
    let s = db
        .analyze("SELECT id FROM docs WHERE body @@ plainto_tsquery($p1)")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text())]);
}

// ── Casts ────────────────────────────────────────────────────────────────────

#[test]
fn text_cast_to_tsvector() {
    let db = setup();
    let s = db.analyze("SELECT body::tsvector AS v FROM docs").unwrap();
    assert_cols(&s, vec![c("v", tsvector())]);
}

// ── Ranking ──────────────────────────────────────────────────────────────────

#[test]
fn ts_rank_returns_float4() {
    let db = setup();
    // `ts_rank(tsvector, tsquery)` is strict — nullable `tsv` column makes
    // the rank nullable too.
    let s = db
        .analyze("SELECT ts_rank(tsv, to_tsquery('foo')) AS r FROM docs")
        .unwrap();
    assert_cols(&s, vec![cn("r", float4())]);
}

#[test]
fn ts_headline_returns_text() {
    let db = setup();
    // `ts_headline(text, tsquery)` over NOT NULL `body` → NOT NULL text.
    let s = db
        .analyze("SELECT ts_headline(body, to_tsquery('foo')) AS hl FROM docs")
        .unwrap();
    assert_cols(&s, vec![c("hl", text())]);
}
