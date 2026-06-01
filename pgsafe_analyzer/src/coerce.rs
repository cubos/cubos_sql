//! Type coercion and common-type resolution.

use crate::oid::PgTypeOid;
use crate::pg_catalog::{CastContext, PgCatalog, TypCategory, oid};

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

/// Whether an *explicit* cast (`x::T` / `CAST(x AS T)`) from `source` to
/// `target` is legal, mirroring PostgreSQL's `can_coerce_type` under
/// `COERCION_EXPLICIT`. Deliberately more permissive than [`can_coerce`]: in
/// addition to any registered `pg_cast` entry, PG allows an explicit I/O cast
/// whenever either side is a string-category type, and relabels freely between
/// domains/base, composites/record, and element-castable arrays.
///
/// Errs toward allowing: it returns `false` only for clear-cut scalar
/// refusals (e.g. `boolean → double precision`), so callers never reject a
/// cast PG would have accepted.
pub(crate) fn can_cast_explicit(
    source: PgTypeOid,
    target: PgTypeOid,
    snapshot: &PgCatalog,
) -> bool {
    if source == target {
        return true;
    }
    let s = snapshot.unwrap_domain(source);
    let t = snapshot.unwrap_domain(target);
    if s == t {
        return true;
    }
    // Untyped literals (`unknown`) coerce to anything; a pseudo `any`-style
    // target accepts anything.
    if s == oid::UNKNOWN || t == oid::UNKNOWN {
        return true;
    }
    // Any registered cast — implicit, assignment, explicit, or
    // binary-coercible — makes it legal (`cast_by_pair` is keyed by pair,
    // independent of context).
    if snapshot.cast_by_pair.contains_key(&(s, t)) {
        return true;
    }
    let scat = snapshot.get_type(s).map(|ty| ty.typcategory);
    let tcat = snapshot.get_type(t).map(|ty| ty.typcategory);
    // Explicit I/O cast: PG allows casting to or from any string-category type.
    if scat == Some(TypCategory::String) || tcat == Some(TypCategory::String) {
        return true;
    }
    // Cases whose legality is structural rather than a simple pair lookup —
    // pseudo-types, composites/`record`, and unknowns. Mis-allowing here is
    // harmless (PG accepts most); we only aim to catch clear scalar refusals.
    let structural = |cat: Option<TypCategory>| {
        matches!(
            cat,
            Some(TypCategory::Pseudo) | Some(TypCategory::Composite) | Some(TypCategory::Unknown)
        )
    };
    if structural(scat) || structural(tcat) {
        return true;
    }
    // Array → array: legal when the element types are themselves castable.
    if scat == Some(TypCategory::Array) && tcat == Some(TypCategory::Array) {
        return match (
            snapshot.get_type(s).and_then(|ty| ty.typelem),
            snapshot.get_type(t).and_then(|ty| ty.typelem),
        ) {
            (Some(se), Some(te)) => can_cast_explicit(se, te, snapshot),
            _ => true,
        };
    }
    false
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
