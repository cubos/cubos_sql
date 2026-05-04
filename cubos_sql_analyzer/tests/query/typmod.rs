//! `pg_attribute.atttypmod` / `pg_type.typtypmod` propagation.
//!
//! These exercise the analyzer's modeling of parametric type modifiers:
//! `varchar(n)`, `numeric(p, s)`, `time(p)`, pgvector's `vector(N)`, and the
//! way they flow through column refs, casts, CASE/COALESCE/UNION, domain
//! inheritance, and `ALTER TABLE … ALTER COLUMN TYPE`.

use crate::common::*;

// ── varchar / numeric basics ──────────────────────────────────────────────

#[test]
fn varchar_typmod_propagates_to_select() {
    let mut db = PgCatalog::new();
    db.apply_sql("CREATE TABLE t (id BIGINT PRIMARY KEY, name VARCHAR(50) NOT NULL);")
        .unwrap();
    let s = db.analyze("SELECT name FROM t").unwrap();
    // varchar(50) → typmod = 50 + 4 = 54.
    assert_cols(&s, vec![c("name", varchar_n(50))]);
}

#[test]
fn numeric_typmod_propagates_to_select() {
    let mut db = PgCatalog::new();
    db.apply_sql("CREATE TABLE t (id BIGINT PRIMARY KEY, price NUMERIC(10, 2) NOT NULL);")
        .unwrap();
    let s = db.analyze("SELECT price FROM t").unwrap();
    assert_cols(&s, vec![c("price", numeric_ps(10, 2))]);
}

#[test]
fn varchar_typmod_within_bounds_is_accepted() {
    let mut db = PgCatalog::new();
    db.apply_sql("CREATE TABLE t (slug VARCHAR(8) NOT NULL);")
        .unwrap();
    db.analyze("INSERT INTO t (slug) VALUES ('hi')").unwrap();
}

#[test]
fn numeric_typmod_within_bounds_is_accepted() {
    let mut db = PgCatalog::new();
    db.apply_sql("CREATE TABLE t (id BIGINT PRIMARY KEY, amount NUMERIC(4,2) NOT NULL);")
        .unwrap();
    db.analyze("INSERT INTO t (id, amount) VALUES ($p1, 12.34)")
        .unwrap();
}

// ── pgvector — the headline use case ──────────────────────────────────────

#[test]
fn vector_dimension_propagates_to_select() {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE EXTENSION vector;
         CREATE TABLE items (id BIGINT PRIMARY KEY, embedding vector(384) NOT NULL);",
    )
    .unwrap();
    let s = db.analyze("SELECT embedding FROM items").unwrap();
    assert_cols(
        &s,
        vec![c(
            "embedding",
            cubos_sql_analyzer::Type::Basic {
                schema: "public".into(),
                name: "vector".into(),
                extension: Some("vector".into()),
                typmod: Some(384),
                collation: None,
            },
        )],
    );
}

#[test]
fn vector_dimension_mismatch_in_insert_rejected() {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE EXTENSION vector;
         CREATE TABLE items (id BIGINT PRIMARY KEY, embedding vector(4) NOT NULL);",
    )
    .unwrap();
    // 3 elements provided for vector(4) → PG: `expected 4 dimensions, not 3`.
    assert_analyze_err!(
        db.analyze("INSERT INTO items (id, embedding) VALUES ($p1, '[1,2,3]'::vector)"),
        AnalyzeError::Invalid(_),
        "expected 4 dimensions",
    );
}

// ── CASE / COALESCE unification ───────────────────────────────────────────

#[test]
fn cast_keeps_typmod_when_target_pinned() {
    let db = PgCatalog::new();
    let s = db.analyze("SELECT 'hi'::varchar(10) AS s").unwrap();
    assert_cols(&s, vec![c("s", varchar_n(10))]);
}

#[test]
fn cast_strips_typmod_when_target_has_none() {
    let mut db = PgCatalog::new();
    db.apply_sql("CREATE TABLE t (s VARCHAR(20) NOT NULL);")
        .unwrap();
    // Cast varchar(20) → text drops typmod since the target type changes.
    let s = db.analyze("SELECT s::text AS s FROM t").unwrap();
    assert_cols(&s, vec![c("s", text())]);
}

// ── Domain typmod inheritance ─────────────────────────────────────────────

#[test]
fn domain_inherits_typmod_to_column() {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE DOMAIN short_name AS VARCHAR(20);
         CREATE TABLE t (id BIGINT PRIMARY KEY, n short_name NOT NULL);",
    )
    .unwrap();
    let s = db.analyze("SELECT n FROM t").unwrap();
    // The column inherits typmod 24 (=20+4) from the domain's base.
    assert_cols(
        &s,
        vec![c(
            "n",
            cubos_sql_analyzer::Type::Domain {
                schema: "public".into(),
                name: "short_name".into(),
                base: Box::new(varchar()),
                extension: None,
                typmod: Some(24),
                collation: None,
            },
        )],
    );
}

// ── UNION propagation ─────────────────────────────────────────────────────

#[test]
fn union_with_uniform_typmod_propagates() {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE a (s VARCHAR(20) NOT NULL);
         CREATE TABLE b (s VARCHAR(20) NOT NULL);",
    )
    .unwrap();
    let s = db
        .analyze("SELECT s FROM a UNION ALL SELECT s FROM b")
        .unwrap();
    assert_cols(&s, vec![c("s", varchar_n(20))]);
}

#[test]
fn union_with_mixed_typmod_drops_to_none() {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE a (s VARCHAR(20) NOT NULL);
         CREATE TABLE b (s VARCHAR(50) NOT NULL);",
    )
    .unwrap();
    // Different typmods on the two arms → result has no typmod.
    let s = db
        .analyze("SELECT s FROM a UNION ALL SELECT s FROM b")
        .unwrap();
    assert_cols(&s, vec![c("s", varchar())]);
}

// ── ALTER TABLE ALTER COLUMN TYPE ─────────────────────────────────────────

#[test]
fn alter_column_type_updates_typmod() {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE t (s VARCHAR(20) NOT NULL);
         ALTER TABLE t ALTER COLUMN s TYPE VARCHAR(80);",
    )
    .unwrap();
    let s = db.analyze("SELECT s FROM t").unwrap();
    assert_cols(&s, vec![c("s", varchar_n(80))]);
}

#[test]
fn alter_column_type_to_unmodified_clears_typmod() {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE t (s VARCHAR(20) NOT NULL);
         ALTER TABLE t ALTER COLUMN s TYPE TEXT;",
    )
    .unwrap();
    let s = db.analyze("SELECT s FROM t").unwrap();
    assert_cols(&s, vec![c("s", text())]);
}

#[test]
fn alter_column_type_to_vector_with_dim() {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE EXTENSION vector;
         CREATE TABLE items (id BIGINT PRIMARY KEY, embedding vector(64) NOT NULL);
         ALTER TABLE items ALTER COLUMN embedding TYPE vector(768);",
    )
    .unwrap();
    let s = db.analyze("SELECT embedding FROM items").unwrap();
    assert_cols(
        &s,
        vec![c(
            "embedding",
            cubos_sql_analyzer::Type::Basic {
                schema: "public".into(),
                name: "vector".into(),
                extension: Some("vector".into()),
                typmod: Some(768),
                collation: None,
            },
        )],
    );
}

// ── UPDATE-side validation ────────────────────────────────────────────────

#[test]
fn update_varchar_too_long_rejected() {
    // Compile-time guard — PG only catches the overflow at runtime, so
    // pglite's `prepare` accepts. Opt out of the mirror.
    let mut db = PgCatalog::new();
    db.skip_pg_sanity();
    db.apply_sql("CREATE TABLE t (slug VARCHAR(3) NOT NULL);")
        .unwrap();
    assert_analyze_err!(
        db.analyze("UPDATE t SET slug = 'toolong'"),
        AnalyzeError::Invalid(_),
        "value too long for type character varying(3)",
    );
}

#[test]
fn update_numeric_overflow_rejected() {
    // Compile-time guard: PG only catches numeric overflow at execution
    // time, so pglite's `prepare` doesn't see it. Opt out of the mirror.
    let mut db = PgCatalog::new();
    db.skip_pg_sanity();
    db.apply_sql("CREATE TABLE t (id BIGINT PRIMARY KEY, amount NUMERIC(4,2) NOT NULL);")
        .unwrap();
    assert_analyze_err!(
        db.analyze("UPDATE t SET amount = 12345.67 WHERE id = $p1"),
        AnalyzeError::Invalid(_),
        "numeric field overflow",
    );
}

// ── Encoder rejects invalid args ──────────────────────────────────────────

#[test]
fn varchar_with_invalid_zero_length_rejected_at_ddl() {
    let mut db = PgCatalog::new();
    let res = db.apply_sql("CREATE TABLE t (s VARCHAR(0));");
    assert!(res.is_err(), "VARCHAR(0) must be rejected, got: {res:?}");
}

#[test]
fn numeric_precision_out_of_range_rejected_at_ddl() {
    let mut db = PgCatalog::new();
    let res = db.apply_sql("CREATE TABLE t (a NUMERIC(2000, 2));");
    assert!(
        res.is_err(),
        "NUMERIC(2000, 2) must be rejected, got: {res:?}"
    );
}
