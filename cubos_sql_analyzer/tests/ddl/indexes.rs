//! CREATE / DROP INDEX.
//!
//! Indexes don't change query result types — they're invisible to the
//! analyzer's type/nullability inference. We still parse the statement so
//! that expression indexes can be validated for volatility and so partial
//! unique indexes don't silently pose as ON CONFLICT targets.

use crate::common::*;

// ── VOLATILE function rejection in index expressions ────────────────────────

#[test]
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
        "IMMUTABLE",
    );
}

#[test]
fn nextval_in_index_expression_is_rejected() {
    assert_ddl_err!(
        try_apply(&[
            (
                "0001.sql",
                "CREATE TABLE t (id INT NOT NULL); CREATE SEQUENCE s;"
            ),
            (
                "0002.sql",
                "CREATE INDEX idx_seq ON t ((nextval('s')::int));"
            ),
        ]),
        DdlError::UnsupportedDdl(_),
        "IMMUTABLE",
    );
}

#[test]
fn plain_column_index_is_accepted() {
    // Plain column indexes have no expression — the volatility walker
    // must not over-reject them.
    try_apply(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);",
        ),
        ("0002.sql", "CREATE INDEX idx_name ON t (name);"),
    ])
    .expect("plain column index must apply cleanly");
}

#[test]
fn immutable_function_in_index_expression_is_accepted() {
    try_apply(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);",
        ),
        (
            "0002.sql",
            "CREATE INDEX idx_lower_name ON t ((lower(name)));",
        ),
    ])
    .expect("IMMUTABLE function in index must apply cleanly");
}

// ── partial unique indexes don't cover ON CONFLICT ──────────────────────────
//
// PG only treats a unique index as a valid ON CONFLICT target when it has
// no predicate (or the predicate covers every row). A partial unique index
// `WHERE deleted_at IS NULL` does NOT satisfy `ON CONFLICT (slug)` for the
// generic insert. CREATE INDEX skips emitting `pg_constraint` for these,
// so the validator correctly fails to find a match.

#[test]
fn on_conflict_against_partial_unique_index_should_error() {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, slug TEXT NOT NULL, deleted_at TIMESTAMPTZ);
         CREATE UNIQUE INDEX t_slug_live ON t (slug) WHERE deleted_at IS NULL;",
    )
    .unwrap();
    // PG: `there is no unique or exclusion constraint matching the ON
    // CONFLICT specification` (the partial index doesn't qualify).
    assert_analyze_err!(
        db.analyze(
            "INSERT INTO t (id, slug) VALUES ($p1, $p2) \
             ON CONFLICT (slug) DO NOTHING",
        ),
        AnalyzeError::Invalid(_),
        "no unique or exclusion constraint",
    );
}

#[test]
fn on_conflict_against_full_unique_index_is_accepted() {
    // A non-partial UNIQUE INDEX makes the column a valid ON CONFLICT
    // target — same shape PG uses to back primary-key/unique constraints.
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, slug TEXT NOT NULL);
         CREATE UNIQUE INDEX t_slug_uniq ON t (slug);",
    )
    .unwrap();
    db.analyze(
        "INSERT INTO t (id, slug) VALUES ($p1, $p2) \
         ON CONFLICT (slug) DO NOTHING",
    )
    .unwrap();
}

#[test]
fn unique_index_does_not_match_against_distinct_columns() {
    // The unique covers `(a, b)`, not `(a)` alone.
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE t (a INT NOT NULL, b INT NOT NULL);
         CREATE UNIQUE INDEX t_ab ON t (a, b);",
    )
    .unwrap();
    assert_analyze_err!(
        db.analyze(
            "INSERT INTO t (a, b) VALUES ($p1, $p2) \
             ON CONFLICT (a) DO NOTHING"
        ),
        AnalyzeError::Invalid(_),
        "no unique or exclusion constraint",
    );
}

#[test]
fn expression_unique_index_does_not_match_column_on_conflict() {
    // PG: ON CONFLICT (lower(slug)) needs an expression-based unique
    // index. We don't model that, so the test just confirms a column
    // ON CONFLICT against a func-only unique index isn't matched.
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, slug TEXT NOT NULL);
         CREATE UNIQUE INDEX t_slug_lower ON t ((lower(slug)));",
    )
    .unwrap();
    assert_analyze_err!(
        db.analyze(
            "INSERT INTO t (id, slug) VALUES ($p1, $p2) \
             ON CONFLICT (slug) DO NOTHING"
        ),
        AnalyzeError::Invalid(_),
        "no unique or exclusion constraint",
    );
}
