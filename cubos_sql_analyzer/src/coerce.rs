//! Type coercion and common-type resolution.

use crate::schema::{CastContext, CastInfo, SchemaSnapshot, oid};

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
    source: u32,
    target: u32,
    context: CoercionContext,
    snapshot: &SchemaSnapshot,
) -> bool {
    if source == target {
        return true;
    }
    // Unwrap domains before checking.
    let source = snapshot.unwrap_domain(source);
    let target_unwrapped = snapshot.unwrap_domain(target);
    if source == target_unwrapped {
        return true;
    }
    let key = format!("{source}:{target_unwrapped}");
    matches!(
        (context, snapshot.casts.get(&key)),
        (
            _,
            Some(CastInfo {
                context: CastContext::Implicit,
                ..
            })
        ) | (
            CoercionContext::Assignment,
            Some(CastInfo {
                context: CastContext::Assignment,
                ..
            })
        )
    )
}

/// Numeric type promotion order (lower index = less preferred).
const NUMERIC_PROMOTION: &[(u32, u8)] = &[
    (oid::INT2, 0),
    (oid::INT4, 1),
    (oid::INT8, 2),
    (oid::NUMERIC, 3),
    (oid::FLOAT4, 4),
    (oid::FLOAT8, 5),
];

fn numeric_rank(type_oid: u32) -> Option<u8> {
    NUMERIC_PROMOTION
        .iter()
        .find(|(oid, _)| *oid == type_oid)
        .map(|(_, rank)| *rank)
}

/// Check if a type is a string-like type.
fn is_string_type(type_oid: u32) -> bool {
    matches!(type_oid, oid::TEXT | oid::VARCHAR | oid::BPCHAR | oid::NAME)
}

/// Find the common supertype for a list of types.
///
/// Used for CASE, COALESCE, UNION column reconciliation.
pub(crate) fn find_common_type(types: &[u32], snapshot: &SchemaSnapshot) -> Option<u32> {
    if types.is_empty() {
        return None;
    }

    // Filter out UNKNOWN (untyped literals).
    let concrete: Vec<u32> = types
        .iter()
        .copied()
        .filter(|&t| t != oid::UNKNOWN)
        .collect();
    if concrete.is_empty() {
        return Some(oid::TEXT); // All unknown → text
    }

    // If all the same, return that.
    if concrete.iter().all(|&t| t == concrete[0]) {
        return Some(concrete[0]);
    }

    // Try numeric promotion.
    if concrete.iter().all(|t| numeric_rank(*t).is_some()) {
        return concrete.iter().max_by_key(|t| numeric_rank(**t)).copied();
    }

    // Try string types → text.
    if concrete.iter().all(|t| is_string_type(*t)) {
        return Some(oid::TEXT);
    }

    // Try implicit casts: find a type that all others can cast to.
    for &candidate in &concrete {
        if concrete
            .iter()
            .all(|&t| t == candidate || snapshot.has_implicit_cast(t, candidate))
        {
            return Some(candidate);
        }
    }

    // Fallback: try text (many types can cast to text).
    if concrete
        .iter()
        .all(|&t| t == oid::TEXT || snapshot.has_implicit_cast(t, oid::TEXT))
    {
        return Some(oid::TEXT);
    }

    None
}
