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
    assert_eq!(table.name, "users");
    assert_eq!(table.kind, RelationKind::Table);
    assert_eq!(table.columns.len(), 4);

    let id_col = &table.columns[0];
    assert_eq!(id_col.name, "id");
    assert!(id_col.not_null);
    assert!(id_col.has_default); // IDENTITY

    let name_col = &table.columns[1];
    assert_eq!(name_col.name, "name");
    assert!(name_col.not_null);
    assert!(!name_col.has_default);

    let age_col = &table.columns[3];
    assert_eq!(age_col.name, "age");
    assert!(!age_col.not_null);
    assert!(!age_col.has_default);
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
    let id_col = &table.columns[0];
    assert!(id_col.has_default); // SERIAL

    let created_col = &table.columns[1];
    assert!(created_col.has_default); // DEFAULT now()
    assert!(created_col.not_null);

    let name_col = &table.columns[2];
    assert!(!name_col.has_default);
}

#[test]
fn create_table_registers_composite_and_array_types() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE items (id INT NOT NULL, name TEXT);",
    )]);

    // Composite type for the table.
    let ct = snap.resolve_type_by_name(Some("public"), "items").unwrap();
    assert!(matches!(ct.kind, TypeKind::Composite { .. }));

    // Array type.
    let at = snap.resolve_type_by_name(Some("public"), "_items").unwrap();
    assert!(matches!(at.kind, TypeKind::Array { .. }));
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
    assert_eq!(table.columns.len(), 1);
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
    assert_eq!(table.columns.len(), 2);
    assert_eq!(table.schema, "myapp");
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
    assert_eq!(t1.columns.len(), 1);

    let t2 = snap.resolve_table(Some("other"), "t").unwrap();
    assert_eq!(t2.columns.len(), 2);
}

// ── SERIAL / BIGSERIAL / SMALLSERIAL ────────────────────────────────────────

#[test]
fn serial_without_pk_is_nullable() {
    // PG: SERIAL alone implies a default (from the sequence) but NOT
    // NOT-NULL — only the PRIMARY KEY constraint adds NOT NULL.
    let snap = build(&[("0001.sql", "CREATE TABLE t (id SERIAL, name TEXT);")]);

    let table = snap.resolve_table(None, "t").unwrap();
    let id_col = table.columns.iter().find(|c| c.name == "id").unwrap();
    assert!(
        !id_col.not_null,
        "SERIAL without PRIMARY KEY should be nullable"
    );
    assert!(id_col.has_default, "SERIAL should have a default");
}

#[test]
fn bigserial_resolves_to_int8() {
    let snap = build(&[("0001.sql", "CREATE TABLE t (id BIGSERIAL PRIMARY KEY);")]);

    let table = snap.resolve_table(None, "t").unwrap();
    let id_col = table.columns.iter().find(|c| c.name == "id").unwrap();
    let int8_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int8")
        .unwrap()
        .oid;
    assert_eq!(
        id_col.type_oid, int8_oid,
        "BIGSERIAL should resolve to int8"
    );
    assert!(id_col.has_default, "BIGSERIAL should have a default");
    assert!(id_col.not_null, "BIGSERIAL PRIMARY KEY should be NOT NULL");
}

#[test]
fn smallserial_resolves_to_int2() {
    let snap = build(&[("0001.sql", "CREATE TABLE t (id SMALLSERIAL NOT NULL);")]);

    let table = snap.resolve_table(None, "t").unwrap();
    let id_col = table.columns.iter().find(|c| c.name == "id").unwrap();
    let int2_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int2")
        .unwrap()
        .oid;
    assert_eq!(
        id_col.type_oid, int2_oid,
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
    let name = table.columns.iter().find(|c| c.name == "name").unwrap();
    let code = table.columns.iter().find(|c| c.name == "code").unwrap();

    assert_ne!(name.type_oid, 0, "VARCHAR(100) should resolve");
    assert_ne!(code.type_oid, 0, "CHAR(5) should resolve");

    let varchar_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "varchar")
        .unwrap()
        .oid;
    assert_eq!(name.type_oid, varchar_oid);
}

#[test]
fn numeric_and_decimal_resolve_to_numeric() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (amount NUMERIC(10,2) NOT NULL, factor DECIMAL NOT NULL);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let amount = table.columns.iter().find(|c| c.name == "amount").unwrap();
    let factor = table.columns.iter().find(|c| c.name == "factor").unwrap();

    let numeric_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "numeric")
        .unwrap()
        .oid;
    assert_eq!(amount.type_oid, numeric_oid);
    assert_eq!(
        factor.type_oid, numeric_oid,
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
    for col in &table.columns {
        assert_ne!(col.type_oid, 0, "column '{}' must resolve", col.name);
    }
}

#[test]
fn json_and_jsonb_types_resolve() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (data JSONB NOT NULL, meta JSON);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let data = table.columns.iter().find(|c| c.name == "data").unwrap();
    let meta = table.columns.iter().find(|c| c.name == "meta").unwrap();

    let jsonb_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "jsonb")
        .unwrap()
        .oid;
    let json_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "json")
        .unwrap()
        .oid;
    assert_eq!(data.type_oid, jsonb_oid);
    assert_eq!(meta.type_oid, json_oid);
    assert!(!meta.not_null, "JSON without NOT NULL should be nullable");
}

#[test]
fn uuid_type_with_default() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (id UUID NOT NULL DEFAULT gen_random_uuid(), name TEXT NOT NULL);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let id = table.columns.iter().find(|c| c.name == "id").unwrap();
    let uuid_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "uuid")
        .unwrap()
        .oid;
    assert_eq!(id.type_oid, uuid_oid);
    assert!(
        id.has_default,
        "DEFAULT gen_random_uuid() must set has_default"
    );
}

// ── Column-level constraints (parse + NOT NULL propagation) ────────────────

#[test]
fn unique_constraint_does_not_imply_not_null() {
    // UNIQUE parses but doesn't add NOT NULL — the user declared `email` as
    // NOT NULL separately, so it inherits that instead.
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (
            id SERIAL PRIMARY KEY,
            email TEXT NOT NULL UNIQUE,
            name TEXT NOT NULL
        );",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    assert_eq!(table.columns.len(), 3);
    let email = table.columns.iter().find(|c| c.name == "email").unwrap();
    assert!(email.not_null);
}

#[test]
fn foreign_key_constraint_parses_without_affecting_column() {
    // FOREIGN KEY is a cross-table constraint; CREATE TABLE must register
    // all columns normally and not report the referenced table as missing.
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
    assert_eq!(posts.columns.len(), 3);
    let user_id = posts.columns.iter().find(|c| c.name == "user_id").unwrap();
    assert!(user_id.not_null);
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
    assert_eq!(table.columns.len(), 3);
}

#[test]
fn generated_stored_column_has_default() {
    // GENERATED ALWAYS AS (...) STORED columns are computed — INSERT must not
    // supply a value for them, so they behave like a column with a default.
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (a INT NOT NULL, b INT GENERATED ALWAYS AS (a * 2) STORED);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let b = table.columns.iter().find(|c| c.name == "b").unwrap();
    assert!(
        b.has_default,
        "GENERATED ALWAYS AS (stored) must set has_default"
    );
}

// ── Param inference after DDL (GROUP BY / HAVING context) ───────────────────

#[test]
fn param_in_group_by_and_having_is_inferred() {
    // Params in GROUP BY / HAVING clauses must also be walked. This ensures
    // the analyzer collects and types them after a fresh CREATE TABLE.
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
