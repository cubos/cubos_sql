//! Tests where the static analyzer produces correct output (types and
//! nullability) without requiring a live PostgreSQL instance.

mod common;
use common::*;

// ══════════════════════════════════════════════════════════════════════════════
// Fully identical (types AND nullability match)
// ══════════════════════════════════════════════════════════════════════════════

// ── Basic SELECT ──────────────────────────────────────────────────────

#[test]
fn identical_simple_select() {
    let db = setup();
    let s = db.analyze("SELECT id, name, age FROM users").unwrap();
    assert_cols(
        &s,
        vec![c("id", int8()), c("name", text()), cn("age", int4())],
    );
}

#[test]
fn identical_select_with_params() {
    let db = setup();
    let s = db
        .analyze("SELECT id, name FROM users WHERE age > $p1 AND name = $p2")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
    assert_params(&s, vec![p(int4()), p(text())]);
}

#[test]
fn identical_select_star() {
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
fn identical_select_star_from_posts() {
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
fn identical_select_star_from_comments() {
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
fn identical_select_aliased_columns() {
    let db = setup();
    let s = db
        .analyze("SELECT id AS user_id, name AS user_name FROM users")
        .unwrap();
    assert_cols(&s, vec![c("user_id", int8()), c("user_name", text())]);
}

#[test]
fn identical_select_table_qualified() {
    let db = setup();
    let s = db
        .analyze("SELECT users.id, users.name FROM users")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

#[test]
fn identical_select_alias_qualified() {
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
fn identical_select_all_columns_explicit() {
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
fn identical_nullable_column() {
    let db = setup();
    let s = db.analyze("SELECT id, age FROM users").unwrap();
    assert_cols(&s, vec![c("id", int8()), cn("age", int4())]);
}

// ── WHERE variations ──────────────────────────────────────────────────

#[test]
fn identical_where_is_not_null() {
    let db = setup();
    // The analyzer doesn't narrow nullability through WHERE clauses.
    let s = db
        .analyze("SELECT id, age FROM users WHERE age IS NOT NULL")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), cn("age", int4())]);
}

#[test]
fn identical_where_and() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE name = $p1 AND email = $p2")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text()), p(text())]);
}

#[test]
fn identical_where_or() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE name = $p1 OR email = $p2")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text()), p(text())]);
}

#[test]
fn identical_where_in_list() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE age IN (1, 2, 3)")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn identical_where_like() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE name LIKE $p1")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text())]);
}

#[test]
fn identical_where_not() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE NOT (age > $p1)")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(int4())]);
}

#[test]
fn identical_where_is_null() {
    let db = setup();
    let s = db
        .analyze("SELECT id, name FROM users WHERE age IS NULL")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

#[test]
fn identical_where_comparison_operators() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE age >= $p1 AND age <= $p2 AND name <> $p3")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(int4()), p(int4()), p(text())]);
}

// ── JOINs ─────────────────────────────────────────────────────────────

#[test]
fn identical_inner_join() {
    let db = setup();
    let s = db
        .analyze("SELECT u.name, p.title FROM users u INNER JOIN posts p ON p.user_id = u.id")
        .unwrap();
    assert_cols(&s, vec![c("name", text()), c("title", text())]);
}

#[test]
fn identical_inner_join_three_tables() {
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
fn identical_cross_join() {
    let db = setup();
    let s = db
        .analyze("SELECT u.name, p.title FROM users u CROSS JOIN posts p")
        .unwrap();
    assert_cols(&s, vec![c("name", text()), c("title", text())]);
}

#[test]
fn identical_self_join() {
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
fn identical_implicit_cross_join() {
    let db = setup();
    let s = db
        .analyze("SELECT u.name, p.title FROM users u, posts p")
        .unwrap();
    assert_cols(&s, vec![c("name", text()), c("title", text())]);
}

// ── ORDER BY / LIMIT / OFFSET ─────────────────────────────────────────

#[test]
fn identical_order_by() {
    let db = setup();
    let s = db
        .analyze("SELECT id, name FROM users ORDER BY name ASC, id DESC")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

#[test]
fn identical_limit_offset_literals() {
    let db = setup();
    let s = db
        .analyze("SELECT id, name FROM users ORDER BY id LIMIT 10 OFFSET 5")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

#[test]
fn identical_limit_offset_params() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users ORDER BY id LIMIT $p1 OFFSET $p2")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    // LIMIT/OFFSET take int8.
    assert_params(&s, vec![p(int8()), p(int8())]);
}

// ── DML ───────────────────────────────────────────────────────────────

#[test]
fn identical_insert_returning() {
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
fn identical_insert_all_columns() {
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
fn identical_insert_multiple_rows() {
    let db = setup();
    let s = db
        .analyze("INSERT INTO users (name, email) VALUES ($p1, $p2), ($p3, $p4) RETURNING id")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text()), p(text()), p(text()), p(text())]);
}

#[test]
fn identical_insert_into_posts() {
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
fn identical_insert_into_comments() {
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

#[test]
fn identical_update_returning() {
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
fn identical_update_multiple_columns() {
    let db = setup();
    let s = db
        .analyze("UPDATE users SET name = $p1, email = $p2, age = $p3 WHERE id = $p4 RETURNING *")
        .unwrap();
    assert_eq!(s.columns.len(), 7);
    assert_params(&s, vec![p(text()), p(text()), pn(int4()), p(int8())]);
}

#[test]
fn identical_update_with_from() {
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

#[test]
fn identical_delete_returning() {
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
fn identical_delete_returning_star() {
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
fn identical_delete_returning_subset() {
    let db = setup();
    let s = db
        .analyze("DELETE FROM posts WHERE user_id = $p1 RETURNING id, title")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("title", text())]);
    assert_params(&s, vec![p(int8())]);
}

// ── CTEs ──────────────────────────────────────────────────────────────

#[test]
fn identical_cte_simple() {
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
fn identical_cte_multiple() {
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
fn identical_cte_with_insert_returning() {
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

// ── DISTINCT / DISTINCT ON ────────────────────────────────────────────

#[test]
fn identical_select_distinct() {
    let db = setup();
    let s = db.analyze("SELECT DISTINCT name FROM users").unwrap();
    assert_cols(&s, vec![c("name", text())]);
}

#[test]
fn identical_distinct_on() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT DISTINCT ON (user_id) user_id, title \
             FROM posts ORDER BY user_id, published_at DESC NULLS LAST",
        )
        .unwrap();
    assert_cols(&s, vec![c("user_id", int8()), c("title", text())]);
}

// ── Mixed param types ─────────────────────────────────────────────────

#[test]
fn identical_params_all_types() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT id FROM users \
             WHERE name = $p1 AND age = $p2 AND id > $p3 AND created_at > $p4",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text()), p(int4()), p(int8()), p(timestamptz())]);
}

#[test]
fn identical_params_with_cast() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE name = $p1::text AND age > $p2::int4")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text()), p(int4())]);
}

// ── Complex combined queries ──────────────────────────────────────────

#[test]
fn identical_join_with_where_and_limit() {
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

#[test]
fn identical_insert_select() {
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
fn identical_subquery_in_from() {
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
fn identical_in_subquery() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT id, name FROM users \
             WHERE id IN (SELECT user_id FROM posts)",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

#[test]
fn identical_exists_subquery() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT id, name FROM users u \
             WHERE EXISTS (SELECT 1 FROM posts p WHERE p.user_id = u.id)",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

// ══════════════════════════════════════════════════════════════════════════════
// Types match, but our analyzer is MORE PRECISE on nullability.
// PG introspect reports computed expressions as nullable; we know they're not.
// ══════════════════════════════════════════════════════════════════════════════

// ── Literals ──────────────────────────────────────────────────────────

#[test]
fn types_match_integer_literal() {
    let db = setup();
    let s = db.analyze("SELECT 42 AS val").unwrap();
    assert_cols(&s, vec![c("val", int4())]);
}

#[test]
fn types_match_boolean_literal() {
    let db = setup();
    let s = db.analyze("SELECT true AS flag, false AS other").unwrap();
    assert_cols(&s, vec![c("flag", bool_ty()), c("other", bool_ty())]);
}

// ── Arithmetic / operators ────────────────────────────────────────────

#[test]
fn types_match_arithmetic() {
    let db = setup();
    let s = db
        .analyze("SELECT id + 1 AS next_id, age * 2 AS double_age FROM users")
        .unwrap();
    assert_cols(&s, vec![c("next_id", int8()), cn("double_age", int4())]);
}

#[test]
fn types_match_string_concat() {
    let db = setup();
    let s = db
        .analyze("SELECT name || ' <' || email || '>' AS display FROM users")
        .unwrap();
    assert_cols(&s, vec![c("display", text())]);
}

// ── Type casts ────────────────────────────────────────────────────────

#[test]
fn types_match_cast_int_to_text() {
    let db = setup();
    let s = db
        .analyze("SELECT age::text AS age_text FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("age_text", text())]);
}

#[test]
fn types_match_cast_bigint_to_int() {
    let db = setup();
    let s = db
        .analyze("SELECT id::int4 AS short_id FROM users")
        .unwrap();
    assert_cols(&s, vec![c("short_id", int4())]);
}

#[test]
fn types_match_cast_literal() {
    let db = setup();
    let s = db.analyze("SELECT '123'::int4 AS val").unwrap();
    assert_cols(&s, vec![c("val", int4())]);
}

// ── Functions ─────────────────────────────────────────────────────────

#[test]
fn types_match_count_star() {
    let db = setup();
    let s = db.analyze("SELECT count(*) AS total FROM users").unwrap();
    assert_cols(&s, vec![c("total", int8())]);
}

#[test]
fn types_match_upper_lower() {
    let db = setup();
    let s = db
        .analyze("SELECT upper(name) AS up, lower(email) AS lo FROM users")
        .unwrap();
    assert_cols(&s, vec![c("up", text()), c("lo", text())]);
}

#[test]
fn types_match_length() {
    let db = setup();
    let s = db.analyze("SELECT length(name) AS len FROM users").unwrap();
    assert_cols(&s, vec![c("len", int4())]);
}

#[test]
fn types_match_coalesce_with_literal() {
    let db = setup();
    let s = db
        .analyze("SELECT COALESCE(age, 0) AS age_or_zero FROM users")
        .unwrap();
    assert_cols(&s, vec![c("age_or_zero", int4())]);
}

#[test]
fn types_match_now() {
    let db = setup();
    let s = db.analyze("SELECT now() AS ts").unwrap();
    assert_cols(&s, vec![c("ts", timestamptz())]);
}

// ── CASE ──────────────────────────────────────────────────────────────

#[test]
fn types_match_case_with_else() {
    let db = setup();
    let s = db
        .analyze("SELECT CASE WHEN age > 18 THEN 'adult' ELSE 'minor' END AS category FROM users")
        .unwrap();
    assert_cols(&s, vec![c("category", text())]);
}

#[test]
fn types_match_case_expression() {
    let db = setup();
    let s = db
        .analyze("SELECT CASE WHEN age IS NULL THEN 0 ELSE age END AS safe_age FROM users")
        .unwrap();
    assert_eq!(col(&s, "safe_age").pg_type, int4());
}

// ── Boolean / NULL tests ──────────────────────────────────────────────

#[test]
fn types_match_null_test() {
    let db = setup();
    let s = db
        .analyze("SELECT id, age IS NULL AS is_null, age IS NOT NULL AS is_not_null FROM users")
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("id", int8()),
            c("is_null", bool_ty()),
            c("is_not_null", bool_ty()),
        ],
    );
}

#[test]
fn types_match_boolean_test() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT (age > 18) IS TRUE AS adult, (age > 18) IS NOT TRUE AS not_adult FROM users",
        )
        .unwrap();
    assert_cols(&s, vec![c("adult", bool_ty()), c("not_adult", bool_ty())]);
}

// ── GROUP BY / HAVING ─────────────────────────────────────────────────

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

// ── UNION / INTERSECT / EXCEPT ────────────────────────────────────────

#[test]
fn types_match_union_all() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT id, name FROM users WHERE age > 20 \
             UNION ALL \
             SELECT id, name FROM users WHERE age <= 20",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

#[test]
fn types_match_union_distinct() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT name FROM users \
             UNION \
             SELECT title FROM posts",
        )
        .unwrap();
    assert_cols(&s, vec![c("name", text())]);
}

#[test]
fn types_match_intersect() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT name FROM users \
             INTERSECT \
             SELECT title FROM posts",
        )
        .unwrap();
    assert_cols(&s, vec![c("name", text())]);
}

#[test]
fn types_match_except() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT name FROM users \
             EXCEPT \
             SELECT title FROM posts",
        )
        .unwrap();
    assert_cols(&s, vec![c("name", text())]);
}

// ── CTE + UNION ───────────────────────────────────────────────────────

#[test]
fn types_match_cte_union() {
    let db = setup();
    let s = db
        .analyze(
            "WITH all_names AS (\
               SELECT name FROM users \
               UNION ALL \
               SELECT author_name AS name FROM comments\
             ) \
             SELECT name FROM all_names",
        )
        .unwrap();
    assert_cols(&s, vec![c("name", text())]);
}

// ══════════════════════════════════════════════════════════════════════════════
// Enum types (CREATE TYPE ... AS ENUM)
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn identical_enum_column_select() {
    let db = setup();
    let s = db.analyze("SELECT id, name, role FROM users").unwrap();
    assert_cols(
        &s,
        vec![
            c("id", int8()),
            c("name", text()),
            c(
                "role",
                enum_ty("public", "user_role", &["admin", "editor", "viewer"]),
            ),
        ],
    );
}

#[test]
fn identical_enum_in_where() {
    let db = setup();
    let s = db.analyze("SELECT id FROM users WHERE role = $p1").unwrap();
    // $p1 is inferred as the enum type (the macro crate maps it to Rust).
    assert_params(
        &s,
        vec![p(enum_ty(
            "public",
            "user_role",
            &["admin", "editor", "viewer"],
        ))],
    );
}

#[test]
fn identical_enum_in_insert() {
    let db = setup();
    let s = db
        .analyze("INSERT INTO users (name, email, role) VALUES ($p1, $p2, $p3) RETURNING id, role")
        .unwrap();
    assert_params(
        &s,
        vec![
            p(text()),
            p(text()),
            p(enum_ty(
                "public",
                "user_role",
                &["admin", "editor", "viewer"],
            )),
        ],
    );
}

#[test]
fn identical_enum_in_update() {
    let db = setup();
    let s = db
        .analyze("UPDATE users SET role = $p1 WHERE id = $p2 RETURNING role")
        .unwrap();
    assert_params(
        &s,
        vec![
            p(enum_ty(
                "public",
                "user_role",
                &["admin", "editor", "viewer"],
            )),
            p(int8()),
        ],
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Domain types (CREATE DOMAIN)
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn identical_domain_column_surfaces_as_domain_type() {
    // Analyzer surfaces the Domain wrapper with its base type preserved; the
    // macro crate decides whether to treat it as opaque JSONB or unwrap.
    let db = setup();
    let s = db.analyze("SELECT id, preferences FROM users").unwrap();
    assert_cols(
        &s,
        vec![
            c("id", int8()),
            cn("preferences", domain("public", "user_prefs", jsonb())),
        ],
    );
}

#[test]
fn identical_domain_param_insert_surfaces_as_domain_type() {
    let db = setup();
    let s = db
        .analyze("INSERT INTO users (name, email, preferences) VALUES ($p1, $p2, $p3) RETURNING id")
        .unwrap();
    assert_params(
        &s,
        vec![
            p(text()),
            p(text()),
            pn(domain("public", "user_prefs", jsonb())),
        ],
    );
    // cast_type unwraps the domain to its schema-qualified base name.
    assert_eq!(s.params[2].cast_type.as_deref(), Some("pg_catalog.jsonb"));
}

#[test]
fn identical_domain_in_where() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE preferences IS NOT NULL")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn identical_schema_qualified_domain_column() {
    let db = setup();
    let s = db
        .analyze("SELECT channel_id, health FROM whatsapp.channels")
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("channel_id", int8()),
            cn("health", domain("whatsapp", "health_data", jsonb())),
        ],
    );
}

#[test]
fn identical_schema_qualified_domain_param() {
    let db = setup();
    let s = db
        .analyze(
            "INSERT INTO whatsapp.channels (channel_id, health, updated_at) \
             VALUES ($p1, $p2, now())",
        )
        .unwrap();
    assert_params(
        &s,
        vec![p(int8()), pn(domain("whatsapp", "health_data", jsonb()))],
    );
    assert_eq!(s.params[1].cast_type.as_deref(), Some("pg_catalog.jsonb"));
}
