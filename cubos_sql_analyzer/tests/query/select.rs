//! SELECT feature: projection, `*`, alias, table qualification,
//! DISTINCT / DISTINCT ON, LIMIT / OFFSET.

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

// ── LIMIT / OFFSET require int8; non-int8 expressions are rejected ───────────

#[test]
fn limit_bool_literal_rejected() {
    let db = setup();
    assert_type_mismatch(&db, "SELECT id FROM users LIMIT true", "bool", "int8");
}

#[test]
fn limit_text_column_rejected() {
    let db = setup();
    assert_type_mismatch(&db, "SELECT id FROM users LIMIT name", "text", "int8");
}

#[test]
fn limit_timestamptz_column_rejected() {
    let db = setup();
    assert_type_mismatch(
        &db,
        "SELECT id FROM users LIMIT created_at",
        "timestamptz",
        "int8",
    );
}

#[test]
fn offset_bool_literal_rejected() {
    let db = setup();
    assert_type_mismatch(&db, "SELECT id FROM users OFFSET false", "bool", "int8");
}

// ── Basic SELECT ─────────────────────────────────────────────────────────────

#[test]
fn simple_select() {
    let db = setup();
    let s = db.analyze("SELECT id, name, age FROM users").unwrap();
    assert_cols(
        &s,
        vec![c("id", int8()), c("name", text()), cn("age", int4())],
    );
}

#[test]
fn select_with_params() {
    let db = setup();
    let s = db
        .analyze("SELECT id, name FROM users WHERE age > $p1 AND name = $p2")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
    assert_params(&s, vec![p(int4()), p(text())]);
}

#[test]
fn select_star() {
    let db = setup();
    let s = db.analyze("SELECT * FROM users").unwrap();
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
}

#[test]
fn select_star_from_posts() {
    let db = setup();
    let s = db.analyze("SELECT * FROM posts").unwrap();
    assert_cols(
        &s,
        vec![
            c("id", int8()),
            c("user_id", int8()),
            c("title", text()),
            cn("body", text()),
            cn("published_at", timestamptz()),
        ],
    );
}

#[test]
fn select_star_from_comments() {
    let db = setup();
    let s = db.analyze("SELECT * FROM comments").unwrap();
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
}

#[test]
fn select_aliased_columns() {
    let db = setup();
    let s = db
        .analyze("SELECT id AS user_id, name AS user_name FROM users")
        .unwrap();
    assert_cols(&s, vec![c("user_id", int8()), c("user_name", text())]);
}

#[test]
fn select_table_qualified() {
    let db = setup();
    let s = db
        .analyze("SELECT users.id, users.name FROM users")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

#[test]
fn select_alias_qualified() {
    let db = setup();
    let s = db
        .analyze("SELECT u.id, u.name, u.age FROM users u")
        .unwrap();
    assert_cols(
        &s,
        vec![c("id", int8()), c("name", text()), cn("age", int4())],
    );
}

#[test]
fn select_all_columns_explicit() {
    let db = setup();
    let s = db
        .analyze("SELECT id, name, email, age, created_at FROM users")
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("id", int8()),
            c("name", text()),
            c("email", text()),
            cn("age", int4()),
            c("created_at", timestamptz()),
        ],
    );
}

#[test]
fn nullable_column() {
    let db = setup();
    let s = db.analyze("SELECT id, age FROM users").unwrap();
    assert_cols(&s, vec![c("id", int8()), cn("age", int4())]);
}

// ── ORDER BY / LIMIT / OFFSET ────────────────────────────────────────────────

#[test]
fn order_by() {
    let db = setup();
    let s = db
        .analyze("SELECT id, name FROM users ORDER BY name ASC, id DESC")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

#[test]
fn limit_offset_literals() {
    let db = setup();
    let s = db
        .analyze("SELECT id, name FROM users ORDER BY id LIMIT 10 OFFSET 5")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

#[test]
fn limit_offset_params() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users ORDER BY id LIMIT $p1 OFFSET $p2")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    // LIMIT/OFFSET take int8.
    assert_params(&s, vec![p(int8()), p(int8())]);
}

// ── DISTINCT / DISTINCT ON ───────────────────────────────────────────────────

#[test]
fn select_distinct() {
    let db = setup();
    let s = db.analyze("SELECT DISTINCT name FROM users").unwrap();
    assert_cols(&s, vec![c("name", text())]);
}

#[test]
fn distinct_on() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT DISTINCT ON (user_id) user_id, title \
             FROM posts ORDER BY user_id, published_at DESC NULLS LAST",
        )
        .unwrap();
    assert_cols(&s, vec![c("user_id", int8()), c("title", text())]);
}

// ── Stress (nullability focus) ───────────────────────────────────────────────

#[test]
fn stress_star_with_left_join() {
    let db = setup();
    // SELECT * from LEFT JOIN — right side columns should be nullable.
    let sql = "SELECT u.id, u.name, p.title, p.body \
               FROM users u \
               LEFT JOIN posts p ON p.user_id = u.id";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "id").nullable);
    assert!(!col(&info, "name").nullable);
    // title is NOT NULL in table but LEFT JOIN makes it nullable.
    assert!(col(&info, "title").nullable);
    // body is nullable in table AND LEFT JOIN.
    assert!(col(&info, "body").nullable);
}

#[test]
fn stress_select_without_from() {
    let db = setup();
    let sql = "SELECT 1 as one, 'hello' as greeting, TRUE as flag";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "one").nullable);
    assert!(!col(&info, "greeting").nullable);
    assert!(!col(&info, "flag").nullable);
}

#[test]
fn stress_select_null_literal() {
    let db = setup();
    let sql = "SELECT NULL as nothing";
    let info = db.analyze(sql).unwrap();
    assert!(col(&info, "nothing").nullable, "NULL literal is nullable");
}

#[test]
fn stress_ambiguous_id_columns() {
    let db = setup();
    // Both tables have 'id' — must use aliases to disambiguate.
    let sql = "SELECT u.id as user_id, p.id as post_id \
               FROM users u INNER JOIN posts p ON p.user_id = u.id";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "user_id").nullable);
    assert!(!col(&info, "post_id").nullable);
}

#[test]
fn stress_limit_offset() {
    let db = setup();
    let sql = "SELECT id, name FROM users ORDER BY id LIMIT 10 OFFSET 5";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "id").nullable);
    assert!(!col(&info, "name").nullable);
}

#[test]
fn stress_distinct_on() {
    let db = setup();
    let sql = "SELECT DISTINCT ON (user_id) user_id, title, body \
               FROM posts ORDER BY user_id, published_at DESC NULLS LAST";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "user_id").nullable);
    assert!(!col(&info, "title").nullable);
    assert!(col(&info, "body").nullable);
}

// ── Mixed computed and direct columns ────────────────────────────────────────

#[test]
fn complex_mixed_computed_and_direct_cols() {
    let db = setup();
    let sql = "SELECT \
                   u.id, \
                   u.name, \
                   u.age, \
                   COUNT(*) as post_count, \
                   COALESCE(u.age, 0) as safe_age, \
                   u.name || ' <' || u.email || '>' as display \
               FROM users u \
               INNER JOIN posts p ON p.user_id = u.id \
               GROUP BY u.id, u.name, u.age, u.email";
    let info = db.analyze(sql).unwrap();
    // Direct columns.
    assert!(!col(&info, "id").nullable);
    assert!(!col(&info, "name").nullable);
    assert!(col(&info, "age").nullable);
    // COUNT(*): NOT NULL.
    assert!(!col(&info, "post_count").nullable);
    // COALESCE(age, 0): NOT NULL.
    assert!(!col(&info, "safe_age").nullable);
    // String concat with NOT NULL cols: NOT NULL.
    assert!(!col(&info, "display").nullable);
}
