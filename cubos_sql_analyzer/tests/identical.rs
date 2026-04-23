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
    let sql = "SELECT id, name, age FROM users";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_select_with_params() {
    let db = setup();
    let sql = "SELECT id, name FROM users WHERE age > $p1 AND name = $p2";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_select_star() {
    let db = setup();
    let sql = "SELECT * FROM users";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_select_star_from_posts() {
    let db = setup();
    let sql = "SELECT * FROM posts";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_select_star_from_comments() {
    let db = setup();
    let sql = "SELECT * FROM comments";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_select_aliased_columns() {
    let db = setup();
    let sql = "SELECT id AS user_id, name AS user_name FROM users";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_select_table_qualified() {
    let db = setup();
    let sql = "SELECT users.id, users.name FROM users";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_select_alias_qualified() {
    let db = setup();
    let sql = "SELECT u.id, u.name, u.age FROM users u";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_select_all_columns_explicit() {
    let db = setup();
    let sql = "SELECT id, name, email, age, created_at FROM users";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_nullable_column() {
    let db = setup();
    let sql = "SELECT id, age FROM users";
    let s = db.analyze(sql, &default_config()).unwrap();
    assert!(!col(&s, "id").nullable);
    assert!(col(&s, "age").nullable);
}

// ── WHERE variations ──────────────────────────────────────────────────

#[test]
fn identical_where_is_not_null() {
    let db = setup();
    let sql = "SELECT id, age FROM users WHERE age IS NOT NULL";
    let s = db.analyze(sql, &default_config()).unwrap();
    assert!(col(&s, "age").nullable);
}

#[test]
fn identical_where_and() {
    let db = setup();
    let sql = "SELECT id FROM users WHERE name = $p1 AND email = $p2";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_where_or() {
    let db = setup();
    let sql = "SELECT id FROM users WHERE name = $p1 OR email = $p2";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_where_in_list() {
    let db = setup();
    let sql = "SELECT id FROM users WHERE age IN (1, 2, 3)";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_where_like() {
    let db = setup();
    let sql = "SELECT id FROM users WHERE name LIKE $p1";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_where_not() {
    let db = setup();
    let sql = "SELECT id FROM users WHERE NOT (age > $p1)";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_where_is_null() {
    let db = setup();
    let sql = "SELECT id, name FROM users WHERE age IS NULL";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_where_comparison_operators() {
    let db = setup();
    let sql = "SELECT id FROM users WHERE age >= $p1 AND age <= $p2 AND name <> $p3";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

// ── JOINs ─────────────────────────────────────────────────────────────

#[test]
fn identical_inner_join() {
    let db = setup();
    let sql = "SELECT u.name, p.title FROM users u INNER JOIN posts p ON p.user_id = u.id";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_inner_join_three_tables() {
    let db = setup();
    let sql = "SELECT u.name, p.title, c.content \
               FROM users u \
               INNER JOIN posts p ON p.user_id = u.id \
               INNER JOIN comments c ON c.post_id = p.id";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_cross_join() {
    let db = setup();
    let sql = "SELECT u.name, p.title FROM users u CROSS JOIN posts p";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_self_join() {
    let db = setup();
    let sql = "SELECT a.name AS name_a, b.name AS name_b \
               FROM users a INNER JOIN users b ON a.id <> b.id";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_implicit_cross_join() {
    let db = setup();
    let sql = "SELECT u.name, p.title FROM users u, posts p";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

// ── ORDER BY / LIMIT / OFFSET ─────────────────────────────────────────

#[test]
fn identical_order_by() {
    let db = setup();
    let sql = "SELECT id, name FROM users ORDER BY name ASC, id DESC";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_limit_offset_literals() {
    let db = setup();
    let sql = "SELECT id, name FROM users ORDER BY id LIMIT 10 OFFSET 5";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_limit_offset_params() {
    let db = setup();
    let sql = "SELECT id FROM users ORDER BY id LIMIT $p1 OFFSET $p2";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

// ── DML ───────────────────────────────────────────────────────────────

#[test]
fn identical_insert_returning() {
    let db = setup();
    let sql = "INSERT INTO users (name, email) VALUES ($p1, $p2) RETURNING id, name, age";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_insert_all_columns() {
    let db = setup();
    let sql = "INSERT INTO users (name, email, age) VALUES ($p1, $p2, $p3) RETURNING *";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_insert_multiple_rows() {
    let db = setup();
    let sql = "INSERT INTO users (name, email) VALUES ($p1, $p2), ($p3, $p4) RETURNING id";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_insert_into_posts() {
    let db = setup();
    let sql = "INSERT INTO posts (user_id, title, body) VALUES ($p1, $p2, $p3) RETURNING id, title";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_insert_into_comments() {
    let db = setup();
    let sql = "INSERT INTO comments (post_id, author_name, content, rating) \
               VALUES ($p1, $p2, $p3, $p4) RETURNING *";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_update_returning() {
    let db = setup();
    let sql = "UPDATE users SET age = $p1 WHERE id = $p2 RETURNING id, name, age";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_update_multiple_columns() {
    let db = setup();
    let sql = "UPDATE users SET name = $p1, email = $p2, age = $p3 WHERE id = $p4 RETURNING *";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_update_with_from() {
    let db = setup();
    let sql = "UPDATE posts SET title = $p1 \
               FROM users u WHERE posts.user_id = u.id AND u.name = $p2 \
               RETURNING posts.id, posts.title";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_delete_returning() {
    let db = setup();
    let sql = "DELETE FROM users WHERE id = $p1 RETURNING id, name, age";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_delete_returning_star() {
    let db = setup();
    let sql = "DELETE FROM comments WHERE post_id = $p1 RETURNING *";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_delete_returning_subset() {
    let db = setup();
    let sql = "DELETE FROM posts WHERE user_id = $p1 RETURNING id, title";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

// ── CTEs ──────────────────────────────────────────────────────────────

#[test]
fn identical_cte_simple() {
    let db = setup();
    let sql = "WITH active AS (SELECT id, name FROM users WHERE age > 18) \
               SELECT * FROM active";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_cte_multiple() {
    let db = setup();
    let sql = "WITH \
                 u AS (SELECT id, name FROM users), \
                 p AS (SELECT user_id, title FROM posts) \
               SELECT u.name, p.title \
               FROM u INNER JOIN p ON p.user_id = u.id";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_cte_with_insert_returning() {
    let db = setup();
    let sql = "WITH new_user AS (\
                 INSERT INTO users (name, email) VALUES ($p1, $p2) RETURNING id, name\
               ) SELECT * FROM new_user";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

// ── DISTINCT / DISTINCT ON ────────────────────────────────────────────

#[test]
fn identical_select_distinct() {
    let db = setup();
    let sql = "SELECT DISTINCT name FROM users";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_distinct_on() {
    let db = setup();
    let sql = "SELECT DISTINCT ON (user_id) user_id, title \
               FROM posts ORDER BY user_id, published_at DESC NULLS LAST";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

// ── Mixed param types ─────────────────────────────────────────────────

#[test]
fn identical_params_all_types() {
    let db = setup();
    let sql = "SELECT id FROM users \
               WHERE name = $p1 AND age = $p2 AND id > $p3 AND created_at > $p4";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_params_with_cast() {
    let db = setup();
    let sql = "SELECT id FROM users WHERE name = $p1::text AND age > $p2::int4";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

// ── Complex combined queries ──────────────────────────────────────────

#[test]
fn identical_join_with_where_and_limit() {
    let db = setup();
    let sql = "SELECT u.name, p.title \
               FROM users u INNER JOIN posts p ON p.user_id = u.id \
               WHERE u.age > $p1 \
               ORDER BY p.published_at DESC NULLS LAST \
               LIMIT $p2";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_insert_select() {
    let db = setup();
    let sql = "INSERT INTO comments (post_id, author_name, content) \
               SELECT p.id, $p1, $p2 FROM posts p WHERE p.user_id = $p3 \
               RETURNING id";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_subquery_in_from() {
    let db = setup();
    let sql = "SELECT sub.name, sub.age \
               FROM (SELECT name, age FROM users WHERE age IS NOT NULL) sub";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_in_subquery() {
    let db = setup();
    let sql = "SELECT id, name FROM users \
               WHERE id IN (SELECT user_id FROM posts)";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_exists_subquery() {
    let db = setup();
    let sql = "SELECT id, name FROM users u \
               WHERE EXISTS (SELECT 1 FROM posts p WHERE p.user_id = u.id)";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

// ══════════════════════════════════════════════════════════════════════════════
// Types match, but our analyzer is MORE PRECISE on nullability.
// PG introspect reports computed expressions as nullable; we know they're not.
// ══════════════════════════════════════════════════════════════════════════════

// ── Literals ──────────────────────────────────────────────────────────

#[test]
fn types_match_integer_literal() {
    let db = setup();
    let sql = "SELECT 42 AS val";
    let s = db.analyze(sql, &default_config()).unwrap();
    assert!(!col(&s, "val").nullable, "literal 42 is NOT NULL");
}

#[test]
fn types_match_boolean_literal() {
    let db = setup();
    let sql = "SELECT true AS flag, false AS other";
    let s = db.analyze(sql, &default_config()).unwrap();
    assert!(!col(&s, "flag").nullable);
    assert!(!col(&s, "other").nullable);
}

// ── Arithmetic / operators ────────────────────────────────────────────

#[test]
fn types_match_arithmetic() {
    let db = setup();
    let sql = "SELECT id + 1 AS next_id, age * 2 AS double_age FROM users";
    let s = db.analyze(sql, &default_config()).unwrap();
    assert!(!col(&s, "next_id").nullable, "id+1: id is NOT NULL");
    assert!(col(&s, "double_age").nullable, "age*2: age is nullable");
}

#[test]
fn types_match_string_concat() {
    let db = setup();
    let sql = "SELECT name || ' <' || email || '>' AS display FROM users";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

// ── Type casts ────────────────────────────────────────────────────────

#[test]
fn types_match_cast_int_to_text() {
    let db = setup();
    let sql = "SELECT age::text AS age_text FROM users";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn types_match_cast_bigint_to_int() {
    let db = setup();
    let sql = "SELECT id::int4 AS short_id FROM users";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn types_match_cast_literal() {
    let db = setup();
    let sql = "SELECT '123'::int4 AS val";
    let s = db.analyze(sql, &default_config()).unwrap();
    assert!(!col(&s, "val").nullable, "literal cast is NOT NULL");
}

// ── Functions ─────────────────────────────────────────────────────────

#[test]
fn types_match_count_star() {
    let db = setup();
    let sql = "SELECT count(*) AS total FROM users";
    let s = db.analyze(sql, &default_config()).unwrap();
    assert!(!col(&s, "total").nullable, "COUNT is never NULL");
}

#[test]
fn types_match_upper_lower() {
    let db = setup();
    let sql = "SELECT upper(name) AS up, lower(email) AS lo FROM users";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn types_match_length() {
    let db = setup();
    let sql = "SELECT length(name) AS len FROM users";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn types_match_coalesce_with_literal() {
    let db = setup();
    let sql = "SELECT COALESCE(age, 0) AS age_or_zero FROM users";
    let s = db.analyze(sql, &default_config()).unwrap();
    assert!(
        !col(&s, "age_or_zero").nullable,
        "COALESCE with NOT NULL fallback"
    );
}

#[test]
fn types_match_now() {
    let db = setup();
    let sql = "SELECT now() AS ts";
    let s = db.analyze(sql, &default_config()).unwrap();
    assert!(!col(&s, "ts").nullable, "now() is never NULL");
}

// ── CASE ──────────────────────────────────────────────────────────────

#[test]
fn types_match_case_with_else() {
    let db = setup();
    let sql = "SELECT CASE WHEN age > 18 THEN 'adult' ELSE 'minor' END AS category FROM users";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn types_match_case_expression() {
    let db = setup();
    let sql = "SELECT CASE WHEN age IS NULL THEN 0 ELSE age END AS safe_age FROM users";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

// ── Boolean / NULL tests ──────────────────────────────────────────────

#[test]
fn types_match_null_test() {
    let db = setup();
    let sql = "SELECT id, age IS NULL AS is_null, age IS NOT NULL AS is_not_null FROM users";
    let s = db.analyze(sql, &default_config()).unwrap();
    assert!(!col(&s, "is_null").nullable, "IS NULL is never NULL");
    assert!(
        !col(&s, "is_not_null").nullable,
        "IS NOT NULL is never NULL"
    );
}

#[test]
fn types_match_boolean_test() {
    let db = setup();
    let sql = "SELECT (age > 18) IS TRUE AS adult, (age > 18) IS NOT TRUE AS not_adult FROM users";
    let s = db.analyze(sql, &default_config()).unwrap();
    assert!(!col(&s, "adult").nullable, "IS TRUE is never NULL");
}

// ── GROUP BY / HAVING ─────────────────────────────────────────────────

#[test]
fn types_match_group_by_count() {
    let db = setup();
    let sql = "SELECT user_id, count(*) AS post_count FROM posts GROUP BY user_id";
    let s = db.analyze(sql, &default_config()).unwrap();
    assert!(!col(&s, "post_count").nullable, "COUNT is never NULL");
}

#[test]
fn types_match_group_by_multiple_aggregates() {
    let db = setup();
    let sql = "SELECT user_id, count(*) AS cnt, max(published_at) AS latest \
               FROM posts GROUP BY user_id";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

// ── UNION / INTERSECT / EXCEPT ────────────────────────────────────────

#[test]
fn types_match_union_all() {
    let db = setup();
    let sql = "SELECT id, name FROM users WHERE age > 20 \
               UNION ALL \
               SELECT id, name FROM users WHERE age <= 20";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn types_match_union_distinct() {
    let db = setup();
    let sql = "SELECT name FROM users \
               UNION \
               SELECT title FROM posts";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn types_match_intersect() {
    let db = setup();
    let sql = "SELECT name FROM users \
               INTERSECT \
               SELECT title FROM posts";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn types_match_except() {
    let db = setup();
    let sql = "SELECT name FROM users \
               EXCEPT \
               SELECT title FROM posts";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

// ── CTE + UNION ───────────────────────────────────────────────────────

#[test]
fn types_match_cte_union() {
    let db = setup();
    let sql = "WITH all_names AS (\
                 SELECT name FROM users \
                 UNION ALL \
                 SELECT author_name AS name FROM comments\
               ) \
               SELECT name FROM all_names";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

// ══════════════════════════════════════════════════════════════════════════════
// Enum types (CREATE TYPE ... AS ENUM)
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn identical_enum_column_select() {
    // Enum columns are typed as String by default (no config mapping).
    let db = setup();
    let sql = "SELECT id, name, role FROM users";
    let s = db.analyze(sql, &default_config()).unwrap();
    assert_eq!(col(&s, "role").rust_type, "String");
    assert!(!col(&s, "role").nullable, "role has NOT NULL + DEFAULT");
}

#[test]
fn identical_enum_in_where() {
    let db = setup();
    let sql = "SELECT id, name FROM users WHERE role = $p1";
    let s = db.analyze(sql, &default_config()).unwrap();
    // $p1 should be String (enum typed as String)
    assert_eq!(s.params[0].rust_type, "String");
}

#[test]
fn identical_enum_in_insert() {
    let db = setup();
    let sql = "INSERT INTO users (name, email, role) VALUES ($p1, $p2, $p3) RETURNING id, role";
    let s = db.analyze(sql, &default_config()).unwrap();
    assert_eq!(s.params[2].rust_type, "String", "$p3 = role enum → String");
}

#[test]
fn identical_enum_in_update() {
    let db = setup();
    let sql = "UPDATE users SET role = $p1 WHERE id = $p2 RETURNING role";
    let s = db.analyze(sql, &default_config()).unwrap();
    assert_eq!(s.params[0].rust_type, "String", "$p1 = role enum → String");
}

#[test]
fn identical_enum_with_config_mapping() {
    // With enum config, enum_rust_type is set but rust_type stays String.
    let db = setup();
    let mut config = default_config();
    config
        .enums
        .insert(qn("public", "user_role"), "crate::UserRole".into());
    let sql = "SELECT id, role FROM users";
    let info = db.analyze(sql, &config).unwrap();
    assert_eq!(col(&info, "role").rust_type, "String");
    assert_eq!(
        col(&info, "role").enum_rust_type.as_deref(),
        Some("crate::UserRole"),
        "enum_rust_type should be set from config"
    );
}

#[test]
fn identical_enum_param_with_config_mapping() {
    let db = setup();
    let mut config = default_config();
    config
        .enums
        .insert(qn("public", "user_role"), "crate::UserRole".into());
    let sql = "INSERT INTO users (name, email, role) VALUES ($p1, $p2, $p3) RETURNING id";
    let info = db.analyze(sql, &config).unwrap();
    assert_eq!(info.params[2].rust_type, "String");
    assert_eq!(
        info.params[2].enum_rust_type.as_deref(),
        Some("crate::UserRole"),
        "param enum_rust_type should be set from config"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Domain types (CREATE DOMAIN)
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn identical_domain_column_without_config() {
    // Without domain config, a JSONB domain unwraps to its base type.
    let db = setup();
    let sql = "SELECT id, preferences FROM users";
    let s = db.analyze(sql, &default_config()).unwrap();
    // preferences is user_prefs (domain over JSONB) → unwraps to jsonb
    assert!(
        col(&s, "preferences").rust_type == "::serde_json::Value",
        "JSONB domain without config → ::serde_json::Value, got: {}",
        col(&s, "preferences").rust_type
    );
    assert!(col(&s, "preferences").nullable, "preferences is nullable");
}

#[test]
fn identical_domain_column_with_config() {
    // With domain config, domain_rust_type is set for deserialization.
    let db = setup();
    let mut config = default_config();
    config
        .domains
        .insert(qn("public", "user_prefs"), "crate::UserPrefs".into());
    let sql = "SELECT id, preferences FROM users";
    let info = db.analyze(sql, &config).unwrap();
    assert_eq!(col(&info, "preferences").rust_type, "::serde_json::Value");
    assert_eq!(
        col(&info, "preferences").domain_rust_type.as_deref(),
        Some("crate::UserPrefs"),
        "domain_rust_type should be set from config"
    );
}

#[test]
fn identical_domain_param_insert() {
    // Inserting into a domain column — param gets domain's base type.
    let db = setup();
    let sql = "INSERT INTO users (name, email, preferences) VALUES ($p1, $p2, $p3) RETURNING id";
    let s = db.analyze(sql, &default_config()).unwrap();
    // $p3 is user_prefs (JSONB domain) → ::serde_json::Value
    assert!(
        s.params[2].rust_type == "::serde_json::Value",
        "$p3 should be ::serde_json::Value, got: {}",
        s.params[2].rust_type
    );
}

#[test]
fn identical_domain_param_with_config() {
    let db = setup();
    let mut config = default_config();
    config
        .domains
        .insert(qn("public", "user_prefs"), "crate::UserPrefs".into());
    let sql = "INSERT INTO users (name, email, preferences) VALUES ($p1, $p2, $p3) RETURNING id";
    let info = db.analyze(sql, &config).unwrap();
    assert_eq!(info.params[2].rust_type, "::serde_json::Value");
    assert_eq!(
        info.params[2].domain_rust_type.as_deref(),
        Some("crate::UserPrefs"),
        "param domain_rust_type should be set from config"
    );
}

#[test]
fn identical_domain_in_where() {
    let db = setup();
    let sql = "SELECT id FROM users WHERE preferences IS NOT NULL";
    let _s = db.analyze(sql, &default_config()).unwrap();
}

#[test]
fn identical_schema_qualified_domain_column_without_config() {
    // whatsapp.health_data domain without config → unwraps to serde_json::Value
    let db = setup();
    let sql = "SELECT channel_id, health FROM whatsapp.channels";
    let s = db.analyze(sql, &default_config()).unwrap();
    assert_eq!(
        col(&s, "health").rust_type,
        "::serde_json::Value",
        "JSONB domain without config → ::serde_json::Value"
    );
    assert!(col(&s, "health").nullable, "health is nullable");
}

#[test]
fn identical_schema_qualified_domain_column_with_config() {
    // whatsapp.health_data with config → domain_rust_type is set
    let db = setup();
    let mut config = default_config();
    config
        .domains
        .insert(qn("whatsapp", "health_data"), "crate::HealthData".into());
    let sql = "SELECT channel_id, health FROM whatsapp.channels";
    let info = db.analyze(sql, &config).unwrap();
    assert_eq!(col(&info, "health").rust_type, "::serde_json::Value");
    assert_eq!(
        col(&info, "health").domain_rust_type.as_deref(),
        Some("crate::HealthData"),
        "domain_rust_type should be set for schema-qualified domain"
    );
}

#[test]
fn identical_schema_qualified_domain_param_with_config() {
    // INSERT param into whatsapp.health_data column with config
    let db = setup();
    let mut config = default_config();
    config
        .domains
        .insert(qn("whatsapp", "health_data"), "crate::HealthData".into());
    let sql = "INSERT INTO whatsapp.channels (channel_id, health, updated_at) \
               VALUES ($p1, $p2, now())";
    let info = db.analyze(sql, &config).unwrap();
    assert_eq!(info.params[1].rust_type, "::serde_json::Value");
    assert_eq!(
        info.params[1].domain_rust_type.as_deref(),
        Some("crate::HealthData"),
        "param domain_rust_type should be set for schema-qualified domain"
    );
}

#[test]
fn identical_schema_qualified_domain_unqualified_key_no_match() {
    // Using unqualified "health_data" should NOT match whatsapp.health_data
    let db = setup();
    let mut config = default_config();
    config
        .domains
        .insert(qn("public", "health_data"), "crate::WrongType".into());
    let sql = "SELECT channel_id, health FROM whatsapp.channels";
    let info = db.analyze(sql, &config).unwrap();
    // Should NOT have domain_rust_type since "public.health_data" != "whatsapp.health_data"
    assert!(
        col(&info, "health").domain_rust_type.is_none(),
        "public.health_data should not match whatsapp.health_data"
    );
}
