//! `WITH RECURSIVE`: self-referential CTEs where the recursive arm sees
//! the CTE's own output. The analyzer registers the seed arm's columns in
//! scope before analyzing the recursive arm, then unifies the two arms'
//! types (mirrors PG's common-type resolution over `UNION ALL`).
//!
//! Not exercised yet (still unsupported): `SEARCH BREADTH/DEPTH FIRST BY
//! …` and `CYCLE … SET … USING …` clauses, which carry their own
//! inference logic around the recursion bookkeeping columns.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE categories (
            id        BIGINT PRIMARY KEY,
            parent_id BIGINT,
            name      TEXT NOT NULL
         );
         CREATE TABLE orgs (
            id        BIGINT PRIMARY KEY,
            parent_id BIGINT,
            name      TEXT NOT NULL
         );",
    )
    .unwrap();
    db
}

// ── Basic counter ────────────────────────────────────────────────────────────

#[test]
fn recursive_counter() {
    let db = setup();
    // Classic integer counter. The recursive arm reads back from `t`, which
    // must be resolvable in scope.
    let s = db
        .analyze(
            "WITH RECURSIVE t(n) AS ( \
                SELECT 1 \
                UNION ALL \
                SELECT n + 1 FROM t WHERE n < 10 \
             ) SELECT n FROM t",
        )
        .unwrap();
    assert_cols(&s, vec![c("n", int4())]);
}

#[test]
fn recursive_counter_with_int8_cast() {
    let db = setup();
    // Seed returns `int4` but the recursive arm pushes to `int8` via cast.
    // PG unifies to `int8`; the analyzer does the same.
    let s = db
        .analyze(
            "WITH RECURSIVE t(n) AS ( \
                SELECT 1::int8 \
                UNION ALL \
                SELECT n + 1 FROM t WHERE n < 100 \
             ) SELECT n FROM t",
        )
        .unwrap();
    assert_cols(&s, vec![c("n", int8())]);
}

// ── Hierarchy traversal ──────────────────────────────────────────────────────

#[test]
fn recursive_category_tree() {
    let db = setup();
    // Walk a parent/child tree, accumulating depth.
    let s = db
        .analyze(
            "WITH RECURSIVE tree(id, name, depth) AS ( \
                SELECT id, name, 0 FROM categories WHERE parent_id IS NULL \
                UNION ALL \
                SELECT c.id, c.name, t.depth + 1 \
                FROM categories c JOIN tree t ON c.parent_id = t.id \
             ) SELECT id, name, depth FROM tree",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![c("id", int8()), c("name", text()), c("depth", int4())],
    );
}

#[test]
fn recursive_with_param_in_seed() {
    let db = setup();
    // `$p1` sits inside the seed's WHERE; the param must be typed as int8
    // to match the column.
    let s = db
        .analyze(
            "WITH RECURSIVE tree(id, name) AS ( \
                SELECT id, name FROM orgs WHERE id = $p1 \
                UNION ALL \
                SELECT o.id, o.name \
                FROM orgs o JOIN tree t ON o.parent_id = t.id \
             ) SELECT id, name FROM tree",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
    assert_params(&s, vec![p(int8())]);
}

// ── Nullability unification ──────────────────────────────────────────────────

#[test]
fn recursive_seed_nullable_column_stays_nullable() {
    let db = setup();
    // `parent_id` is nullable in both arms — the CTE column stays nullable.
    let s = db
        .analyze(
            "WITH RECURSIVE tree(id, parent_id) AS ( \
                SELECT id, parent_id FROM categories WHERE parent_id IS NULL \
                UNION ALL \
                SELECT c.id, c.parent_id \
                FROM categories c JOIN tree t ON c.parent_id = t.id \
             ) SELECT id, parent_id FROM tree",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), cn("parent_id", int8())]);
}

#[test]
fn recursive_one_arm_nullable_propagates() {
    let db = setup();
    // Seed always produces a non-null constant, recursive arm reads a
    // nullable column — the union result is nullable.
    let s = db
        .analyze(
            "WITH RECURSIVE tree(id, label) AS ( \
                SELECT id, name FROM categories WHERE parent_id IS NULL \
                UNION ALL \
                SELECT c.id, c.parent_id::text \
                FROM categories c JOIN tree t ON c.parent_id = t.id \
             ) SELECT id, label FROM tree",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), cn("label", text())]);
}

// ── Non-recursive WITH still picks up aliascolnames ──────────────────────────

#[test]
fn non_recursive_cte_with_column_aliases() {
    let db = setup();
    // `WITH t(renamed) AS (SELECT …)` — even without RECURSIVE, the alias
    // list must rewrite the CTE's column names.
    let s = db
        .analyze(
            "WITH t(renamed) AS (SELECT name FROM categories) \
             SELECT renamed FROM t",
        )
        .unwrap();
    assert_cols(&s, vec![c("renamed", text())]);
}
