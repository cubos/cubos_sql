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
    let snapshot = setup();
    let sql = "SELECT id, name, age FROM users";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_select_with_params() {
    let snapshot = setup();
    let sql = "SELECT id, name FROM users WHERE age > $1 AND name = $2";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_select_star() {
    let snapshot = setup();
    let sql = "SELECT * FROM users";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_select_star_from_posts() {
    let snapshot = setup();
    let sql = "SELECT * FROM posts";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_select_star_from_comments() {
    let snapshot = setup();
    let sql = "SELECT * FROM comments";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_select_aliased_columns() {
    let snapshot = setup();
    let sql = "SELECT id AS user_id, name AS user_name FROM users";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_select_table_qualified() {
    let snapshot = setup();
    let sql = "SELECT users.id, users.name FROM users";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_select_alias_qualified() {
    let snapshot = setup();
    let sql = "SELECT u.id, u.name, u.age FROM users u";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_select_all_columns_explicit() {
    let snapshot = setup();
    let sql = "SELECT id, name, email, age, created_at FROM users";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_nullable_column() {
    let snapshot = setup();
    let sql = "SELECT id, age FROM users";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    assert!(!col(&s, "id").nullable);
    assert!(col(&s, "age").nullable);
}

// ── WHERE variations ──────────────────────────────────────────────────

#[test]
fn identical_where_is_not_null() {
    let snapshot = setup();
    let sql = "SELECT id, age FROM users WHERE age IS NOT NULL";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    assert!(col(&s, "age").nullable);
}

#[test]
fn identical_where_and() {
    let snapshot = setup();
    let sql = "SELECT id FROM users WHERE name = $1 AND email = $2";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_where_or() {
    let snapshot = setup();
    let sql = "SELECT id FROM users WHERE name = $1 OR email = $2";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_where_in_list() {
    let snapshot = setup();
    let sql = "SELECT id FROM users WHERE age IN (1, 2, 3)";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_where_like() {
    let snapshot = setup();
    let sql = "SELECT id FROM users WHERE name LIKE $1";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_where_not() {
    let snapshot = setup();
    let sql = "SELECT id FROM users WHERE NOT (age > $1)";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_where_is_null() {
    let snapshot = setup();
    let sql = "SELECT id, name FROM users WHERE age IS NULL";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_where_comparison_operators() {
    let snapshot = setup();
    let sql = "SELECT id FROM users WHERE age >= $1 AND age <= $2 AND name <> $3";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

// ── JOINs ─────────────────────────────────────────────────────────────

#[test]
fn identical_inner_join() {
    let snapshot = setup();
    let sql = "SELECT u.name, p.title FROM users u INNER JOIN posts p ON p.user_id = u.id";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_inner_join_three_tables() {
    let snapshot = setup();
    let sql = "SELECT u.name, p.title, c.content \
               FROM users u \
               INNER JOIN posts p ON p.user_id = u.id \
               INNER JOIN comments c ON c.post_id = p.id";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_cross_join() {
    let snapshot = setup();
    let sql = "SELECT u.name, p.title FROM users u CROSS JOIN posts p";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_self_join() {
    let snapshot = setup();
    let sql = "SELECT a.name AS name_a, b.name AS name_b \
               FROM users a INNER JOIN users b ON a.id <> b.id";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_implicit_cross_join() {
    let snapshot = setup();
    let sql = "SELECT u.name, p.title FROM users u, posts p";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

// ── ORDER BY / LIMIT / OFFSET ─────────────────────────────────────────

#[test]
fn identical_order_by() {
    let snapshot = setup();
    let sql = "SELECT id, name FROM users ORDER BY name ASC, id DESC";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_limit_offset_literals() {
    let snapshot = setup();
    let sql = "SELECT id, name FROM users ORDER BY id LIMIT 10 OFFSET 5";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_limit_offset_params() {
    let snapshot = setup();
    let sql = "SELECT id FROM users ORDER BY id LIMIT $1 OFFSET $2";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

// ── DML ───────────────────────────────────────────────────────────────

#[test]
fn identical_insert_returning() {
    let snapshot = setup();
    let sql = "INSERT INTO users (name, email) VALUES ($1, $2) RETURNING id, name, age";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_insert_all_columns() {
    let snapshot = setup();
    let sql = "INSERT INTO users (name, email, age) VALUES ($1, $2, $3) RETURNING *";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_insert_multiple_rows() {
    let snapshot = setup();
    let sql = "INSERT INTO users (name, email) VALUES ($1, $2), ($3, $4) RETURNING id";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_insert_into_posts() {
    let snapshot = setup();
    let sql = "INSERT INTO posts (user_id, title, body) VALUES ($1, $2, $3) RETURNING id, title";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_insert_into_comments() {
    let snapshot = setup();
    let sql = "INSERT INTO comments (post_id, author_name, content, rating) \
               VALUES ($1, $2, $3, $4) RETURNING *";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_update_returning() {
    let snapshot = setup();
    let sql = "UPDATE users SET age = $1 WHERE id = $2 RETURNING id, name, age";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_update_multiple_columns() {
    let snapshot = setup();
    let sql = "UPDATE users SET name = $1, email = $2, age = $3 WHERE id = $4 RETURNING *";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_update_with_from() {
    let snapshot = setup();
    let sql = "UPDATE posts SET title = $1 \
               FROM users u WHERE posts.user_id = u.id AND u.name = $2 \
               RETURNING posts.id, posts.title";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_delete_returning() {
    let snapshot = setup();
    let sql = "DELETE FROM users WHERE id = $1 RETURNING id, name, age";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_delete_returning_star() {
    let snapshot = setup();
    let sql = "DELETE FROM comments WHERE post_id = $1 RETURNING *";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_delete_returning_subset() {
    let snapshot = setup();
    let sql = "DELETE FROM posts WHERE user_id = $1 RETURNING id, title";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

// ── CTEs ──────────────────────────────────────────────────────────────

#[test]
fn identical_cte_simple() {
    let snapshot = setup();
    let sql = "WITH active AS (SELECT id, name FROM users WHERE age > 18) \
               SELECT * FROM active";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_cte_multiple() {
    let snapshot = setup();
    let sql = "WITH \
                 u AS (SELECT id, name FROM users), \
                 p AS (SELECT user_id, title FROM posts) \
               SELECT u.name, p.title \
               FROM u INNER JOIN p ON p.user_id = u.id";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_cte_with_insert_returning() {
    let snapshot = setup();
    let sql = "WITH new_user AS (\
                 INSERT INTO users (name, email) VALUES ($1, $2) RETURNING id, name\
               ) SELECT * FROM new_user";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

// ── DISTINCT / DISTINCT ON ────────────────────────────────────────────

#[test]
fn identical_select_distinct() {
    let snapshot = setup();
    let sql = "SELECT DISTINCT name FROM users";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_distinct_on() {
    let snapshot = setup();
    let sql = "SELECT DISTINCT ON (user_id) user_id, title \
               FROM posts ORDER BY user_id, published_at DESC NULLS LAST";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

// ── Mixed param types ─────────────────────────────────────────────────

#[test]
fn identical_params_all_types() {
    let snapshot = setup();
    let sql = "SELECT id FROM users \
               WHERE name = $1 AND age = $2 AND id > $3 AND created_at > $4";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_params_with_cast() {
    let snapshot = setup();
    let sql = "SELECT id FROM users WHERE name = $1::text AND age > $2::int4";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

// ── Complex combined queries ──────────────────────────────────────────

#[test]
fn identical_join_with_where_and_limit() {
    let snapshot = setup();
    let sql = "SELECT u.name, p.title \
               FROM users u INNER JOIN posts p ON p.user_id = u.id \
               WHERE u.age > $1 \
               ORDER BY p.published_at DESC NULLS LAST \
               LIMIT $2";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_insert_select() {
    let snapshot = setup();
    let sql = "INSERT INTO comments (post_id, author_name, content) \
               SELECT p.id, $1, $2 FROM posts p WHERE p.user_id = $3 \
               RETURNING id";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_subquery_in_from() {
    let snapshot = setup();
    let sql = "SELECT sub.name, sub.age \
               FROM (SELECT name, age FROM users WHERE age IS NOT NULL) sub";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_in_subquery() {
    let snapshot = setup();
    let sql = "SELECT id, name FROM users \
               WHERE id IN (SELECT user_id FROM posts)";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_exists_subquery() {
    let snapshot = setup();
    let sql = "SELECT id, name FROM users u \
               WHERE EXISTS (SELECT 1 FROM posts p WHERE p.user_id = u.id)";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

// ══════════════════════════════════════════════════════════════════════════════
// Types match, but our analyzer is MORE PRECISE on nullability.
// PG introspect reports computed expressions as nullable; we know they're not.
// ══════════════════════════════════════════════════════════════════════════════

// ── Literals ──────────────────────────────────────────────────────────

#[test]
fn types_match_integer_literal() {
    let snapshot = setup();
    let sql = "SELECT 42 AS val";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    assert!(!col(&s, "val").nullable, "literal 42 is NOT NULL");
}

#[test]
fn types_match_boolean_literal() {
    let snapshot = setup();
    let sql = "SELECT true AS flag, false AS other";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    assert!(!col(&s, "flag").nullable);
    assert!(!col(&s, "other").nullable);
}

// ── Arithmetic / operators ────────────────────────────────────────────

#[test]
fn types_match_arithmetic() {
    let snapshot = setup();
    let sql = "SELECT id + 1 AS next_id, age * 2 AS double_age FROM users";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    assert!(!col(&s, "next_id").nullable, "id+1: id is NOT NULL");
    assert!(col(&s, "double_age").nullable, "age*2: age is nullable");
}

#[test]
fn types_match_string_concat() {
    let snapshot = setup();
    let sql = "SELECT name || ' <' || email || '>' AS display FROM users";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

// ── Type casts ────────────────────────────────────────────────────────

#[test]
fn types_match_cast_int_to_text() {
    let snapshot = setup();
    let sql = "SELECT age::text AS age_text FROM users";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn types_match_cast_bigint_to_int() {
    let snapshot = setup();
    let sql = "SELECT id::int4 AS short_id FROM users";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn types_match_cast_literal() {
    let snapshot = setup();
    let sql = "SELECT '123'::int4 AS val";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    assert!(!col(&s, "val").nullable, "literal cast is NOT NULL");
}

// ── Functions ─────────────────────────────────────────────────────────

#[test]
fn types_match_count_star() {
    let snapshot = setup();
    let sql = "SELECT count(*) AS total FROM users";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    assert!(!col(&s, "total").nullable, "COUNT is never NULL");
}

#[test]
fn types_match_upper_lower() {
    let snapshot = setup();
    let sql = "SELECT upper(name) AS up, lower(email) AS lo FROM users";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn types_match_length() {
    let snapshot = setup();
    let sql = "SELECT length(name) AS len FROM users";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn types_match_coalesce_with_literal() {
    let snapshot = setup();
    let sql = "SELECT COALESCE(age, 0) AS age_or_zero FROM users";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    assert!(
        !col(&s, "age_or_zero").nullable,
        "COALESCE with NOT NULL fallback"
    );
}

#[test]
fn types_match_now() {
    let snapshot = setup();
    let sql = "SELECT now() AS ts";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    assert!(!col(&s, "ts").nullable, "now() is never NULL");
}

// ── CASE ──────────────────────────────────────────────────────────────

#[test]
fn types_match_case_with_else() {
    let snapshot = setup();
    let sql = "SELECT CASE WHEN age > 18 THEN 'adult' ELSE 'minor' END AS category FROM users";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn types_match_case_expression() {
    let snapshot = setup();
    let sql = "SELECT CASE WHEN age IS NULL THEN 0 ELSE age END AS safe_age FROM users";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

// ── Boolean / NULL tests ──────────────────────────────────────────────

#[test]
fn types_match_null_test() {
    let snapshot = setup();
    let sql = "SELECT id, age IS NULL AS is_null, age IS NOT NULL AS is_not_null FROM users";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    assert!(!col(&s, "is_null").nullable, "IS NULL is never NULL");
    assert!(
        !col(&s, "is_not_null").nullable,
        "IS NOT NULL is never NULL"
    );
}

#[test]
fn types_match_boolean_test() {
    let snapshot = setup();
    let sql = "SELECT (age > 18) IS TRUE AS adult, (age > 18) IS NOT TRUE AS not_adult FROM users";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    assert!(!col(&s, "adult").nullable, "IS TRUE is never NULL");
}

// ── GROUP BY / HAVING ─────────────────────────────────────────────────

#[test]
fn types_match_group_by_count() {
    let snapshot = setup();
    let sql = "SELECT user_id, count(*) AS post_count FROM posts GROUP BY user_id";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    assert!(!col(&s, "post_count").nullable, "COUNT is never NULL");
}

#[test]
fn types_match_group_by_multiple_aggregates() {
    let snapshot = setup();
    let sql = "SELECT user_id, count(*) AS cnt, max(published_at) AS latest \
               FROM posts GROUP BY user_id";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

// ── UNION / INTERSECT / EXCEPT ────────────────────────────────────────

#[test]
fn types_match_union_all() {
    let snapshot = setup();
    let sql = "SELECT id, name FROM users WHERE age > 20 \
               UNION ALL \
               SELECT id, name FROM users WHERE age <= 20";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn types_match_union_distinct() {
    let snapshot = setup();
    let sql = "SELECT name FROM users \
               UNION \
               SELECT title FROM posts";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn types_match_intersect() {
    let snapshot = setup();
    let sql = "SELECT name FROM users \
               INTERSECT \
               SELECT title FROM posts";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn types_match_except() {
    let snapshot = setup();
    let sql = "SELECT name FROM users \
               EXCEPT \
               SELECT title FROM posts";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

// ── CTE + UNION ───────────────────────────────────────────────────────

#[test]
fn types_match_cte_union() {
    let snapshot = setup();
    let sql = "WITH all_names AS (\
                 SELECT name FROM users \
                 UNION ALL \
                 SELECT author_name AS name FROM comments\
               ) \
               SELECT name FROM all_names";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

// ══════════════════════════════════════════════════════════════════════════════
// Enum types (CREATE TYPE ... AS ENUM)
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn identical_enum_column_select() {
    // Enum columns are typed as String by default (no config mapping).
    let snapshot = setup();
    let sql = "SELECT id, name, role FROM users";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    assert_eq!(col(&s, "role").rust_type, "String");
    assert!(!col(&s, "role").nullable, "role has NOT NULL + DEFAULT");
}

#[test]
fn identical_enum_in_where() {
    let snapshot = setup();
    let sql = "SELECT id, name FROM users WHERE role = $1";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    // $1 should be String (enum typed as String)
    assert_eq!(s.params[0].rust_type, "String");
}

#[test]
fn identical_enum_in_insert() {
    let snapshot = setup();
    let sql = "INSERT INTO users (name, email, role) VALUES ($1, $2, $3) RETURNING id, role";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    assert_eq!(s.params[2].rust_type, "String", "$3 = role enum → String");
}

#[test]
fn identical_enum_in_update() {
    let snapshot = setup();
    let sql = "UPDATE users SET role = $1 WHERE id = $2 RETURNING role";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    assert_eq!(s.params[0].rust_type, "String", "$1 = role enum → String");
}

#[test]
fn identical_enum_with_config_mapping() {
    // With enum config, enum_rust_type is set but rust_type stays String.
    let snapshot = setup();
    let mut config = default_config();
    config
        .enums
        .insert("public.user_role".into(), "crate::UserRole".into());
    let sql = "SELECT id, role FROM users";
    let info = analyze(&snapshot, sql, &config).unwrap();
    assert_eq!(col(&info, "role").rust_type, "String");
    assert_eq!(
        col(&info, "role").enum_rust_type.as_deref(),
        Some("crate::UserRole"),
        "enum_rust_type should be set from config"
    );
}

#[test]
fn identical_enum_param_with_config_mapping() {
    let snapshot = setup();
    let mut config = default_config();
    config
        .enums
        .insert("public.user_role".into(), "crate::UserRole".into());
    let sql = "INSERT INTO users (name, email, role) VALUES ($1, $2, $3) RETURNING id";
    let info = analyze(&snapshot, sql, &config).unwrap();
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
    let snapshot = setup();
    let sql = "SELECT id, preferences FROM users";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
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
    let snapshot = setup();
    let mut config = default_config();
    config
        .domains
        .insert("public.user_prefs".into(), "crate::UserPrefs".into());
    let sql = "SELECT id, preferences FROM users";
    let info = analyze(&snapshot, sql, &config).unwrap();
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
    let snapshot = setup();
    let sql = "INSERT INTO users (name, email, preferences) VALUES ($1, $2, $3) RETURNING id";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
    // $3 is user_prefs (JSONB domain) → ::serde_json::Value
    assert!(
        s.params[2].rust_type == "::serde_json::Value",
        "$3 should be ::serde_json::Value, got: {}",
        s.params[2].rust_type
    );
}

#[test]
fn identical_domain_param_with_config() {
    let snapshot = setup();
    let mut config = default_config();
    config
        .domains
        .insert("public.user_prefs".into(), "crate::UserPrefs".into());
    let sql = "INSERT INTO users (name, email, preferences) VALUES ($1, $2, $3) RETURNING id";
    let info = analyze(&snapshot, sql, &config).unwrap();
    assert_eq!(info.params[2].rust_type, "::serde_json::Value");
    assert_eq!(
        info.params[2].domain_rust_type.as_deref(),
        Some("crate::UserPrefs"),
        "param domain_rust_type should be set from config"
    );
}

#[test]
fn identical_domain_in_where() {
    let snapshot = setup();
    let sql = "SELECT id FROM users WHERE preferences IS NOT NULL";
    let _s = analyze(&snapshot, sql, &default_config()).unwrap();
}

#[test]
fn identical_schema_qualified_domain_column_without_config() {
    // whatsapp.health_data domain without config → unwraps to serde_json::Value
    let snapshot = setup();
    let sql = "SELECT channel_id, health FROM whatsapp.channels";
    let s = analyze(&snapshot, sql, &default_config()).unwrap();
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
    let snapshot = setup();
    let mut config = default_config();
    config
        .domains
        .insert("whatsapp.health_data".into(), "crate::HealthData".into());
    let sql = "SELECT channel_id, health FROM whatsapp.channels";
    let info = analyze(&snapshot, sql, &config).unwrap();
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
    let snapshot = setup();
    let mut config = default_config();
    config
        .domains
        .insert("whatsapp.health_data".into(), "crate::HealthData".into());
    let sql = "INSERT INTO whatsapp.channels (channel_id, health, updated_at) \
               VALUES ($1, $2, now())";
    let info = analyze(&snapshot, sql, &config).unwrap();
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
    let snapshot = setup();
    let mut config = default_config();
    config
        .domains
        .insert("public.health_data".into(), "crate::WrongType".into());
    let sql = "SELECT channel_id, health FROM whatsapp.channels";
    let info = analyze(&snapshot, sql, &config).unwrap();
    // Should NOT have domain_rust_type since "public.health_data" != "whatsapp.health_data"
    assert!(
        col(&info, "health").domain_rust_type.is_none(),
        "public.health_data should not match whatsapp.health_data"
    );
}
