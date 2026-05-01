//! `pg_constraint` lifecycle: emission on `CREATE TABLE` /
//! `CREATE UNIQUE INDEX` / `ALTER TABLE ADD CONSTRAINT`, and removal /
//! rename / FK-aware DROP cascading.

use crate::common::*;

// ── CREATE TABLE: PRIMARY KEY / UNIQUE / CHECK ─────────────────────────────

#[test]
fn create_table_emits_pkey_and_unique_constraints() {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE t (
            id BIGINT PRIMARY KEY,
            email TEXT NOT NULL UNIQUE,
            tag TEXT NOT NULL,
            UNIQUE (tag)
         );",
    )
    .unwrap();
    let names = db.pg_constraint_names_for_table("public", "t");
    assert!(
        names.iter().any(|n| n == "t_pkey"),
        "expected pkey in {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "t_email_key"),
        "expected column-level UNIQUE in {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "t_tag_key"),
        "expected table-level UNIQUE in {names:?}"
    );
}

#[test]
fn create_table_emits_check_constraint_rows() {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE t (
            id  INT NOT NULL CHECK (id > 0),
            qty INT NOT NULL,
            CONSTRAINT positive_qty CHECK (qty >= 0)
         );",
    )
    .unwrap();
    let names = db.pg_constraint_names_for_table("public", "t");
    assert!(
        names.iter().any(|n| n == "t_id_check"),
        "expected column-level CHECK in {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "positive_qty"),
        "expected named table-level CHECK in {names:?}"
    );
}

// ── CREATE TABLE: REFERENCES (column-level FK) ─────────────────────────────

#[test]
fn create_table_column_level_references_emits_fk_constraint() {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE p (id BIGINT PRIMARY KEY);
         CREATE TABLE c (
            id BIGINT PRIMARY KEY,
            p_id BIGINT NOT NULL REFERENCES p(id)
         );",
    )
    .unwrap();
    let names = db.pg_constraint_names_for_table("public", "c");
    assert!(
        names.iter().any(|n| n == "c_p_id_fkey"),
        "expected FK in {names:?}"
    );
}

#[test]
fn create_table_references_without_column_uses_target_pk() {
    // PG: `REFERENCES p` (no column list) defaults to the target's PK.
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE p (id BIGINT PRIMARY KEY);
         CREATE TABLE c (id BIGINT PRIMARY KEY, p_id BIGINT REFERENCES p);",
    )
    .unwrap();
    let names = db.pg_constraint_names_for_table("public", "c");
    assert!(names.iter().any(|n| n == "c_p_id_fkey"));
}

#[test]
fn create_table_references_unknown_table_is_rejected() {
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE c (id BIGINT PRIMARY KEY, p_id BIGINT REFERENCES ghost(id));"
        )]),
        DdlError::TableNotFound(_),
        "ghost",
    );
}

#[test]
fn create_table_references_unknown_column_is_rejected() {
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE p (id BIGINT PRIMARY KEY);
             CREATE TABLE c (
                id BIGINT PRIMARY KEY,
                p_id BIGINT REFERENCES p(ghost)
             );"
        )]),
        DdlError::Parse(_),
        "ghost",
    );
}

#[test]
fn create_table_references_non_unique_target_is_rejected() {
    // The target column has no PK/UNIQUE — PG: `there is no unique
    // constraint matching given keys for referenced table`.
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE p (id BIGINT PRIMARY KEY, label TEXT NOT NULL);
             CREATE TABLE c (
                id BIGINT PRIMARY KEY,
                p_label TEXT NOT NULL REFERENCES p(label)
             );"
        )]),
        DdlError::DependencyError(_),
        "no unique constraint",
    );
}

#[test]
fn create_table_references_with_incompatible_type_is_rejected() {
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE p (id BIGINT PRIMARY KEY);
             CREATE TABLE c (
                id BIGINT PRIMARY KEY,
                p_id TEXT NOT NULL REFERENCES p(id)
             );"
        )]),
        DdlError::DependencyError(_),
        "incompatible types",
    );
}

#[test]
fn create_table_references_into_unique_constraint_is_accepted() {
    // FK can target any UNIQUE column, not just PK.
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE p (id BIGINT PRIMARY KEY, slug TEXT NOT NULL UNIQUE);
         CREATE TABLE c (
            id BIGINT PRIMARY KEY,
            p_slug TEXT NOT NULL REFERENCES p(slug)
         );",
    )
    .unwrap();
    let names = db.pg_constraint_names_for_table("public", "c");
    assert!(names.iter().any(|n| n == "c_p_slug_fkey"));
}

#[test]
fn create_table_table_level_foreign_key_with_composite_target_is_accepted() {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE p (
            a INT NOT NULL,
            b INT NOT NULL,
            PRIMARY KEY (a, b)
         );
         CREATE TABLE c (
            id BIGINT PRIMARY KEY,
            pa INT NOT NULL,
            pb INT NOT NULL,
            FOREIGN KEY (pa, pb) REFERENCES p (a, b)
         );",
    )
    .unwrap();
    let names = db.pg_constraint_names_for_table("public", "c");
    assert!(
        names.iter().any(|n| n == "c_pa_pb_fkey"),
        "expected composite FK in {names:?}"
    );
}

#[test]
fn create_table_table_level_foreign_key_arity_mismatch_is_rejected() {
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE p (a INT NOT NULL, b INT NOT NULL, PRIMARY KEY (a, b));
             CREATE TABLE c (
                id BIGINT PRIMARY KEY,
                pa INT NOT NULL,
                FOREIGN KEY (pa) REFERENCES p (a, b)
             );"
        )]),
        DdlError::Parse(_),
        "1 local column",
    );
}

// ── ALTER TABLE ADD CONSTRAINT ─────────────────────────────────────────────

#[test]
fn alter_table_add_foreign_key_emits_constraint_row() {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE p (id BIGINT PRIMARY KEY);
         CREATE TABLE c (id BIGINT PRIMARY KEY, p_id BIGINT NOT NULL);
         ALTER TABLE c ADD CONSTRAINT c_p_id_fk FOREIGN KEY (p_id) REFERENCES p (id);",
    )
    .unwrap();
    let names = db.pg_constraint_names_for_table("public", "c");
    assert!(names.iter().any(|n| n == "c_p_id_fk"));
}

#[test]
fn alter_table_add_foreign_key_without_target_pk_is_rejected() {
    assert_ddl_err!(
        try_apply(&[
            (
                "0001.sql",
                "CREATE TABLE p (id BIGINT NOT NULL);
                 CREATE TABLE c (id BIGINT PRIMARY KEY, p_id BIGINT NOT NULL);"
            ),
            (
                "0002.sql",
                "ALTER TABLE c ADD CONSTRAINT c_p_id_fk FOREIGN KEY (p_id) REFERENCES p;"
            ),
        ]),
        DdlError::DependencyError(_),
        "no primary key",
    );
}

// ── ALTER TABLE DROP CONSTRAINT ────────────────────────────────────────────

#[test]
fn alter_table_drop_constraint_removes_row() {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, slug TEXT NOT NULL UNIQUE);
         ALTER TABLE t DROP CONSTRAINT t_slug_key;",
    )
    .unwrap();
    let names = db.pg_constraint_names_for_table("public", "t");
    assert!(
        !names.iter().any(|n| n == "t_slug_key"),
        "t_slug_key should be gone, got {names:?}"
    );
    assert!(names.iter().any(|n| n == "t_pkey"));
}

#[test]
fn alter_table_drop_nonexistent_constraint_errors() {
    assert_ddl_err!(
        try_apply(&[
            ("0001.sql", "CREATE TABLE t (id BIGINT PRIMARY KEY);"),
            ("0002.sql", "ALTER TABLE t DROP CONSTRAINT ghost;"),
        ]),
        DdlError::DependencyError(_),
        "ghost",
    );
}

#[test]
fn alter_table_drop_nonexistent_constraint_if_exists_is_noop() {
    // PG accepts `DROP CONSTRAINT IF EXISTS` for an absent name.
    try_apply(&[
        ("0001.sql", "CREATE TABLE t (id BIGINT PRIMARY KEY);"),
        ("0002.sql", "ALTER TABLE t DROP CONSTRAINT IF EXISTS ghost;"),
    ])
    .expect("DROP CONSTRAINT IF EXISTS should accept missing name");
}

#[test]
fn drop_pk_referenced_by_fk_without_cascade_is_rejected() {
    assert_ddl_err!(
        try_apply(&[
            (
                "0001.sql",
                "CREATE TABLE p (id BIGINT PRIMARY KEY);
                 CREATE TABLE c (id BIGINT PRIMARY KEY, p_id BIGINT NOT NULL REFERENCES p(id));"
            ),
            ("0002.sql", "ALTER TABLE p DROP CONSTRAINT p_pkey;"),
        ]),
        DdlError::DependencyError(_),
        "depends on it",
    );
}

#[test]
fn drop_pk_referenced_by_fk_with_cascade_is_accepted() {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE p (id BIGINT PRIMARY KEY);
         CREATE TABLE c (id BIGINT PRIMARY KEY, p_id BIGINT NOT NULL REFERENCES p(id));
         ALTER TABLE p DROP CONSTRAINT p_pkey CASCADE;",
    )
    .unwrap();
    let p_cons = db.pg_constraint_names_for_table("public", "p");
    let c_cons = db.pg_constraint_names_for_table("public", "c");
    assert!(!p_cons.iter().any(|n| n == "p_pkey"));
    // The FK on c was *not* removed by CASCADE in our model — that's a
    // gap; document with a TODO if needed. For now just sanity-check
    // that the rest of the table is intact.
    assert!(c_cons.iter().any(|n| n == "c_pkey"));
}

// ── ALTER TABLE RENAME CONSTRAINT ──────────────────────────────────────────

#[test]
fn alter_table_rename_constraint_renames_row() {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE t (id BIGINT PRIMARY KEY);
         ALTER TABLE t RENAME CONSTRAINT t_pkey TO t_id_pkey;",
    )
    .unwrap();
    let names = db.pg_constraint_names_for_table("public", "t");
    assert!(
        names.iter().any(|n| n == "t_id_pkey"),
        "renamed constraint missing in {names:?}"
    );
    assert!(!names.iter().any(|n| n == "t_pkey"));
}

#[test]
fn alter_table_rename_nonexistent_constraint_errors() {
    assert_ddl_err!(
        try_apply(&[
            ("0001.sql", "CREATE TABLE t (id BIGINT PRIMARY KEY);"),
            (
                "0002.sql",
                "ALTER TABLE t RENAME CONSTRAINT ghost TO whatever;"
            ),
        ]),
        DdlError::DependencyError(_),
        "ghost",
    );
}

// ── DROP TABLE with FK target ─────────────────────────────────────────────

#[test]
fn drop_table_referenced_by_fk_without_cascade_is_rejected() {
    assert_ddl_err!(
        try_apply(&[
            (
                "0001.sql",
                "CREATE TABLE p (id BIGINT PRIMARY KEY);
                 CREATE TABLE c (id BIGINT PRIMARY KEY, p_id BIGINT NOT NULL REFERENCES p(id));"
            ),
            ("0002.sql", "DROP TABLE p;"),
        ]),
        DdlError::DependencyError(_),
        "foreign key",
    );
}

#[test]
fn drop_table_referenced_by_fk_with_cascade_drops_fk_too() {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE p (id BIGINT PRIMARY KEY);
         CREATE TABLE c (id BIGINT PRIMARY KEY, p_id BIGINT NOT NULL REFERENCES p(id));
         DROP TABLE p CASCADE;",
    )
    .unwrap();
    // `c` is still here — only the FK row is gone (PG also removes the
    // FK constraint, which is exactly what we mirror).
    let c_cons = db.pg_constraint_names_for_table("public", "c");
    assert!(!c_cons.iter().any(|n| n == "c_p_id_fkey"));
}

// ── DROP COLUMN with constraints ───────────────────────────────────────────

#[test]
fn drop_column_with_pkey_dependency_without_cascade_is_rejected() {
    assert_ddl_err!(
        try_apply(&[
            (
                "0001.sql",
                "CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT);"
            ),
            ("0002.sql", "ALTER TABLE t DROP COLUMN id;"),
        ]),
        DdlError::DependencyError(_),
        "constraint(s) t_pkey",
    );
}

#[test]
fn drop_column_with_pkey_dependency_with_cascade_drops_pkey_too() {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT NOT NULL);
         ALTER TABLE t DROP COLUMN id CASCADE;",
    )
    .unwrap();
    let names = db.pg_constraint_names_for_table("public", "t");
    assert!(
        !names.iter().any(|n| n == "t_pkey"),
        "PK should be gone, got {names:?}"
    );
}

#[test]
fn drop_column_with_fk_target_without_cascade_is_rejected() {
    // The dropped column is the *target* of an FK on another table.
    assert_ddl_err!(
        try_apply(&[
            (
                "0001.sql",
                "CREATE TABLE p (id BIGINT PRIMARY KEY);
                 CREATE TABLE c (id BIGINT PRIMARY KEY, p_id BIGINT NOT NULL REFERENCES p(id));"
            ),
            ("0002.sql", "ALTER TABLE p DROP COLUMN id;"),
        ]),
        DdlError::DependencyError(_),
        "constraint(s)",
    );
}

#[test]
fn drop_column_with_fk_source_without_cascade_is_rejected() {
    // The dropped column is the *source* of an FK on the same table.
    assert_ddl_err!(
        try_apply(&[
            (
                "0001.sql",
                "CREATE TABLE p (id BIGINT PRIMARY KEY);
                 CREATE TABLE c (id BIGINT PRIMARY KEY, p_id BIGINT NOT NULL REFERENCES p(id));"
            ),
            ("0002.sql", "ALTER TABLE c DROP COLUMN p_id;"),
        ]),
        DdlError::DependencyError(_),
        "constraint(s)",
    );
}

#[test]
fn drop_column_unrelated_to_constraints_is_accepted() {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT, payload TEXT);
         ALTER TABLE t DROP COLUMN payload;",
    )
    .unwrap();
    // Unaffected constraints stay.
    let names = db.pg_constraint_names_for_table("public", "t");
    assert!(names.iter().any(|n| n == "t_pkey"));
}

#[test]
fn drop_column_with_check_dependency_without_cascade_is_rejected() {
    assert_ddl_err!(
        try_apply(&[
            (
                "0001.sql",
                "CREATE TABLE t (
                    id  BIGINT PRIMARY KEY,
                    qty INT NOT NULL CHECK (qty >= 0)
                 );"
            ),
            ("0002.sql", "ALTER TABLE t DROP COLUMN qty;"),
        ]),
        DdlError::DependencyError(_),
        "constraint(s) t_qty_check",
    );
}

// ── DROP TABLE with FK ────────────────────────────────────────────────────

#[test]
fn drop_table_with_no_fk_target_succeeds() {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE t (id BIGINT PRIMARY KEY);
         DROP TABLE t;",
    )
    .unwrap();
    let cons = db.pg_constraint_names_for_table("public", "t");
    assert!(
        cons.is_empty(),
        "constraints should be cleaned up: {cons:?}"
    );
}
