//! CREATE / DROP INDEX — **coverage gap**.
//!
//! TODO: plain indexes (CREATE INDEX), UNIQUE indexes, partial indexes
//! (WHERE), expression indexes, covering indexes (INCLUDE), concurrent
//! creation, drop with CASCADE, the absence of effect on query analysis
//! (indexes don't change result types).

use crate::common::*;

// ── pg_proc.provolatile not modeled — VOLATILE functions in index expressions
//
// PG forbids VOLATILE functions like `random()` or `now()` (in some forms)
// from appearing in index expressions, since the index would never agree
// with itself. Without `provolatile` the analyzer can't distinguish them
// from IMMUTABLE callees, so the migration parses cleanly.

#[test]
#[ignore = "pg_proc.provolatile not modeled — VOLATILE function in index expression is not rejected"]
fn volatile_function_in_index_expression_should_error() {
    // PG: `functions in index expression must be marked IMMUTABLE`.
    assert_ddl_err!(
        try_apply(&[
            ("0001.sql", "CREATE TABLE t (id INT NOT NULL);"),
            (
                "0002.sql",
                "CREATE INDEX idx_random ON t ((random() * id));"
            ),
        ]),
        DdlError::UnsupportedDdl(_),
        "must be marked IMMUTABLE",
    );
}

// ── pg_index not modeled — partial unique index does NOT cover ON CONFLICT ──
//
// PG only treats a unique index as a valid ON CONFLICT target when it has
// no predicate (or the predicate covers every row). A partial unique index
// `WHERE deleted_at IS NULL` does NOT satisfy `ON CONFLICT (slug)` for the
// generic insert. Without `pg_index` the analyzer can't tell either way.

#[test]
#[ignore = "pg_index not modeled — partial unique index is silently treated as a valid ON CONFLICT target"]
fn on_conflict_against_partial_unique_index_should_error() {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, slug TEXT NOT NULL, deleted_at TIMESTAMPTZ);
         CREATE UNIQUE INDEX t_slug_live ON t (slug) WHERE deleted_at IS NULL;",
    )
    .unwrap();
    // PG: `there is no unique or exclusion constraint matching the ON
    // CONFLICT specification` (the partial index doesn't qualify because
    // the INSERT has no matching WHERE). The analyzer accepts blindly.
    assert_analyze_err!(
        db.analyze(
            "INSERT INTO t (id, slug) VALUES ($p1, $p2) \
             ON CONFLICT (slug) DO NOTHING",
        ),
        AnalyzeError::Invalid(_),
        "no unique or exclusion constraint",
    );
}
