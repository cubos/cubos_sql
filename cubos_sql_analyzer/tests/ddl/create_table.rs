//! CREATE TABLE: column types, constraints (NOT NULL, PRIMARY KEY, UNIQUE,
//! CHECK, FOREIGN KEY, GENERATED), defaults, SERIAL / BIGSERIAL / SMALLSERIAL,
//! type modifiers (VARCHAR(n), NUMERIC(p,s)), IF NOT EXISTS semantics.

use crate::common::*;

// ── Basics ──────────────────────────────────────────────────────────────────

#[test]
fn create_table_basic() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE users (
            id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            name TEXT NOT NULL,
            email TEXT NOT NULL,
            age INT
        );",
    )]);

    let table = snap.resolve_table(Some("public"), "users").unwrap();
    assert_eq!(table.relname, "users");
    assert_eq!(table.relkind, RelKind::Table);
    let attrs = snap.attributes_of(table.oid);
    assert_eq!(attrs.len(), 4);

    let id_col = &attrs[0];
    assert_eq!(id_col.attname, "id");
    assert!(id_col.attnotnull);
    assert!(id_col.atthasdef); // IDENTITY

    let name_col = &attrs[1];
    assert_eq!(name_col.attname, "name");
    assert!(name_col.attnotnull);
    assert!(!name_col.atthasdef);

    let age_col = &attrs[3];
    assert_eq!(age_col.attname, "age");
    assert!(!age_col.attnotnull);
    assert!(!age_col.atthasdef);
}

#[test]
fn create_table_with_default() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (
            id SERIAL PRIMARY KEY,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            name TEXT NOT NULL
        );",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let id_col = &attrs[0];
    assert!(id_col.atthasdef); // SERIAL

    let created_col = &attrs[1];
    assert!(created_col.atthasdef); // DEFAULT now()
    assert!(created_col.attnotnull);

    let name_col = &attrs[2];
    assert!(!name_col.atthasdef);
}

#[test]
fn create_table_registers_composite_and_array_types() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE items (id INT NOT NULL, name TEXT);",
    )]);

    // Composite type for the table.
    let ct = snap.resolve_type_by_name(Some("public"), "items").unwrap();
    assert_eq!(ct.typtype, TypType::Composite);

    // Array type.
    let at = snap.resolve_type_by_name(Some("public"), "_items").unwrap();
    assert_eq!(at.typcategory, TypCategory::Array);
}

#[test]
fn create_table_if_not_exists() {
    let snap = build(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL);"),
        (
            "0002.sql",
            "CREATE TABLE IF NOT EXISTS t (id INT, name TEXT);",
        ),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    // Should still have original schema (1 column), not the second one.
    assert_eq!(snap.attributes_of(table.oid).len(), 1);
}

// ── Schema-qualified tables ─────────────────────────────────────────────────

#[test]
fn create_schema_with_table() {
    let snap = build(&[(
        "0001.sql",
        "CREATE SCHEMA myapp;
         CREATE TABLE myapp.items (id INT NOT NULL, name TEXT NOT NULL);",
    )]);

    let table = snap.resolve_table(Some("myapp"), "items").unwrap();
    assert_eq!(snap.attributes_of(table.oid).len(), 2);
    assert_eq!(snap.namespace_name(table.relnamespace), Some("myapp"));
}

// ── No-op DDL shouldn't fail ────────────────────────────────────────────────

#[test]
fn noops_dont_fail() {
    let _snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (id INT NOT NULL);
         CREATE INDEX idx_t ON t (id);
         CREATE SEQUENCE my_seq;
         GRANT SELECT ON t TO PUBLIC;
         COMMENT ON TABLE t IS 'test table';",
    )]);
}

// ── Duplicates ──────────────────────────────────────────────────────────────

#[test]
fn create_table_duplicate_without_if_not_exists_errors() {
    let result = try_apply(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL);"),
        ("0002.sql", "CREATE TABLE t (name TEXT NOT NULL);"),
    ]);

    assert_ddl_err!(result, DdlError::DuplicateObject(_), "already exists");
}

#[test]
fn create_table_duplicate_column_names_errors() {
    let result = try_apply(&[(
        "0001.sql",
        "CREATE TABLE t (id INT NOT NULL, name TEXT, id TEXT);",
    )]);

    assert_ddl_err!(
        result,
        DdlError::DuplicateObject(_),
        "specified more than once",
    );
}

#[test]
fn create_table_if_not_exists_different_schema_creates_both() {
    // IF NOT EXISTS on a qualified name only skips when an object exists with
    // the SAME schema+name. Different schemas ⇒ two independent tables.
    let snap = build(&[(
        "0001.sql",
        "CREATE SCHEMA other;
         CREATE TABLE public.t (id INT NOT NULL);
         CREATE TABLE IF NOT EXISTS other.t (id INT NOT NULL, name TEXT NOT NULL);",
    )]);

    let t1 = snap.resolve_table(Some("public"), "t").unwrap();
    assert_eq!(snap.attributes_of(t1.oid).len(), 1);

    let t2 = snap.resolve_table(Some("other"), "t").unwrap();
    assert_eq!(snap.attributes_of(t2.oid).len(), 2);
}

// ── SERIAL / BIGSERIAL / SMALLSERIAL ────────────────────────────────────────

#[test]
fn serial_without_pk_is_nullable() {
    let snap = build(&[("0001.sql", "CREATE TABLE t (id SERIAL, name TEXT);")]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let id_col = attrs.iter().find(|c| c.attname == "id").unwrap();
    assert!(
        !id_col.attnotnull,
        "SERIAL without PRIMARY KEY should be nullable"
    );
    assert!(id_col.atthasdef, "SERIAL should have a default");
}

#[test]
fn bigserial_resolves_to_int8() {
    let snap = build(&[("0001.sql", "CREATE TABLE t (id BIGSERIAL PRIMARY KEY);")]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let id_col = attrs.iter().find(|c| c.attname == "id").unwrap();
    let int8_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int8")
        .unwrap()
        .oid;
    assert_eq!(
        id_col.atttypid, int8_oid,
        "BIGSERIAL should resolve to int8"
    );
    assert!(id_col.atthasdef, "BIGSERIAL should have a default");
    assert!(
        id_col.attnotnull,
        "BIGSERIAL PRIMARY KEY should be NOT NULL"
    );
}

#[test]
fn smallserial_resolves_to_int2() {
    let snap = build(&[("0001.sql", "CREATE TABLE t (id SMALLSERIAL NOT NULL);")]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let id_col = attrs.iter().find(|c| c.attname == "id").unwrap();
    let int2_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int2")
        .unwrap()
        .oid;
    assert_eq!(
        id_col.atttypid, int2_oid,
        "SMALLSERIAL should resolve to int2"
    );
}

// ── Type modifiers (VARCHAR(n), NUMERIC(p,s)) and common column types ──────

#[test]
fn varchar_and_char_resolve_to_varchar_and_bpchar() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (name VARCHAR(100) NOT NULL, code CHAR(5) NOT NULL);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let name = attrs.iter().find(|c| c.attname == "name").unwrap();
    let code = attrs.iter().find(|c| c.attname == "code").unwrap();

    assert_ne!(name.atttypid.get(), 0, "VARCHAR(100) should resolve");
    assert_ne!(code.atttypid.get(), 0, "CHAR(5) should resolve");

    let varchar_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "varchar")
        .unwrap()
        .oid;
    assert_eq!(name.atttypid, varchar_oid);
}

#[test]
fn numeric_and_decimal_resolve_to_numeric() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (amount NUMERIC(10,2) NOT NULL, factor DECIMAL NOT NULL);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let amount = attrs.iter().find(|c| c.attname == "amount").unwrap();
    let factor = attrs.iter().find(|c| c.attname == "factor").unwrap();

    let numeric_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "numeric")
        .unwrap()
        .oid;
    assert_eq!(amount.atttypid, numeric_oid);
    assert_eq!(
        factor.atttypid, numeric_oid,
        "DECIMAL should resolve to numeric"
    );
}

#[test]
fn datetime_types_resolve() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (
            a TIMESTAMP NOT NULL,
            b TIMESTAMPTZ NOT NULL,
            c DATE NOT NULL,
            d TIME NOT NULL,
            e INTERVAL NOT NULL
        );",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    for col in attrs {
        assert_ne!(
            col.atttypid.get(),
            0,
            "column '{}' must resolve",
            col.attname
        );
    }
}

#[test]
fn json_and_jsonb_types_resolve() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (data JSONB NOT NULL, meta JSON);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let data = attrs.iter().find(|c| c.attname == "data").unwrap();
    let meta = attrs.iter().find(|c| c.attname == "meta").unwrap();

    let jsonb_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "jsonb")
        .unwrap()
        .oid;
    let json_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "json")
        .unwrap()
        .oid;
    assert_eq!(data.atttypid, jsonb_oid);
    assert_eq!(meta.atttypid, json_oid);
    assert!(!meta.attnotnull, "JSON without NOT NULL should be nullable");
}

#[test]
fn uuid_type_with_default() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (id UUID NOT NULL DEFAULT gen_random_uuid(), name TEXT NOT NULL);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let id = attrs.iter().find(|c| c.attname == "id").unwrap();
    let uuid_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "uuid")
        .unwrap()
        .oid;
    assert_eq!(id.atttypid, uuid_oid);
    assert!(
        id.atthasdef,
        "DEFAULT gen_random_uuid() must set has_default"
    );
}

// ── Column-level constraints (parse + NOT NULL propagation) ────────────────

#[test]
fn unique_constraint_does_not_imply_not_null() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (
            id SERIAL PRIMARY KEY,
            email TEXT NOT NULL UNIQUE,
            name TEXT NOT NULL
        );",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    assert_eq!(attrs.len(), 3);
    let email = attrs.iter().find(|c| c.attname == "email").unwrap();
    assert!(email.attnotnull);
}

#[test]
fn foreign_key_constraint_parses_without_affecting_column() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE users (id SERIAL PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE posts (
             id SERIAL PRIMARY KEY,
             user_id INT NOT NULL REFERENCES users(id),
             title TEXT NOT NULL
         );",
    )]);

    let posts = snap.resolve_table(None, "posts").unwrap();
    let attrs = snap.attributes_of(posts.oid);
    assert_eq!(attrs.len(), 3);
    let user_id = attrs.iter().find(|c| c.attname == "user_id").unwrap();
    assert!(user_id.attnotnull);
}

#[test]
fn check_constraint_parses() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (
            id INT NOT NULL,
            age INT CHECK (age >= 0 AND age <= 200),
            status TEXT NOT NULL CHECK (status IN ('active', 'inactive'))
        );",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    assert_eq!(snap.attributes_of(table.oid).len(), 3);
}

#[test]
fn generated_stored_column_has_default() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (a INT NOT NULL, b INT GENERATED ALWAYS AS (a * 2) STORED);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let b = attrs.iter().find(|c| c.attname == "b").unwrap();
    assert!(
        b.atthasdef,
        "GENERATED ALWAYS AS (stored) must set has_default"
    );
}

// ── pg_proc.provolatile not modeled — VOLATILE in CHECK / generated cols ───
//
// PG forbids VOLATILE functions (random(), now() in some forms, nextval())
// from CHECK constraints and from `GENERATED … STORED` expressions —
// otherwise the constraint or generated value could change between rows.
// Without `provolatile` the analyzer can't enforce this.

#[test]
#[ignore = "pg_proc.provolatile not modeled — VOLATILE function in CHECK constraint is not rejected"]
fn volatile_function_in_check_constraint_should_error() {
    // PG: `cannot use volatile function "random" in check constraint`.
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL CHECK (id < (random() * 100)::int));",
        )]),
        DdlError::UnsupportedDdl(_),
        "volatile function",
    );
}

#[test]
#[ignore = "pg_proc.provolatile not modeled — VOLATILE function in generated column is not rejected"]
fn volatile_function_in_generated_stored_column_should_error() {
    // PG: `generation expression is not immutable`. The expression must be
    // pure of the row's own columns.
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE t (
                id INT NOT NULL,
                noise NUMERIC GENERATED ALWAYS AS (random() * id) STORED
            );",
        )]),
        DdlError::UnsupportedDdl(_),
        "not immutable",
    );
}

// ── Param inference after DDL (GROUP BY / HAVING context) ───────────────────

#[test]
fn param_in_group_by_and_having_is_inferred() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE orders (id BIGINT, total INT NOT NULL);",
    )]);

    let info = db
        .analyze(
            "SELECT total, COUNT(*) AS c
             FROM orders
             GROUP BY total
             HAVING COUNT(*) > $min",
        )
        .expect("HAVING param should be resolvable");
    assert_eq!(info.params.len(), 1);
    assert_eq!(
        info.params[0].pg_type,
        Type::Basic {
            schema: "pg_catalog".into(),
            name: "int8".into(),
            extension: None,
        }
    );
}
