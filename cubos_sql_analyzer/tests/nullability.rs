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
#[ignore] // requires PostgreSQL (Docker)

fn more_precise_left_join() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT u.name, p.title FROM users u LEFT JOIN posts p ON p.user_id = u.id";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    // Types are the same.
    assert_same_types(&static_info, &live_info, "LEFT JOIN");

    // Nullability: static is MORE PRECISE.
    // posts.title is NOT NULL in the table, but LEFT JOIN makes it nullable.
    assert!(
        !col(&live_info, "title").nullable,
        "live introspect: WRONG — reports NOT NULL because table_oid sees base table"
    );
    assert!(
        col(&static_info, "title").nullable,
        "static analyzer: CORRECT — knows LEFT JOIN makes right side nullable"
    );
}

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn more_precise_right_join() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT u.name, p.title FROM users u RIGHT JOIN posts p ON p.user_id = u.id";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "RIGHT JOIN");

    assert!(
        !col(&live_info, "name").nullable,
        "live introspect: WRONG — reports NOT NULL"
    );
    assert!(
        col(&static_info, "name").nullable,
        "static analyzer: CORRECT — RIGHT JOIN makes left side nullable"
    );
}

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn more_precise_full_join() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT u.name, p.title FROM users u FULL OUTER JOIN posts p ON p.user_id = u.id";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "FULL JOIN");

    // Live says both NOT NULL (wrong). Static says both nullable (correct).
    assert!(!col(&live_info, "name").nullable, "live: WRONG");
    assert!(!col(&live_info, "title").nullable, "live: WRONG");
    assert!(col(&static_info, "name").nullable, "static: CORRECT");
    assert!(col(&static_info, "title").nullable, "static: CORRECT");
}

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn more_precise_count() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT COUNT(*) as cnt FROM users";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "COUNT(*)");

    // COUNT(*) is NEVER NULL, but live introspect has no table_oid → defaults nullable.
    assert!(
        col(&live_info, "cnt").nullable,
        "live introspect: WRONG — no table_oid, defaults to nullable"
    );
    assert!(
        !col(&static_info, "cnt").nullable,
        "static analyzer: CORRECT — COUNT(*) is never NULL"
    );
}

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn more_precise_coalesce() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT COALESCE(age, 0) as safe_age FROM users";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "COALESCE");

    assert!(
        col(&live_info, "safe_age").nullable,
        "live introspect: WRONG — no table_oid for COALESCE"
    );
    assert!(
        !col(&static_info, "safe_age").nullable,
        "static analyzer: CORRECT — COALESCE with literal fallback is NOT NULL"
    );
}

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn more_precise_literal() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id, 'constant' as label FROM users";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "literal");

    assert!(
        col(&live_info, "label").nullable,
        "live introspect: WRONG — no table_oid for literal"
    );
    assert!(
        !col(&static_info, "label").nullable,
        "static analyzer: CORRECT — string literal is NOT NULL"
    );
}

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn more_precise_case_with_else() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT CASE WHEN age > 18 THEN 'adult' ELSE 'minor' END as category FROM users";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "CASE with ELSE");

    assert!(
        col(&live_info, "category").nullable,
        "live introspect: WRONG — no table_oid for CASE expression"
    );
    assert!(
        !col(&static_info, "category").nullable,
        "static analyzer: CORRECT — CASE with ELSE and all-literal branches is NOT NULL"
    );
}

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn more_precise_union_all_not_null() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT name as val FROM users UNION ALL SELECT title as val FROM posts";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "UNION ALL NOT NULL");

    assert!(
        col(&live_info, "val").nullable,
        "live introspect: WRONG — no table_oid for UNION"
    );
    assert!(
        !col(&static_info, "val").nullable,
        "static analyzer: CORRECT — both branches are NOT NULL"
    );
}

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn more_precise_cte_dml_left_join() {
    let (snapshot, mut client) = setup();
    let sql = "WITH ins AS (\
        INSERT INTO posts (user_id, title) VALUES ($1, $2) RETURNING id, user_id\
    ) \
    SELECT ins.id, u.name \
    FROM ins \
    LEFT JOIN users u ON u.id = ins.user_id";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "CTE + DML + LEFT JOIN");

    // Live says name NOT NULL (table_oid fallback). Static correctly says nullable.
    assert!(
        !col(&live_info, "name").nullable,
        "live introspect: WRONG — table_oid sees base table NOT NULL"
    );
    assert!(
        col(&static_info, "name").nullable,
        "static analyzer: CORRECT — LEFT JOIN in DML CTE makes right side nullable"
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: CASE without ELSE (both agree: nullable)
// ──────────────────────────────────────────────────────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn identical_case_without_else() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT CASE WHEN age > 18 THEN 'adult' END as category FROM users";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    // Both agree: CASE without ELSE is nullable.
    // (Live defaults to nullable because no table_oid; static knows it's nullable
    // because there's no ELSE branch. Same result, different reasoning.)
    assert_same_types(&static_info, &live_info, "CASE without ELSE");
    assert!(col(&static_info, "category").nullable);
    assert!(col(&live_info, "category").nullable);
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: UNION with nullable branch (both agree)
// ──────────────────────────────────────────────────────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn identical_union_nullable_branch() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT name as val FROM users UNION ALL SELECT body as val FROM posts";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    // Both agree: nullable (body is nullable + live has no table_oid).
    assert_same_types(&static_info, &live_info, "UNION nullable branch");
    assert!(col(&static_info, "val").nullable);
    assert!(col(&live_info, "val").nullable);
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: complex scenarios
// ──────────────────────────────────────────────────────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn complex_chained_left_joins_cascade_nullability() {
    let (snapshot, mut client) = setup();
    // users INNER JOIN posts LEFT JOIN comments:
    // comments columns become nullable, posts/users stay NOT NULL.
    let sql = "SELECT u.name, p.title, c.author_name, c.rating \
               FROM users u \
               INNER JOIN posts p ON p.user_id = u.id \
               LEFT JOIN comments c ON c.post_id = p.id";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "chained JOINs");

    // Both agree on INNER JOIN columns.
    assert!(!col(&static_info, "name").nullable);
    assert!(!col(&static_info, "title").nullable);

    // comments.author_name is NOT NULL in table but LEFT JOIN makes it nullable.
    assert!(
        !col(&live_info, "author_name").nullable,
        "live: WRONG — NOT NULL from base table"
    );
    assert!(
        col(&static_info, "author_name").nullable,
        "static: CORRECT — LEFT JOIN makes it nullable"
    );

    // comments.rating is nullable in table AND LEFT JOIN → both agree nullable.
    assert!(col(&static_info, "rating").nullable);
    assert!(col(&live_info, "rating").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn complex_three_table_mixed_joins() {
    let (snapshot, mut client) = setup();
    // LEFT JOIN posts, then RIGHT JOIN comments on posts.
    // users: left side of LEFT → NOT NULL.
    // posts: right side of LEFT → nullable. THEN left side of RIGHT → doubly nullable.
    // comments: right side of RIGHT → NOT NULL.
    let sql = "SELECT u.name, p.title, c.content \
               FROM users u \
               LEFT JOIN posts p ON p.user_id = u.id \
               RIGHT JOIN comments c ON c.post_id = p.id";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "3-table mixed JOINs");

    // users.name: RIGHT JOIN makes left side (users+posts) nullable.
    assert!(
        !col(&live_info, "name").nullable,
        "live: WRONG — doesn't track JOIN chain"
    );
    assert!(
        col(&static_info, "name").nullable,
        "static: CORRECT — RIGHT JOIN nullifies the entire left side"
    );

    // posts.title: nullable from LEFT JOIN, then also from RIGHT JOIN.
    assert!(
        !col(&live_info, "title").nullable,
        "live: WRONG — base table says NOT NULL"
    );
    assert!(
        col(&static_info, "title").nullable,
        "static: CORRECT — nullable via LEFT JOIN and RIGHT JOIN"
    );

    // comments.content: right side of RIGHT JOIN, NOT NULL in table.
    assert!(!col(&static_info, "content").nullable);
    assert!(!col(&live_info, "content").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn complex_subquery_in_from() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT sub.user_name, sub.post_count \
               FROM ( \
                   SELECT u.name as user_name, COUNT(*) as post_count \
                   FROM users u \
                   INNER JOIN posts p ON p.user_id = u.id \
                   GROUP BY u.name \
               ) sub";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "subquery in FROM");

    // user_name comes from NOT NULL column → NOT NULL through subquery.
    assert!(!col(&static_info, "user_name").nullable);

    // post_count is COUNT(*) → NOT NULL in static, nullable in live.
    assert!(
        col(&live_info, "post_count").nullable,
        "live: WRONG — subquery column has no table_oid"
    );
    assert!(
        !col(&static_info, "post_count").nullable,
        "static: CORRECT — COUNT(*) is never NULL, propagated through subquery"
    );
}

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn complex_arithmetic_on_nullable() {
    let (snapshot, mut client) = setup();
    // age is nullable → age + 1 should also be nullable.
    let sql = "SELECT id, age + 1 as age_plus_one FROM users";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "arithmetic nullable");

    // Both agree: age + 1 is nullable (age can be NULL → NULL + 1 = NULL).
    assert!(col(&static_info, "age_plus_one").nullable);
    assert!(col(&live_info, "age_plus_one").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn complex_arithmetic_on_not_null() {
    let (snapshot, mut client) = setup();
    // id is NOT NULL → id + 1 should also be NOT NULL.
    let sql = "SELECT id + 1 as next_id FROM users";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "arithmetic not null");

    // Live says nullable (no table_oid for expression). Static knows it's NOT NULL.
    assert!(
        col(&live_info, "next_id").nullable,
        "live: WRONG — no table_oid for id + 1"
    );
    assert!(
        !col(&static_info, "next_id").nullable,
        "static: CORRECT — NOT NULL + literal = NOT NULL"
    );
}

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn complex_coalesce_in_arithmetic() {
    let (snapshot, mut client) = setup();
    // COALESCE(age, 0) is NOT NULL → adding 10 should stay NOT NULL.
    let sql = "SELECT COALESCE(age, 0) + 10 as safe_age_plus FROM users";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "COALESCE in arithmetic");

    assert!(
        col(&live_info, "safe_age_plus").nullable,
        "live: WRONG — no table_oid"
    );
    assert!(
        !col(&static_info, "safe_age_plus").nullable,
        "static: CORRECT — COALESCE(nullable, literal) + literal = NOT NULL"
    );
}

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn complex_boolean_with_nullable_input() {
    let (snapshot, mut client) = setup();
    // age IS NOT NULL → bool, NOT NULL. age > 18 → bool, nullable (age can be NULL).
    let sql = "SELECT age IS NOT NULL as has_age, age > 18 as is_adult FROM users";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "boolean expressions");

    // IS NOT NULL is always NOT NULL (returns true/false, never NULL).
    assert!(
        col(&live_info, "has_age").nullable,
        "live: WRONG — no table_oid"
    );
    assert!(
        !col(&static_info, "has_age").nullable,
        "static: CORRECT — IS NOT NULL always returns non-null bool"
    );

    // age > 18: age is nullable → comparison is nullable.
    assert!(col(&static_info, "is_adult").nullable);
    assert!(col(&live_info, "is_adult").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn exists_without_from() {
    let (snapshot, _client) = setup();
    // SELECT EXISTS(...) without FROM — always returns exactly 1 row, never NULL.
    let sql = "SELECT EXISTS(SELECT 1 FROM users WHERE id = $1)";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    assert_eq!(static_info.columns.len(), 1);
    assert!(
        !static_info.columns[0].nullable,
        "EXISTS should be NOT NULL"
    );
    assert_eq!(static_info.columns[0].rust_type, "bool");
}

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn exists_constant_without_from() {
    let (snapshot, _client) = setup();
    // SELECT EXISTS(SELECT 1) — pure constant, no table reference at all.
    let sql = "SELECT EXISTS(SELECT 1)";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    assert_eq!(static_info.columns.len(), 1);
    assert!(
        !static_info.columns[0].nullable,
        "EXISTS should be NOT NULL"
    );
    assert_eq!(static_info.columns[0].rust_type, "bool");
}

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn complex_exists_subquery() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT u.name, EXISTS(SELECT 1 FROM posts p WHERE p.user_id = u.id) as has_posts \
               FROM users u";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "EXISTS subquery");

    // EXISTS always returns bool, never NULL.
    assert!(
        col(&live_info, "has_posts").nullable,
        "live: WRONG — no table_oid for EXISTS"
    );
    assert!(
        !col(&static_info, "has_posts").nullable,
        "static: CORRECT — EXISTS is never NULL"
    );
}

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn complex_scalar_subquery_always_nullable() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT u.name, \
                      (SELECT p.title FROM posts p WHERE p.user_id = u.id LIMIT 1) as first_post \
               FROM users u";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "scalar subquery");

    // Both agree: scalar subquery is always nullable (zero rows → NULL).
    assert!(col(&static_info, "first_post").nullable);
    assert!(col(&live_info, "first_post").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn complex_cast_preserves_nullability() {
    let (snapshot, mut client) = setup();
    // Casting a nullable column preserves nullability.
    let sql = "SELECT age::text as age_text, id::text as id_text FROM users";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "cast nullability");

    // age is nullable → age::text is nullable.
    assert!(col(&static_info, "age_text").nullable);

    // id is NOT NULL → id::text is NOT NULL.
    // Live says nullable (no table_oid for cast). Static preserves it.
    assert!(
        col(&live_info, "id_text").nullable,
        "live: WRONG — no table_oid for cast"
    );
    assert!(
        !col(&static_info, "id_text").nullable,
        "static: CORRECT — cast on NOT NULL col stays NOT NULL"
    );
}

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn complex_multiple_params_from_different_contexts() {
    let (snapshot, mut client) = setup();
    // $1 from WHERE, $2 from SET-like context via comparison, $3 from another comparison.
    let sql = "SELECT id, name FROM users WHERE id = $1 AND age > $2 AND email = $3";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_identical(&static_info, &live_info, "multiple params");

    assert_eq!(static_info.params.len(), 3);
    assert_eq!(static_info.params[0].rust_type, "i64"); // id is BIGINT
    assert_eq!(static_info.params[1].rust_type, "i32"); // age is INT
    assert_eq!(static_info.params[2].rust_type, "String"); // email is TEXT
}

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn complex_mixed_computed_and_direct_cols() {
    let (snapshot, mut client) = setup();
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
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "mixed computed + direct");

    // Direct columns: types and nullability match.
    assert!(!col(&static_info, "id").nullable);
    assert!(!col(&live_info, "id").nullable);
    assert!(!col(&static_info, "name").nullable);
    assert!(col(&static_info, "age").nullable);

    // COUNT(*): static knows NOT NULL.
    assert!(col(&live_info, "post_count").nullable, "live: WRONG");
    assert!(!col(&static_info, "post_count").nullable, "static: CORRECT");

    // COALESCE(age, 0): static knows NOT NULL.
    assert!(col(&live_info, "safe_age").nullable, "live: WRONG");
    assert!(!col(&static_info, "safe_age").nullable, "static: CORRECT");

    // string concatenation with NOT NULL cols: static knows NOT NULL.
    assert!(col(&live_info, "display").nullable, "live: WRONG");
    assert!(!col(&static_info, "display").nullable, "static: CORRECT");
}

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn complex_cte_chain() {
    let (snapshot, mut client) = setup();
    // Two CTEs: first gets users, second joins with posts.
    let sql = "WITH \
                   active_users AS (SELECT id, name FROM users), \
                   user_posts AS ( \
                       SELECT au.name, p.title \
                       FROM active_users au \
                       INNER JOIN posts p ON p.user_id = au.id \
                   ) \
               SELECT name, title FROM user_posts";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    // CTE chain: types propagate correctly.
    assert_same_types(&static_info, &live_info, "CTE chain");
    assert!(!col(&static_info, "name").nullable);
    assert!(!col(&static_info, "title").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn complex_union_three_branches() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT name as label FROM users \
               UNION ALL \
               SELECT title as label FROM posts \
               UNION ALL \
               SELECT author_name as label FROM comments";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "3-branch UNION");

    // All three are NOT NULL → union is NOT NULL.
    assert!(col(&live_info, "label").nullable, "live: WRONG");
    assert!(!col(&static_info, "label").nullable, "static: CORRECT");
}

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn complex_insert_select_with_join() {
    let (snapshot, mut client) = setup();
    // INSERT ... SELECT from a JOIN — params come from WHERE.
    let sql = "INSERT INTO comments (post_id, author_name, content) \
               SELECT p.id, $1, $2 \
               FROM posts p \
               WHERE p.user_id = $3 \
               RETURNING id, post_id, author_name";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "INSERT SELECT JOIN");

    assert!(!col(&static_info, "id").nullable);
    assert!(!col(&static_info, "post_id").nullable);
    assert!(!col(&static_info, "author_name").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)

fn complex_left_join_with_subquery() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT u.name, latest.title as latest_title \
               FROM users u \
               LEFT JOIN ( \
                   SELECT DISTINCT ON (user_id) user_id, title \
                   FROM posts \
                   ORDER BY user_id, published_at DESC NULLS LAST \
               ) latest ON latest.user_id = u.id";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "LEFT JOIN subquery");

    // latest_title: NOT NULL in posts but LEFT JOIN makes it nullable.
    assert!(
        !col(&live_info, "latest_title").nullable,
        "live: WRONG — base table NOT NULL"
    );
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
#[ignore] // requires PostgreSQL (Docker)
fn stress_nested_coalesce() {
    let (snapshot, _) = setup();
    // COALESCE(COALESCE(nullable, nullable), literal) → NOT NULL
    let sql = "SELECT COALESCE(COALESCE(age, age), 0) as val FROM users";
    let info = static_analyze(&snapshot, sql);
    assert!(
        !col(&info, "val").nullable,
        "nested COALESCE with final literal → NOT NULL"
    );
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_coalesce_all_nullable() {
    let (snapshot, _) = setup();
    // COALESCE(nullable, nullable) → still nullable (no non-null fallback)
    let sql = "SELECT COALESCE(age, age) as val FROM users";
    let info = static_analyze(&snapshot, sql);
    assert!(
        col(&info, "val").nullable,
        "COALESCE of only nullable args → nullable"
    );
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_case_with_null_branch() {
    let (snapshot, _) = setup();
    // CASE with one branch returning NULL explicitly
    let sql = "SELECT CASE WHEN age > 18 THEN name ELSE NULL END as val FROM users";
    let info = static_analyze(&snapshot, sql);
    assert!(col(&info, "val").nullable, "CASE with NULL ELSE → nullable");
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_case_mixing_nullable_branches() {
    let (snapshot, _) = setup();
    // CASE with one NOT NULL branch and one nullable branch
    let sql = "SELECT CASE WHEN id > 0 THEN name ELSE body END as val \
               FROM users u INNER JOIN posts p ON p.user_id = u.id";
    let info = static_analyze(&snapshot, sql);
    // name is NOT NULL but body is nullable → result is nullable
    assert!(
        col(&info, "val").nullable,
        "CASE with mixed nullable branches → nullable"
    );
}

// ── Star expansion edge cases ──────────────────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_star_with_left_join() {
    let (snapshot, mut client) = setup();
    // SELECT * from LEFT JOIN — right side columns should be nullable
    let sql = "SELECT u.id, u.name, p.title, p.body \
               FROM users u \
               LEFT JOIN posts p ON p.user_id = u.id";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "star LEFT JOIN");
    assert!(!col(&static_info, "id").nullable);
    assert!(!col(&static_info, "name").nullable);
    // title is NOT NULL in table but LEFT JOIN makes it nullable
    assert!(col(&static_info, "title").nullable);
    // body is nullable in table AND LEFT JOIN
    assert!(col(&static_info, "body").nullable);
}

// ── SELECT without FROM ────────────────────────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_select_without_from() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT 1 as one, 'hello' as greeting, TRUE as flag";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "SELECT without FROM");
    assert!(!col(&static_info, "one").nullable);
    assert!(!col(&static_info, "greeting").nullable);
    assert!(!col(&static_info, "flag").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_select_null_literal() {
    let (snapshot, _) = setup();
    let sql = "SELECT NULL as nothing";
    let info = static_analyze(&snapshot, sql);
    assert!(col(&info, "nothing").nullable, "NULL literal is nullable");
}

// ── Multiple same-name columns from different tables ───────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_ambiguous_id_columns() {
    let (snapshot, mut client) = setup();
    // Both tables have 'id' — must use aliases to disambiguate
    let sql = "SELECT u.id as user_id, p.id as post_id \
               FROM users u INNER JOIN posts p ON p.user_id = u.id";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_identical(&static_info, &live_info, "ambiguous id columns");
    assert!(!col(&static_info, "user_id").nullable);
    assert!(!col(&static_info, "post_id").nullable);
}

// ── Nested subqueries ──────────────────────────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_deeply_nested_subquery() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT * FROM ( \
                   SELECT * FROM ( \
                       SELECT id, name, age FROM users \
                   ) inner_sq \
               ) outer_sq";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "nested subquery");
    assert!(!col(&static_info, "id").nullable);
    assert!(!col(&static_info, "name").nullable);
    assert!(col(&static_info, "age").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_subquery_with_left_join_inside() {
    let (snapshot, mut client) = setup();
    // Subquery does LEFT JOIN, outer SELECT sees nullable cols
    let sql = "SELECT sq.name, sq.title FROM ( \
                   SELECT u.name, p.title \
                   FROM users u \
                   LEFT JOIN posts p ON p.user_id = u.id \
               ) sq";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "subquery with LEFT JOIN");

    // title is nullable because of LEFT JOIN inside subquery.
    // Live sees the subquery column as coming from posts (NOT NULL).
    assert!(
        !col(&live_info, "title").nullable,
        "live: WRONG — can't see through subquery"
    );
    assert!(
        col(&static_info, "title").nullable,
        "static: CORRECT — LEFT JOIN nullability propagates through subquery"
    );
}

// ── UNION edge cases ───────────────────────────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_union_with_null_literal_branch() {
    let (snapshot, _) = setup();
    // One branch is a literal NULL → union should be nullable
    let sql = "SELECT name as val FROM users \
               UNION ALL \
               SELECT NULL as val";
    let info = static_analyze(&snapshot, sql);
    assert!(
        col(&info, "val").nullable,
        "UNION with NULL branch → nullable"
    );
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_union_mixed_types() {
    let (snapshot, mut client) = setup();
    // int + bigint → bigint (coercion)
    let sql = "SELECT age as num FROM users \
               UNION ALL \
               SELECT id as num FROM users";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "UNION mixed types");
    // age is nullable → union is nullable (even though id is NOT NULL)
    assert!(col(&static_info, "num").nullable);
}

// ── CTE + UNION combo ──────────────────────────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_cte_used_in_union() {
    let (snapshot, _) = setup();
    let sql = "WITH active AS (SELECT name FROM users) \
               SELECT name FROM active \
               UNION ALL \
               SELECT title as name FROM posts";
    let info = static_analyze(&snapshot, sql);
    // Both branches NOT NULL
    assert!(!col(&info, "name").nullable);
}

// ── DML with complex RETURNING ─────────────────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_update_returning_expression() {
    let (snapshot, mut client) = setup();
    let sql = "UPDATE users SET age = $1 WHERE id = $2 \
               RETURNING id, COALESCE(age, 0) as safe_age, name || '!' as excited";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "UPDATE RETURNING expressions");

    assert!(!col(&static_info, "id").nullable);
    // COALESCE in RETURNING
    assert!(col(&live_info, "safe_age").nullable, "live: WRONG");
    assert!(!col(&static_info, "safe_age").nullable, "static: CORRECT");
    // string concat in RETURNING
    assert!(col(&live_info, "excited").nullable, "live: WRONG");
    assert!(!col(&static_info, "excited").nullable, "static: CORRECT");
}

// ── DELETE with complex WHERE + RETURNING ──────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_delete_returning_all_columns() {
    let (snapshot, mut client) = setup();
    let sql = "DELETE FROM users WHERE id = $1 \
               RETURNING id, name, email, age, created_at";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_identical(&static_info, &live_info, "DELETE RETURNING all");
    assert!(!col(&static_info, "id").nullable);
    assert!(!col(&static_info, "name").nullable);
    assert!(!col(&static_info, "email").nullable);
    assert!(col(&static_info, "age").nullable);
    assert!(!col(&static_info, "created_at").nullable);
}

// ── CTE with DML + expressions ─────────────────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_cte_insert_returning_coalesce() {
    let (snapshot, mut client) = setup();
    let sql = "WITH ins AS ( \
                   INSERT INTO users (name, email) VALUES ($1, $2) \
                   RETURNING id, age \
               ) \
               SELECT id, COALESCE(age, 0) as safe_age FROM ins";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "CTE INSERT + COALESCE");

    assert!(!col(&static_info, "id").nullable);
    // COALESCE on CTE column: static knows it's NOT NULL
    assert!(col(&live_info, "safe_age").nullable, "live: WRONG");
    assert!(!col(&static_info, "safe_age").nullable, "static: CORRECT");
}

// ── Param inference edge cases ─────────────────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_param_from_insert_values() {
    let (snapshot, mut client) = setup();
    let sql = "INSERT INTO posts (user_id, title, body) VALUES ($1, $2, $3) RETURNING id";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_identical(&static_info, &live_info, "INSERT VALUES params");
    assert_eq!(static_info.params.len(), 3);
    assert_eq!(static_info.params[0].rust_type, "i64"); // user_id BIGINT
    assert_eq!(static_info.params[1].rust_type, "String"); // title TEXT
    assert_eq!(static_info.params[2].rust_type, "String"); // body TEXT
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_param_with_cast() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id FROM users WHERE id = $1::bigint";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_identical(&static_info, &live_info, "param with cast");
    assert_eq!(static_info.params[0].rust_type, "i64");
}

// ── Self-join ──────────────────────────────────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_self_join() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT a.name as name_a, b.name as name_b \
               FROM users a \
               INNER JOIN users b ON a.id = b.id";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_identical(&static_info, &live_info, "self join");
    assert!(!col(&static_info, "name_a").nullable);
    assert!(!col(&static_info, "name_b").nullable);
}

// ── CROSS JOIN ─────────────────────────────────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_cross_join() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT u.name, p.title FROM users u CROSS JOIN posts p";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    // CROSS JOIN doesn't make anything nullable
    assert_identical(&static_info, &live_info, "CROSS JOIN");
    assert!(!col(&static_info, "name").nullable);
    assert!(!col(&static_info, "title").nullable);
}

// ── Implicit CROSS JOIN (comma in FROM) ────────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_implicit_cross_join() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT u.name, p.title FROM users u, posts p WHERE p.user_id = u.id";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_identical(&static_info, &live_info, "implicit CROSS JOIN");
    assert!(!col(&static_info, "name").nullable);
    assert!(!col(&static_info, "title").nullable);
}

// ── Complex WHERE with AND/OR/NOT ──────────────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_complex_where_params() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id FROM users \
               WHERE (name = $1 OR email = $2) AND age > $3";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_identical(&static_info, &live_info, "complex WHERE params");
    assert_eq!(static_info.params.len(), 3);
}

// ── RETURNING with star ────────────────────────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_insert_returning_star() {
    let (snapshot, mut client) = setup();
    let sql = "INSERT INTO posts (user_id, title) VALUES ($1, $2) RETURNING *";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "INSERT RETURNING *");
    assert!(!col(&static_info, "id").nullable);
    assert!(!col(&static_info, "user_id").nullable);
    assert!(!col(&static_info, "title").nullable);
    assert!(col(&static_info, "body").nullable);
    assert!(col(&static_info, "published_at").nullable);
}

// ── Aliased subquery with computed columns ─────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_subquery_computed_columns() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT sq.cnt, sq.max_age FROM ( \
                   SELECT COUNT(*) as cnt, MAX(age) as max_age FROM users \
               ) sq";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "subquery computed");

    // COUNT is NOT NULL, MAX is nullable
    assert!(col(&live_info, "cnt").nullable, "live: WRONG");
    assert!(!col(&static_info, "cnt").nullable, "static: CORRECT");
    assert!(col(&static_info, "max_age").nullable);
}

// ── LIMIT / OFFSET don't affect types ──────────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_limit_offset() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id, name FROM users ORDER BY id LIMIT 10 OFFSET 5";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_identical(&static_info, &live_info, "LIMIT OFFSET");
}

// ── Annotations with complex expressions ───────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_annotation_on_left_join_star() {
    let (snapshot, _) = setup();
    // Force nullable LEFT JOIN column to NOT NULL via annotation
    let sql = "SELECT u.name, p.title as \"title!\" \
               FROM users u LEFT JOIN posts p ON p.user_id = u.id";
    let info = static_analyze(&snapshot, sql);
    assert!(
        !col(&info, "title").nullable,
        "! overrides LEFT JOIN nullable"
    );
}

// ── DISTINCT ON ────────────────────────────────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_distinct_on() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT DISTINCT ON (user_id) user_id, title, body \
               FROM posts ORDER BY user_id, published_at DESC NULLS LAST";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_identical(&static_info, &live_info, "DISTINCT ON");
    assert!(!col(&static_info, "user_id").nullable);
    assert!(!col(&static_info, "title").nullable);
    assert!(col(&static_info, "body").nullable);
}

// ── Mixing aggregates and non-aggregates in subquery ───────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_aggregate_subquery_in_select() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT u.name, \
                      (SELECT COUNT(*) FROM posts p WHERE p.user_id = u.id) as post_count \
               FROM users u";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "aggregate in scalar subquery");

    // Static analyzer knows: aggregate without GROUP BY → exactly 1 row,
    // and COUNT is NOT NULL → scalar subquery result is NOT NULL.
    // Live introspect can't see this — always marks scalar subqueries nullable.
    assert!(!col(&static_info, "post_count").nullable);
    assert!(col(&live_info, "post_count").nullable);
}

// ── INSERT with DEFAULT values (no explicit columns) ───────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_insert_minimal() {
    let (snapshot, mut client) = setup();
    let sql = "INSERT INTO users (name, email) VALUES ($1, $2) RETURNING id";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_identical(&static_info, &live_info, "INSERT minimal");
    assert_eq!(static_info.params.len(), 2);
    assert!(!col(&static_info, "id").nullable);
}

// ── Deeply nested CTE with DML and JOINs ──────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_cte_dml_chain() {
    let (snapshot, mut client) = setup();
    let sql = "WITH \
                   new_user AS ( \
                       INSERT INTO users (name, email) VALUES ($1, $2) RETURNING id, name \
                   ), \
                   new_post AS ( \
                       INSERT INTO posts (user_id, title) \
                       SELECT id, $3 FROM new_user \
                       RETURNING id as post_id, user_id, title \
                   ) \
               SELECT nu.name, np.title, np.post_id \
               FROM new_user nu \
               INNER JOIN new_post np ON np.user_id = nu.id";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "CTE DML chain");
    assert!(!col(&static_info, "name").nullable);
    assert!(!col(&static_info, "title").nullable);
    assert!(!col(&static_info, "post_id").nullable);
}

// ── SELECT with only aggregates (no GROUP BY) ──────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn stress_aggregates_no_group_by() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT COUNT(*) as cnt, SUM(age) as total_age, MAX(name) as last_name FROM users";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "aggregates no GROUP BY");

    // COUNT is NOT NULL
    assert!(col(&live_info, "cnt").nullable, "live: WRONG");
    assert!(!col(&static_info, "cnt").nullable, "static: CORRECT");

    // SUM and MAX are nullable (empty table → NULL)
    assert!(col(&static_info, "total_age").nullable);
    assert!(col(&static_info, "last_name").nullable);
}

// ──────────────────────────────────────────────────────────────────────────────
// TORTURE TESTS: even more evil SQL
// ──────────────────────────────────────────────────────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn torture_union_in_subquery_in_from() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT sq.val FROM ( \
                   SELECT name as val FROM users \
                   UNION ALL \
                   SELECT title as val FROM posts \
               ) sq";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "UNION in subquery");
    // Both NOT NULL → union NOT NULL → subquery NOT NULL.
    assert!(col(&live_info, "val").nullable, "live: WRONG");
    assert!(!col(&static_info, "val").nullable, "static: CORRECT");
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn torture_left_join_on_union_subquery() {
    let (snapshot, mut client) = setup();
    // LEFT JOIN on a UNION subquery
    let sql = "SELECT u.name, all_content.val \
               FROM users u \
               LEFT JOIN ( \
                   SELECT user_id, title as val FROM posts \
                   UNION ALL \
                   SELECT p.user_id, c.content as val FROM comments c \
                   INNER JOIN posts p ON p.id = c.post_id \
               ) all_content ON all_content.user_id = u.id";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "LEFT JOIN on UNION subquery");

    // val is NOT NULL in the union, but LEFT JOIN makes it nullable.
    assert!(
        col(&static_info, "val").nullable,
        "LEFT JOIN on subquery → nullable"
    );
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn torture_cte_with_union_and_left_join() {
    let (snapshot, mut client) = setup();
    let sql = "WITH all_names AS ( \
                   SELECT name FROM users \
                   UNION ALL \
                   SELECT title as name FROM posts \
               ) \
               SELECT u.id, an.name as other_name \
               FROM users u \
               LEFT JOIN all_names an ON true";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "CTE+UNION+LEFT JOIN");
    assert!(!col(&static_info, "id").nullable);
    // LEFT JOIN on CTE → nullable
    assert!(col(&static_info, "other_name").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn torture_nested_case_in_coalesce() {
    let (snapshot, _) = setup();
    // COALESCE(CASE without ELSE, literal) → NOT NULL
    let sql = "SELECT COALESCE( \
                   CASE WHEN age > 18 THEN age END, \
                   0 \
               ) as val FROM users";
    let info = static_analyze(&snapshot, sql);
    // CASE without ELSE is nullable, but COALESCE with 0 fallback makes it NOT NULL.
    assert!(!col(&info, "val").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn torture_triple_left_join() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT u.name, p.title, c.content, c.rating \
               FROM users u \
               LEFT JOIN posts p ON p.user_id = u.id \
               LEFT JOIN comments c ON c.post_id = p.id";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "triple LEFT JOIN");
    assert!(!col(&static_info, "name").nullable);
    assert!(col(&static_info, "title").nullable, "1st LEFT JOIN");
    assert!(col(&static_info, "content").nullable, "2nd LEFT JOIN");
    assert!(
        col(&static_info, "rating").nullable,
        "2nd LEFT JOIN + nullable col"
    );
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn torture_full_join_with_coalesce_fix() {
    let (snapshot, _) = setup();
    // FULL JOIN makes both sides nullable, but COALESCE can fix it.
    let sql = "SELECT COALESCE(u.name, p.title, 'unknown') as label \
               FROM users u \
               FULL OUTER JOIN posts p ON p.user_id = u.id";
    let info = static_analyze(&snapshot, sql);
    // Both u.name and p.title are nullable due to FULL JOIN,
    // but COALESCE with 'unknown' fallback → NOT NULL.
    assert!(!col(&info, "label").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn torture_param_in_coalesce() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT COALESCE(age, $1) as val FROM users";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "param in COALESCE");
    // $1 is NOT NULL by default → COALESCE has a NOT NULL arg → NOT NULL.
    assert!(!col(&static_info, "val").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn torture_multiple_ctes_cross_reference() {
    let (snapshot, _) = setup();
    let sql = "WITH \
                   a AS (SELECT id, name FROM users), \
                   b AS (SELECT a.name, p.title FROM a INNER JOIN posts p ON p.user_id = a.id), \
                   c AS (SELECT b.name, b.title, cm.content \
                         FROM b LEFT JOIN comments cm ON true) \
               SELECT name, title, content FROM c";
    let info = static_analyze(&snapshot, sql);
    assert!(!col(&info, "name").nullable);
    assert!(!col(&info, "title").nullable);
    assert!(col(&info, "content").nullable, "LEFT JOIN in CTE c");
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn torture_update_from_join() {
    let (snapshot, mut client) = setup();
    let sql = "UPDATE posts SET body = $1 \
               FROM users u \
               WHERE posts.user_id = u.id AND u.name = $2 \
               RETURNING posts.id, posts.title, posts.body";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "UPDATE FROM JOIN");
    assert!(!col(&static_info, "id").nullable);
    assert!(!col(&static_info, "title").nullable);
    assert!(col(&static_info, "body").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn torture_select_from_cte_left_join_cte() {
    let (snapshot, _) = setup();
    let sql = "WITH \
                   u AS (SELECT id, name FROM users), \
                   p AS (SELECT user_id, title FROM posts) \
               SELECT u.name, p.title \
               FROM u LEFT JOIN p ON p.user_id = u.id";
    let info = static_analyze(&snapshot, sql);
    assert!(!col(&info, "name").nullable);
    assert!(col(&info, "title").nullable, "LEFT JOIN between CTEs");
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn torture_count_with_group_by() {
    let (snapshot, mut client) = setup();
    // COUNT in GROUP BY context is still NOT NULL.
    let sql = "SELECT u.name, COUNT(p.id) as post_count \
               FROM users u \
               LEFT JOIN posts p ON p.user_id = u.id \
               GROUP BY u.name";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "COUNT with GROUP BY");
    assert!(col(&live_info, "post_count").nullable, "live: WRONG");
    assert!(
        !col(&static_info, "post_count").nullable,
        "static: CORRECT — COUNT is never NULL"
    );
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn torture_expression_in_insert_returning() {
    let (snapshot, mut client) = setup();
    let sql = "INSERT INTO users (name, email, age) VALUES ($1, $2, $3) \
               RETURNING id, \
                         name || ' (' || email || ')' as display, \
                         CASE WHEN age >= 18 THEN true ELSE false END as is_adult";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "INSERT RETURNING expr");
    assert!(!col(&static_info, "id").nullable);
    // concat of NOT NULL → NOT NULL
    assert!(col(&live_info, "display").nullable, "live: WRONG");
    assert!(!col(&static_info, "display").nullable, "static: CORRECT");
    // CASE with ELSE, all literal booleans → NOT NULL
    assert!(col(&live_info, "is_adult").nullable, "live: WRONG");
    assert!(!col(&static_info, "is_adult").nullable, "static: CORRECT");
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn torture_deeply_nested_cte_union_join() {
    let (snapshot, _) = setup();
    // CTE → UNION → subquery → LEFT JOIN
    let sql = "WITH names AS ( \
                   SELECT name as val FROM users \
                   UNION ALL \
                   SELECT title as val FROM posts \
               ) \
               SELECT n.val, u.age \
               FROM names n \
               LEFT JOIN users u ON u.name = n.val";
    let info = static_analyze(&snapshot, sql);
    assert!(!col(&info, "val").nullable, "CTE UNION of NOT NULL");
    assert!(col(&info, "age").nullable, "LEFT JOIN + nullable col");
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: scalar subquery + aggregate nullability
// ──────────────────────────────────────────────────────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn subquery_count_star_not_null() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT u.name, \
                      (SELECT COUNT(*) FROM posts p WHERE p.user_id = u.id) as cnt \
               FROM users u";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "COUNT(*) scalar subquery");
    // Static: aggregate without GROUP BY → guaranteed 1 row, COUNT is NOT NULL.
    assert!(!col(&static_info, "cnt").nullable);
    // Live: always marks scalar subquery nullable.
    assert!(col(&live_info, "cnt").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn subquery_count_plus_one_not_null() {
    let (snapshot, _) = setup();
    // COUNT(*) + 1 wraps the aggregate in an AExpr — must still detect it.
    let sql = "SELECT u.name, \
                      (SELECT COUNT(*) + 1 FROM posts p WHERE p.user_id = u.id) as cnt \
               FROM users u";
    let info = static_analyze(&snapshot, sql);
    assert!(!col(&info, "cnt").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn subquery_count_cast_not_null() {
    let (snapshot, _) = setup();
    // COUNT(*)::int wraps aggregate in TypeCast.
    let sql = "SELECT (SELECT COUNT(*)::int FROM posts) as cnt FROM users";
    let info = static_analyze(&snapshot, sql);
    assert!(!col(&info, "cnt").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn subquery_coalesce_sum_not_null() {
    let (snapshot, _) = setup();
    // COALESCE(SUM(rating), 0) — aggregate detected through COALESCE.
    // SUM is nullable (empty group), but COALESCE with literal → NOT NULL.
    // Also: aggregate without GROUP BY → guaranteed 1 row.
    let sql = "SELECT (SELECT COALESCE(SUM(rating), 0) FROM comments) as total";
    let info = static_analyze(&snapshot, sql);
    assert!(!col(&info, "total").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn subquery_sum_nullable() {
    let (snapshot, _) = setup();
    // SUM without COALESCE: aggregate != COUNT → nullable result.
    // Even though guaranteed 1 row, SUM itself returns NULL for empty input.
    let sql = "SELECT (SELECT SUM(rating) FROM comments) as total";
    let info = static_analyze(&snapshot, sql);
    assert!(col(&info, "total").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn subquery_with_group_by_still_nullable() {
    let (snapshot, _) = setup();
    // COUNT(*) with GROUP BY: subquery may return 0 rows → nullable.
    let sql = "SELECT u.name, \
                      (SELECT COUNT(*) FROM posts p WHERE p.user_id = u.id GROUP BY p.user_id) as cnt \
               FROM users u";
    let info = static_analyze(&snapshot, sql);
    assert!(col(&info, "cnt").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn subquery_non_aggregate_still_nullable() {
    let (snapshot, _) = setup();
    // Non-aggregate scalar subquery: may return 0 rows → nullable.
    let sql = "SELECT u.name, \
                      (SELECT p.title FROM posts p WHERE p.user_id = u.id LIMIT 1) as first_title \
               FROM users u";
    let info = static_analyze(&snapshot, sql);
    assert!(col(&info, "first_title").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn subquery_case_wrapping_count_not_null() {
    let (snapshot, _) = setup();
    // CASE WHEN ... THEN COUNT(*) ELSE 0 END — aggregate inside CASE with ELSE.
    let sql = "SELECT (SELECT CASE WHEN true THEN COUNT(*) ELSE 0 END FROM posts) as cnt";
    let info = static_analyze(&snapshot, sql);
    assert!(!col(&info, "cnt").nullable);
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: aggregates with GROUP BY
// ──────────────────────────────────────────────────────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn agg_sum_with_group_by_not_null_input() {
    let (snapshot, mut client) = setup();
    // user_id is NOT NULL + GROUP BY → SUM guaranteed non-null.
    let sql = "SELECT user_id, SUM(user_id) as total FROM posts GROUP BY user_id";
    let static_info = analyze(&snapshot, sql, &default_config()).unwrap();
    let live_info = live_introspect(&mut client, sql);

    assert_same_types(&static_info, &live_info, "SUM with GROUP BY not-null input");
    // Static: GROUP BY + NOT NULL input → NOT NULL.
    assert!(!col(&static_info, "total").nullable);
    // Live: always marks non-COUNT aggregates nullable.
    assert!(col(&live_info, "total").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn agg_sum_with_group_by_nullable_input() {
    let (snapshot, _) = setup();
    // rating is nullable + GROUP BY → SUM still nullable (all rows in group could be NULL).
    let sql = "SELECT post_id, SUM(rating) as total FROM comments GROUP BY post_id";
    let info = static_analyze(&snapshot, sql);
    assert!(col(&info, "total").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn agg_min_max_with_group_by_not_null() {
    let (snapshot, _) = setup();
    // title is NOT NULL + GROUP BY → MIN/MAX are NOT NULL.
    let sql = "SELECT user_id, MIN(title) as first_title, MAX(title) as last_title \
               FROM posts GROUP BY user_id";
    let info = static_analyze(&snapshot, sql);
    assert!(!col(&info, "first_title").nullable);
    assert!(!col(&info, "last_title").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn agg_avg_with_group_by_not_null() {
    let (snapshot, _) = setup();
    // id is NOT NULL + GROUP BY → AVG is NOT NULL.
    let sql = "SELECT user_id, AVG(id) as avg_id FROM posts GROUP BY user_id";
    let info = static_analyze(&snapshot, sql);
    assert!(!col(&info, "avg_id").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn agg_count_with_group_by() {
    let (snapshot, _) = setup();
    // COUNT is always NOT NULL, with or without GROUP BY.
    let sql = "SELECT user_id, COUNT(*) as cnt FROM posts GROUP BY user_id";
    let info = static_analyze(&snapshot, sql);
    assert!(!col(&info, "cnt").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn agg_count_without_group_by() {
    let (snapshot, _) = setup();
    // COUNT without GROUP BY: still NOT NULL (returns 0).
    let sql = "SELECT COUNT(*) as cnt FROM posts";
    let info = static_analyze(&snapshot, sql);
    assert!(!col(&info, "cnt").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn agg_sum_without_group_by_always_nullable() {
    let (snapshot, _) = setup();
    // SUM without GROUP BY: table could be empty → NULL.
    // Even with NOT NULL input.
    let sql = "SELECT SUM(id) as total FROM posts";
    let info = static_analyze(&snapshot, sql);
    assert!(col(&info, "total").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn agg_mixed_nullability_with_group_by() {
    let (snapshot, _) = setup();
    // Mix of NOT NULL and nullable aggregates in same GROUP BY query.
    let sql = "SELECT post_id, \
                      COUNT(*) as cnt, \
                      SUM(rating) as sum_rating, \
                      MIN(author_name) as first_author, \
                      MAX(rating) as max_rating \
               FROM comments GROUP BY post_id";
    let info = static_analyze(&snapshot, sql);
    assert!(!col(&info, "cnt").nullable); // COUNT: always NOT NULL
    assert!(col(&info, "sum_rating").nullable); // SUM(nullable): nullable
    assert!(!col(&info, "first_author").nullable); // MIN(NOT NULL): NOT NULL
    assert!(col(&info, "max_rating").nullable); // MAX(nullable): nullable
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn agg_with_group_by_and_left_join() {
    let (snapshot, _) = setup();
    // LEFT JOIN + GROUP BY: right-side columns are nullable from JOIN,
    // so aggregate on them is nullable even with GROUP BY.
    let sql = "SELECT u.id, COUNT(p.id) as post_count, MAX(p.title) as last_title \
               FROM users u \
               LEFT JOIN posts p ON p.user_id = u.id \
               GROUP BY u.id";
    let info = static_analyze(&snapshot, sql);
    assert!(!col(&info, "post_count").nullable); // COUNT: always NOT NULL
    assert!(col(&info, "last_title").nullable); // MAX(nullable from LEFT JOIN): nullable
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn agg_string_agg_with_group_by_not_null() {
    let (snapshot, _) = setup();
    // string_agg(NOT NULL, delimiter) with GROUP BY → NOT NULL.
    // The literal ', ' has type UNKNOWN — resolved via UNKNOWN-compatible matching.
    let sql = "SELECT post_id, string_agg(author_name, ', ') as authors \
               FROM comments GROUP BY post_id";
    let info = static_analyze(&snapshot, sql);
    assert!(!col(&info, "authors").nullable);
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: function/operator nullability (strict, pg_catalog, exceptions)
// ──────────────────────────────────────────────────────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn strict_pg_catalog_function_not_null() {
    let (snapshot, _) = setup();
    // length(text) is pg_catalog, strict, not in exceptions → NOT NULL with NOT NULL input.
    let sql = "SELECT length(name) as len FROM users";
    let info = static_analyze(&snapshot, sql);
    assert!(!col(&info, "len").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn strict_pg_catalog_function_nullable_with_nullable_arg() {
    let (snapshot, _) = setup();
    // length(text) is strict: nullable input → nullable output.
    let sql = "SELECT length(body) as len FROM posts";
    let info = static_analyze(&snapshot, sql);
    assert!(col(&info, "len").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn strict_pg_catalog_upper_not_null() {
    let (snapshot, _) = setup();
    // upper(text) is pg_catalog, strict → NOT NULL with NOT NULL input.
    let sql = "SELECT upper(name) as uname FROM users";
    let info = static_analyze(&snapshot, sql);
    assert!(!col(&info, "uname").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn operator_plus_not_null() {
    let (snapshot, _) = setup();
    // 1 + 1: both non-null, operator not in exceptions → NOT NULL.
    let sql = "SELECT 1 + 1 as result";
    let info = static_analyze(&snapshot, sql);
    assert!(!col(&info, "result").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn operator_plus_nullable_arg() {
    let (snapshot, _) = setup();
    // age is nullable → result is nullable.
    let sql = "SELECT age + 1 as next_age FROM users";
    let info = static_analyze(&snapshot, sql);
    assert!(col(&info, "next_age").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn operator_concat_not_null() {
    let (snapshot, _) = setup();
    // || with two NOT NULL → NOT NULL.
    let sql = "SELECT name || ' <' || email || '>' as display FROM users";
    let info = static_analyze(&snapshot, sql);
    assert!(!col(&info, "display").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn operator_concat_nullable_arg() {
    let (snapshot, _) = setup();
    // body is nullable → concat is nullable.
    let sql = "SELECT title || body as combined FROM posts";
    let info = static_analyze(&snapshot, sql);
    assert!(col(&info, "combined").nullable);
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: non-strict pg_catalog functions that never return NULL
// ──────────────────────────────────────────────────────────────────────────────

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn nonstrict_concat_never_null() {
    let (snapshot, _) = setup();
    // concat is non-strict but never returns NULL (treats NULLs as '').
    let sql = "SELECT concat(p.title, ' ', p.body) as full_text FROM posts p";
    let info = static_analyze(&snapshot, sql);
    assert!(!col(&info, "full_text").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn nonstrict_concat_ws_never_null() {
    let (snapshot, _) = setup();
    let sql = "SELECT concat_ws(', '::text, name, email) as combined FROM users";
    let info = static_analyze(&snapshot, sql);
    assert!(!col(&info, "combined").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn nonstrict_now_never_null() {
    let (snapshot, _) = setup();
    let sql = "SELECT now() as ts";
    let info = static_analyze(&snapshot, sql);
    assert!(!col(&info, "ts").nullable);
}

#[test]
#[ignore] // requires PostgreSQL (Docker)
fn nonstrict_random_never_null() {
    let (snapshot, _) = setup();
    let sql = "SELECT random() as r";
    let info = static_analyze(&snapshot, sql);
    assert!(!col(&info, "r").nullable);
}
