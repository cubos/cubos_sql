//! JOIN semantics: INNER / LEFT / RIGHT / FULL / CROSS / self / implicit,
//! plus the nullability rules each introduces.

use crate::common::*;

fn setup() -> Database {
    let mut db = Database::new();
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

// ── INNER / CROSS / self / implicit ──────────────────────────────────────────

#[test]
fn inner_join() {
    let db = setup();
    let s = db
        .analyze("SELECT u.name, p.title FROM users u INNER JOIN posts p ON p.user_id = u.id")
        .unwrap();
    assert_cols(&s, vec![c("name", text()), c("title", text())]);
}

#[test]
fn inner_join_three_tables() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT u.name, p.title, c.content \
             FROM users u \
             INNER JOIN posts p ON p.user_id = u.id \
             INNER JOIN comments c ON c.post_id = p.id",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![c("name", text()), c("title", text()), c("content", text())],
    );
}

#[test]
fn cross_join() {
    let db = setup();
    let s = db
        .analyze("SELECT u.name, p.title FROM users u CROSS JOIN posts p")
        .unwrap();
    assert_cols(&s, vec![c("name", text()), c("title", text())]);
}

#[test]
fn self_join() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT a.name AS name_a, b.name AS name_b \
             FROM users a INNER JOIN users b ON a.id <> b.id",
        )
        .unwrap();
    assert_cols(&s, vec![c("name_a", text()), c("name_b", text())]);
}

#[test]
fn implicit_cross_join() {
    let db = setup();
    let s = db
        .analyze("SELECT u.name, p.title FROM users u, posts p")
        .unwrap();
    assert_cols(&s, vec![c("name", text()), c("title", text())]);
}

#[test]
fn join_with_where_and_limit() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT u.name, p.title \
             FROM users u INNER JOIN posts p ON p.user_id = u.id \
             WHERE u.age > $p1 \
             ORDER BY p.published_at DESC NULLS LAST \
             LIMIT $p2",
        )
        .unwrap();
    assert_cols(&s, vec![c("name", text()), c("title", text())]);
    assert_params(&s, vec![p(int4()), p(int8())]);
}

// ── Outer joins: LEFT/RIGHT/FULL nullability ─────────────────────────────────

#[test]
fn left_join_nullifies_right_side() {
    let db = setup();
    let sql = "SELECT u.name, p.title FROM users u LEFT JOIN posts p ON p.user_id = u.id";
    let info = db.analyze(sql).unwrap();
    // posts.title is NOT NULL in table, but LEFT JOIN makes it nullable.
    assert!(col(&info, "title").nullable);
}

#[test]
fn right_join_nullifies_left_side() {
    let db = setup();
    let sql = "SELECT u.name, p.title FROM users u RIGHT JOIN posts p ON p.user_id = u.id";
    let info = db.analyze(sql).unwrap();
    assert!(col(&info, "name").nullable);
}

#[test]
fn full_outer_join_nullifies_both_sides() {
    let db = setup();
    let sql = "SELECT u.name, p.title FROM users u FULL OUTER JOIN posts p ON p.user_id = u.id";
    let info = db.analyze(sql).unwrap();
    assert!(col(&info, "name").nullable);
    assert!(col(&info, "title").nullable);
}

#[test]
fn chained_left_joins_cascade_nullability() {
    let db = setup();
    // users INNER JOIN posts LEFT JOIN comments:
    // comments columns become nullable, posts/users stay NOT NULL.
    let sql = "SELECT u.name, p.title, c.author_name, c.rating \
               FROM users u \
               INNER JOIN posts p ON p.user_id = u.id \
               LEFT JOIN comments c ON c.post_id = p.id";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "name").nullable);
    assert!(!col(&info, "title").nullable);
    // comments.author_name is NOT NULL in table but LEFT JOIN makes it nullable.
    assert!(col(&info, "author_name").nullable);
    // comments.rating is nullable in table AND LEFT JOIN → nullable.
    assert!(col(&info, "rating").nullable);
}

#[test]
fn three_table_mixed_joins() {
    let db = setup();
    // LEFT JOIN posts, then RIGHT JOIN comments on posts.
    // users: left side of LEFT → NOT NULL.
    // posts: right side of LEFT → nullable. THEN left side of RIGHT → doubly nullable.
    // comments: right side of RIGHT → NOT NULL.
    let sql = "SELECT u.name, p.title, c.content \
               FROM users u \
               LEFT JOIN posts p ON p.user_id = u.id \
               RIGHT JOIN comments c ON c.post_id = p.id";
    let info = db.analyze(sql).unwrap();
    // users.name: RIGHT JOIN makes left side (users+posts) nullable.
    assert!(col(&info, "name").nullable);
    // posts.title: nullable from LEFT JOIN, then also from RIGHT JOIN.
    assert!(col(&info, "title").nullable);
    // comments.content: right side of RIGHT JOIN, NOT NULL in table.
    assert!(!col(&info, "content").nullable);
}

#[test]
fn left_join_with_subquery() {
    let db = setup();
    let sql = "SELECT u.name, latest.title as latest_title \
               FROM users u \
               LEFT JOIN ( \
                   SELECT DISTINCT ON (user_id) user_id, title \
                   FROM posts \
                   ORDER BY user_id, published_at DESC NULLS LAST \
               ) latest ON latest.user_id = u.id";
    let info = db.analyze(sql).unwrap();
    // latest_title: NOT NULL in posts but LEFT JOIN makes it nullable.
    assert!(col(&info, "latest_title").nullable);
}

// ── Stress ───────────────────────────────────────────────────────────────────

#[test]
fn stress_self_join() {
    let db = setup();
    let sql = "SELECT a.name as name_a, b.name as name_b \
               FROM users a \
               INNER JOIN users b ON a.id = b.id";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "name_a").nullable);
    assert!(!col(&info, "name_b").nullable);
}

#[test]
fn stress_cross_join() {
    let db = setup();
    let sql = "SELECT u.name, p.title FROM users u CROSS JOIN posts p";
    let info = db.analyze(sql).unwrap();
    // CROSS JOIN doesn't make anything nullable.
    assert!(!col(&info, "name").nullable);
    assert!(!col(&info, "title").nullable);
}

#[test]
fn stress_implicit_cross_join() {
    let db = setup();
    let sql = "SELECT u.name, p.title FROM users u, posts p WHERE p.user_id = u.id";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "name").nullable);
    assert!(!col(&info, "title").nullable);
}

// ── Torture ──────────────────────────────────────────────────────────────────

#[test]
fn torture_triple_left_join() {
    let db = setup();
    let sql = "SELECT u.name, p.title, c.content, c.rating \
               FROM users u \
               LEFT JOIN posts p ON p.user_id = u.id \
               LEFT JOIN comments c ON c.post_id = p.id";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "name").nullable);
    assert!(col(&info, "title").nullable, "1st LEFT JOIN");
    assert!(col(&info, "content").nullable, "2nd LEFT JOIN");
    assert!(
        col(&info, "rating").nullable,
        "2nd LEFT JOIN + nullable col"
    );
}

#[test]
fn torture_full_join_with_coalesce_fix() {
    let db = setup();
    // FULL JOIN makes both sides nullable, but COALESCE can fix it.
    let sql = "SELECT COALESCE(u.name, p.title, 'unknown') as label \
               FROM users u \
               FULL OUTER JOIN posts p ON p.user_id = u.id";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "label").nullable);
}

#[test]
fn torture_left_join_on_union_subquery() {
    let db = setup();
    // LEFT JOIN on a UNION subquery.
    let sql = "SELECT u.name, all_content.val \
               FROM users u \
               LEFT JOIN ( \
                   SELECT user_id, title as val FROM posts \
                   UNION ALL \
                   SELECT p.user_id, c.content as val FROM comments c \
                   INNER JOIN posts p ON p.id = c.post_id \
               ) all_content ON all_content.user_id = u.id";
    let info = db.analyze(sql).unwrap();
    // val is NOT NULL in the union, but LEFT JOIN makes it nullable.
    assert!(col(&info, "val").nullable);
}
