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
        // PG's coerce-via-I/O fallback (`find_coercion_pathway`): in assignment
        // context, any type may be coerced to a **string-category** target via
        // its I/O functions (`UPDATE t SET text_col = 780` is valid). The rule
        // is asymmetric — a string *source* to a non-string target needs an
        // *explicit* cast, so we deliberately only check the target side here.
        (CoercionContext::Assignment, _)
            if snapshot
                .get_type(target_unwrapped)
                .is_some_and(|t| t.typcategory == TypCategory::String) =>
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
    // Pseudo-types (incl. `record`, `any*`) and unknowns cast freely — they're
    // resolved structurally and PG accepts almost anything to/from them.
    let pseudo_or_unknown =
        |cat: Option<TypCategory>| matches!(cat, Some(TypCategory::Pseudo | TypCategory::Unknown));
    if pseudo_or_unknown(scat) || pseudo_or_unknown(tcat) {
        return true;
    }
    // Casting *to* a composite is allowed from `record`/another composite
    // (a pseudo source, handled above) or a string type (handled above), but
    // NOT from an arbitrary scalar — `numeric::some_composite` is a clear
    // refusal PG rejects (`cannot cast type numeric to <composite>`).
    if tcat == Some(TypCategory::Composite) {
        return scat == Some(TypCategory::Composite);
    }
    // A composite *source* to a non-composite target is rare; err toward
    // allowing (composite→record/text are already covered above).
    if scat == Some(TypCategory::Composite) {
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

/// Find the common supertype for a list of types.
///
/// Used for CASE, COALESCE, UNION, ARRAY, and VALUES column reconciliation.
///
/// Mirrors PG's `select_common_type` (parse_coerce.c): start from the first
/// concrete type and switch the running candidate to a later type only when
/// the candidate isn't its category's preferred type and the implicit cast
/// between them is *one-way* (candidate → next but not back). The
/// directionality matters: `varchar` then `text` keeps **varchar** (the casts
/// are bidirectional), while `int4` then `int8` promotes to **int8**. A
/// category mismatch — or a survivor some input can't implicitly reach —
/// yields `None`, which callers render as PG's "X and Y cannot be matched".
pub(crate) fn find_common_type(types: &[PgTypeOid], snapshot: &PgCatalog) -> Option<PgTypeOid> {
    if types.is_empty() {
        return None;
    }

    // PG's first pass: when *every* input — unknowns/NULLs included — is the
    // exact same type, keep it as-is. This is the only path that preserves a
    // domain: `COALESCE(d, d)` is `d`.
    if types[0] != oid::UNKNOWN && types.iter().all(|&t| t == types[0]) {
        return Some(types[0]);
    }

    let concrete: Vec<PgTypeOid> = types
        .iter()
        .copied()
        .filter(|&t| t != oid::UNKNOWN)
        .collect();
    if concrete.is_empty() {
        return Some(oid::TEXT);
    }

    // Any mixed input — even just a NULL alongside a single domain — goes
    // through PG's main loop, which smashes every input to its base type
    // up front (`getBaseType`): `COALESCE(d, NULL)` is the *base*, and the
    // "cannot be matched" wording reports base names. Verified on PG 18.
    let concrete: Vec<PgTypeOid> = concrete
        .iter()
        .map(|&t| snapshot.unwrap_domain(t))
        .collect();

    if concrete.iter().all(|&t| t == concrete[0]) {
        return Some(concrete[0]);
    }

    let category = |t: PgTypeOid| snapshot.get_type(t).map(|ty| ty.typcategory);
    let preferred = |t: PgTypeOid| snapshot.get_type(t).is_some_and(|ty| ty.typispreferred);

    let mut ptype = concrete[0];
    let pcategory = category(ptype)?;
    for &n in &concrete[1..] {
        if n == ptype {
            continue;
        }
        if category(n) != Some(pcategory) {
            return None;
        }
        if !preferred(ptype)
            && snapshot.has_implicit_cast(ptype, n)
            && !snapshot.has_implicit_cast(n, ptype)
        {
            ptype = n;
        }
    }

    // PG defers this check to the per-value coercion step; folding it in here
    // keeps the callers' single "cannot be matched" path for same-category
    // pairs with no implicit route (e.g. two different enum types).
    if concrete
        .iter()
        .all(|&t| t == ptype || snapshot.has_implicit_cast(t, ptype))
    {
        Some(ptype)
    } else {
        None
    }
}
