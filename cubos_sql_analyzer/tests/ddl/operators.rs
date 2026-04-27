//! CREATE / DROP OPERATOR: operator signatures, schema-qualified drops,
//! user-defined operators.

use crate::common::*;

// ── DROP OPERATOR ───────────────────────────────────────────────────────────

#[test]
fn drop_operator_removes_only_matching_signature() {
    // pgvector registers multiple `<=>` overloads (vector/halfvec/sparsevec).
    // Dropping the (vector, vector) overload must leave the others alone.
    //
    // We inspect the raw registry directly because pgvector also defines
    // implicit casts between vector, halfvec and sparsevec — so
    // `find_operator` would still succeed via cast-based resolution after
    // the exact (vector, vector) entry is removed.
    let snap = build(&[(
        "0001.sql",
        "CREATE EXTENSION vector;
         DROP OPERATOR <=> (vector, vector);",
    )]);

    let vector_oid = snap.resolve_type_by_name(None, "vector").unwrap().oid;
    let halfvec_oid = snap.resolve_type_by_name(None, "halfvec").unwrap().oid;

    let ops = snap
        .operators_by_name()
        .get(&QualifiedName::new("public", "<=>"))
        .expect("other overloads should still be registered");

    assert!(
        !ops.iter()
            .any(|o| o.left_type_oid == Some(vector_oid) && o.right_type_oid == vector_oid),
        "(vector, vector) overload should have been dropped"
    );
    assert!(
        ops.iter()
            .any(|o| o.left_type_oid == Some(halfvec_oid) && o.right_type_oid == halfvec_oid),
        "(halfvec, halfvec) overload should still be registered"
    );
}

#[test]
fn drop_operator_if_exists_no_error() {
    let _snap = build(&[("0001.sql", "DROP OPERATOR IF EXISTS <=> (int4, int4);")]);
}

#[test]
fn drop_operator_missing_errors_without_if_exists() {
    let result = try_apply(&[("0001.sql", "DROP OPERATOR <=> (int4, int4);")]);
    assert_ddl_err!(result, DdlError::DependencyError(_), "operator <=>");
}

// ── ALTER OPERATOR ──────────────────────────────────────────────────────────

#[test]
fn alter_operator_is_noop_but_does_not_crash() {
    // ALTER OPERATOR currently only changes attributes (join selectivity,
    // restriction selectivity). None of those affect type analysis, so this
    // must be a successful no-op.
    let _snap = build(&[(
        "0001.sql",
        "CREATE EXTENSION vector;
         ALTER OPERATOR <=> (vector, vector) SET (RESTRICT = scalarlesel);",
    )]);
}
