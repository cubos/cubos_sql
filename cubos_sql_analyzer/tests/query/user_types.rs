//! Querying against user-defined types: enums, domains, composite types,
//! ranges, arrays as column types.
//!
//! Also: `alias.*` used as a composite value (e.g. fed to `row_to_json`),
//! which relies on the table's implicit composite type.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE users (
            id   BIGINT PRIMARY KEY,
            name TEXT NOT NULL
        );",
    )
    .unwrap();
    db
}

fn setup_user_types() -> PgCatalog {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TYPE user_role AS ENUM ('admin', 'editor', 'viewer');
         CREATE DOMAIN user_prefs AS JSONB;
         CREATE SCHEMA whatsapp;
         CREATE DOMAIN whatsapp.health_data AS JSONB;
         CREATE TABLE users (
            id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            name        TEXT NOT NULL,
            email       TEXT NOT NULL UNIQUE,
            age         INT,
            role        user_role NOT NULL DEFAULT 'viewer',
            preferences user_prefs,
            created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
         );
         CREATE TABLE whatsapp.channels (
            channel_id BIGINT PRIMARY KEY,
            health     whatsapp.health_data,
            updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
         );",
    )
    .unwrap();
    db
}

// ── `alias.*` resolves to the table's composite type ─────────────────────────

#[test]
fn star_expr_resolves_to_composite_type_via_row_to_json() {
    let db = setup();
    // `u.*` feeds into row_to_json, which takes `record` / any composite.
    let sql = "SELECT row_to_json(u.*) AS payload FROM users u";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "payload").pg_type, json_ty());
}

#[test]
fn star_expr_not_null_because_row_is_always_present() {
    let db = setup();
    let sql = "SELECT row_to_json(u.*) AS payload FROM users u";
    let info = db.analyze(sql).unwrap();
    // `alias.*` is a composite value that exists iff the row exists — and a
    // row is always present for every returned tuple → NOT NULL.
    assert!(!col(&info, "payload").nullable);
}

#[test]
fn star_expr_on_cte_is_unsupported() {
    let db = setup();
    // CTE rows don't have a registered composite type — analyzer errors.
    let sql = "WITH u AS (SELECT id, name FROM users) \
               SELECT row_to_json(u.*) FROM u";
    assert_analyze_err!(
        db.analyze(sql),
        AnalyzeError::Unsupported(_),
        "CTE or subquery, not a real relation",
    );
}

#[test]
fn star_expr_on_unknown_alias_fails() {
    let db = setup();
    let sql = "SELECT row_to_json(nope.*) FROM users u";
    assert_analyze_err!(
        db.analyze(sql),
        AnalyzeError::UndefinedTable(_),
        "missing FROM-clause entry for table \"nope\"",
    );
}

// ── Enum types (CREATE TYPE ... AS ENUM) ─────────────────────────────────────

#[test]
fn enum_column_select() {
    let db = setup_user_types();
    let s = db.analyze("SELECT id, name, role FROM users").unwrap();
    assert_cols(
        &s,
        vec![
            c("id", int8()),
            c("name", text()),
            c(
                "role",
                enum_ty("public", "user_role", &["admin", "editor", "viewer"]),
            ),
        ],
    );
}

#[test]
fn enum_in_where() {
    let db = setup_user_types();
    let s = db.analyze("SELECT id FROM users WHERE role = $p1").unwrap();
    // $p1 inferred as the enum type.
    assert_params(
        &s,
        vec![p(enum_ty(
            "public",
            "user_role",
            &["admin", "editor", "viewer"],
        ))],
    );
}

#[test]
fn enum_in_insert() {
    let db = setup_user_types();
    let s = db
        .analyze("INSERT INTO users (name, email, role) VALUES ($p1, $p2, $p3) RETURNING id, role")
        .unwrap();
    assert_params(
        &s,
        vec![
            p(text()),
            p(text()),
            p(enum_ty(
                "public",
                "user_role",
                &["admin", "editor", "viewer"],
            )),
        ],
    );
}

#[test]
fn enum_in_update() {
    let db = setup_user_types();
    let s = db
        .analyze("UPDATE users SET role = $p1 WHERE id = $p2 RETURNING role")
        .unwrap();
    assert_params(
        &s,
        vec![
            p(enum_ty(
                "public",
                "user_role",
                &["admin", "editor", "viewer"],
            )),
            p(int8()),
        ],
    );
}

// ── Domain types (CREATE DOMAIN) ─────────────────────────────────────────────

#[test]
fn domain_column_surfaces_as_domain_type() {
    // Analyzer surfaces the Domain wrapper with its base type preserved; the
    // macro crate decides whether to treat it as opaque JSONB or unwrap.
    let db = setup_user_types();
    let s = db.analyze("SELECT id, preferences FROM users").unwrap();
    assert_cols(
        &s,
        vec![
            c("id", int8()),
            cn("preferences", domain("public", "user_prefs", jsonb())),
        ],
    );
}

#[test]
fn domain_param_insert_surfaces_as_domain_type() {
    let db = setup_user_types();
    let s = db
        .analyze("INSERT INTO users (name, email, preferences) VALUES ($p1, $p2, $p3) RETURNING id")
        .unwrap();
    assert_params(
        &s,
        vec![
            p(text()),
            p(text()),
            pn(domain("public", "user_prefs", jsonb())),
        ],
    );
    // `Type::cast_name` unwraps the domain to its schema-qualified base name.
    assert_eq!(
        s.params[2].pg_type.cast_name().as_deref(),
        Some("pg_catalog.jsonb"),
    );
}

#[test]
fn domain_in_where() {
    let db = setup_user_types();
    let s = db
        .analyze("SELECT id FROM users WHERE preferences IS NOT NULL")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn schema_qualified_domain_column() {
    let db = setup_user_types();
    let s = db
        .analyze("SELECT channel_id, health FROM whatsapp.channels")
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("channel_id", int8()),
            cn("health", domain("whatsapp", "health_data", jsonb())),
        ],
    );
}

// ── Array column types ────────────────────────────────────────────────────

#[test]
fn text_array_column_type_resolves_to_array_kind() {
    // TEXT[] must land as an Array TypeKind in the snapshot (not OID 0).
    let mut db = PgCatalog::new();
    db.apply_sql("CREATE TABLE t (id INT NOT NULL, tags TEXT[] NOT NULL);")
        .unwrap();
    let snap = db.into_snapshot();

    let table = snap.resolve_table(None, "t").unwrap();
    let tags_col = table.columns.iter().find(|c| c.name == "tags").unwrap();
    assert_ne!(tags_col.type_oid, 0);

    let type_entry = snap.get_type(tags_col.type_oid).unwrap();
    assert!(
        matches!(type_entry.kind, TypeKind::Array { .. }),
        "TEXT[] should be an Array type, got {:?}",
        type_entry.kind
    );
}

// ── Type alias resolution ─────────────────────────────────────────────────

#[test]
fn builtin_type_aliases_resolve_to_canonical_oid() {
    // PG accepts "integer"/"int"/"bigint"/"smallint"/"boolean"/"real" as
    // aliases for int4/int4/int8/int2/bool/float4. A column declared with
    // each alias must land on the canonical OID.
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE t (
            a integer NOT NULL,
            b int NOT NULL,
            c bigint NOT NULL,
            d smallint NOT NULL,
            e boolean NOT NULL,
            f real NOT NULL,
            g text NOT NULL
        );",
    )
    .unwrap();
    let snap = db.into_snapshot();

    let table = snap.resolve_table(None, "t").unwrap();
    let int4_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int4")
        .unwrap()
        .oid;
    let int8_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int8")
        .unwrap()
        .oid;
    let int2_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int2")
        .unwrap()
        .oid;
    let bool_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "bool")
        .unwrap()
        .oid;
    let float4_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "float4")
        .unwrap()
        .oid;
    let text_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "text")
        .unwrap()
        .oid;

    assert_eq!(table.columns[0].type_oid, int4_oid, "integer -> int4");
    assert_eq!(table.columns[1].type_oid, int4_oid, "int -> int4");
    assert_eq!(table.columns[2].type_oid, int8_oid, "bigint -> int8");
    assert_eq!(table.columns[3].type_oid, int2_oid, "smallint -> int2");
    assert_eq!(table.columns[4].type_oid, bool_oid, "boolean -> bool");
    assert_eq!(table.columns[5].type_oid, float4_oid, "real -> float4");
    assert_eq!(table.columns[6].type_oid, text_oid, "text -> text");
}

// ── Generated columns (GENERATED ALWAYS AS (expr) STORED) ──────────────────

fn setup_generated() -> PgCatalog {
    let mut db = PgCatalog::new();
    db.apply_sql(
        "CREATE TABLE invoices (
            id        BIGINT PRIMARY KEY,
            net       NUMERIC(12,2) NOT NULL,
            tax_rate  NUMERIC(4,3)  NOT NULL,
            gross     NUMERIC(12,2) GENERATED ALWAYS AS (net * (1 + tax_rate)) STORED
         );",
    )
    .unwrap();
    db
}

#[test]
fn generated_column_select_uses_declared_type() {
    let db = setup_generated();
    // The declared type wins over the expression type.
    let s = db.analyze("SELECT gross FROM invoices").unwrap();
    assert_cols(&s, vec![cn("gross", numeric())]);
}

#[test]
fn insert_into_generated_column_rejected() {
    let db = setup_generated();
    // PG: `cannot insert a non-DEFAULT value into column "gross"`. The
    // analyzer should reject this statically — the column is generated,
    // and accepting a $p1 value here would mask a real bug.
    assert_analyze_err!(
        db.analyze(
            "INSERT INTO invoices (id, net, tax_rate, gross) \
             VALUES ($p1, $p2, $p3, $p4)",
        ),
        AnalyzeError::Invalid(_),
        "generated",
    );
}

#[test]
fn update_generated_column_rejected() {
    let db = setup_generated();
    // PG: `column "gross" can only be updated to DEFAULT`.
    assert_analyze_err!(
        db.analyze("UPDATE invoices SET gross = $p1 WHERE id = $p2"),
        AnalyzeError::Invalid(_),
        "generated",
    );
}

#[test]
fn insert_into_generated_column_with_literal_rejected() {
    let db = setup_generated();
    // Even a NUMERIC literal cannot be assigned to a generated column.
    assert_analyze_err!(
        db.analyze(
            "INSERT INTO invoices (id, net, tax_rate, gross) \
             VALUES ($p1, $p2, $p3, 42.0)",
        ),
        AnalyzeError::Invalid(_),
        "generated",
    );
}

#[test]
fn update_generated_column_to_default_accepted() {
    let db = setup_generated();
    // PG accepts `UPDATE … SET gen_col = DEFAULT` (resets the computed value).
    let s = db
        .analyze("UPDATE invoices SET gross = DEFAULT WHERE id = $p1 RETURNING gross")
        .unwrap();
    assert_cols(&s, vec![cn("gross", numeric())]);
}

#[test]
fn insert_into_generated_column_with_default_keyword_accepted() {
    let db = setup_generated();
    // `DEFAULT` is the only value PG accepts for a generated column. The
    // analyzer must not reject this case.
    let s = db
        .analyze(
            "INSERT INTO invoices (id, net, tax_rate, gross) \
             VALUES ($p1, $p2, $p3, DEFAULT) RETURNING gross",
        )
        .unwrap();
    assert_cols(&s, vec![cn("gross", numeric())]);
}

#[test]
fn insert_skipping_generated_column_accepted() {
    let db = setup_generated();
    // The standard pattern: just leave the generated column out of the
    // column list. PG fills it in itself.
    let s = db
        .analyze(
            "INSERT INTO invoices (id, net, tax_rate) \
             VALUES ($p1, $p2, $p3) RETURNING gross",
        )
        .unwrap();
    assert_cols(&s, vec![cn("gross", numeric())]);
}

#[test]
fn schema_qualified_domain_param() {
    let db = setup_user_types();
    let s = db
        .analyze(
            "INSERT INTO whatsapp.channels (channel_id, health, updated_at) \
             VALUES ($p1, $p2, now())",
        )
        .unwrap();
    assert_params(
        &s,
        vec![p(int8()), pn(domain("whatsapp", "health_data", jsonb()))],
    );
    assert_eq!(
        s.params[1].pg_type.cast_name().as_deref(),
        Some("pg_catalog.jsonb"),
    );
}
