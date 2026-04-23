//! INSERT / UPDATE / DELETE with RETURNING, FROM, WHERE, and their
//! interaction with JOINs, subqueries, and CTEs.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TYPE user_role AS ENUM ('admin', 'editor', 'viewer');
         CREATE DOMAIN user_prefs AS JSONB;
         CREATE TABLE users (
            id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            name        TEXT NOT NULL,
            email       TEXT NOT NULL UNIQUE,
            age         INT,
            role        user_role NOT NULL DEFAULT 'viewer',
            preferences user_prefs,
            created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
         );
         CREATE TABLE posts (
            id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            user_id      BIGINT NOT NULL REFERENCES users(id),
            title        TEXT NOT NULL,
            body         TEXT,
            published_at TIMESTAMPTZ
         );
         CREATE TABLE comments (
            id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            post_id     BIGINT NOT NULL REFERENCES posts(id),
            author_name TEXT NOT NULL,
            content     TEXT NOT NULL,
            rating      INT
         );",
    )
    .unwrap();
    db
}

// ── Unknown column in DML — must match PostgreSQL's error ────────────────────
//
// PG rejects `INSERT INTO t (ghost) VALUES (...)` and `UPDATE t SET ghost = ...`
// with `column "ghost" of relation "t" does not exist`. The analyzer must do
// the same — treating the column as unknown-typed and silently picking `text`
// would mask a real bug in the caller's SQL.

#[test]
fn insert_into_nonexistent_column_errors() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("INSERT INTO users (nonexistent) VALUES ($p1)"),
        AnalyzeError::UnknownColumn(_),
        "users.nonexistent",
    );
}

#[test]
fn update_set_nonexistent_column_errors() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("UPDATE users SET nonexistent = $p1 WHERE id = $p2"),
        AnalyzeError::UnknownColumn(_),
        "users.nonexistent",
    );
}

// ── INSERT … RETURNING ───────────────────────────────────────────────────────

#[test]
fn insert_returning() {
    let db = setup();
    let s = db
        .analyze("INSERT INTO users (name, email) VALUES ($p1, $p2) RETURNING id, name, age")
        .unwrap();
    assert_cols(
        &s,
        vec![c("id", int8()), c("name", text()), cn("age", int4())],
    );
    assert_params(&s, vec![p(text()), p(text())]);
}

#[test]
fn insert_all_columns() {
    let db = setup();
    let s = db
        .analyze("INSERT INTO users (name, email, age) VALUES ($p1, $p2, $p3) RETURNING *")
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("id", int8()),
            c("name", text()),
            c("email", text()),
            cn("age", int4()),
            c(
                "role",
                enum_ty("public", "user_role", &["admin", "editor", "viewer"]),
            ),
            cn("preferences", domain("public", "user_prefs", jsonb())),
            c("created_at", timestamptz()),
        ],
    );
    assert_params(&s, vec![p(text()), p(text()), pn(int4())]);
}

#[test]
fn insert_multiple_rows() {
    let db = setup();
    let s = db
        .analyze("INSERT INTO users (name, email) VALUES ($p1, $p2), ($p3, $p4) RETURNING id")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text()), p(text()), p(text()), p(text())]);
}

#[test]
fn insert_into_posts() {
    let db = setup();
    let s = db
        .analyze(
            "INSERT INTO posts (user_id, title, body) VALUES ($p1, $p2, $p3) RETURNING id, title",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("title", text())]);
    assert_params(&s, vec![p(int8()), p(text()), pn(text())]);
}

#[test]
fn insert_into_comments() {
    let db = setup();
    let s = db
        .analyze(
            "INSERT INTO comments (post_id, author_name, content, rating) \
             VALUES ($p1, $p2, $p3, $p4) RETURNING *",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("id", int8()),
            c("post_id", int8()),
            c("author_name", text()),
            c("content", text()),
            cn("rating", int4()),
        ],
    );
    assert_params(&s, vec![p(int8()), p(text()), p(text()), pn(int4())]);
}

// ── UPDATE … RETURNING ───────────────────────────────────────────────────────

#[test]
fn update_returning() {
    let db = setup();
    let s = db
        .analyze("UPDATE users SET age = $p1 WHERE id = $p2 RETURNING id, name, age")
        .unwrap();
    assert_cols(
        &s,
        vec![c("id", int8()), c("name", text()), cn("age", int4())],
    );
    assert_params(&s, vec![pn(int4()), p(int8())]);
}

#[test]
fn update_multiple_columns() {
    let db = setup();
    let s = db
        .analyze("UPDATE users SET name = $p1, email = $p2, age = $p3 WHERE id = $p4 RETURNING *")
        .unwrap();
    assert_eq!(s.columns.len(), 7);
    assert_params(&s, vec![p(text()), p(text()), pn(int4()), p(int8())]);
}

#[test]
fn update_with_from() {
    let db = setup();
    let s = db
        .analyze(
            "UPDATE posts SET title = $p1 \
             FROM users u WHERE posts.user_id = u.id AND u.name = $p2 \
             RETURNING posts.id, posts.title",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("title", text())]);
    assert_params(&s, vec![p(text()), p(text())]);
}

// ── DELETE … RETURNING ───────────────────────────────────────────────────────

#[test]
fn delete_returning() {
    let db = setup();
    let s = db
        .analyze("DELETE FROM users WHERE id = $p1 RETURNING id, name, age")
        .unwrap();
    assert_cols(
        &s,
        vec![c("id", int8()), c("name", text()), cn("age", int4())],
    );
    assert_params(&s, vec![p(int8())]);
}

#[test]
fn delete_returning_star() {
    let db = setup();
    let s = db
        .analyze("DELETE FROM comments WHERE post_id = $p1 RETURNING *")
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("id", int8()),
            c("post_id", int8()),
            c("author_name", text()),
            c("content", text()),
            cn("rating", int4()),
        ],
    );
    assert_params(&s, vec![p(int8())]);
}

#[test]
fn delete_returning_subset() {
    let db = setup();
    let s = db
        .analyze("DELETE FROM posts WHERE user_id = $p1 RETURNING id, title")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("title", text())]);
    assert_params(&s, vec![p(int8())]);
}

// ── INSERT … SELECT ──────────────────────────────────────────────────────────

#[test]
fn insert_select() {
    let db = setup();
    let s = db
        .analyze(
            "INSERT INTO comments (post_id, author_name, content) \
             SELECT p.id, $p1, $p2 FROM posts p WHERE p.user_id = $p3 \
             RETURNING id",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text()), p(text()), p(int8())]);
}

#[test]
fn complex_insert_select_with_join() {
    let db = setup();
    // INSERT ... SELECT from a JOIN — params come from WHERE.
    let sql = "INSERT INTO comments (post_id, author_name, content) \
               SELECT p.id, $p1, $p2 \
               FROM posts p \
               WHERE p.user_id = $p3 \
               RETURNING id, post_id, author_name";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "id").nullable);
    assert!(!col(&info, "post_id").nullable);
    assert!(!col(&info, "author_name").nullable);
}

// ── Stress ───────────────────────────────────────────────────────────────────

#[test]
fn stress_update_returning_expression() {
    let db = setup();
    let sql = "UPDATE users SET age = $p1 WHERE id = $p2 \
               RETURNING id, COALESCE(age, 0) as safe_age, name || '!' as excited";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "id").nullable);
    // COALESCE in RETURNING.
    assert!(!col(&info, "safe_age").nullable);
    // String concat in RETURNING.
    assert!(!col(&info, "excited").nullable);
}

#[test]
fn stress_delete_returning_all_columns() {
    let db = setup();
    let sql = "DELETE FROM users WHERE id = $p1 \
               RETURNING id, name, email, age, created_at";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "id").nullable);
    assert!(!col(&info, "name").nullable);
    assert!(!col(&info, "email").nullable);
    assert!(col(&info, "age").nullable);
    assert!(!col(&info, "created_at").nullable);
}

#[test]
fn stress_insert_returning_star() {
    let db = setup();
    let sql = "INSERT INTO posts (user_id, title) VALUES ($p1, $p2) RETURNING *";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "id").nullable);
    assert!(!col(&info, "user_id").nullable);
    assert!(!col(&info, "title").nullable);
    assert!(col(&info, "body").nullable);
    assert!(col(&info, "published_at").nullable);
}

#[test]
fn stress_insert_minimal() {
    let db = setup();
    let sql = "INSERT INTO users (name, email) VALUES ($p1, $p2) RETURNING id";
    let info = db.analyze(sql).unwrap();
    assert_eq!(info.params.len(), 2);
    assert!(!col(&info, "id").nullable);
}

// ── Torture ──────────────────────────────────────────────────────────────────

#[test]
fn torture_update_from_join() {
    let db = setup();
    let sql = "UPDATE posts SET body = $p1 \
               FROM users u \
               WHERE posts.user_id = u.id AND u.name = $p2 \
               RETURNING posts.id, posts.title, posts.body";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "id").nullable);
    assert!(!col(&info, "title").nullable);
    assert!(col(&info, "body").nullable);
}

#[test]
fn torture_expression_in_insert_returning() {
    let db = setup();
    let sql = "INSERT INTO users (name, email, age) VALUES ($p1, $p2, $p3) \
               RETURNING id, \
                         name || ' (' || email || ')' as display, \
                         CASE WHEN age >= 18 THEN true ELSE false END as is_adult";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "id").nullable);
    // Concat of NOT NULL → NOT NULL.
    assert!(!col(&info, "display").nullable);
    // CASE with ELSE, all literal booleans → NOT NULL.
    assert!(!col(&info, "is_adult").nullable);
}
