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

// ── pg_index modeling ───────────────────────────────────────────────────────
//
// CREATE INDEX writes pg_class (relkind = 'i') + pg_index. PK/UNIQUE
// inline constraints write the same backing rows so the constraint and
// its supporting index share a name and round-trip through DROP/RENAME
// the way PG does it.

#[test]
fn create_index_emits_pg_class_and_pg_index() {
    let db = build(&[(
        "0001.sql",
        "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
         CREATE INDEX t_name_idx ON t (name);",
    )]);

    let idx_class = db.resolve_table(None, "t_name_idx").unwrap();
    assert!(matches!(idx_class.relkind, RelKind::Index));
    assert!(idx_class.reltype.is_none());

    let table_oid = db.resolve_table(None, "t").unwrap().oid;
    let idx = db
        .pg_index_values()
        .find(|i| i.indexrelid == idx_class.oid)
        .expect("pg_index row missing for created index");
    assert_eq!(idx.indrelid, table_oid);
    assert_eq!(idx.indkey, vec![2]); // name is attnum 2
    assert_eq!(idx.indnatts, 1);
    assert!(!idx.indisunique);
    assert!(!idx.indisprimary);
    assert!(idx.indpred.is_none());
    assert!(idx.indexprs.is_empty());
}

#[test]
fn primary_key_emits_backing_pg_index() {
    let db = build(&[("0001.sql", "CREATE TABLE t (id BIGINT PRIMARY KEY);")]);

    let pkey_class = db.resolve_table(None, "t_pkey").unwrap();
    assert!(matches!(pkey_class.relkind, RelKind::Index));

    let idx = db
        .pg_index_values()
        .find(|i| i.indexrelid == pkey_class.oid)
        .expect("backing pg_index row missing for PRIMARY KEY");
    assert!(idx.indisprimary);
    assert!(idx.indisunique);
    assert_eq!(idx.indkey, vec![1]);
}

#[test]
fn unique_index_with_partial_predicate_records_indpred() {
    let db = build(&[(
        "0001.sql",
        "CREATE TABLE t (id BIGINT PRIMARY KEY, slug TEXT NOT NULL, deleted_at TIMESTAMPTZ);
         CREATE UNIQUE INDEX t_slug_live ON t (slug) WHERE deleted_at IS NULL;",
    )]);

    let idx_class = db.resolve_table(None, "t_slug_live").unwrap();
    let idx = db
        .pg_index_values()
        .find(|i| i.indexrelid == idx_class.oid)
        .unwrap();
    assert!(idx.indisunique);
    assert!(
        idx.indpred.is_some(),
        "partial unique index should populate indpred"
    );
}

#[test]
fn expression_index_records_indexprs_with_zero_indkey_slots() {
    let db = build(&[(
        "0001.sql",
        "CREATE TABLE t (id INT NOT NULL, slug TEXT NOT NULL);
         CREATE INDEX t_lower_slug ON t (id, (lower(slug)));",
    )]);

    let idx_class = db.resolve_table(None, "t_lower_slug").unwrap();
    let idx = db
        .pg_index_values()
        .find(|i| i.indexrelid == idx_class.oid)
        .unwrap();
    // First slot is `id` (attnum 1), second is the expression (0).
    assert_eq!(idx.indkey, vec![1, 0]);
    assert_eq!(idx.indexprs.len(), 1);
}

#[test]
fn create_index_duplicate_name_in_schema_errors() {
    // PG: `relation "t_idx" already exists`. The index shares pg_class with
    // tables/views, so the name must be unique within a schema.
    assert_ddl_err!(
        try_apply(&[
            (
                "0001.sql",
                "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
                 CREATE INDEX my_idx ON t (id);",
            ),
            ("0002.sql", "CREATE INDEX my_idx ON t (name);"),
        ]),
        DdlError::DuplicateObject(_),
        "already exists",
    );
}

#[test]
fn create_index_if_not_exists_skips_duplicate() {
    // PG: `IF NOT EXISTS` swallows the duplicate-name error and leaves
    // the existing index in place.
    let db = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
             CREATE INDEX my_idx ON t (id);",
        ),
        ("0002.sql", "CREATE INDEX IF NOT EXISTS my_idx ON t (name);"),
    ]);

    let idx_class = db.resolve_table(None, "my_idx").unwrap();
    let idx = db
        .pg_index_values()
        .find(|i| i.indexrelid == idx_class.oid)
        .unwrap();
    // Still indexes `id` (attnum 1) — the second statement was a no-op.
    assert_eq!(idx.indkey, vec![1]);
}

#[test]
fn drop_index_removes_pg_class_and_pg_index_rows() {
    let db = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
             CREATE INDEX t_name_idx ON t (name);",
        ),
        ("0002.sql", "DROP INDEX t_name_idx;"),
    ]);

    assert!(db.resolve_table(None, "t_name_idx").is_none());
    assert!(
        db.pg_index_values().next().is_none(),
        "no pg_index rows should remain after DROP INDEX"
    );
}

#[test]
fn drop_table_cascades_to_indexes() {
    // Even without CASCADE, indexes belong to their table — DROP TABLE
    // tears them down implicitly. Mirrors PG.
    let db = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
             CREATE INDEX t_name_idx ON t (name);",
        ),
        ("0002.sql", "DROP TABLE t;"),
    ]);

    assert!(db.resolve_table(None, "t_name_idx").is_none());
    assert!(db.pg_index_values().next().is_none());
}

#[test]
fn drop_index_missing_without_if_exists_errors() {
    assert_ddl_err!(
        try_apply(&[("0001.sql", "DROP INDEX no_such_idx;")]),
        DdlError::DependencyError(_),
        "does not exist",
    );
}

#[test]
fn drop_index_if_exists_no_error_when_missing() {
    let _ = build(&[("0001.sql", "DROP INDEX IF EXISTS no_such_idx;")]);
}

#[test]
fn drop_index_when_target_is_a_table_errors() {
    // PG: `"t" is not an index`. DROP INDEX must reject when the resolved
    // pg_class row isn't relkind = 'i'.
    assert_ddl_err!(
        try_apply(&[
            ("0001.sql", "CREATE TABLE t (id INT NOT NULL);"),
            ("0002.sql", "DROP INDEX t;"),
        ]),
        DdlError::DependencyError(_),
        "not an index",
    );
}

#[test]
fn alter_table_drop_constraint_removes_backing_index() {
    let db = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id BIGINT PRIMARY KEY, slug TEXT NOT NULL UNIQUE);",
        ),
        ("0002.sql", "ALTER TABLE t DROP CONSTRAINT t_slug_key;"),
    ]);

    assert!(db.resolve_table(None, "t_slug_key").is_none());
    // pg_index entries: only the pkey one should remain.
    let remaining: Vec<_> = db.pg_index_values().collect();
    assert_eq!(remaining.len(), 1);
    assert!(remaining[0].indisprimary);
}

#[test]
fn drop_column_referenced_by_index_fails_without_cascade() {
    // PG: `cannot drop column ... because index ... depends on it`. The
    // analyzer mirrors the protection so a stand-alone CREATE INDEX is
    // honored just like a UNIQUE constraint would be.
    assert_ddl_err!(
        try_apply(&[
            (
                "0001.sql",
                "CREATE TABLE t (id INT NOT NULL, slug TEXT NOT NULL);
                 CREATE INDEX t_slug_idx ON t (slug);",
            ),
            ("0002.sql", "ALTER TABLE t DROP COLUMN slug;"),
        ]),
        DdlError::DependencyError(_),
        "depend",
    );
}

#[test]
fn drop_column_cascade_removes_dependent_index() {
    let db = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, slug TEXT NOT NULL);
             CREATE INDEX t_slug_idx ON t (slug);",
        ),
        ("0002.sql", "ALTER TABLE t DROP COLUMN slug CASCADE;"),
    ]);

    assert!(db.resolve_table(None, "t_slug_idx").is_none());
    assert!(db.pg_index_values().next().is_none());
}

#[test]
fn alter_table_rename_constraint_renames_backing_index() {
    let db = build(&[
        ("0001.sql", "CREATE TABLE t (id BIGINT PRIMARY KEY);"),
        (
            "0002.sql",
            "ALTER TABLE t RENAME CONSTRAINT t_pkey TO t_primary;",
        ),
    ]);

    assert!(db.resolve_table(None, "t_pkey").is_none());
    let renamed = db.resolve_table(None, "t_primary").unwrap();
    assert!(matches!(renamed.relkind, RelKind::Index));
}
