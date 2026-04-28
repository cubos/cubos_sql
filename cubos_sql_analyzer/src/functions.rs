//! Function and aggregate resolution.

use crate::error::AnalyzeError;
use crate::oid::PgTypeOid;
use crate::pg_catalog::{ArgMode, PgCatalog, PgProc, ProKind, TypCategory, TypType, oid};

/// One output column of a SRF / OUT-arg function. Mirrors the named-field
/// shape that the analyzer needs from `pg_proc`'s `proallargtypes` /
/// `proargmodes` / `proargnames` triple.
#[derive(Debug, Clone)]
pub(crate) struct OutArg {
    pub name: String,
    pub type_oid: PgTypeOid,
    pub not_null: bool,
}

/// Resolved function call result.
pub(crate) struct ResolvedFunction {
    pub return_type_oid: PgTypeOid,
    /// The resolved argument types from the matched function signature.
    pub arg_types: Vec<PgTypeOid>,
    pub schema: String,
    pub is_aggregate: bool,
    pub is_strict: bool,
    /// Named output columns for SRFs / OUT-arg functions, derived from the
    /// matched `pg_proc`'s `proallargtypes`/`proargmodes`/`proargnames`.
    /// Empty for plain scalar returns.
    pub out_args: Vec<OutArg>,
}

/// pg_catalog strict functions that can still return NULL with non-null inputs.
const NULLABLE_STRICT_PG_CATALOG_FUNCTIONS: &[&str] = &[
    "array_position",
    "array_upper",
    "array_lower",
    "array_length",
    "regexp_match",
    "regexp_matches",
    "substring",
    "json_object_field",
    "json_object_field_text",
    "json_array_element",
    "json_array_element_text",
    "jsonb_object_field",
    "jsonb_object_field_text",
    "jsonb_array_element",
    "jsonb_array_element_text",
    "jsonb_path_query_first",
    "jsonb_path_match",
    "jsonb_extract_path",
    "jsonb_extract_path_text",
    "json_extract_path",
    "json_extract_path_text",
    "nullif",
    "obj_description",
    "col_description",
    "shobj_description",
];

/// pg_catalog operators that can return NULL with non-null inputs.
const NULLABLE_PG_CATALOG_OPERATORS: &[&str] = &["->", "->>", "#>", "#>>"];

/// Resolve a function call by name and argument types.
pub(crate) fn resolve_function(
    snapshot: &PgCatalog,
    schema: Option<&str>,
    name: &str,
    arg_types: &[PgTypeOid],
    _is_agg_star: bool,
) -> Result<ResolvedFunction, AnalyzeError> {
    let candidates: Vec<&PgProc> = snapshot
        .find_functions(schema, name)
        .into_iter()
        .filter(|f| !matches!(f.prokind, ProKind::Procedure))
        .collect();
    if candidates.is_empty() {
        return Err(AnalyzeError::UndefinedFunction(format!(
            "function {name}() does not exist"
        )));
    }

    if let Some(f) = find_exact_match(&candidates, arg_types) {
        return Ok(make_resolved(f, snapshot));
    }
    if let Some(f) = find_unknown_compatible_match(&candidates, arg_types) {
        return Ok(make_resolved(f, snapshot));
    }
    if let Some(f) = find_default_args_match(&candidates, arg_types, snapshot) {
        return Ok(make_resolved(f, snapshot));
    }
    if let Some(f) = find_cast_match(&candidates, arg_types, snapshot) {
        return Ok(make_resolved(f, snapshot));
    }
    if let Some(f) = find_polymorphic_match(&candidates, arg_types, snapshot) {
        return Ok(make_resolved_polymorphic(f, arg_types, snapshot));
    }

    let agg_candidates: Vec<_> = candidates
        .iter()
        .filter(|f| matches!(f.prokind, ProKind::Aggregate))
        .collect();
    if agg_candidates.len() == 1 {
        return Ok(make_resolved(agg_candidates[0], snapshot));
    }

    let count_matches: Vec<_> = candidates
        .iter()
        .filter(|f| f.proargtypes.len() == arg_types.len() || f.provariadic.is_some())
        .collect();
    if count_matches.len() == 1 {
        return Ok(make_resolved(count_matches[0], snapshot));
    }

    Err(AnalyzeError::UndefinedFunction(format!(
        "function {name} with {} argument(s) does not exist (found {} candidate(s))",
        arg_types.len(),
        candidates.len()
    )))
}

fn find_unknown_compatible_match<'a>(
    candidates: &[&'a PgProc],
    arg_types: &[PgTypeOid],
) -> Option<&'a PgProc> {
    if !arg_types.contains(&oid::UNKNOWN) {
        return None;
    }
    let matches: Vec<_> = candidates
        .iter()
        .filter(|f| {
            f.proargtypes.len() == arg_types.len()
                && f.proargtypes
                    .iter()
                    .zip(arg_types.iter())
                    .all(|(&expected, &actual)| expected == actual || actual == oid::UNKNOWN)
        })
        .collect();
    if matches.len() == 1 {
        Some(matches[0])
    } else {
        None
    }
}

fn find_exact_match<'a>(candidates: &[&'a PgProc], arg_types: &[PgTypeOid]) -> Option<&'a PgProc> {
    candidates
        .iter()
        .find(|f| f.proargtypes == arg_types)
        .copied()
}

fn find_default_args_match<'a>(
    candidates: &[&'a PgProc],
    arg_types: &[PgTypeOid],
    snapshot: &PgCatalog,
) -> Option<&'a PgProc> {
    let provided = arg_types.len();
    let matching: Vec<&PgProc> = candidates
        .iter()
        .filter(|f| {
            let total = f.proargtypes.len();
            let defaults = f.pronargdefaults.max(0) as usize;
            total >= provided
                && defaults >= total - provided
                && f.proargtypes
                    .iter()
                    .take(provided)
                    .zip(arg_types.iter())
                    .all(|(&expected, &actual)| {
                        expected == actual
                            || actual == oid::UNKNOWN
                            || snapshot.has_implicit_cast(actual, expected)
                    })
        })
        .copied()
        .collect();
    if matching.len() == 1 {
        Some(matching[0])
    } else {
        None
    }
}

/// `has_implicit_cast` extended with PG's element-wise rule for arrays:
/// `numeric[] → float8[]` is allowed because `numeric → float8` is. This is
/// only used by overload resolution; the bare `has_implicit_cast` query
/// stays strict so explicit cast lookups don't get accidentally relaxed.
fn casts_implicitly(source: PgTypeOid, target: PgTypeOid, snapshot: &PgCatalog) -> bool {
    if snapshot.has_implicit_cast(source, target) {
        return true;
    }
    let (Some(s), Some(t)) = (snapshot.get_type(source), snapshot.get_type(target)) else {
        return false;
    };
    if s.typcategory != TypCategory::Array || t.typcategory != TypCategory::Array {
        return false;
    }
    let (Some(se), Some(te)) = (s.typelem, t.typelem) else {
        return false;
    };
    snapshot.has_implicit_cast(se, te)
}

fn find_polymorphic_match<'a>(
    candidates: &[&'a PgProc],
    arg_types: &[PgTypeOid],
    snapshot: &PgCatalog,
) -> Option<&'a PgProc> {
    // First pass: only exact / UNKNOWN / polymorphic matches — what PG calls
    // a "type-conformant" candidate without any cast.
    let strict: Vec<&PgProc> = candidates
        .iter()
        .filter(|f| f.proargtypes.len() == arg_types.len())
        .filter(|f| {
            f.proargtypes
                .iter()
                .zip(arg_types.iter())
                .all(|(&expected, &actual)| {
                    expected == actual
                        || actual == oid::UNKNOWN
                        || matches_polymorphic(expected, actual, snapshot)
                })
        })
        .copied()
        .collect();
    if strict.len() == 1 {
        return Some(strict[0]);
    }
    if !strict.is_empty() {
        return None;
    }

    // Second pass: allow implicit casts on the non-polymorphic args. This is
    // what makes `percentile_disc(0.5) WITHIN GROUP (ORDER BY int_col)`
    // resolve — the direct arg is `numeric` but the candidate expects
    // `float8`, while the ordered arg matches the polymorphic `anyelement`.
    let lax: Vec<&PgProc> = candidates
        .iter()
        .filter(|f| f.proargtypes.len() == arg_types.len())
        .filter(|f| {
            f.proargtypes
                .iter()
                .zip(arg_types.iter())
                .all(|(&expected, &actual)| {
                    expected == actual
                        || actual == oid::UNKNOWN
                        || matches_polymorphic(expected, actual, snapshot)
                        || (!is_polymorphic(expected)
                            && casts_implicitly(actual, expected, snapshot))
                })
        })
        .copied()
        .collect();
    if lax.len() == 1 { Some(lax[0]) } else { None }
}

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

fn find_cast_match<'a>(
    candidates: &[&'a PgProc],
    arg_types: &[PgTypeOid],
    snapshot: &PgCatalog,
) -> Option<&'a PgProc> {
    let matching: Vec<&PgProc> = candidates
        .iter()
        .filter(|f| f.proargtypes.len() == arg_types.len())
        .filter(|f| {
            f.proargtypes
                .iter()
                .zip(arg_types.iter())
                .all(|(&expected, &actual)| {
                    expected == actual
                        || actual == oid::UNKNOWN
                        || casts_implicitly(actual, expected, snapshot)
                })
        })
        .copied()
        .collect();

    if matching.len() <= 1 {
        return matching.into_iter().next();
    }

    if !arg_types.contains(&oid::UNKNOWN) {
        return matching.into_iter().next();
    }

    let string_compatible: Vec<&PgProc> = matching
        .iter()
        .filter(|f| {
            f.proargtypes
                .iter()
                .zip(arg_types.iter())
                .all(|(&param_oid, &actual)| {
                    if actual != oid::UNKNOWN {
                        return true;
                    }
                    snapshot
                        .get_type(param_oid)
                        .is_some_and(|t| t.typcategory == TypCategory::String)
                })
        })
        .copied()
        .collect();

    if string_compatible.is_empty() {
        return matching.into_iter().next();
    }
    if string_compatible.len() == 1 {
        return Some(string_compatible[0]);
    }

    let preferred: Vec<&PgProc> = string_compatible
        .iter()
        .filter(|f| {
            f.proargtypes
                .iter()
                .zip(arg_types.iter())
                .all(|(&param_oid, &actual)| {
                    if actual != oid::UNKNOWN {
                        return true;
                    }
                    snapshot
                        .get_type(param_oid)
                        .is_some_and(|t| t.typispreferred)
                })
        })
        .copied()
        .collect();

    if !preferred.is_empty() {
        return preferred.into_iter().next();
    }
    string_compatible.into_iter().next()
}

/// Build the named output-argument list for an SRF / OUT-arg function from
/// its `pg_proc` row. Returns an empty vec when the function has no OUT-like
/// args.
fn build_out_args(p: &PgProc) -> Vec<OutArg> {
    if p.proargmodes.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let len = p
        .proallargtypes
        .len()
        .min(p.proargmodes.len())
        .min(p.proargnames.len());
    for i in 0..len {
        let mode = p.proargmodes[i];
        if !matches!(mode, ArgMode::Out | ArgMode::InOut | ArgMode::Table) {
            continue;
        }
        let name = &p.proargnames[i];
        if name.is_empty() {
            continue;
        }
        out.push(OutArg {
            name: name.clone(),
            type_oid: p.proallargtypes[i],
            not_null: false,
        });
    }
    out
}

fn make_resolved(f: &PgProc, snapshot: &PgCatalog) -> ResolvedFunction {
    let agg_final = snapshot
        .pg_aggregate
        .get(&f.oid)
        .and_then(|a| a.aggfinaltype);
    ResolvedFunction {
        return_type_oid: agg_final.unwrap_or(f.prorettype),
        arg_types: f.proargtypes.clone(),
        schema: snapshot
            .namespace_name(f.pronamespace)
            .map(str::to_owned)
            .unwrap_or_default(),
        is_aggregate: matches!(f.prokind, ProKind::Aggregate),
        is_strict: f.proisstrict,
        out_args: build_out_args(f),
    }
}

fn make_resolved_polymorphic(
    f: &PgProc,
    actual_args: &[PgTypeOid],
    snapshot: &PgCatalog,
) -> ResolvedFunction {
    let mut bound_element: Option<PgTypeOid> = None;
    let mut bound_array: Option<PgTypeOid> = None;

    for (&expected, &actual) in f.proargtypes.iter().zip(actual_args.iter()) {
        bind_polymorphic_from(
            expected,
            actual,
            snapshot,
            &mut bound_element,
            &mut bound_array,
        );
    }

    let agg_final = snapshot
        .pg_aggregate
        .get(&f.oid)
        .and_then(|a| a.aggfinaltype);
    let return_type_oid = substitute_polymorphic(
        agg_final.unwrap_or(f.prorettype),
        bound_element,
        bound_array,
        snapshot,
    );
    let out_args = build_out_args(f)
        .into_iter()
        .map(|field| OutArg {
            name: field.name,
            type_oid: substitute_polymorphic(field.type_oid, bound_element, bound_array, snapshot),
            not_null: field.not_null,
        })
        .collect();

    ResolvedFunction {
        return_type_oid,
        arg_types: f.proargtypes.clone(),
        schema: snapshot
            .namespace_name(f.pronamespace)
            .map(str::to_owned)
            .unwrap_or_default(),
        is_aggregate: matches!(f.prokind, ProKind::Aggregate),
        is_strict: f.proisstrict,
        out_args,
    }
}

pub(crate) fn is_nullable_strict_exception(name: &str) -> bool {
    NULLABLE_STRICT_PG_CATALOG_FUNCTIONS.contains(&name)
}

pub(crate) fn is_nullable_operator(name: &str) -> bool {
    NULLABLE_PG_CATALOG_OPERATORS.contains(&name)
}

const NOT_NULL_NONSTRICT_PG_CATALOG_FUNCTIONS: &[&str] = &[
    "concat",
    "format",
    "now",
    "transaction_timestamp",
    "statement_timestamp",
    "clock_timestamp",
    "timeofday",
    "current_timestamp",
    "current_date",
    "localtime",
    "localtimestamp",
    "current_user",
    "session_user",
    "current_schema",
    "current_database",
    "current_catalog",
    "inet_client_addr",
    "inet_server_addr",
    "pg_backend_pid",
    "pg_postmaster_start_time",
    "version",
    "random",
    "gen_random_uuid",
    "setseed",
    "nextval",
    "currval",
    "lastval",
    "setval",
    "txid_current",
    "txid_current_if_assigned",
    "array_cat",
    "array_append",
    "array_prepend",
    "coalesce",
    "greatest",
    "least",
    "json_build_object",
    "jsonb_build_object",
    "json_build_array",
    "jsonb_build_array",
    "row_number",
    "rank",
    "dense_rank",
    "percent_rank",
    "cume_dist",
    "ntile",
];

pub(crate) fn is_not_null_nonstrict(name: &str) -> bool {
    NOT_NULL_NONSTRICT_PG_CATALOG_FUNCTIONS.contains(&name)
}
