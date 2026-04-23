//! Subqueries: in FROM, scalar, IN / NOT IN, EXISTS / NOT EXISTS,
//! ARRAY(SELECT …).

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE users (
            id    BIGINT PRIMARY KEY,
            name  TEXT NOT NULL,
            age   INT
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

// ── Subquery in FROM ─────────────────────────────────────────────────────────

#[test]
fn subquery_in_from() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT sub.name, sub.age \
             FROM (SELECT name, age FROM users WHERE age IS NOT NULL) sub",
        )
        .unwrap();
    assert_cols(&s, vec![c("name", text()), cn("age", int4())]);
}

#[test]
fn complex_subquery_in_from() {
    let db = setup();
    let sql = "SELECT sub.user_name, sub.post_count \
               FROM ( \
                   SELECT u.name as user_name, COUNT(*) as post_count \
                   FROM users u \
                   INNER JOIN posts p ON p.user_id = u.id \
                   GROUP BY u.name \
               ) sub";
    let info = db.analyze(sql).unwrap();
    // user_name from NOT NULL column → NOT NULL through subquery.
    assert!(!col(&info, "user_name").nullable);
    // post_count is COUNT(*) → NOT NULL.
    assert!(!col(&info, "post_count").nullable);
}

// ── IN subquery ──────────────────────────────────────────────────────────────

#[test]
fn in_subquery() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT id, name FROM users \
             WHERE id IN (SELECT user_id FROM posts)",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

// ── EXISTS subquery ──────────────────────────────────────────────────────────

#[test]
fn exists_subquery() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT id, name FROM users u \
             WHERE EXISTS (SELECT 1 FROM posts p WHERE p.user_id = u.id)",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

#[test]
fn complex_exists_subquery() {
    let db = setup();
    let sql = "SELECT u.name, EXISTS(SELECT 1 FROM posts p WHERE p.user_id = u.id) as has_posts \
               FROM users u";
    let info = db.analyze(sql).unwrap();
    // EXISTS always returns bool, never NULL.
    assert_cols(&info, vec![c("name", text()), c("has_posts", bool_ty())]);
}

// ── Scalar subqueries (always nullable unless aggregate without GROUP BY) ────

#[test]
fn complex_scalar_subquery_always_nullable() {
    let db = setup();
    let sql = "SELECT u.name, \
                      (SELECT p.title FROM posts p WHERE p.user_id = u.id LIMIT 1) as first_post \
               FROM users u";
    let info = db.analyze(sql).unwrap();
    // Scalar subquery is always nullable (zero rows → NULL).
    assert!(col(&info, "first_post").nullable);
}

#[test]
fn subquery_count_star_not_null() {
    let db = setup();
    let sql = "SELECT u.name, \
                      (SELECT COUNT(*) FROM posts p WHERE p.user_id = u.id) as cnt \
               FROM users u";
    let info = db.analyze(sql).unwrap();
    // Aggregate without GROUP BY → guaranteed 1 row, COUNT is NOT NULL.
    assert!(!col(&info, "cnt").nullable);
}

#[test]
fn subquery_count_plus_one_not_null() {
    let db = setup();
    // COUNT(*) + 1 wraps the aggregate in an AExpr — must still detect it.
    let sql = "SELECT u.name, \
                      (SELECT COUNT(*) + 1 FROM posts p WHERE p.user_id = u.id) as cnt \
               FROM users u";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "cnt").nullable);
}

#[test]
fn subquery_count_cast_not_null() {
    let db = setup();
    // COUNT(*)::int wraps aggregate in TypeCast.
    let sql = "SELECT (SELECT COUNT(*)::int FROM posts) as cnt FROM users";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "cnt").nullable);
}

#[test]
fn subquery_coalesce_sum_not_null() {
    let db = setup();
    // COALESCE(SUM(rating), 0) — aggregate detected through COALESCE.
    // SUM is nullable (empty group), but COALESCE with literal → NOT NULL.
    // Also: aggregate without GROUP BY → guaranteed 1 row.
    let sql = "SELECT (SELECT COALESCE(SUM(rating), 0) FROM comments) as total";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "total").nullable);
}

#[test]
fn subquery_sum_nullable() {
    let db = setup();
    // SUM without COALESCE: aggregate != COUNT → nullable result.
    // Even though guaranteed 1 row, SUM itself returns NULL for empty input.
    let sql = "SELECT (SELECT SUM(rating) FROM comments) as total";
    let info = db.analyze(sql).unwrap();
    assert!(col(&info, "total").nullable);
}

#[test]
fn subquery_with_group_by_still_nullable() {
    let db = setup();
    // COUNT(*) with GROUP BY: subquery may return 0 rows → nullable.
    let sql = "SELECT u.name, \
                      (SELECT COUNT(*) FROM posts p WHERE p.user_id = u.id GROUP BY p.user_id) as cnt \
               FROM users u";
    let info = db.analyze(sql).unwrap();
    assert!(col(&info, "cnt").nullable);
}

#[test]
fn subquery_non_aggregate_still_nullable() {
    let db = setup();
    // Non-aggregate scalar subquery: may return 0 rows → nullable.
    let sql = "SELECT u.name, \
                      (SELECT p.title FROM posts p WHERE p.user_id = u.id LIMIT 1) as first_title \
               FROM users u";
    let info = db.analyze(sql).unwrap();
    assert!(col(&info, "first_title").nullable);
}

#[test]
fn subquery_case_wrapping_count_not_null() {
    let db = setup();
    // CASE WHEN ... THEN COUNT(*) ELSE 0 END — aggregate inside CASE with ELSE.
    let sql = "SELECT (SELECT CASE WHEN true THEN COUNT(*) ELSE 0 END FROM posts) as cnt";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "cnt").nullable);
}

// ── Stress ───────────────────────────────────────────────────────────────────

#[test]
fn stress_deeply_nested_subquery() {
    let db = setup();
    let sql = "SELECT * FROM ( \
                   SELECT * FROM ( \
                       SELECT id, name, age FROM users \
                   ) inner_sq \
               ) outer_sq";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
        vec![c("id", int8()), c("name", text()), cn("age", int4())],
    );
}

#[test]
fn stress_subquery_with_left_join_inside() {
    let db = setup();
    // Subquery does LEFT JOIN, outer SELECT sees nullable cols.
    let sql = "SELECT sq.name, sq.title FROM ( \
                   SELECT u.name, p.title \
                   FROM users u \
                   LEFT JOIN posts p ON p.user_id = u.id \
               ) sq";
    let info = db.analyze(sql).unwrap();
    // title is nullable because of LEFT JOIN inside subquery; name stays NOT NULL.
    assert_cols(&info, vec![c("name", text()), cn("title", text())]);
}

#[test]
fn stress_subquery_computed_columns() {
    let db = setup();
    let sql = "SELECT sq.cnt, sq.max_age FROM ( \
                   SELECT COUNT(*) as cnt, MAX(age) as max_age FROM users \
               ) sq";
    let info = db.analyze(sql).unwrap();
    // COUNT is NOT NULL, MAX is nullable.
    assert_cols(&info, vec![c("cnt", int8()), cn("max_age", int4())]);
}

#[test]
fn stress_aggregate_subquery_in_select() {
    let db = setup();
    let sql = "SELECT u.name, \
                      (SELECT COUNT(*) FROM posts p WHERE p.user_id = u.id) as post_count \
               FROM users u";
    let info = db.analyze(sql).unwrap();
    // Aggregate without GROUP BY → exactly 1 row, and COUNT is NOT NULL →
    // scalar subquery result is NOT NULL.
    assert_cols(&info, vec![c("name", text()), c("post_count", int8())]);
}

// ── Torture ──────────────────────────────────────────────────────────────────

#[test]
fn torture_union_in_subquery_in_from() {
    let db = setup();
    let sql = "SELECT sq.val FROM ( \
                   SELECT name as val FROM users \
                   UNION ALL \
                   SELECT title as val FROM posts \
               ) sq";
    let info = db.analyze(sql).unwrap();
    // Both NOT NULL → union NOT NULL → subquery NOT NULL.
    assert_cols(&info, vec![c("val", text())]);
}
