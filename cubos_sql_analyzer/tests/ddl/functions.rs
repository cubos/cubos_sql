//! CREATE FUNCTION and CREATE AGGREGATE: signatures, overloading,
//! CREATE OR REPLACE, ALTER FUNCTION (rename, SET SCHEMA),
//! DROP FUNCTION — including overload-safe drops.

use crate::common::*;

// ── CREATE FUNCTION ─────────────────────────────────────────────────────────

#[test]
fn create_function_basic() {
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION add_one(x INT) RETURNS INT AS $$ SELECT x + 1 $$ LANGUAGE sql;",
    )]);

    let fns = snap.find_functions(None, "add_one");
    assert_eq!(fns.len(), 1);
    let f = fns[0];
    assert_eq!(f.arg_types.len(), 1);
    let int4_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int4")
        .unwrap()
        .oid;
    assert_eq!(f.arg_types[0], int4_oid);
    assert_eq!(f.return_type_oid, int4_oid);
}

// ── CREATE / DROP AGGREGATE ─────────────────────────────────────────────────

#[test]
fn create_aggregate_registers_function() {
    // CREATE AGGREGATE should register the aggregate in functions_by_name
    // with is_aggregate = true and the STYPE as the return type when no
    // FINALFUNC is declared.
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION my_sum_sfunc(int8, int4) RETURNS int8
             AS 'SELECT $1 + $2::int8' LANGUAGE SQL;
         CREATE AGGREGATE my_sum(int4) (
             SFUNC = my_sum_sfunc,
             STYPE = int8,
             INITCOND = '0'
         );",
    )]);

    let fns = snap.find_functions(None, "my_sum");
    let agg = fns
        .iter()
        .find(|f| f.is_aggregate)
        .expect("my_sum should be registered as an aggregate");
    let int4_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int4")
        .unwrap()
        .oid;
    let int8_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int8")
        .unwrap()
        .oid;
    assert_eq!(agg.arg_types, vec![int4_oid]);
    assert_eq!(
        agg.return_type_oid, int8_oid,
        "no FINALFUNC ⇒ return type equals STYPE"
    );
}

#[test]
fn create_aggregate_with_finalfunc_uses_final_return_type() {
    // When FINALFUNC is declared, the aggregate's effective return type is
    // the final function's return type, not the STYPE.
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION my_avg_sfunc(int8, int4) RETURNS int8
             AS 'SELECT $1' LANGUAGE SQL;
         CREATE FUNCTION my_avg_finalfunc(int8) RETURNS float8
             AS 'SELECT $1::float8' LANGUAGE SQL;
         CREATE AGGREGATE my_avg(int4) (
             SFUNC = my_avg_sfunc,
             STYPE = int8,
             FINALFUNC = my_avg_finalfunc
         );",
    )]);

    let float8_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "float8")
        .unwrap()
        .oid;
    let agg = snap
        .find_functions(None, "my_avg")
        .into_iter()
        .find(|f| f.is_aggregate)
        .expect("my_avg aggregate should exist");
    assert_eq!(agg.return_type_oid, float8_oid);
    assert_eq!(agg.agg_final_type_oid, Some(float8_oid));
}

#[test]
fn drop_aggregate_removes_only_aggregate() {
    // DROP AGGREGATE must match on (name, arg_types, is_aggregate) — a
    // scalar function with the same name/signature must survive.
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION dup(int4) RETURNS int4 AS 'SELECT $1' LANGUAGE SQL;
         CREATE FUNCTION dup_sfunc(int4, int4) RETURNS int4 AS 'SELECT $1 + $2' LANGUAGE SQL;
         CREATE AGGREGATE dup(int4) (
             SFUNC = dup_sfunc,
             STYPE = int4
         );
         DROP AGGREGATE dup(int4);",
    )]);

    let fns = snap.find_functions(None, "dup");
    assert_eq!(fns.len(), 1, "scalar dup(int4) should remain");
    assert!(!fns[0].is_aggregate);
}

#[test]
fn drop_aggregate_missing_errors_without_if_exists() {
    let result = try_apply(&[("0001.sql", "DROP AGGREGATE nonexistent(int4);")]);
    assert_ddl_err!(
        result,
        DdlError::DependencyError(_),
        "aggregate nonexistent",
    );
}

#[test]
fn drop_aggregate_if_exists_no_error() {
    let _snap = build(&[("0001.sql", "DROP AGGREGATE IF EXISTS nonexistent(int4);")]);
}

// ── ALTER FUNCTION / AGGREGATE ─────────────────────────────────────────────

#[test]
fn alter_function_rename_moves_overload() {
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION add_one(x int) RETURNS int AS 'SELECT $1 + 1' LANGUAGE SQL;
         ALTER FUNCTION add_one(int) RENAME TO plus_one;",
    )]);

    assert!(
        !snap
            .functions_by_name
            .contains_key(&QualifiedName::new("public", "add_one")),
        "old name should be gone"
    );
    let fns = snap
        .functions_by_name
        .get(&QualifiedName::new("public", "plus_one"))
        .expect("renamed function should exist");
    assert_eq!(fns.len(), 1);
    let int4_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int4")
        .unwrap()
        .oid;
    assert_eq!(fns[0].arg_types, vec![int4_oid]);
}

#[test]
fn alter_function_set_schema_moves_it() {
    let snap = build(&[(
        "0001.sql",
        "CREATE SCHEMA utils;
         CREATE FUNCTION add_one(x int) RETURNS int AS 'SELECT $1 + 1' LANGUAGE SQL;
         ALTER FUNCTION add_one(int) SET SCHEMA utils;",
    )]);

    let fns = snap
        .functions_by_name
        .get(&QualifiedName::new("utils", "add_one"))
        .unwrap();
    assert_eq!(fns[0].schema, "utils");
}

#[test]
fn alter_function_rename_with_overloads_only_moves_matching_signature() {
    // Two overloads of the same function; the rename must only move the
    // one whose arg_types match.
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION do_it(x int) RETURNS int AS 'SELECT $1' LANGUAGE SQL;
         CREATE FUNCTION do_it(x text) RETURNS text AS 'SELECT $1' LANGUAGE SQL;
         ALTER FUNCTION do_it(int) RENAME TO do_it_int;",
    )]);

    let int4_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int4")
        .unwrap()
        .oid;
    let text_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "text")
        .unwrap()
        .oid;

    let renamed = snap
        .functions_by_name
        .get(&QualifiedName::new("public", "do_it_int"))
        .unwrap();
    assert_eq!(renamed.len(), 1);
    assert_eq!(renamed[0].arg_types, vec![int4_oid]);

    let remaining = snap
        .functions_by_name
        .get(&QualifiedName::new("public", "do_it"))
        .unwrap();
    assert_eq!(
        remaining.len(),
        1,
        "text overload should still be under do_it"
    );
    assert_eq!(remaining[0].arg_types, vec![text_oid]);
}

#[test]
fn alter_aggregate_rename_only_touches_aggregate() {
    // A scalar function and an aggregate share the same name. ALTER
    // AGGREGATE must only rename the aggregate.
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION ag(x int) RETURNS int AS 'SELECT $1' LANGUAGE SQL;
         CREATE FUNCTION ag_sfunc(state int, val int) RETURNS int AS 'SELECT $1 + $2' LANGUAGE SQL;
         CREATE AGGREGATE ag(int) (SFUNC = ag_sfunc, STYPE = int);
         ALTER AGGREGATE ag(int) RENAME TO ag_total;",
    )]);

    // Scalar survives under original name.
    let scalar = snap
        .functions_by_name
        .get(&QualifiedName::new("public", "ag"))
        .unwrap();
    assert_eq!(scalar.len(), 1);
    assert!(!scalar[0].is_aggregate);

    // Aggregate moved.
    let moved = snap
        .functions_by_name
        .get(&QualifiedName::new("public", "ag_total"))
        .unwrap();
    assert_eq!(moved.len(), 1);
    assert!(moved[0].is_aggregate);
}

// ── DROP FUNCTION vs procedure of same name ────────────────────────────────

#[test]
fn drop_function_does_not_touch_procedure_of_same_name() {
    // DROP FUNCTION must not remove a procedure with the same name+sig,
    // mirroring PostgreSQL's asymmetry between the two object kinds.
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION f(x int) RETURNS int AS 'SELECT $1' LANGUAGE SQL;
         CREATE PROCEDURE f(x int) LANGUAGE SQL AS $$ SELECT $1 $$;
         DROP FUNCTION f(int);",
    )]);

    let fns = snap
        .functions_by_name
        .get(&QualifiedName::new("public", "f"))
        .unwrap();
    assert_eq!(fns.len(), 1, "procedure must survive DROP FUNCTION");
    assert!(fns[0].is_procedure);
}

// ── CREATE OR REPLACE / overloading ────────────────────────────────────────

#[test]
fn create_or_replace_function_updates_existing() {
    // CREATE OR REPLACE with the same signature replaces the prior function,
    // including a different return type.
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION foo(x INT) RETURNS INT AS $$ SELECT x $$ LANGUAGE sql;
         CREATE OR REPLACE FUNCTION foo(x INT) RETURNS BIGINT AS $$ SELECT x::bigint $$ LANGUAGE sql;",
    )]);

    let fns = snap.find_functions(None, "foo");
    assert_eq!(fns.len(), 1, "should have exactly 1 overload");
    let int8_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int8")
        .unwrap()
        .oid;
    assert_eq!(
        fns[0].return_type_oid, int8_oid,
        "return type must be updated to int8",
    );
}

#[test]
fn create_function_overloading_distinct_arg_types() {
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION foo(x INT) RETURNS INT AS $$ SELECT x $$ LANGUAGE sql;
         CREATE FUNCTION foo(x TEXT) RETURNS TEXT AS $$ SELECT x $$ LANGUAGE sql;",
    )]);

    let fns = snap.find_functions(None, "foo");
    assert_eq!(fns.len(), 2, "should have 2 overloads");
}

#[test]
fn create_function_duplicate_signature_errors() {
    // Without OR REPLACE, creating a second function with the same name+sig
    // must fail with a DuplicateObject variant.
    let result = try_apply(&[(
        "0001.sql",
        "CREATE FUNCTION foo(x INT) RETURNS INT AS $$ SELECT x $$ LANGUAGE sql;
         CREATE FUNCTION foo(x INT) RETURNS INT AS $$ SELECT x + 1 $$ LANGUAGE sql;",
    )]);

    assert_ddl_err!(result, DdlError::DuplicateObject(_), "already exists");
}

// ── Full blog schema: multi-migration integration test ─────────────────────

#[test]
fn full_blog_schema() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE users (
                id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                name TEXT NOT NULL,
                email TEXT NOT NULL UNIQUE,
                age INT,
                created_at TIMESTAMPTZ NOT NULL DEFAULT now()
            );",
        ),
        (
            "0002.sql",
            "CREATE TYPE post_status AS ENUM ('draft', 'published', 'archived');
             CREATE TABLE posts (
                id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                user_id BIGINT NOT NULL REFERENCES users(id),
                title TEXT NOT NULL,
                body TEXT,
                status post_status NOT NULL DEFAULT 'draft',
                published_at TIMESTAMPTZ,
                created_at TIMESTAMPTZ NOT NULL DEFAULT now()
             );
             CREATE INDEX idx_posts_user_id ON posts (user_id);",
        ),
        (
            "0003.sql",
            "ALTER TABLE users ADD COLUMN bio TEXT;
             ALTER TYPE post_status ADD VALUE 'deleted' AFTER 'archived';",
        ),
    ]);

    // Users table has 6 columns now (id, name, email, age, created_at, bio).
    let users = snap.resolve_table(None, "users").unwrap();
    assert_eq!(users.columns.len(), 6);
    assert_eq!(users.columns[5].name, "bio");
    assert!(!users.columns[5].not_null);

    // Posts table.
    let posts = snap.resolve_table(None, "posts").unwrap();
    assert_eq!(posts.columns.len(), 7);

    // post_status enum has 4 values.
    let ps = snap.resolve_type_by_name(None, "post_status").unwrap();
    match &ps.kind {
        TypeKind::Enum { labels } => {
            assert_eq!(labels, &["draft", "published", "archived", "deleted"]);
        }
        _ => panic!("expected Enum"),
    }
}
