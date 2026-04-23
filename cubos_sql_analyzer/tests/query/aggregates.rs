//! Aggregates and GROUP BY / HAVING: COUNT, SUM, MIN, MAX, AVG,
//! string_agg, array_agg. Nullability rules for empty sets and grouped
//! columns. Strict vs non-strict builtin functions.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new();
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
    let sql = "SELECT user_id, SUM(user_id) as total FROM posts GROUP BY user_id";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "total").nullable);
}

#[test]
fn agg_sum_with_group_by_nullable_input() {
    let db = setup();
    // rating is nullable + GROUP BY → SUM still nullable (all rows in group could be NULL).
    let sql = "SELECT post_id, SUM(rating) as total FROM comments GROUP BY post_id";
    let info = db.analyze(sql).unwrap();
    assert!(col(&info, "total").nullable);
}

#[test]
fn agg_min_max_with_group_by_not_null() {
    let db = setup();
    // title is NOT NULL + GROUP BY → MIN/MAX are NOT NULL.
    let sql = "SELECT user_id, MIN(title) as first_title, MAX(title) as last_title \
               FROM posts GROUP BY user_id";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "first_title").nullable);
    assert!(!col(&info, "last_title").nullable);
}

#[test]
fn agg_avg_with_group_by_not_null() {
    let db = setup();
    // id is NOT NULL + GROUP BY → AVG is NOT NULL.
    let sql = "SELECT user_id, AVG(id) as avg_id FROM posts GROUP BY user_id";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "avg_id").nullable);
}

#[test]
fn agg_count_with_group_by() {
    let db = setup();
    // COUNT is always NOT NULL, with or without GROUP BY.
    let sql = "SELECT user_id, COUNT(*) as cnt FROM posts GROUP BY user_id";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "cnt").nullable);
}

#[test]
fn agg_count_without_group_by() {
    let db = setup();
    // COUNT without GROUP BY: still NOT NULL (returns 0).
    let sql = "SELECT COUNT(*) as cnt FROM posts";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "cnt").nullable);
}

#[test]
fn agg_sum_without_group_by_always_nullable() {
    let db = setup();
    // SUM without GROUP BY: table could be empty → NULL.
    // Even with NOT NULL input.
    let sql = "SELECT SUM(id) as total FROM posts";
    let info = db.analyze(sql).unwrap();
    assert!(col(&info, "total").nullable);
}

#[test]
fn agg_mixed_nullability_with_group_by() {
    let db = setup();
    // Mix of NOT NULL and nullable aggregates in same GROUP BY query.
    let sql = "SELECT post_id, \
                      COUNT(*) as cnt, \
                      SUM(rating) as sum_rating, \
                      MIN(author_name) as first_author, \
                      MAX(rating) as max_rating \
               FROM comments GROUP BY post_id";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "cnt").nullable);
    assert!(col(&info, "sum_rating").nullable);
    assert!(!col(&info, "first_author").nullable);
    assert!(col(&info, "max_rating").nullable);
}

#[test]
fn agg_with_group_by_and_left_join() {
    let db = setup();
    // LEFT JOIN + GROUP BY: right-side columns are nullable from JOIN,
    // so aggregate on them is nullable even with GROUP BY.
    let sql = "SELECT u.id, COUNT(p.id) as post_count, MAX(p.title) as last_title \
               FROM users u \
               LEFT JOIN posts p ON p.user_id = u.id \
               GROUP BY u.id";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "post_count").nullable);
    assert!(col(&info, "last_title").nullable);
}

#[test]
fn agg_string_agg_with_group_by_not_null() {
    let db = setup();
    // string_agg(NOT NULL, delimiter) with GROUP BY → NOT NULL.
    // The literal ', ' has type UNKNOWN — resolved via UNKNOWN-compatible matching.
    let sql = "SELECT post_id, string_agg(author_name, ', ') as authors \
               FROM comments GROUP BY post_id";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "authors").nullable);
}

// ── Stress / torture ─────────────────────────────────────────────────────────

#[test]
fn stress_aggregates_no_group_by() {
    let db = setup();
    let sql = "SELECT COUNT(*) as cnt, SUM(age) as total_age, MAX(name) as last_name FROM users";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "cnt").nullable);
    // SUM and MAX are nullable (empty table → NULL).
    assert!(col(&info, "total_age").nullable);
    assert!(col(&info, "last_name").nullable);
}

#[test]
fn torture_count_with_group_by() {
    let db = setup();
    // COUNT in GROUP BY context is still NOT NULL.
    let sql = "SELECT u.name, COUNT(p.id) as post_count \
               FROM users u \
               LEFT JOIN posts p ON p.user_id = u.id \
               GROUP BY u.name";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "post_count").nullable);
}
