//! PostgreSQL polymorphic pseudo-type handling (`anyelement`,
//! `anyarray`, `anycompatible`, …): matching actual args against a
//! pseudo-type, binding the concrete types implied by a call, and
//! substituting them back into a result type.
//!
//! Shared by both function resolution ([`crate::functions`]) and operator
//! resolution ([`crate::lookup`]) — PG applies the same rules to both.

use crate::oid::PgTypeOid;
use crate::pg_catalog::{PgCatalog, TypCategory, TypType, oid};

// Polymorphic pseudo-type OIDs (stable across PG versions).
pub(crate) const ANYELEMENT: PgTypeOid = PgTypeOid::from_raw(2283);
pub(crate) const ANYARRAY: PgTypeOid = PgTypeOid::from_raw(2277);
pub(crate) const ANYNONARRAY: PgTypeOid = PgTypeOid::from_raw(2776);
pub(crate) const ANYENUM: PgTypeOid = PgTypeOid::from_raw(3500);
pub(crate) const ANYRANGE: PgTypeOid = PgTypeOid::from_raw(3831);
pub(crate) const ANYMULTIRANGE: PgTypeOid = PgTypeOid::from_raw(4537);
pub(crate) const ANYCOMPATIBLE: PgTypeOid = PgTypeOid::from_raw(5077);
pub(crate) const ANYCOMPATIBLEARRAY: PgTypeOid = PgTypeOid::from_raw(5078);
pub(crate) const ANYCOMPATIBLENONARRAY: PgTypeOid = PgTypeOid::from_raw(5079);
pub(crate) const ANYCOMPATIBLERANGE: PgTypeOid = PgTypeOid::from_raw(5080);
pub(crate) const ANYCOMPATIBLEMULTIRANGE: PgTypeOid = PgTypeOid::from_raw(4538);

/// Is `expected` a polymorphic pseudo-type that PG would accept `actual` for?
pub(crate) fn matches_polymorphic(
    expected: PgTypeOid,
    actual: PgTypeOid,
    snapshot: &PgCatalog,
) -> bool {
    const INT2VECTOR: PgTypeOid = PgTypeOid::from_raw(22);
    const OIDVECTOR: PgTypeOid = PgTypeOid::from_raw(30);

    let actual_is_array = matches!(
        snapshot.get_type(actual).map(|t| t.typcategory),
        Some(TypCategory::Array)
    ) || actual == INT2VECTOR
        || actual == OIDVECTOR;

    match expected {
        ANYELEMENT | ANYCOMPATIBLE => true,
        ANYARRAY | ANYCOMPATIBLEARRAY => actual_is_array,
        ANYNONARRAY | ANYCOMPATIBLENONARRAY => !actual_is_array,
        ANYENUM => matches!(
            snapshot.get_type(actual).map(|t| t.typtype),
            Some(TypType::Enum)
        ),
        ANYRANGE | ANYMULTIRANGE | ANYCOMPATIBLERANGE | ANYCOMPATIBLEMULTIRANGE => matches!(
            snapshot.get_type(actual).map(|t| t.typtype),
            Some(TypType::Range)
        ),
        _ => false,
    }
}

pub(crate) fn is_polymorphic(oid: PgTypeOid) -> bool {
    matches!(
        oid,
        ANYELEMENT
            | ANYARRAY
            | ANYNONARRAY
            | ANYENUM
            | ANYRANGE
            | ANYMULTIRANGE
            | ANYCOMPATIBLE
            | ANYCOMPATIBLEARRAY
            | ANYCOMPATIBLENONARRAY
            | ANYCOMPATIBLERANGE
            | ANYCOMPATIBLEMULTIRANGE
    )
}

pub(crate) fn polymorphic_specificity(oid: PgTypeOid) -> u8 {
    match oid {
        ANYELEMENT | ANYCOMPATIBLE => 1,
        ANYARRAY | ANYNONARRAY | ANYCOMPATIBLEARRAY | ANYCOMPATIBLENONARRAY => 2,
        ANYENUM | ANYRANGE | ANYMULTIRANGE | ANYCOMPATIBLERANGE | ANYCOMPATIBLEMULTIRANGE => 3,
        _ => 10,
    }
}

pub(crate) fn bind_polymorphic_from(
    expected: PgTypeOid,
    actual: PgTypeOid,
    snapshot: &PgCatalog,
    bound_element: &mut Option<PgTypeOid>,
    bound_array: &mut Option<PgTypeOid>,
) {
    // PG (`enforce_generic_type_consistency`) skips UNKNOWN actuals when
    // unifying polymorphic args — the unknown is coerced to the resolved
    // polymorphic type *after* binding, so letting it land as the bound
    // type would freeze the resolution at UNKNOWN and propagate to the
    // other side (e.g. `'admin'::unknown = role::user_role` would resolve
    // both anyenum slots to UNKNOWN instead of `user_role`).
    if actual == oid::UNKNOWN {
        return;
    }
    match expected {
        ANYELEMENT | ANYNONARRAY | ANYENUM | ANYCOMPATIBLE | ANYCOMPATIBLENONARRAY => {
            bound_element.get_or_insert(actual);
        }
        ANYARRAY | ANYCOMPATIBLEARRAY => {
            bound_array.get_or_insert(actual);
            if let Some(t) = snapshot.get_type(actual)
                && t.typcategory == TypCategory::Array
                && let Some(elem) = t.typelem
            {
                bound_element.get_or_insert(elem);
            }
        }
        ANYRANGE | ANYMULTIRANGE | ANYCOMPATIBLERANGE | ANYCOMPATIBLEMULTIRANGE => {
            bound_array.get_or_insert(actual);
        }
        _ => {}
    }
}

pub(crate) fn substitute_polymorphic(
    oid: PgTypeOid,
    bound_element: Option<PgTypeOid>,
    bound_array: Option<PgTypeOid>,
    snapshot: &PgCatalog,
) -> PgTypeOid {
    match oid {
        ANYELEMENT | ANYNONARRAY | ANYENUM | ANYCOMPATIBLE | ANYCOMPATIBLENONARRAY => {
            bound_element.unwrap_or(oid)
        }
        ANYARRAY | ANYCOMPATIBLEARRAY => bound_array
            .or_else(|| bound_element.and_then(|e| snapshot.array_type_of(e)))
            .unwrap_or(oid),
        ANYRANGE | ANYMULTIRANGE | ANYCOMPATIBLERANGE | ANYCOMPATIBLEMULTIRANGE => {
            bound_array.unwrap_or(oid)
        }
        _ => oid,
    }
}
