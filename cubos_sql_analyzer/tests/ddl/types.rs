//! CREATE TYPE — enum, composite, range, domain — and ALTER ENUM
//! (ADD VALUE, ADD VALUE IF NOT EXISTS, BEFORE/AFTER).

use crate::common::*;

// ── CREATE DOMAIN ───────────────────────────────────────────────────────────

#[test]
fn create_domain() {
    let snap = build(&[("0001.sql", "CREATE DOMAIN email AS TEXT;")]);

    let te = snap.resolve_type_by_name(None, "email").unwrap();
    match &te.kind {
        TypeKind::Domain { base_type_oid } => {
            let base = snap.get_type(*base_type_oid).unwrap();
            assert_eq!(base.name, "text");
        }
        _ => panic!("expected Domain, got {:?}", te.kind),
    }

    // Array type.
    assert!(
        snap.resolve_type_by_name(Some("public"), "_email")
            .is_some()
    );
}

// ── CREATE TYPE AS ENUM ─────────────────────────────────────────────────────

#[test]
fn create_enum() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');",
    )]);

    let te = snap.resolve_type_by_name(None, "mood").unwrap();
    match &te.kind {
        TypeKind::Enum { labels } => {
            assert_eq!(labels, &["sad", "ok", "happy"]);
        }
        _ => panic!("expected Enum, got {:?}", te.kind),
    }
}

#[test]
fn alter_enum_add_value() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');",
        ),
        (
            "0002.sql",
            "ALTER TYPE mood ADD VALUE 'ecstatic' AFTER 'happy';",
        ),
    ]);

    let te = snap.resolve_type_by_name(None, "mood").unwrap();
    match &te.kind {
        TypeKind::Enum { labels } => {
            assert_eq!(labels, &["sad", "ok", "happy", "ecstatic"]);
        }
        _ => panic!("expected Enum"),
    }
}

#[test]
fn alter_enum_add_value_before() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');",
        ),
        (
            "0002.sql",
            "ALTER TYPE mood ADD VALUE 'anxious' BEFORE 'sad';",
        ),
    ]);

    let te = snap.resolve_type_by_name(None, "mood").unwrap();
    match &te.kind {
        TypeKind::Enum { labels } => {
            assert_eq!(labels, &["anxious", "sad", "ok", "happy"]);
        }
        _ => panic!("expected Enum"),
    }
}

// ── CREATE TYPE AS (composite) ──────────────────────────────────────────────

#[test]
fn create_composite_type() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TYPE address AS (
            street TEXT,
            city TEXT,
            zip TEXT
        );",
    )]);

    let te = snap.resolve_type_by_name(None, "address").unwrap();
    match &te.kind {
        TypeKind::Composite { fields } => {
            assert_eq!(fields.len(), 3);
            assert_eq!(fields[0].name, "street");
            assert_eq!(fields[1].name, "city");
            assert_eq!(fields[2].name, "zip");
        }
        _ => panic!("expected Composite"),
    }
}

#[test]
fn composite_type_field_types_resolved() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TYPE address AS (
            street TEXT,
            city TEXT,
            zip INT
        );",
    )]);

    let te = snap.resolve_type_by_name(None, "address").unwrap();
    match &te.kind {
        TypeKind::Composite { fields } => {
            let text_oid = snap
                .resolve_type_by_name(Some("pg_catalog"), "text")
                .unwrap()
                .oid;
            let int4_oid = snap
                .resolve_type_by_name(Some("pg_catalog"), "int4")
                .unwrap()
                .oid;
            assert_eq!(fields[0].type_oid, text_oid);
            assert_eq!(fields[1].type_oid, text_oid);
            assert_eq!(fields[2].type_oid, int4_oid);
        }
        _ => panic!("expected Composite"),
    }
}

// ── CREATE TYPE AS RANGE ────────────────────────────────────────────────────

#[test]
fn create_range_type_with_subtype() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TYPE floatrange AS RANGE (subtype = float8);",
    )]);

    let te = snap.resolve_type_by_name(None, "floatrange").unwrap();
    match &te.kind {
        TypeKind::Range { subtype_oid } => {
            let float8_oid = snap
                .resolve_type_by_name(Some("pg_catalog"), "float8")
                .unwrap()
                .oid;
            assert_eq!(*subtype_oid, float8_oid);
        }
        _ => panic!("expected Range"),
    }
}

// ── User-defined types as column types ─────────────────────────────────────

#[test]
fn enum_as_column_type() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TYPE status AS ENUM ('active', 'inactive');
         CREATE TABLE t (id INT NOT NULL, s status NOT NULL);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let s_col = table.columns.iter().find(|c| c.name == "s").unwrap();
    let status_oid = snap.resolve_type_by_name(None, "status").unwrap().oid;
    assert_eq!(s_col.type_oid, status_oid);
}

#[test]
fn domain_as_column_type() {
    let snap = build(&[(
        "0001.sql",
        "CREATE DOMAIN email AS TEXT;
         CREATE TABLE t (id INT NOT NULL, contact email NOT NULL);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let contact = table.columns.iter().find(|c| c.name == "contact").unwrap();
    let email_oid = snap.resolve_type_by_name(None, "email").unwrap().oid;
    assert_eq!(contact.type_oid, email_oid);
}

#[test]
fn enum_array_as_column_type_is_array_kind() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TYPE role AS ENUM ('admin', 'user', 'guest');
         CREATE TABLE t (id INT NOT NULL, roles role[] NOT NULL);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let roles = table.columns.iter().find(|c| c.name == "roles").unwrap();
    assert_ne!(roles.type_oid, 0);

    let type_entry = snap.get_type(roles.type_oid).unwrap();
    assert!(
        matches!(type_entry.kind, TypeKind::Array { .. }),
        "role[] should be an Array type, got {:?}",
        type_entry.kind
    );
}

// ── Duplicates ─────────────────────────────────────────────────────────────

#[test]
fn create_enum_duplicate_errors() {
    let result = try_apply(&[
        ("0001.sql", "CREATE TYPE mood AS ENUM ('happy', 'sad');"),
        ("0002.sql", "CREATE TYPE mood AS ENUM ('angry');"),
    ]);

    assert_ddl_err!(result, DdlError::DuplicateObject(_), "already exists");
}

#[test]
fn create_composite_duplicate_errors() {
    let result = try_apply(&[(
        "0001.sql",
        "CREATE TYPE point2d AS (x float8, y float8);
         CREATE TYPE point2d AS (a int, b int);",
    )]);

    assert_ddl_err!(result, DdlError::DuplicateObject(_), "already exists");
}

#[test]
fn create_range_duplicate_errors() {
    let result = try_apply(&[(
        "0001.sql",
        "CREATE TYPE floatrange AS RANGE (subtype = float8);
         CREATE TYPE floatrange AS RANGE (subtype = float8);",
    )]);

    assert_ddl_err!(result, DdlError::DuplicateObject(_), "already exists");
}

// ── ALTER TYPE ADD VALUE edge cases ────────────────────────────────────────

#[test]
fn alter_enum_add_duplicate_value_errors() {
    // PG: without IF NOT EXISTS on the VALUE, adding an existing label fails.
    let result = try_apply(&[
        ("0001.sql", "CREATE TYPE mood AS ENUM ('happy', 'sad');"),
        ("0002.sql", "ALTER TYPE mood ADD VALUE 'happy';"),
    ]);

    assert_ddl_err!(result, DdlError::DuplicateObject(_), "already exists");
}

#[test]
fn alter_enum_add_value_if_not_exists_on_missing_type_errors() {
    // IF NOT EXISTS modifies only the VALUE clause — not the TYPE clause.
    // A missing type must still surface as an error.
    let result = try_apply(&[(
        "0001.sql",
        "ALTER TYPE nonexistent ADD VALUE IF NOT EXISTS 'x';",
    )]);

    assert_ddl_err!(result, DdlError::TypeNotFound(_), "nonexistent");
}
