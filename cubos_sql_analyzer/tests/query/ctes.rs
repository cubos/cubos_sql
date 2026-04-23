//! Common Table Expressions (WITH): basic CTEs, multi-CTE, DML in CTE,
//! CTE referenced in joins/unions/subqueries.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE users (
            id    BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            name  TEXT NOT NULL,
            email TEXT NOT NULL,
            age   INT
         );
         CREATE TABLE posts (
            id      BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            user_id BIGINT NOT NULL,
            title   TEXT NOT NULL,
            body    TEXT
         );
         CREATE TABLE comments (
            id      BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            post_id BIGINT NOT NULL,
            content TEXT NOT NULL
         );",
    )
    .unwrap();
    db
}

// ── Basic CTEs ───────────────────────────────────────────────────────────────

#[test]
fn cte_simple() {
    let db = setup();
    let s = db
        .analyze(
            "WITH active AS (SELECT id, name FROM users WHERE age > 18) \
             SELECT * FROM active",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

#[test]
fn cte_multiple() {
    let db = setup();
    let s = db
        .analyze(
            "WITH \
               u AS (SELECT id, name FROM users), \
               p AS (SELECT user_id, title FROM posts) \
             SELECT u.name, p.title \
             FROM u INNER JOIN p ON p.user_id = u.id",
        )
        .unwrap();
    assert_cols(&s, vec![c("name", text()), c("title", text())]);
}

#[test]
fn cte_with_insert_returning() {
    let db = setup();
    let s = db
        .analyze(
            "WITH new_user AS (\
               INSERT INTO users (name, email) VALUES ($p1, $p2) RETURNING id, name\
             ) SELECT * FROM new_user",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
    assert_params(&s, vec![p(text()), p(text())]);
}

// ── CTE with DML and LEFT JOIN nullability ───────────────────────────────────

#[test]
fn cte_dml_left_join() {
    let db = setup();
    let sql = "WITH ins AS (\
        INSERT INTO posts (user_id, title) VALUES ($p1, $p2) RETURNING id, user_id\
    ) \
    SELECT ins.id, u.name \
    FROM ins \
    LEFT JOIN users u ON u.id = ins.user_id";
    let info = db.analyze(sql).unwrap();
    // LEFT JOIN in DML CTE makes right side nullable.
    assert!(col(&info, "name").nullable);
}

#[test]
fn complex_cte_chain() {
    let db = setup();
    let sql = "WITH \
                   active_users AS (SELECT id, name FROM users), \
                   user_posts AS ( \
                       SELECT au.name, p.title \
                       FROM active_users au \
                       INNER JOIN posts p ON p.user_id = au.id \
                   ) \
               SELECT name, title FROM user_posts";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "name").nullable);
    assert!(!col(&info, "title").nullable);
}

// ── Stress ───────────────────────────────────────────────────────────────────

#[test]
fn stress_cte_used_in_union() {
    let db = setup();
    let sql = "WITH active AS (SELECT name FROM users) \
               SELECT name FROM active \
               UNION ALL \
               SELECT title as name FROM posts";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "name").nullable);
}

#[test]
fn stress_cte_insert_returning_coalesce() {
    let db = setup();
    let sql = "WITH ins AS ( \
                   INSERT INTO users (name, email) VALUES ($p1, $p2) \
                   RETURNING id, age \
               ) \
               SELECT id, COALESCE(age, 0) as safe_age FROM ins";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "id").nullable);
    assert!(!col(&info, "safe_age").nullable);
}

#[test]
fn stress_cte_dml_chain() {
    let db = setup();
    let sql = "WITH \
                   new_user AS ( \
                       INSERT INTO users (name, email) VALUES ($p1, $p2) RETURNING id, name \
                   ), \
                   new_post AS ( \
                       INSERT INTO posts (user_id, title) \
                       SELECT id, $p3 FROM new_user \
                       RETURNING id as post_id, user_id, title \
                   ) \
               SELECT nu.name, np.title, np.post_id \
               FROM new_user nu \
               INNER JOIN new_post np ON np.user_id = nu.id";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "name").nullable);
    assert!(!col(&info, "title").nullable);
    assert!(!col(&info, "post_id").nullable);
}

// ── Torture ──────────────────────────────────────────────────────────────────

#[test]
fn torture_cte_with_union_and_left_join() {
    let db = setup();
    let sql = "WITH all_names AS ( \
                   SELECT name FROM users \
                   UNION ALL \
                   SELECT title as name FROM posts \
               ) \
               SELECT u.id, an.name as other_name \
               FROM users u \
               LEFT JOIN all_names an ON true";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "id").nullable);
    // LEFT JOIN on CTE → nullable.
    assert!(col(&info, "other_name").nullable);
}

#[test]
fn torture_multiple_ctes_cross_reference() {
    let db = setup();
    let sql = "WITH \
                   a AS (SELECT id, name FROM users), \
                   b AS (SELECT a.name, p.title FROM a INNER JOIN posts p ON p.user_id = a.id), \
                   c AS (SELECT b.name, b.title, cm.content \
                         FROM b LEFT JOIN comments cm ON true) \
               SELECT name, title, content FROM c";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "name").nullable);
    assert!(!col(&info, "title").nullable);
    assert!(col(&info, "content").nullable, "LEFT JOIN in CTE c");
}

#[test]
fn torture_select_from_cte_left_join_cte() {
    let db = setup();
    let sql = "WITH \
                   u AS (SELECT id, name FROM users), \
                   p AS (SELECT user_id, title FROM posts) \
               SELECT u.name, p.title \
               FROM u LEFT JOIN p ON p.user_id = u.id";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "name").nullable);
    assert!(col(&info, "title").nullable);
}

#[test]
fn torture_deeply_nested_cte_union_join() {
    let db = setup();
    // CTE → UNION → subquery → LEFT JOIN.
    let sql = "WITH names AS ( \
                   SELECT name as val FROM users \
                   UNION ALL \
                   SELECT title as val FROM posts \
               ) \
               SELECT n.val, u.age \
               FROM names n \
               LEFT JOIN users u ON u.name = n.val";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "val").nullable);
    assert!(col(&info, "age").nullable);
}
