//! Tests for nullability analysis: cases where the static analyzer is more
//! precise than live introspection, CASE/UNION nullability, complex scenarios,
//! stress tests, scalar subquery/aggregate nullability, GROUP BY aggregates,
//! function/operator nullability, and non-strict pg_catalog functions.

mod common;
use common::*;

// ──────────────────────────────────────────────────────────────────────────────
// Tests: analyzer is MORE PRECISE (nullability differs)
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn more_precise_left_join() {
    let db = setup();
    let sql = "SELECT u.name, p.title FROM users u LEFT JOIN posts p ON p.user_id = u.id";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    // posts.title is NOT NULL in the table, but LEFT JOIN makes it nullable.
    assert!(
        col(&static_info, "title").nullable,
        "static analyzer: CORRECT — knows LEFT JOIN makes right side nullable"
    );
}

#[test]
fn more_precise_right_join() {
    let db = setup();
    let sql = "SELECT u.name, p.title FROM users u RIGHT JOIN posts p ON p.user_id = u.id";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(
        col(&static_info, "name").nullable,
        "static analyzer: CORRECT — RIGHT JOIN makes left side nullable"
    );
}

#[test]
fn more_precise_full_join() {
    let db = setup();
    let sql = "SELECT u.name, p.title FROM users u FULL OUTER JOIN posts p ON p.user_id = u.id";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(col(&static_info, "name").nullable, "static: CORRECT");
    assert!(col(&static_info, "title").nullable, "static: CORRECT");
}

#[test]
fn more_precise_count() {
    let db = setup();
    let sql = "SELECT COUNT(*) as cnt FROM users";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(
        !col(&static_info, "cnt").nullable,
        "static analyzer: CORRECT — COUNT(*) is never NULL"
    );
}

#[test]
fn more_precise_coalesce() {
    let db = setup();
    let sql = "SELECT COALESCE(age, 0) as safe_age FROM users";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(
        !col(&static_info, "safe_age").nullable,
        "static analyzer: CORRECT — COALESCE with literal fallback is NOT NULL"
    );
}

#[test]
fn more_precise_literal() {
    let db = setup();
    let sql = "SELECT id, 'constant' as label FROM users";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(
        !col(&static_info, "label").nullable,
        "static analyzer: CORRECT — string literal is NOT NULL"
    );
}

#[test]
fn more_precise_case_with_else() {
    let db = setup();
    let sql = "SELECT CASE WHEN age > 18 THEN 'adult' ELSE 'minor' END as category FROM users";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(
        !col(&static_info, "category").nullable,
        "static analyzer: CORRECT — CASE with ELSE and all-literal branches is NOT NULL"
    );
}

#[test]
fn more_precise_union_all_not_null() {
    let db = setup();
    let sql = "SELECT name as val FROM users UNION ALL SELECT title as val FROM posts";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(
        !col(&static_info, "val").nullable,
        "static analyzer: CORRECT — both branches are NOT NULL"
    );
}

#[test]
fn more_precise_cte_dml_left_join() {
    let db = setup();
    let sql = "WITH ins AS (\
        INSERT INTO posts (user_id, title) VALUES ($p1, $p2) RETURNING id, user_id\
    ) \
    SELECT ins.id, u.name \
    FROM ins \
    LEFT JOIN users u ON u.id = ins.user_id";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(
        col(&static_info, "name").nullable,
        "static analyzer: CORRECT — LEFT JOIN in DML CTE makes right side nullable"
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: CASE without ELSE (both agree: nullable)
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn identical_case_without_else() {
    let db = setup();
    let sql = "SELECT CASE WHEN age > 18 THEN 'adult' END as category FROM users";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    // CASE without ELSE is nullable because there's no ELSE branch.
    assert!(col(&static_info, "category").nullable);
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: UNION with nullable branch (both agree)
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn identical_union_nullable_branch() {
    let db = setup();
    let sql = "SELECT name as val FROM users UNION ALL SELECT body as val FROM posts";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    // nullable because body is nullable
    assert!(col(&static_info, "val").nullable);
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: complex scenarios
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn complex_chained_left_joins_cascade_nullability() {
    let db = setup();
    // users INNER JOIN posts LEFT JOIN comments:
    // comments columns become nullable, posts/users stay NOT NULL.
    let sql = "SELECT u.name, p.title, c.author_name, c.rating \
               FROM users u \
               INNER JOIN posts p ON p.user_id = u.id \
               LEFT JOIN comments c ON c.post_id = p.id";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    // INNER JOIN columns stay NOT NULL.
    assert!(!col(&static_info, "name").nullable);
    assert!(!col(&static_info, "title").nullable);

    // comments.author_name is NOT NULL in table but LEFT JOIN makes it nullable.
    assert!(
        col(&static_info, "author_name").nullable,
        "static: CORRECT — LEFT JOIN makes it nullable"
    );

    // comments.rating is nullable in table AND LEFT JOIN → nullable.
    assert!(col(&static_info, "rating").nullable);
}

#[test]
fn complex_three_table_mixed_joins() {
    let db = setup();
    // LEFT JOIN posts, then RIGHT JOIN comments on posts.
    // users: left side of LEFT → NOT NULL.
    // posts: right side of LEFT → nullable. THEN left side of RIGHT → doubly nullable.
    // comments: right side of RIGHT → NOT NULL.
    let sql = "SELECT u.name, p.title, c.content \
               FROM users u \
               LEFT JOIN posts p ON p.user_id = u.id \
               RIGHT JOIN comments c ON c.post_id = p.id";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    // users.name: RIGHT JOIN makes left side (users+posts) nullable.
    assert!(
        col(&static_info, "name").nullable,
        "static: CORRECT — RIGHT JOIN nullifies the entire left side"
    );

    // posts.title: nullable from LEFT JOIN, then also from RIGHT JOIN.
    assert!(
        col(&static_info, "title").nullable,
        "static: CORRECT — nullable via LEFT JOIN and RIGHT JOIN"
    );

    // comments.content: right side of RIGHT JOIN, NOT NULL in table.
    assert!(!col(&static_info, "content").nullable);
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
    let static_info = db.analyze(sql, &default_config()).unwrap();

    // user_name comes from NOT NULL column → NOT NULL through subquery.
    assert!(!col(&static_info, "user_name").nullable);

    // post_count is COUNT(*) → NOT NULL.
    assert!(
        !col(&static_info, "post_count").nullable,
        "static: CORRECT — COUNT(*) is never NULL, propagated through subquery"
    );
}

#[test]
fn complex_arithmetic_on_nullable() {
    let db = setup();
    // age is nullable → age + 1 should also be nullable.
    let sql = "SELECT id, age + 1 as age_plus_one FROM users";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    // age + 1 is nullable (age can be NULL → NULL + 1 = NULL).
    assert!(col(&static_info, "age_plus_one").nullable);
}

#[test]
fn complex_arithmetic_on_not_null() {
    let db = setup();
    // id is NOT NULL → id + 1 should also be NOT NULL.
    let sql = "SELECT id + 1 as next_id FROM users";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(
        !col(&static_info, "next_id").nullable,
        "static: CORRECT — NOT NULL + literal = NOT NULL"
    );
}

#[test]
fn complex_coalesce_in_arithmetic() {
    let db = setup();
    // COALESCE(age, 0) is NOT NULL → adding 10 should stay NOT NULL.
    let sql = "SELECT COALESCE(age, 0) + 10 as safe_age_plus FROM users";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(
        !col(&static_info, "safe_age_plus").nullable,
        "static: CORRECT — COALESCE(nullable, literal) + literal = NOT NULL"
    );
}

#[test]
fn complex_boolean_with_nullable_input() {
    let db = setup();
    // age IS NOT NULL → bool, NOT NULL. age > 18 → bool, nullable (age can be NULL).
    let sql = "SELECT age IS NOT NULL as has_age, age > 18 as is_adult FROM users";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    // IS NOT NULL is always NOT NULL (returns true/false, never NULL).
    assert!(
        !col(&static_info, "has_age").nullable,
        "static: CORRECT — IS NOT NULL always returns non-null bool"
    );

    // age > 18: age is nullable → comparison is nullable.
    assert!(col(&static_info, "is_adult").nullable);
}

#[test]
fn exists_without_from() {
    let db = setup();
    // SELECT EXISTS(...) without FROM — always returns exactly 1 row, never NULL.
    let sql = "SELECT EXISTS(SELECT 1 FROM users WHERE id = $p1)";
    let static_info = db.analyze(sql, &default_config()).unwrap();
    assert_eq!(static_info.columns.len(), 1);
    assert!(
        !static_info.columns[0].nullable,
        "EXISTS should be NOT NULL"
    );
    assert_eq!(static_info.columns[0].rust_type, "bool");
}

#[test]
fn exists_constant_without_from() {
    let db = setup();
    // SELECT EXISTS(SELECT 1) — pure constant, no table reference at all.
    let sql = "SELECT EXISTS(SELECT 1)";
    let static_info = db.analyze(sql, &default_config()).unwrap();
    assert_eq!(static_info.columns.len(), 1);
    assert!(
        !static_info.columns[0].nullable,
        "EXISTS should be NOT NULL"
    );
    assert_eq!(static_info.columns[0].rust_type, "bool");
}

#[test]
fn complex_exists_subquery() {
    let db = setup();
    let sql = "SELECT u.name, EXISTS(SELECT 1 FROM posts p WHERE p.user_id = u.id) as has_posts \
               FROM users u";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    // EXISTS always returns bool, never NULL.
    assert!(
        !col(&static_info, "has_posts").nullable,
        "static: CORRECT — EXISTS is never NULL"
    );
}

#[test]
fn complex_scalar_subquery_always_nullable() {
    let db = setup();
    let sql = "SELECT u.name, \
                      (SELECT p.title FROM posts p WHERE p.user_id = u.id LIMIT 1) as first_post \
               FROM users u";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    // scalar subquery is always nullable (zero rows → NULL).
    assert!(col(&static_info, "first_post").nullable);
}

#[test]
fn complex_cast_preserves_nullability() {
    let db = setup();
    // Casting a nullable column preserves nullability.
    let sql = "SELECT age::text as age_text, id::text as id_text FROM users";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    // age is nullable → age::text is nullable.
    assert!(col(&static_info, "age_text").nullable);

    // id is NOT NULL → id::text is NOT NULL.
    assert!(
        !col(&static_info, "id_text").nullable,
        "static: CORRECT — cast on NOT NULL col stays NOT NULL"
    );
}

#[test]
fn complex_multiple_params_from_different_contexts() {
    let db = setup();
    // $p1 from WHERE, $p2 from SET-like context via comparison, $p3 from another comparison.
    let sql = "SELECT id, name FROM users WHERE id = $p1 AND age > $p2 AND email = $p3";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert_eq!(static_info.params.len(), 3);
    assert_eq!(static_info.params[0].rust_type, "i64"); // id is BIGINT
    assert_eq!(static_info.params[1].rust_type, "i32"); // age is INT
    assert_eq!(static_info.params[2].rust_type, "String"); // email is TEXT
}

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
    let static_info = db.analyze(sql, &default_config()).unwrap();

    // Direct columns.
    assert!(!col(&static_info, "id").nullable);
    assert!(!col(&static_info, "name").nullable);
    assert!(col(&static_info, "age").nullable);

    // COUNT(*): NOT NULL.
    assert!(!col(&static_info, "post_count").nullable, "static: CORRECT");

    // COALESCE(age, 0): NOT NULL.
    assert!(!col(&static_info, "safe_age").nullable, "static: CORRECT");

    // string concatenation with NOT NULL cols: NOT NULL.
    assert!(!col(&static_info, "display").nullable, "static: CORRECT");
}

#[test]
fn complex_cte_chain() {
    let db = setup();
    // Two CTEs: first gets users, second joins with posts.
    let sql = "WITH \
                   active_users AS (SELECT id, name FROM users), \
                   user_posts AS ( \
                       SELECT au.name, p.title \
                       FROM active_users au \
                       INNER JOIN posts p ON p.user_id = au.id \
                   ) \
               SELECT name, title FROM user_posts";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    // CTE chain: types propagate correctly.
    assert!(!col(&static_info, "name").nullable);
    assert!(!col(&static_info, "title").nullable);
}

#[test]
fn complex_union_three_branches() {
    let db = setup();
    let sql = "SELECT name as label FROM users \
               UNION ALL \
               SELECT title as label FROM posts \
               UNION ALL \
               SELECT author_name as label FROM comments";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    // All three are NOT NULL → union is NOT NULL.
    assert!(!col(&static_info, "label").nullable, "static: CORRECT");
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
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(!col(&static_info, "id").nullable);
    assert!(!col(&static_info, "post_id").nullable);
    assert!(!col(&static_info, "author_name").nullable);
}

#[test]
fn complex_left_join_with_subquery() {
    let db = setup();
    let sql = "SELECT u.name, latest.title as latest_title \
               FROM users u \
               LEFT JOIN ( \
                   SELECT DISTINCT ON (user_id) user_id, title \
                   FROM posts \
                   ORDER BY user_id, published_at DESC NULLS LAST \
               ) latest ON latest.user_id = u.id";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    // latest_title: NOT NULL in posts but LEFT JOIN makes it nullable.
    assert!(
        col(&static_info, "latest_title").nullable,
        "static: CORRECT — LEFT JOIN on subquery makes it nullable"
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// STRESS TESTS: evil SQL designed to break the analyzer
// ──────────────────────────────────────────────────────────────────────────────

// ── Deeply nested COALESCE / CASE / expressions ────────────────────────

#[test]
fn stress_nested_coalesce() {
    let db = setup();
    // COALESCE(COALESCE(nullable, nullable), literal) → NOT NULL
    let sql = "SELECT COALESCE(COALESCE(age, age), 0) as val FROM users";
    let info = static_analyze(&db, sql);
    assert!(
        !col(&info, "val").nullable,
        "nested COALESCE with final literal → NOT NULL"
    );
}

#[test]
fn stress_coalesce_all_nullable() {
    let db = setup();
    // COALESCE(nullable, nullable) → still nullable (no non-null fallback)
    let sql = "SELECT COALESCE(age, age) as val FROM users";
    let info = static_analyze(&db, sql);
    assert!(
        col(&info, "val").nullable,
        "COALESCE of only nullable args → nullable"
    );
}

#[test]
fn stress_case_with_null_branch() {
    let db = setup();
    // CASE with one branch returning NULL explicitly
    let sql = "SELECT CASE WHEN age > 18 THEN name ELSE NULL END as val FROM users";
    let info = static_analyze(&db, sql);
    assert!(col(&info, "val").nullable, "CASE with NULL ELSE → nullable");
}

#[test]
fn stress_case_mixing_nullable_branches() {
    let db = setup();
    // CASE with one NOT NULL branch and one nullable branch
    let sql = "SELECT CASE WHEN id > 0 THEN name ELSE body END as val \
               FROM users u INNER JOIN posts p ON p.user_id = u.id";
    let info = static_analyze(&db, sql);
    // name is NOT NULL but body is nullable → result is nullable
    assert!(
        col(&info, "val").nullable,
        "CASE with mixed nullable branches → nullable"
    );
}

// ── Star expansion edge cases ──────────────────────────────────────────

#[test]
fn stress_star_with_left_join() {
    let db = setup();
    // SELECT * from LEFT JOIN — right side columns should be nullable
    let sql = "SELECT u.id, u.name, p.title, p.body \
               FROM users u \
               LEFT JOIN posts p ON p.user_id = u.id";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(!col(&static_info, "id").nullable);
    assert!(!col(&static_info, "name").nullable);
    // title is NOT NULL in table but LEFT JOIN makes it nullable
    assert!(col(&static_info, "title").nullable);
    // body is nullable in table AND LEFT JOIN
    assert!(col(&static_info, "body").nullable);
}

// ── SELECT without FROM ────────────────────────────────────────────────

#[test]
fn stress_select_without_from() {
    let db = setup();
    let sql = "SELECT 1 as one, 'hello' as greeting, TRUE as flag";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(!col(&static_info, "one").nullable);
    assert!(!col(&static_info, "greeting").nullable);
    assert!(!col(&static_info, "flag").nullable);
}

#[test]
fn stress_select_null_literal() {
    let db = setup();
    let sql = "SELECT NULL as nothing";
    let info = static_analyze(&db, sql);
    assert!(col(&info, "nothing").nullable, "NULL literal is nullable");
}

// ── Multiple same-name columns from different tables ───────────────────

#[test]
fn stress_ambiguous_id_columns() {
    let db = setup();
    // Both tables have 'id' — must use aliases to disambiguate
    let sql = "SELECT u.id as user_id, p.id as post_id \
               FROM users u INNER JOIN posts p ON p.user_id = u.id";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(!col(&static_info, "user_id").nullable);
    assert!(!col(&static_info, "post_id").nullable);
}

// ── Nested subqueries ──────────────────────────────────────────────────

#[test]
fn stress_deeply_nested_subquery() {
    let db = setup();
    let sql = "SELECT * FROM ( \
                   SELECT * FROM ( \
                       SELECT id, name, age FROM users \
                   ) inner_sq \
               ) outer_sq";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(!col(&static_info, "id").nullable);
    assert!(!col(&static_info, "name").nullable);
    assert!(col(&static_info, "age").nullable);
}

#[test]
fn stress_subquery_with_left_join_inside() {
    let db = setup();
    // Subquery does LEFT JOIN, outer SELECT sees nullable cols
    let sql = "SELECT sq.name, sq.title FROM ( \
                   SELECT u.name, p.title \
                   FROM users u \
                   LEFT JOIN posts p ON p.user_id = u.id \
               ) sq";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    // title is nullable because of LEFT JOIN inside subquery.
    assert!(
        col(&static_info, "title").nullable,
        "static: CORRECT — LEFT JOIN nullability propagates through subquery"
    );
}

// ── UNION edge cases ───────────────────────────────────────────────────

#[test]
fn stress_union_with_null_literal_branch() {
    let db = setup();
    // One branch is a literal NULL → union should be nullable
    let sql = "SELECT name as val FROM users \
               UNION ALL \
               SELECT NULL as val";
    let info = static_analyze(&db, sql);
    assert!(
        col(&info, "val").nullable,
        "UNION with NULL branch → nullable"
    );
}

#[test]
fn stress_union_mixed_types() {
    let db = setup();
    // int + bigint → bigint (coercion)
    let sql = "SELECT age as num FROM users \
               UNION ALL \
               SELECT id as num FROM users";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    // age is nullable → union is nullable (even though id is NOT NULL)
    assert!(col(&static_info, "num").nullable);
}

// ── CTE + UNION combo ──────────────────────────────────────────────────

#[test]
fn stress_cte_used_in_union() {
    let db = setup();
    let sql = "WITH active AS (SELECT name FROM users) \
               SELECT name FROM active \
               UNION ALL \
               SELECT title as name FROM posts";
    let info = static_analyze(&db, sql);
    // Both branches NOT NULL
    assert!(!col(&info, "name").nullable);
}

// ── DML with complex RETURNING ─────────────────────────────────────────

#[test]
fn stress_update_returning_expression() {
    let db = setup();
    let sql = "UPDATE users SET age = $p1 WHERE id = $p2 \
               RETURNING id, COALESCE(age, 0) as safe_age, name || '!' as excited";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(!col(&static_info, "id").nullable);
    // COALESCE in RETURNING
    assert!(!col(&static_info, "safe_age").nullable, "static: CORRECT");
    // string concat in RETURNING
    assert!(!col(&static_info, "excited").nullable, "static: CORRECT");
}

// ── DELETE with complex WHERE + RETURNING ──────────────────────────────

#[test]
fn stress_delete_returning_all_columns() {
    let db = setup();
    let sql = "DELETE FROM users WHERE id = $p1 \
               RETURNING id, name, email, age, created_at";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(!col(&static_info, "id").nullable);
    assert!(!col(&static_info, "name").nullable);
    assert!(!col(&static_info, "email").nullable);
    assert!(col(&static_info, "age").nullable);
    assert!(!col(&static_info, "created_at").nullable);
}

// ── CTE with DML + expressions ─────────────────────────────────────────

#[test]
fn stress_cte_insert_returning_coalesce() {
    let db = setup();
    let sql = "WITH ins AS ( \
                   INSERT INTO users (name, email) VALUES ($p1, $p2) \
                   RETURNING id, age \
               ) \
               SELECT id, COALESCE(age, 0) as safe_age FROM ins";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(!col(&static_info, "id").nullable);
    // COALESCE on CTE column: static knows it's NOT NULL
    assert!(!col(&static_info, "safe_age").nullable, "static: CORRECT");
}

// ── Param inference edge cases ─────────────────────────────────────────

#[test]
fn stress_param_from_insert_values() {
    let db = setup();
    let sql = "INSERT INTO posts (user_id, title, body) VALUES ($p1, $p2, $p3) RETURNING id";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert_eq!(static_info.params.len(), 3);
    assert_eq!(static_info.params[0].rust_type, "i64"); // user_id BIGINT
    assert_eq!(static_info.params[1].rust_type, "String"); // title TEXT
    assert_eq!(static_info.params[2].rust_type, "String"); // body TEXT
}

#[test]
fn stress_param_with_cast() {
    let db = setup();
    let sql = "SELECT id FROM users WHERE id = $p1::bigint";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert_eq!(static_info.params[0].rust_type, "i64");
}

// ── Self-join ──────────────────────────────────────────────────────────

#[test]
fn stress_self_join() {
    let db = setup();
    let sql = "SELECT a.name as name_a, b.name as name_b \
               FROM users a \
               INNER JOIN users b ON a.id = b.id";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(!col(&static_info, "name_a").nullable);
    assert!(!col(&static_info, "name_b").nullable);
}

// ── CROSS JOIN ─────────────────────────────────────────────────────────

#[test]
fn stress_cross_join() {
    let db = setup();
    let sql = "SELECT u.name, p.title FROM users u CROSS JOIN posts p";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    // CROSS JOIN doesn't make anything nullable
    assert!(!col(&static_info, "name").nullable);
    assert!(!col(&static_info, "title").nullable);
}

// ── Implicit CROSS JOIN (comma in FROM) ────────────────────────────────

#[test]
fn stress_implicit_cross_join() {
    let db = setup();
    let sql = "SELECT u.name, p.title FROM users u, posts p WHERE p.user_id = u.id";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(!col(&static_info, "name").nullable);
    assert!(!col(&static_info, "title").nullable);
}

// ── Complex WHERE with AND/OR/NOT ──────────────────────────────────────

#[test]
fn stress_complex_where_params() {
    let db = setup();
    let sql = "SELECT id FROM users \
               WHERE (name = $p1 OR email = $p2) AND age > $p3";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert_eq!(static_info.params.len(), 3);
}

// ── RETURNING with star ────────────────────────────────────────────────

#[test]
fn stress_insert_returning_star() {
    let db = setup();
    let sql = "INSERT INTO posts (user_id, title) VALUES ($p1, $p2) RETURNING *";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(!col(&static_info, "id").nullable);
    assert!(!col(&static_info, "user_id").nullable);
    assert!(!col(&static_info, "title").nullable);
    assert!(col(&static_info, "body").nullable);
    assert!(col(&static_info, "published_at").nullable);
}

// ── Aliased subquery with computed columns ─────────────────────────────

#[test]
fn stress_subquery_computed_columns() {
    let db = setup();
    let sql = "SELECT sq.cnt, sq.max_age FROM ( \
                   SELECT COUNT(*) as cnt, MAX(age) as max_age FROM users \
               ) sq";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    // COUNT is NOT NULL, MAX is nullable
    assert!(!col(&static_info, "cnt").nullable, "static: CORRECT");
    assert!(col(&static_info, "max_age").nullable);
}

// ── LIMIT / OFFSET don't affect types ──────────────────────────────────

#[test]
fn stress_limit_offset() {
    let db = setup();
    let sql = "SELECT id, name FROM users ORDER BY id LIMIT 10 OFFSET 5";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(!col(&static_info, "id").nullable);
    assert!(!col(&static_info, "name").nullable);
}

// ── Annotations with complex expressions ───────────────────────────────

#[test]
fn stress_annotation_on_left_join_star() {
    let db = setup();
    // Force nullable LEFT JOIN column to NOT NULL via annotation
    let sql = "SELECT u.name, p.title as \"title!\" \
               FROM users u LEFT JOIN posts p ON p.user_id = u.id";
    let info = static_analyze(&db, sql);
    assert!(
        !col(&info, "title").nullable,
        "! overrides LEFT JOIN nullable"
    );
}

// ── DISTINCT ON ────────────────────────────────────────────────────────

#[test]
fn stress_distinct_on() {
    let db = setup();
    let sql = "SELECT DISTINCT ON (user_id) user_id, title, body \
               FROM posts ORDER BY user_id, published_at DESC NULLS LAST";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(!col(&static_info, "user_id").nullable);
    assert!(!col(&static_info, "title").nullable);
    assert!(col(&static_info, "body").nullable);
}

// ── Mixing aggregates and non-aggregates in subquery ───────────────────

#[test]
fn stress_aggregate_subquery_in_select() {
    let db = setup();
    let sql = "SELECT u.name, \
                      (SELECT COUNT(*) FROM posts p WHERE p.user_id = u.id) as post_count \
               FROM users u";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    // Static analyzer knows: aggregate without GROUP BY → exactly 1 row,
    // and COUNT is NOT NULL → scalar subquery result is NOT NULL.
    assert!(!col(&static_info, "post_count").nullable);
}

// ── INSERT with DEFAULT values (no explicit columns) ───────────────────

#[test]
fn stress_insert_minimal() {
    let db = setup();
    let sql = "INSERT INTO users (name, email) VALUES ($p1, $p2) RETURNING id";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert_eq!(static_info.params.len(), 2);
    assert!(!col(&static_info, "id").nullable);
}

// ── Deeply nested CTE with DML and JOINs ──────────────────────────────

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
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(!col(&static_info, "name").nullable);
    assert!(!col(&static_info, "title").nullable);
    assert!(!col(&static_info, "post_id").nullable);
}

// ── SELECT with only aggregates (no GROUP BY) ──────────────────────────

#[test]
fn stress_aggregates_no_group_by() {
    let db = setup();
    let sql = "SELECT COUNT(*) as cnt, SUM(age) as total_age, MAX(name) as last_name FROM users";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    // COUNT is NOT NULL
    assert!(!col(&static_info, "cnt").nullable, "static: CORRECT");

    // SUM and MAX are nullable (empty table → NULL)
    assert!(col(&static_info, "total_age").nullable);
    assert!(col(&static_info, "last_name").nullable);
}

// ──────────────────────────────────────────────────────────────────────────────
// TORTURE TESTS: even more evil SQL
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn torture_union_in_subquery_in_from() {
    let db = setup();
    let sql = "SELECT sq.val FROM ( \
                   SELECT name as val FROM users \
                   UNION ALL \
                   SELECT title as val FROM posts \
               ) sq";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    // Both NOT NULL → union NOT NULL → subquery NOT NULL.
    assert!(!col(&static_info, "val").nullable, "static: CORRECT");
}

#[test]
fn torture_left_join_on_union_subquery() {
    let db = setup();
    // LEFT JOIN on a UNION subquery
    let sql = "SELECT u.name, all_content.val \
               FROM users u \
               LEFT JOIN ( \
                   SELECT user_id, title as val FROM posts \
                   UNION ALL \
                   SELECT p.user_id, c.content as val FROM comments c \
                   INNER JOIN posts p ON p.id = c.post_id \
               ) all_content ON all_content.user_id = u.id";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    // val is NOT NULL in the union, but LEFT JOIN makes it nullable.
    assert!(
        col(&static_info, "val").nullable,
        "LEFT JOIN on subquery → nullable"
    );
}

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
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(!col(&static_info, "id").nullable);
    // LEFT JOIN on CTE → nullable
    assert!(col(&static_info, "other_name").nullable);
}

#[test]
fn torture_nested_case_in_coalesce() {
    let db = setup();
    // COALESCE(CASE without ELSE, literal) → NOT NULL
    let sql = "SELECT COALESCE( \
                   CASE WHEN age > 18 THEN age END, \
                   0 \
               ) as val FROM users";
    let info = static_analyze(&db, sql);
    // CASE without ELSE is nullable, but COALESCE with 0 fallback makes it NOT NULL.
    assert!(!col(&info, "val").nullable);
}

#[test]
fn torture_triple_left_join() {
    let db = setup();
    let sql = "SELECT u.name, p.title, c.content, c.rating \
               FROM users u \
               LEFT JOIN posts p ON p.user_id = u.id \
               LEFT JOIN comments c ON c.post_id = p.id";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(!col(&static_info, "name").nullable);
    assert!(col(&static_info, "title").nullable, "1st LEFT JOIN");
    assert!(col(&static_info, "content").nullable, "2nd LEFT JOIN");
    assert!(
        col(&static_info, "rating").nullable,
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
    let info = static_analyze(&db, sql);
    // Both u.name and p.title are nullable due to FULL JOIN,
    // but COALESCE with 'unknown' fallback → NOT NULL.
    assert!(!col(&info, "label").nullable);
}

#[test]
fn torture_param_in_coalesce() {
    let db = setup();
    let sql = "SELECT COALESCE(age, $p1) as val FROM users";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    // $p1 is NOT NULL by default → COALESCE has a NOT NULL arg → NOT NULL.
    assert!(!col(&static_info, "val").nullable);
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
    let info = static_analyze(&db, sql);
    assert!(!col(&info, "name").nullable);
    assert!(!col(&info, "title").nullable);
    assert!(col(&info, "content").nullable, "LEFT JOIN in CTE c");
}

#[test]
fn torture_update_from_join() {
    let db = setup();
    let sql = "UPDATE posts SET body = $p1 \
               FROM users u \
               WHERE posts.user_id = u.id AND u.name = $p2 \
               RETURNING posts.id, posts.title, posts.body";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(!col(&static_info, "id").nullable);
    assert!(!col(&static_info, "title").nullable);
    assert!(col(&static_info, "body").nullable);
}

#[test]
fn torture_select_from_cte_left_join_cte() {
    let db = setup();
    let sql = "WITH \
                   u AS (SELECT id, name FROM users), \
                   p AS (SELECT user_id, title FROM posts) \
               SELECT u.name, p.title \
               FROM u LEFT JOIN p ON p.user_id = u.id";
    let info = static_analyze(&db, sql);
    assert!(!col(&info, "name").nullable);
    assert!(col(&info, "title").nullable, "LEFT JOIN between CTEs");
}

#[test]
fn torture_count_with_group_by() {
    let db = setup();
    // COUNT in GROUP BY context is still NOT NULL.
    let sql = "SELECT u.name, COUNT(p.id) as post_count \
               FROM users u \
               LEFT JOIN posts p ON p.user_id = u.id \
               GROUP BY u.name";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(
        !col(&static_info, "post_count").nullable,
        "static: CORRECT — COUNT is never NULL"
    );
}

#[test]
fn torture_expression_in_insert_returning() {
    let db = setup();
    let sql = "INSERT INTO users (name, email, age) VALUES ($p1, $p2, $p3) \
               RETURNING id, \
                         name || ' (' || email || ')' as display, \
                         CASE WHEN age >= 18 THEN true ELSE false END as is_adult";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    assert!(!col(&static_info, "id").nullable);
    // concat of NOT NULL → NOT NULL
    assert!(!col(&static_info, "display").nullable, "static: CORRECT");
    // CASE with ELSE, all literal booleans → NOT NULL
    assert!(!col(&static_info, "is_adult").nullable, "static: CORRECT");
}

#[test]
fn torture_deeply_nested_cte_union_join() {
    let db = setup();
    // CTE → UNION → subquery → LEFT JOIN
    let sql = "WITH names AS ( \
                   SELECT name as val FROM users \
                   UNION ALL \
                   SELECT title as val FROM posts \
               ) \
               SELECT n.val, u.age \
               FROM names n \
               LEFT JOIN users u ON u.name = n.val";
    let info = static_analyze(&db, sql);
    assert!(!col(&info, "val").nullable, "CTE UNION of NOT NULL");
    assert!(col(&info, "age").nullable, "LEFT JOIN + nullable col");
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: scalar subquery + aggregate nullability
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn subquery_count_star_not_null() {
    let db = setup();
    let sql = "SELECT u.name, \
                      (SELECT COUNT(*) FROM posts p WHERE p.user_id = u.id) as cnt \
               FROM users u";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    // Static: aggregate without GROUP BY → guaranteed 1 row, COUNT is NOT NULL.
    assert!(!col(&static_info, "cnt").nullable);
}

#[test]
fn subquery_count_plus_one_not_null() {
    let db = setup();
    // COUNT(*) + 1 wraps the aggregate in an AExpr — must still detect it.
    let sql = "SELECT u.name, \
                      (SELECT COUNT(*) + 1 FROM posts p WHERE p.user_id = u.id) as cnt \
               FROM users u";
    let info = static_analyze(&db, sql);
    assert!(!col(&info, "cnt").nullable);
}

#[test]
fn subquery_count_cast_not_null() {
    let db = setup();
    // COUNT(*)::int wraps aggregate in TypeCast.
    let sql = "SELECT (SELECT COUNT(*)::int FROM posts) as cnt FROM users";
    let info = static_analyze(&db, sql);
    assert!(!col(&info, "cnt").nullable);
}

#[test]
fn subquery_coalesce_sum_not_null() {
    let db = setup();
    // COALESCE(SUM(rating), 0) — aggregate detected through COALESCE.
    // SUM is nullable (empty group), but COALESCE with literal → NOT NULL.
    // Also: aggregate without GROUP BY → guaranteed 1 row.
    let sql = "SELECT (SELECT COALESCE(SUM(rating), 0) FROM comments) as total";
    let info = static_analyze(&db, sql);
    assert!(!col(&info, "total").nullable);
}

#[test]
fn subquery_sum_nullable() {
    let db = setup();
    // SUM without COALESCE: aggregate != COUNT → nullable result.
    // Even though guaranteed 1 row, SUM itself returns NULL for empty input.
    let sql = "SELECT (SELECT SUM(rating) FROM comments) as total";
    let info = static_analyze(&db, sql);
    assert!(col(&info, "total").nullable);
}

#[test]
fn subquery_with_group_by_still_nullable() {
    let db = setup();
    // COUNT(*) with GROUP BY: subquery may return 0 rows → nullable.
    let sql = "SELECT u.name, \
                      (SELECT COUNT(*) FROM posts p WHERE p.user_id = u.id GROUP BY p.user_id) as cnt \
               FROM users u";
    let info = static_analyze(&db, sql);
    assert!(col(&info, "cnt").nullable);
}

#[test]
fn subquery_non_aggregate_still_nullable() {
    let db = setup();
    // Non-aggregate scalar subquery: may return 0 rows → nullable.
    let sql = "SELECT u.name, \
                      (SELECT p.title FROM posts p WHERE p.user_id = u.id LIMIT 1) as first_title \
               FROM users u";
    let info = static_analyze(&db, sql);
    assert!(col(&info, "first_title").nullable);
}

#[test]
fn subquery_case_wrapping_count_not_null() {
    let db = setup();
    // CASE WHEN ... THEN COUNT(*) ELSE 0 END — aggregate inside CASE with ELSE.
    let sql = "SELECT (SELECT CASE WHEN true THEN COUNT(*) ELSE 0 END FROM posts) as cnt";
    let info = static_analyze(&db, sql);
    assert!(!col(&info, "cnt").nullable);
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: aggregates with GROUP BY
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn agg_sum_with_group_by_not_null_input() {
    let db = setup();
    // user_id is NOT NULL + GROUP BY → SUM guaranteed non-null.
    let sql = "SELECT user_id, SUM(user_id) as total FROM posts GROUP BY user_id";
    let static_info = db.analyze(sql, &default_config()).unwrap();

    // Static: GROUP BY + NOT NULL input → NOT NULL.
    assert!(!col(&static_info, "total").nullable);
}

#[test]
fn agg_sum_with_group_by_nullable_input() {
    let db = setup();
    // rating is nullable + GROUP BY → SUM still nullable (all rows in group could be NULL).
    let sql = "SELECT post_id, SUM(rating) as total FROM comments GROUP BY post_id";
    let info = static_analyze(&db, sql);
    assert!(col(&info, "total").nullable);
}

#[test]
fn agg_min_max_with_group_by_not_null() {
    let db = setup();
    // title is NOT NULL + GROUP BY → MIN/MAX are NOT NULL.
    let sql = "SELECT user_id, MIN(title) as first_title, MAX(title) as last_title \
               FROM posts GROUP BY user_id";
    let info = static_analyze(&db, sql);
    assert!(!col(&info, "first_title").nullable);
    assert!(!col(&info, "last_title").nullable);
}

#[test]
fn agg_avg_with_group_by_not_null() {
    let db = setup();
    // id is NOT NULL + GROUP BY → AVG is NOT NULL.
    let sql = "SELECT user_id, AVG(id) as avg_id FROM posts GROUP BY user_id";
    let info = static_analyze(&db, sql);
    assert!(!col(&info, "avg_id").nullable);
}

#[test]
fn agg_count_with_group_by() {
    let db = setup();
    // COUNT is always NOT NULL, with or without GROUP BY.
    let sql = "SELECT user_id, COUNT(*) as cnt FROM posts GROUP BY user_id";
    let info = static_analyze(&db, sql);
    assert!(!col(&info, "cnt").nullable);
}

#[test]
fn agg_count_without_group_by() {
    let db = setup();
    // COUNT without GROUP BY: still NOT NULL (returns 0).
    let sql = "SELECT COUNT(*) as cnt FROM posts";
    let info = static_analyze(&db, sql);
    assert!(!col(&info, "cnt").nullable);
}

#[test]
fn agg_sum_without_group_by_always_nullable() {
    let db = setup();
    // SUM without GROUP BY: table could be empty → NULL.
    // Even with NOT NULL input.
    let sql = "SELECT SUM(id) as total FROM posts";
    let info = static_analyze(&db, sql);
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
    let info = static_analyze(&db, sql);
    assert!(!col(&info, "cnt").nullable); // COUNT: always NOT NULL
    assert!(col(&info, "sum_rating").nullable); // SUM(nullable): nullable
    assert!(!col(&info, "first_author").nullable); // MIN(NOT NULL): NOT NULL
    assert!(col(&info, "max_rating").nullable); // MAX(nullable): nullable
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
    let info = static_analyze(&db, sql);
    assert!(!col(&info, "post_count").nullable); // COUNT: always NOT NULL
    assert!(col(&info, "last_title").nullable); // MAX(nullable from LEFT JOIN): nullable
}

#[test]
fn agg_string_agg_with_group_by_not_null() {
    let db = setup();
    // string_agg(NOT NULL, delimiter) with GROUP BY → NOT NULL.
    // The literal ', ' has type UNKNOWN — resolved via UNKNOWN-compatible matching.
    let sql = "SELECT post_id, string_agg(author_name, ', ') as authors \
               FROM comments GROUP BY post_id";
    let info = static_analyze(&db, sql);
    assert!(!col(&info, "authors").nullable);
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: function/operator nullability (strict, pg_catalog, exceptions)
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn strict_pg_catalog_function_not_null() {
    let db = setup();
    // length(text) is pg_catalog, strict, not in exceptions → NOT NULL with NOT NULL input.
    let sql = "SELECT length(name) as len FROM users";
    let info = static_analyze(&db, sql);
    assert!(!col(&info, "len").nullable);
}

#[test]
fn strict_pg_catalog_function_nullable_with_nullable_arg() {
    let db = setup();
    // length(text) is strict: nullable input → nullable output.
    let sql = "SELECT length(body) as len FROM posts";
    let info = static_analyze(&db, sql);
    assert!(col(&info, "len").nullable);
}

#[test]
fn strict_pg_catalog_upper_not_null() {
    let db = setup();
    // upper(text) is pg_catalog, strict → NOT NULL with NOT NULL input.
    let sql = "SELECT upper(name) as uname FROM users";
    let info = static_analyze(&db, sql);
    assert!(!col(&info, "uname").nullable);
}

#[test]
fn operator_plus_not_null() {
    let db = setup();
    // 1 + 1: both non-null, operator not in exceptions → NOT NULL.
    let sql = "SELECT 1 + 1 as result";
    let info = static_analyze(&db, sql);
    assert!(!col(&info, "result").nullable);
}

#[test]
fn operator_plus_nullable_arg() {
    let db = setup();
    // age is nullable → result is nullable.
    let sql = "SELECT age + 1 as next_age FROM users";
    let info = static_analyze(&db, sql);
    assert!(col(&info, "next_age").nullable);
}

#[test]
fn operator_concat_not_null() {
    let db = setup();
    // || with two NOT NULL → NOT NULL.
    let sql = "SELECT name || ' <' || email || '>' as display FROM users";
    let info = static_analyze(&db, sql);
    assert!(!col(&info, "display").nullable);
}

#[test]
fn operator_concat_nullable_arg() {
    let db = setup();
    // body is nullable → concat is nullable.
    let sql = "SELECT title || body as combined FROM posts";
    let info = static_analyze(&db, sql);
    assert!(col(&info, "combined").nullable);
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: non-strict pg_catalog functions that never return NULL
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn nonstrict_concat_never_null() {
    let db = setup();
    // concat is non-strict but never returns NULL (treats NULLs as '').
    let sql = "SELECT concat(p.title, ' ', p.body) as full_text FROM posts p";
    let info = static_analyze(&db, sql);
    assert!(!col(&info, "full_text").nullable);
}

#[test]
fn nonstrict_concat_ws_never_null() {
    let db = setup();
    let sql = "SELECT concat_ws(', '::text, name, email) as combined FROM users";
    let info = static_analyze(&db, sql);
    assert!(!col(&info, "combined").nullable);
}

#[test]
fn nonstrict_now_never_null() {
    let db = setup();
    let sql = "SELECT now() as ts";
    let info = static_analyze(&db, sql);
    assert!(!col(&info, "ts").nullable);
}

#[test]
fn nonstrict_random_never_null() {
    let db = setup();
    let sql = "SELECT random() as r";
    let info = static_analyze(&db, sql);
    assert!(!col(&info, "r").nullable);
}
