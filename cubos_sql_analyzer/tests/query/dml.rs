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
        AnalyzeError::UndefinedColumn(_),
        "column \"nonexistent\"",
    );
}

#[test]
fn update_set_nonexistent_column_errors() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("UPDATE users SET nonexistent = $p1 WHERE id = $p2"),
        AnalyzeError::UndefinedColumn(_),
        "column \"nonexistent\"",
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
    assert_cols(
        &info,
        vec![
            c("id", int8()),
            c("post_id", int8()),
            c("author_name", text()),
        ],
    );
}

// ── Static rejection of NOT NULL violations and arity mismatches ────────────

#[test]
fn update_set_not_null_column_to_null_rejected() {
    let db = setup();
    // PG rejects this at runtime (`null value in column "name" violates
    // not-null constraint`). We can catch it statically because the table
    // schema says `name` is NOT NULL.
    assert_analyze_err!(
        db.analyze("UPDATE users SET name = NULL WHERE id = $p1 RETURNING id"),
        AnalyzeError::Invalid(_),
        "NOT NULL column `users.name`",
    );
}

#[test]
fn insert_null_into_not_null_column_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("INSERT INTO users (name, email) VALUES (NULL, $p1)"),
        AnalyzeError::Invalid(_),
        "NOT NULL column `users.name`",
    );
}

#[test]
fn insert_values_row_wrong_arity_rejected() {
    let db = setup();
    // Explicit column list (name, email) expects 2 values per row; we
    // pass 3. PG: `INSERT has more expressions than target columns`.
    assert_analyze_err!(
        db.analyze("INSERT INTO users (name, email) VALUES ($p1, $p2, $p3)"),
        AnalyzeError::Invalid(_),
        "expects 2 values per row",
    );
}

#[test]
fn insert_select_column_count_mismatch_rejected() {
    let db = setup();
    // Target has 2 columns, SELECT has 1.
    assert_analyze_err!(
        db.analyze("INSERT INTO users (name, email) SELECT name FROM users"),
        AnalyzeError::Invalid(_),
        "expects 2 columns, SELECT produces 1",
    );
}

// ── DEFAULT keyword ─────────────────────────────────────────────────────────

#[test]
fn insert_values_with_default_keyword() {
    let db = setup();
    // `DEFAULT` replaces a VALUES item and adopts the target column's type.
    // Only the $p1 param surfaces in the bindings — DEFAULT is not a param.
    let s = db
        .analyze(
            "INSERT INTO users (name, email, role) VALUES ($p1, $p2, DEFAULT) \
             RETURNING id, role",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("id", int8()),
            c(
                "role",
                enum_ty("public", "user_role", &["admin", "editor", "viewer"]),
            ),
        ],
    );
    assert_params(&s, vec![p(text()), p(text())]);
}

#[test]
fn update_set_column_to_default() {
    let db = setup();
    // `UPDATE … SET col = DEFAULT` is PG's spelling for "reset to the
    // column default". The analyzer must accept it without erroring out.
    let s = db
        .analyze(
            "UPDATE users SET role = DEFAULT WHERE id = $p1 \
             RETURNING id, role",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("id", int8()),
            c(
                "role",
                enum_ty("public", "user_role", &["admin", "editor", "viewer"]),
            ),
        ],
    );
    assert_params(&s, vec![p(int8())]);
}

// ── INSERT … ON CONFLICT ────────────────────────────────────────────────────

#[test]
fn insert_on_conflict_do_nothing() {
    let db = setup();
    // DO NOTHING with no RETURNING surfaces only as a param-typed statement.
    let s = db
        .analyze(
            "INSERT INTO users (name, email) VALUES ($p1, $p2) \
             ON CONFLICT (email) DO NOTHING",
        )
        .unwrap();
    assert_params(&s, vec![p(text()), p(text())]);
}

#[test]
fn insert_on_conflict_do_update_with_excluded() {
    let db = setup();
    // `EXCLUDED.name` refers to the row the INSERT was trying to add — the
    // analyzer must resolve it against the target table's schema so the
    // SET assignment type-checks.
    let s = db
        .analyze(
            "INSERT INTO users (name, email) VALUES ($p1, $p2) \
             ON CONFLICT (email) DO UPDATE SET name = EXCLUDED.name \
             RETURNING id, name, email",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![c("id", int8()), c("name", text()), c("email", text())],
    );
    assert_params(&s, vec![p(text()), p(text())]);
}

#[test]
fn insert_on_conflict_do_nothing_returning_not_null_columns() {
    let db = setup();
    // `ON CONFLICT DO NOTHING RETURNING` only emits a row when the INSERT
    // actually inserts. Each returned row is a fresh INSERT, so NOT NULL
    // columns are still NOT NULL — the analyzer should NOT promote them
    // to nullable just because the statement might return zero rows.
    let s = db
        .analyze(
            "INSERT INTO users (name, email) VALUES ($p1, $p2) \
             ON CONFLICT (email) DO NOTHING \
             RETURNING id, name, email",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![c("id", int8()), c("name", text()), c("email", text())],
    );
    assert_params(&s, vec![p(text()), p(text())]);
}

#[test]
fn insert_on_conflict_do_nothing_returning_star() {
    let db = setup();
    // `RETURNING *` over DO NOTHING — every column has its base nullability.
    let s = db
        .analyze(
            "INSERT INTO users (name, email) VALUES ($p1, $p2) \
             ON CONFLICT (email) DO NOTHING RETURNING *",
        )
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
}

#[test]
fn insert_on_conflict_do_update_with_param_expression() {
    let db = setup();
    // `SET age = EXCLUDED.age + $p3` mixes a column reference with a new
    // param — the param must be inferred as int4 (the column's type).
    let s = db
        .analyze(
            "INSERT INTO users (name, email, age) VALUES ($p1, $p2, $p3) \
             ON CONFLICT (email) DO UPDATE SET age = EXCLUDED.age + $p4 \
             RETURNING id",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text()), p(text()), pn(int4()), p(int4())]);
}

// ── Stress ───────────────────────────────────────────────────────────────────

#[test]
fn stress_update_returning_expression() {
    let db = setup();
    let sql = "UPDATE users SET age = $p1 WHERE id = $p2 \
               RETURNING id, COALESCE(age, 0) as safe_age, name || '!' as excited";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
        vec![
            c("id", int8()),
            // COALESCE in RETURNING.
            c("safe_age", int4()),
            // String concat in RETURNING.
            c("excited", text()),
        ],
    );
}

#[test]
fn stress_delete_returning_all_columns() {
    let db = setup();
    let sql = "DELETE FROM users WHERE id = $p1 \
               RETURNING id, name, email, age, created_at";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
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
fn stress_insert_returning_star() {
    let db = setup();
    let sql = "INSERT INTO posts (user_id, title) VALUES ($p1, $p2) RETURNING *";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
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
fn stress_insert_minimal() {
    let db = setup();
    let sql = "INSERT INTO users (name, email) VALUES ($p1, $p2) RETURNING id";
    let info = db.analyze(sql).unwrap();
    assert_params(&info, vec![p(text()), p(text())]);
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
    assert_cols(
        &info,
        vec![c("id", int8()), c("title", text()), cn("body", text())],
    );
}

// ── MERGE (PG 15+) ───────────────────────────────────────────────────────────
//
// Marked ignored: MERGE support is not implemented in the analyzer yet.
// These tests pin the expected shape so they can be flipped on once the
// feature lands.

#[test]
#[ignore = "MERGE statement not yet supported"]
fn merge_when_matched_update() {
    let db = setup();
    // Classic upsert via MERGE. RETURNING preserves the target table's
    // base nullabilities — `id` and `name` NOT NULL, `age` nullable.
    let s = db
        .analyze(
            "MERGE INTO users u \
             USING (SELECT $p1::bigint AS id, $p2::text AS name) src \
             ON u.id = src.id \
             WHEN MATCHED THEN UPDATE SET name = src.name \
             WHEN NOT MATCHED THEN INSERT (name, email) VALUES (src.name, $p3) \
             RETURNING u.id, u.name",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
    assert_params(&s, vec![p(int8()), p(text()), p(text())]);
}

#[test]
#[ignore = "MERGE statement not yet supported"]
fn merge_when_not_matched_insert() {
    let db = setup();
    let s = db
        .analyze(
            "MERGE INTO users u \
             USING (SELECT $p1::text AS email) src \
             ON u.email = src.email \
             WHEN NOT MATCHED THEN INSERT (name, email) VALUES ($p2, src.email)",
        )
        .unwrap();
    assert_params(&s, vec![p(text()), p(text())]);
}

#[test]
#[ignore = "MERGE statement not yet supported"]
fn merge_when_matched_delete() {
    let db = setup();
    let s = db
        .analyze(
            "MERGE INTO users u \
             USING (SELECT $p1::bigint AS id) src \
             ON u.id = src.id \
             WHEN MATCHED THEN DELETE \
             RETURNING u.id, u.name",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
    assert_params(&s, vec![p(int8())]);
}

#[test]
fn torture_expression_in_insert_returning() {
    let db = setup();
    let sql = "INSERT INTO users (name, email, age) VALUES ($p1, $p2, $p3) \
               RETURNING id, \
                         name || ' (' || email || ')' as display, \
                         CASE WHEN age >= 18 THEN true ELSE false END as is_adult";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
        vec![
            c("id", int8()),
            // Concat of NOT NULL → NOT NULL.
            c("display", text()),
            // CASE with ELSE, all literal booleans → NOT NULL.
            c("is_adult", bool_ty()),
        ],
    );
}
