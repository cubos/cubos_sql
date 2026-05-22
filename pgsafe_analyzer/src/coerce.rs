//! Type coercion and common-type resolution.

use crate::oid::PgTypeOid;
use crate::pg_catalog::{CastContext, PgCatalog, oid};

/// Describes the level of implicit coercion allowed in a given context.
///
/// Mirrors PostgreSQL's `CoercionContext` enum in `primnodes.h`.
/// - `Implicit`: only casts registered as implicit in `pg_cast` are allowed
///   (used inside operator/function argument matching).
/// - `Assignment`: implicit **and** assignment casts are allowed
///   (used for INSERT/UPDATE target columns, WHERE, LIMIT, OFFSET —
///   matches PG's `COERCION_ASSIGNMENT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CoercionContext {
    Implicit,
    Assignment,
}

/// Check whether a cast from `source` to `target` is permitted under
/// the given coercion context, consulting the snapshot's cast catalog.
pub(crate) fn can_coerce(
    source: PgTypeOid,
    target: PgTypeOid,
    context: CoercionContext,
    snapshot: &PgCatalog,
) -> bool {
    if source == target {
        return true;
    }
    let source = snapshot.unwrap_domain(source);
    let target_unwrapped = snapshot.unwrap_domain(target);
    if source == target_unwrapped {
        return true;
    }
    let cast = snapshot
        .cast_by_pair
        .get(&(source, target_unwrapped))
        .and_then(|oid| snapshot.pg_cast.get(oid));
    match (context, cast) {
        (_, Some(c)) if matches!(c.castcontext, CastContext::Implicit) => true,
        (CoercionContext::Assignment, Some(c))
            if matches!(c.castcontext, CastContext::Assignment) =>
        {
            true
        }
        _ => false,
    }
}

/// Numeric type promotion order (lower index = less preferred).
const NUMERIC_PROMOTION: &[(PgTypeOid, u8)] = &[
    (oid::INT2, 0),
    (oid::INT4, 1),
    (oid::INT8, 2),
    (oid::NUMERIC, 3),
    (oid::FLOAT4, 4),
    (oid::FLOAT8, 5),
];

fn numeric_rank(type_oid: PgTypeOid) -> Option<u8> {
    NUMERIC_PROMOTION
        .iter()
        .find(|(oid, _)| *oid == type_oid)
        .map(|(_, rank)| *rank)
}

/// Check if a type is a string-like type.
fn is_string_type(type_oid: PgTypeOid) -> bool {
    matches!(type_oid, oid::TEXT | oid::VARCHAR | oid::BPCHAR | oid::NAME)
}

/// Find the common supertype for a list of types.
///
/// Used for CASE, COALESCE, UNION column reconciliation.
pub(crate) fn find_common_type(types: &[PgTypeOid], snapshot: &PgCatalog) -> Option<PgTypeOid> {
    if types.is_empty() {
        return None;
    }

    let concrete: Vec<PgTypeOid> = types
        .iter()
        .copied()
        .filter(|&t| t != oid::UNKNOWN)
        .collect();
    if concrete.is_empty() {
        return Some(oid::TEXT);
    }

    if concrete.iter().all(|&t| t == concrete[0]) {
        return Some(concrete[0]);
    }

    if concrete.iter().all(|t| numeric_rank(*t).is_some()) {
        return concrete.iter().max_by_key(|t| numeric_rank(**t)).copied();
    }

    if concrete.iter().all(|t| is_string_type(*t)) {
        return Some(oid::TEXT);
    }

    for &candidate in &concrete {
        if concrete
            .iter()
            .all(|&t| t == candidate || snapshot.has_implicit_cast(t, candidate))
        {
            return Some(candidate);
        }
    }

    if concrete
        .iter()
        .all(|&t| t == oid::TEXT || snapshot.has_implicit_cast(t, oid::TEXT))
    {
        return Some(oid::TEXT);
    }

    None
}
