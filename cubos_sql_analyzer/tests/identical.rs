//! Tests where the static analyzer and live PostgreSQL introspection produce
//! completely identical output (types and nullability agree), or where types
//! agree and our analyzer is more precise on nullability (marked with
//! `assert_same_types`).

mod common;
use common::*;

// ══════════════════════════════════════════════════════════════════════════════
// Fully identical (types AND nullability match)
// ══════════════════════════════════════════════════════════════════════════════

// ── Basic SELECT ──────────────────────────────────────────────────────

#[test]
#[ignore]
fn identical_simple_select() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id, name, age FROM users";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "simple SELECT");
}

#[test]
#[ignore]
fn identical_select_with_params() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id, name FROM users WHERE age > $1 AND name = $2";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "SELECT with params");
}

#[test]
#[ignore]
fn identical_select_star() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT * FROM users";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "SELECT *");
}

#[test]
#[ignore]
fn identical_select_star_from_posts() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT * FROM posts";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "SELECT * FROM posts");
}

#[test]
#[ignore]
fn identical_select_star_from_comments() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT * FROM comments";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "SELECT * FROM comments");
}

#[test]
#[ignore]
fn identical_select_aliased_columns() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id AS user_id, name AS user_name FROM users";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "aliased columns");
}

#[test]
#[ignore]
fn identical_select_table_qualified() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT users.id, users.name FROM users";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "table-qualified columns");
}

#[test]
#[ignore]
fn identical_select_alias_qualified() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT u.id, u.name, u.age FROM users u";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "alias-qualified columns");
}

#[test]
#[ignore]
fn identical_select_all_columns_explicit() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id, name, email, age, created_at FROM users";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "all columns explicit");
}

#[test]
#[ignore]
fn identical_nullable_column() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id, age FROM users";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "nullable column");
    assert!(!col(&s, "id").nullable);
    assert!(col(&s, "age").nullable);
}

// ── WHERE variations ──────────────────────────────────────────────────

#[test]
#[ignore]
fn identical_where_is_not_null() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id, age FROM users WHERE age IS NOT NULL";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "WHERE IS NOT NULL");
    assert!(col(&s, "age").nullable);
}

#[test]
#[ignore]
fn identical_where_and() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id FROM users WHERE name = $1 AND email = $2";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "WHERE AND");
}

#[test]
#[ignore]
fn identical_where_or() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id FROM users WHERE name = $1 OR email = $2";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "WHERE OR");
}

#[test]
#[ignore]
fn identical_where_in_list() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id FROM users WHERE age IN (1, 2, 3)";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "WHERE IN list");
}

#[test]
#[ignore]
fn identical_where_like() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id FROM users WHERE name LIKE $1";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "WHERE LIKE");
}

#[test]
#[ignore]
fn identical_where_not() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id FROM users WHERE NOT (age > $1)";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "WHERE NOT");
}

#[test]
#[ignore]
fn identical_where_is_null() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id, name FROM users WHERE age IS NULL";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "WHERE IS NULL");
}

#[test]
#[ignore]
fn identical_where_comparison_operators() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id FROM users WHERE age >= $1 AND age <= $2 AND name <> $3";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "WHERE comparison operators");
}

// ── JOINs ─────────────────────────────────────────────────────────────

#[test]
#[ignore]
fn identical_inner_join() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT u.name, p.title FROM users u INNER JOIN posts p ON p.user_id = u.id";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "INNER JOIN");
}

#[test]
#[ignore]
fn identical_inner_join_three_tables() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT u.name, p.title, c.content \
               FROM users u \
               INNER JOIN posts p ON p.user_id = u.id \
               INNER JOIN comments c ON c.post_id = p.id";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "three-table INNER JOIN");
}

#[test]
#[ignore]
fn identical_cross_join() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT u.name, p.title FROM users u CROSS JOIN posts p";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "CROSS JOIN");
}

#[test]
#[ignore]
fn identical_self_join() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT a.name AS name_a, b.name AS name_b \
               FROM users a INNER JOIN users b ON a.id <> b.id";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "self join");
}

#[test]
#[ignore]
fn identical_implicit_cross_join() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT u.name, p.title FROM users u, posts p";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "implicit cross join");
}

// ── ORDER BY / LIMIT / OFFSET ─────────────────────────────────────────

#[test]
#[ignore]
fn identical_order_by() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id, name FROM users ORDER BY name ASC, id DESC";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "ORDER BY");
}

#[test]
#[ignore]
fn identical_limit_offset_literals() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id, name FROM users ORDER BY id LIMIT 10 OFFSET 5";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "LIMIT/OFFSET literals");
}

#[test]
#[ignore]
fn identical_limit_offset_params() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id FROM users ORDER BY id LIMIT $1 OFFSET $2";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "LIMIT/OFFSET params");
}

// ── DML ───────────────────────────────────────────────────────────────

#[test]
#[ignore]
fn identical_insert_returning() {
    let (snapshot, mut client) = setup();
    let sql = "INSERT INTO users (name, email) VALUES ($1, $2) RETURNING id, name, age";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "INSERT RETURNING");
}

#[test]
#[ignore]
fn identical_insert_all_columns() {
    let (snapshot, mut client) = setup();
    let sql = "INSERT INTO users (name, email, age) VALUES ($1, $2, $3) RETURNING *";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "INSERT all columns RETURNING *");
}

#[test]
#[ignore]
fn identical_insert_multiple_rows() {
    let (snapshot, mut client) = setup();
    let sql = "INSERT INTO users (name, email) VALUES ($1, $2), ($3, $4) RETURNING id";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "INSERT multiple rows");
}

#[test]
#[ignore]
fn identical_insert_into_posts() {
    let (snapshot, mut client) = setup();
    let sql = "INSERT INTO posts (user_id, title, body) VALUES ($1, $2, $3) RETURNING id, title";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "INSERT into posts");
}

#[test]
#[ignore]
fn identical_insert_into_comments() {
    let (snapshot, mut client) = setup();
    let sql = "INSERT INTO comments (post_id, author_name, content, rating) \
               VALUES ($1, $2, $3, $4) RETURNING *";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "INSERT into comments");
}

#[test]
#[ignore]
fn identical_update_returning() {
    let (snapshot, mut client) = setup();
    let sql = "UPDATE users SET age = $1 WHERE id = $2 RETURNING id, name, age";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "UPDATE RETURNING");
}

#[test]
#[ignore]
fn identical_update_multiple_columns() {
    let (snapshot, mut client) = setup();
    let sql = "UPDATE users SET name = $1, email = $2, age = $3 WHERE id = $4 RETURNING *";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "UPDATE multiple columns RETURNING *");
}

#[test]
#[ignore]
fn identical_update_with_from() {
    let (snapshot, mut client) = setup();
    let sql = "UPDATE posts SET title = $1 \
               FROM users u WHERE posts.user_id = u.id AND u.name = $2 \
               RETURNING posts.id, posts.title";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "UPDATE with FROM");
}

#[test]
#[ignore]
fn identical_delete_returning() {
    let (snapshot, mut client) = setup();
    let sql = "DELETE FROM users WHERE id = $1 RETURNING id, name, age";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "DELETE RETURNING");
}

#[test]
#[ignore]
fn identical_delete_returning_star() {
    let (snapshot, mut client) = setup();
    let sql = "DELETE FROM comments WHERE post_id = $1 RETURNING *";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "DELETE RETURNING *");
}

#[test]
#[ignore]
fn identical_delete_returning_subset() {
    let (snapshot, mut client) = setup();
    let sql = "DELETE FROM posts WHERE user_id = $1 RETURNING id, title";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "DELETE RETURNING subset");
}

// ── CTEs ──────────────────────────────────────────────────────────────

#[test]
#[ignore]
fn identical_cte_simple() {
    let (snapshot, mut client) = setup();
    let sql = "WITH active AS (SELECT id, name FROM users WHERE age > 18) \
               SELECT * FROM active";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "simple CTE");
}

#[test]
#[ignore]
fn identical_cte_multiple() {
    let (snapshot, mut client) = setup();
    let sql = "WITH \
                 u AS (SELECT id, name FROM users), \
                 p AS (SELECT user_id, title FROM posts) \
               SELECT u.name, p.title \
               FROM u INNER JOIN p ON p.user_id = u.id";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "multiple CTEs");
}

#[test]
#[ignore]
fn identical_cte_with_insert_returning() {
    let (snapshot, mut client) = setup();
    let sql = "WITH new_user AS (\
                 INSERT INTO users (name, email) VALUES ($1, $2) RETURNING id, name\
               ) SELECT * FROM new_user";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "CTE with INSERT RETURNING");
}

// ── DISTINCT / DISTINCT ON ────────────────────────────────────────────

#[test]
#[ignore]
fn identical_select_distinct() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT DISTINCT name FROM users";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "SELECT DISTINCT");
}

#[test]
#[ignore]
fn identical_distinct_on() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT DISTINCT ON (user_id) user_id, title \
               FROM posts ORDER BY user_id, published_at DESC NULLS LAST";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "DISTINCT ON");
}

// ── Mixed param types ─────────────────────────────────────────────────

#[test]
#[ignore]
fn identical_params_all_types() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id FROM users \
               WHERE name = $1 AND age = $2 AND id > $3 AND created_at > $4";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "params of all types");
}

#[test]
#[ignore]
fn identical_params_with_cast() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id FROM users WHERE name = $1::text AND age > $2::int4";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "params with cast");
}

// ── Complex combined queries ──────────────────────────────────────────

#[test]
#[ignore]
fn identical_join_with_where_and_limit() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT u.name, p.title \
               FROM users u INNER JOIN posts p ON p.user_id = u.id \
               WHERE u.age > $1 \
               ORDER BY p.published_at DESC NULLS LAST \
               LIMIT $2";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "JOIN + WHERE + ORDER + LIMIT");
}

#[test]
#[ignore]
fn identical_insert_select() {
    let (snapshot, mut client) = setup();
    let sql = "INSERT INTO comments (post_id, author_name, content) \
               SELECT p.id, $1, $2 FROM posts p WHERE p.user_id = $3 \
               RETURNING id";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "INSERT ... SELECT");
}

#[test]
#[ignore]
fn identical_subquery_in_from() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT sub.name, sub.age \
               FROM (SELECT name, age FROM users WHERE age IS NOT NULL) sub";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "FROM subquery");
}

#[test]
#[ignore]
fn identical_in_subquery() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id, name FROM users \
               WHERE id IN (SELECT user_id FROM posts)";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "IN subquery");
}

#[test]
#[ignore]
fn identical_exists_subquery() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id, name FROM users u \
               WHERE EXISTS (SELECT 1 FROM posts p WHERE p.user_id = u.id)";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_identical(&s, &l, "EXISTS subquery");
}

// ══════════════════════════════════════════════════════════════════════════════
// Types match, but our analyzer is MORE PRECISE on nullability.
// PG introspect reports computed expressions as nullable; we know they're not.
// ══════════════════════════════════════════════════════════════════════════════

// ── Literals ──────────────────────────────────────────────────────────

#[test]
#[ignore]
fn types_match_integer_literal() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT 42 AS val";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_same_types(&s, &l, "integer literal");
    assert!(!col(&s, "val").nullable, "literal 42 is NOT NULL");
}

#[test]
#[ignore]
fn types_match_boolean_literal() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT true AS flag, false AS other";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_same_types(&s, &l, "boolean literals");
    assert!(!col(&s, "flag").nullable);
    assert!(!col(&s, "other").nullable);
}

// ── Arithmetic / operators ────────────────────────────────────────────

#[test]
#[ignore]
fn types_match_arithmetic() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id + 1 AS next_id, age * 2 AS double_age FROM users";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_same_types(&s, &l, "arithmetic on columns");
    assert!(!col(&s, "next_id").nullable, "id+1: id is NOT NULL");
    assert!(col(&s, "double_age").nullable, "age*2: age is nullable");
}

#[test]
#[ignore]
fn types_match_string_concat() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT name || ' <' || email || '>' AS display FROM users";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_same_types(&s, &l, "string concat");
}

// ── Type casts ────────────────────────────────────────────────────────

#[test]
#[ignore]
fn types_match_cast_int_to_text() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT age::text AS age_text FROM users";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_same_types(&s, &l, "cast int to text");
}

#[test]
#[ignore]
fn types_match_cast_bigint_to_int() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id::int4 AS short_id FROM users";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_same_types(&s, &l, "cast bigint to int");
}

#[test]
#[ignore]
fn types_match_cast_literal() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT '123'::int4 AS val";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_same_types(&s, &l, "cast text to int");
    assert!(!col(&s, "val").nullable, "literal cast is NOT NULL");
}

// ── Functions ─────────────────────────────────────────────────────────

#[test]
#[ignore]
fn types_match_count_star() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT count(*) AS total FROM users";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_same_types(&s, &l, "count(*)");
    assert!(!col(&s, "total").nullable, "COUNT is never NULL");
}

#[test]
#[ignore]
fn types_match_upper_lower() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT upper(name) AS up, lower(email) AS lo FROM users";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_same_types(&s, &l, "upper/lower");
}

#[test]
#[ignore]
fn types_match_length() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT length(name) AS len FROM users";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_same_types(&s, &l, "length");
}

#[test]
#[ignore]
fn types_match_coalesce_with_literal() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT COALESCE(age, 0) AS age_or_zero FROM users";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_same_types(&s, &l, "COALESCE with literal");
    assert!(
        !col(&s, "age_or_zero").nullable,
        "COALESCE with NOT NULL fallback"
    );
}

#[test]
#[ignore]
fn types_match_now() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT now() AS ts";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_same_types(&s, &l, "now()");
    assert!(!col(&s, "ts").nullable, "now() is never NULL");
}

// ── CASE ──────────────────────────────────────────────────────────────

#[test]
#[ignore]
fn types_match_case_with_else() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT CASE WHEN age > 18 THEN 'adult' ELSE 'minor' END AS category FROM users";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_same_types(&s, &l, "CASE with ELSE");
}

#[test]
#[ignore]
fn types_match_case_expression() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT CASE WHEN age IS NULL THEN 0 ELSE age END AS safe_age FROM users";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_same_types(&s, &l, "CASE expression");
}

// ── Boolean / NULL tests ──────────────────────────────────────────────

#[test]
#[ignore]
fn types_match_null_test() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id, age IS NULL AS is_null, age IS NOT NULL AS is_not_null FROM users";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_same_types(&s, &l, "NULL test expressions");
    assert!(!col(&s, "is_null").nullable, "IS NULL is never NULL");
    assert!(
        !col(&s, "is_not_null").nullable,
        "IS NOT NULL is never NULL"
    );
}

#[test]
#[ignore]
fn types_match_boolean_test() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT (age > 18) IS TRUE AS adult, (age > 18) IS NOT TRUE AS not_adult FROM users";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_same_types(&s, &l, "boolean test");
    assert!(!col(&s, "adult").nullable, "IS TRUE is never NULL");
}

// ── GROUP BY / HAVING ─────────────────────────────────────────────────

#[test]
#[ignore]
fn types_match_group_by_count() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT user_id, count(*) AS post_count FROM posts GROUP BY user_id";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_same_types(&s, &l, "GROUP BY with count");
    assert!(!col(&s, "post_count").nullable, "COUNT is never NULL");
}

#[test]
#[ignore]
fn types_match_group_by_multiple_aggregates() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT user_id, count(*) AS cnt, max(published_at) AS latest \
               FROM posts GROUP BY user_id";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_same_types(&s, &l, "GROUP BY multiple aggregates");
}

// ── UNION / INTERSECT / EXCEPT ────────────────────────────────────────

#[test]
#[ignore]
fn types_match_union_all() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT id, name FROM users WHERE age > 20 \
               UNION ALL \
               SELECT id, name FROM users WHERE age <= 20";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_same_types(&s, &l, "UNION ALL");
}

#[test]
#[ignore]
fn types_match_union_distinct() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT name FROM users \
               UNION \
               SELECT title FROM posts";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_same_types(&s, &l, "UNION DISTINCT");
}

#[test]
#[ignore]
fn types_match_intersect() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT name FROM users \
               INTERSECT \
               SELECT title FROM posts";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_same_types(&s, &l, "INTERSECT");
}

#[test]
#[ignore]
fn types_match_except() {
    let (snapshot, mut client) = setup();
    let sql = "SELECT name FROM users \
               EXCEPT \
               SELECT title FROM posts";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_same_types(&s, &l, "EXCEPT");
}

// ── CTE + UNION ───────────────────────────────────────────────────────

#[test]
#[ignore]
fn types_match_cte_union() {
    let (snapshot, mut client) = setup();
    let sql = "WITH all_names AS (\
                 SELECT name FROM users \
                 UNION ALL \
                 SELECT author_name AS name FROM comments\
               ) \
               SELECT name FROM all_names";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    let l = live_introspect(&mut client, sql);
    assert_same_types(&s, &l, "CTE + UNION");
}
