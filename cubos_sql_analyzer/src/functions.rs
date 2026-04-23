//! Function and aggregate resolution.

use crate::error::AnalyzeError;
use crate::schema::{CompositeField, FunctionEntry, SchemaSnapshot};
use crate::type_map::oid;

/// Resolved function call result.
pub(crate) struct ResolvedFunction {
    pub return_type_oid: u32,
    /// The resolved argument types from the matched function signature.
    pub arg_types: Vec<u32>,
    pub schema: String,
    pub is_aggregate: bool,
    pub is_strict: bool,
    /// Named output columns for SRFs / OUT-arg functions, copied from the
    /// matched `FunctionEntry`. Empty for plain scalar returns.
    pub out_args: Vec<CompositeField>,
}

/// pg_catalog strict functions that can still return NULL with non-null inputs.
///
/// These are exceptions to the general rule that pg_catalog strict functions
/// are "total" (non-null inputs → non-null output). Each function listed here
/// has legitimate cases where all inputs are non-null but the output is NULL.
const NULLABLE_STRICT_PG_CATALOG_FUNCTIONS: &[&str] = &[
    // Array: returns NULL if element not found or dimension doesn't exist.
    "array_position",
    "array_upper",
    "array_lower",
    "array_length",
    // Regex: returns NULL if pattern doesn't match.
    "regexp_match",
    "regexp_matches",
    // Substring with pattern: returns NULL if no match.
    "substring",
    // JSON/JSONB field extraction: returns NULL if key/index doesn't exist.
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
    // JSON/JSONB path extraction: returns NULL if path doesn't exist.
    "jsonb_extract_path",
    "jsonb_extract_path_text",
    "json_extract_path",
    "json_extract_path_text",
    // NULLIF: returns NULL when arguments are equal.
    "nullif",
    // Catalog description: returns NULL if object has no comment.
    "obj_description",
    "col_description",
    "shobj_description",
    // NOTE: lower(anyrange)/upper(anyrange) return NULL for empty/unbounded
    // ranges, but share names with lower(text)/upper(text) which are total.
    // We can't distinguish by name alone, so they are NOT listed here.
    // Use "col!" annotation for range lower/upper if needed.
];

/// pg_catalog operators that can return NULL with non-null inputs.
///
/// Most operators are "total" (non-null inputs → non-null output), but
/// JSON/JSONB field access operators return NULL when the key/path doesn't exist.
const NULLABLE_PG_CATALOG_OPERATORS: &[&str] = &[
    // jsonb/json -> key/index: NULL if key doesn't exist.
    "->",  // jsonb/json ->> key/index: NULL if key doesn't exist.
    "->>", // jsonb/json #> path: NULL if path doesn't exist.
    "#>",  // jsonb/json #>> path: NULL if path doesn't exist.
    "#>>",
];

/// Resolve a function call by name and argument types.
pub(crate) fn resolve_function(
    snapshot: &SchemaSnapshot,
    schema: Option<&str>,
    name: &str,
    arg_types: &[u32],
    is_agg_star: bool,
) -> Result<ResolvedFunction, AnalyzeError> {
    // Special case: COUNT(*)
    if name == "count" && is_agg_star {
        return Ok(ResolvedFunction {
            return_type_oid: oid::INT8,
            arg_types: vec![],
            schema: "pg_catalog".into(),
            is_aggregate: true,
            is_strict: true,
            out_args: Vec::new(),
        });
    }

    // Procedures are only callable via `CALL stmt`, never inside expressions,
    // so filter them out of the candidate set for expression-level lookups.
    let candidates: Vec<&FunctionEntry> = snapshot
        .find_functions(schema, name)
        .into_iter()
        .filter(|f| !f.is_procedure)
        .collect();
    if candidates.is_empty() {
        return Err(AnalyzeError::UnresolvedFunction(format!(
            "function {name} not found"
        )));
    }

    // Phase 1: exact match on arg count and types.
    if let Some(f) = find_exact_match(&candidates, arg_types) {
        return Ok(make_resolved(f));
    }

    // Phase 1b: match treating UNKNOWN args as compatible with any expected type.
    // This handles untyped string literals like ', ' in string_agg(col, ', ').
    if let Some(f) = find_unknown_compatible_match(&candidates, arg_types) {
        return Ok(make_resolved(f));
    }

    // Phase 2: match with implicit casts.
    if let Some(f) = find_cast_match(&candidates, arg_types, snapshot) {
        return Ok(make_resolved(f));
    }

    // Phase 2b: polymorphic pseudo-types (`anyelement`, `anyarray`, …). These
    // never appear as concrete arg types, so `find_exact_match`/`find_cast_match`
    // miss them. Matching `_pg_expandarray(anyarray)` against a concrete
    // `int4[]` lives here.
    if let Some(f) = find_polymorphic_match(&candidates, arg_types, snapshot) {
        return Ok(make_resolved_polymorphic(f, arg_types, snapshot));
    }

    // Phase 3: for single-candidate aggregates with zero args (e.g., COUNT),
    // try matching with any number of args.
    let agg_candidates: Vec<_> = candidates.iter().filter(|f| f.is_aggregate).collect();
    if agg_candidates.len() == 1 {
        return Ok(make_resolved(agg_candidates[0]));
    }

    // Phase 4: if only one candidate matches arg count, use it.
    let count_matches: Vec<_> = candidates
        .iter()
        .filter(|f| f.arg_types.len() == arg_types.len() || f.is_variadic)
        .collect();
    if count_matches.len() == 1 {
        return Ok(make_resolved(count_matches[0]));
    }

    Err(AnalyzeError::UnresolvedFunction(format!(
        "cannot resolve function {name} with {} args (found {} candidates)",
        arg_types.len(),
        candidates.len()
    )))
}

/// Match candidates treating UNKNOWN (OID 705) args as compatible with any expected type.
/// This handles untyped string literals (e.g., `', '` in `string_agg(col, ', ')`).
/// Returns a match only if exactly one candidate matches (to avoid ambiguity).
fn find_unknown_compatible_match<'a>(
    candidates: &[&'a FunctionEntry],
    arg_types: &[u32],
) -> Option<&'a FunctionEntry> {
    if !arg_types.contains(&oid::UNKNOWN) {
        return None;
    }
    let matches: Vec<_> = candidates
        .iter()
        .filter(|f| {
            f.arg_types.len() == arg_types.len()
                && f.arg_types
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

fn find_exact_match<'a>(
    candidates: &[&'a FunctionEntry],
    arg_types: &[u32],
) -> Option<&'a FunctionEntry> {
    candidates
        .iter()
        .find(|f| f.arg_types == arg_types)
        .copied()
}

/// Find a candidate where every parameter type either equals the caller's
/// actual type OR is a polymorphic pseudo-type (`anyelement`, `anyarray`,
/// `anyenum`, `anyrange`, `anymultirange`, `anynonarray`) compatible with
/// the actual. Consistency across positions (PG's rule that all `anyelement`
/// positions must resolve to the same type) is intentionally NOT enforced:
/// our goal is type inference for view columns, not full call-site checking.
fn find_polymorphic_match<'a>(
    candidates: &[&'a FunctionEntry],
    arg_types: &[u32],
    snapshot: &SchemaSnapshot,
) -> Option<&'a FunctionEntry> {
    let matching: Vec<&FunctionEntry> = candidates
        .iter()
        .filter(|f| f.arg_types.len() == arg_types.len())
        .filter(|f| {
            f.arg_types
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
    if matching.len() == 1 {
        Some(matching[0])
    } else {
        // If more than one polymorphic candidate matches, we can't
        // disambiguate without tracking per-position consistency — return
        // None rather than guessing.
        None
    }
}

/// Is `expected` a polymorphic pseudo-type that PG would accept `actual` for?
fn matches_polymorphic(expected: u32, actual: u32, snapshot: &SchemaSnapshot) -> bool {
    // Pseudo-type OIDs (stable across PG versions).
    const ANYELEMENT: u32 = 2283;
    const ANYARRAY: u32 = 2277;
    const ANYNONARRAY: u32 = 2776;
    const ANYENUM: u32 = 3500;
    const ANYRANGE: u32 = 3831;
    const ANYMULTIRANGE: u32 = 4537;
    // Historical array-shaped types that pg_cast doesn't mark as arrays in
    // the normal sense but still satisfy `anyarray` / `anynonarray` for
    // legacy catalog functions.
    const INT2VECTOR: u32 = 22;
    const OIDVECTOR: u32 = 30;

    let actual_is_array = matches!(
        snapshot.get_type(actual).map(|t| &t.kind),
        Some(crate::schema::TypeKind::Array { .. })
    ) || actual == INT2VECTOR
        || actual == OIDVECTOR;

    match expected {
        ANYELEMENT => true,
        ANYARRAY => actual_is_array,
        ANYNONARRAY => !actual_is_array,
        ANYENUM => matches!(
            snapshot.get_type(actual).map(|t| &t.kind),
            Some(crate::schema::TypeKind::Enum { .. })
        ),
        ANYRANGE | ANYMULTIRANGE => matches!(
            snapshot.get_type(actual).map(|t| &t.kind),
            Some(crate::schema::TypeKind::Range { .. })
        ),
        _ => false,
    }
}

fn find_cast_match<'a>(
    candidates: &[&'a FunctionEntry],
    arg_types: &[u32],
    snapshot: &SchemaSnapshot,
) -> Option<&'a FunctionEntry> {
    let matching: Vec<&FunctionEntry> = candidates
        .iter()
        .filter(|f| f.arg_types.len() == arg_types.len())
        .filter(|f| {
            f.arg_types
                .iter()
                .zip(arg_types.iter())
                .all(|(&expected, &actual)| {
                    expected == actual
                        || actual == oid::UNKNOWN
                        || snapshot.has_implicit_cast(actual, expected)
                })
        })
        .copied()
        .collect();

    if matching.len() <= 1 {
        return matching.into_iter().next();
    }

    // Tie-break for UNKNOWN arguments (PG §10.3 step 4e).
    //
    // Untyped literals (`'foo'`, `$1` without context) arrive as UNKNOWN. For
    // each UNKNOWN-arg position, PG assumes the string category, then:
    //   1. Keep candidates whose param at that position is in category 'S'.
    //   2. Among those, prefer candidates whose param is `typispreferred`
    //      (e.g. `text` over `varchar`/`bpchar`, `text` over `bytea`).
    //
    // Without this, relying on the natural pg_proc order would be fragile —
    // extensions installed later could reorder overloads and flip results.
    if !arg_types.contains(&oid::UNKNOWN) {
        return matching.into_iter().next();
    }

    let string_compatible: Vec<&FunctionEntry> = matching
        .iter()
        .filter(|f| {
            f.arg_types
                .iter()
                .zip(arg_types.iter())
                .all(|(&param_oid, &actual)| {
                    if actual != oid::UNKNOWN {
                        return true;
                    }
                    snapshot
                        .get_type(param_oid)
                        .is_some_and(|t| t.category == 'S')
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

    let preferred: Vec<&FunctionEntry> = string_compatible
        .iter()
        .filter(|f| {
            f.arg_types
                .iter()
                .zip(arg_types.iter())
                .all(|(&param_oid, &actual)| {
                    if actual != oid::UNKNOWN {
                        return true;
                    }
                    snapshot.get_type(param_oid).is_some_and(|t| t.is_preferred)
                })
        })
        .copied()
        .collect();

    if !preferred.is_empty() {
        return preferred.into_iter().next();
    }
    string_compatible.into_iter().next()
}

fn make_resolved(f: &FunctionEntry) -> ResolvedFunction {
    ResolvedFunction {
        return_type_oid: f.agg_final_type_oid.unwrap_or(f.return_type_oid),
        arg_types: f.arg_types.clone(),
        schema: f.schema.clone(),
        is_aggregate: f.is_aggregate,
        is_strict: f.is_strict,
        out_args: f.out_args.clone(),
    }
}

/// Build a `ResolvedFunction` after a polymorphic match, substituting every
/// `any*` OID in the return type and `out_args` with the concrete type
/// bound at the matching arg position.
///
/// The binding rules (simplified vs. PG's full §10.3.4 algorithm):
/// - `anyelement` / `anynonarray` / `anyenum` → bind to the concrete arg OID.
/// - `anyarray` → bind to the array OID *and* derive its element OID for
///   downstream `anyelement` slots.
/// - `anyrange` / `anymultirange` → bind to the range OID.
///
/// If multiple positions leave the bindings inconsistent we keep the first
/// we see — consistency enforcement happens in the caller via the single-
/// match rule in `find_polymorphic_match`.
fn make_resolved_polymorphic(
    f: &FunctionEntry,
    actual_args: &[u32],
    snapshot: &SchemaSnapshot,
) -> ResolvedFunction {
    let mut bound_element: Option<u32> = None;
    let mut bound_array: Option<u32> = None;

    const ANYELEMENT: u32 = 2283;
    const ANYARRAY: u32 = 2277;
    const ANYNONARRAY: u32 = 2776;
    const ANYENUM: u32 = 3500;
    const ANYRANGE: u32 = 3831;
    const ANYMULTIRANGE: u32 = 4537;

    for (&expected, &actual) in f.arg_types.iter().zip(actual_args.iter()) {
        match expected {
            ANYELEMENT | ANYNONARRAY | ANYENUM => {
                bound_element.get_or_insert(actual);
            }
            ANYARRAY => {
                bound_array.get_or_insert(actual);
                // Remember the element type too so `anyelement` in the
                // return or out_args resolves consistently.
                if let Some(crate::schema::TypeKind::Array { element_type_oid }) =
                    snapshot.get_type(actual).map(|t| t.kind.clone())
                {
                    bound_element.get_or_insert(element_type_oid);
                }
            }
            ANYRANGE | ANYMULTIRANGE => {
                bound_array.get_or_insert(actual);
            }
            _ => {}
        }
    }

    let substitute = |oid: u32| -> u32 {
        match oid {
            ANYELEMENT | ANYNONARRAY | ANYENUM => bound_element.unwrap_or(oid),
            ANYARRAY => {
                // Prefer a direct `anyarray` binding; else derive the array
                // type from the bound element.
                bound_array
                    .or_else(|| bound_element.and_then(|e| snapshot.array_type_of(e)))
                    .unwrap_or(oid)
            }
            ANYRANGE | ANYMULTIRANGE => bound_array.unwrap_or(oid),
            _ => oid,
        }
    };

    let return_type_oid = substitute(f.agg_final_type_oid.unwrap_or(f.return_type_oid));
    let out_args = f
        .out_args
        .iter()
        .map(|field| crate::schema::CompositeField {
            name: field.name.clone(),
            type_oid: substitute(field.type_oid),
            not_null: field.not_null,
        })
        .collect();

    ResolvedFunction {
        return_type_oid,
        arg_types: f.arg_types.clone(),
        schema: f.schema.clone(),
        is_aggregate: f.is_aggregate,
        is_strict: f.is_strict,
        out_args,
    }
}

/// Returns true if a pg_catalog strict function is known to possibly return NULL
/// even with non-null inputs.
pub(crate) fn is_nullable_strict_exception(name: &str) -> bool {
    NULLABLE_STRICT_PG_CATALOG_FUNCTIONS.contains(&name)
}

/// Returns true if an operator can return NULL with non-null inputs.
pub(crate) fn is_nullable_operator(name: &str) -> bool {
    NULLABLE_PG_CATALOG_OPERATORS.contains(&name)
}

/// pg_catalog non-strict functions that are guaranteed to NEVER return NULL,
/// regardless of input nullability. These are safe to mark as NOT NULL
/// unconditionally.
const NOT_NULL_NONSTRICT_PG_CATALOG_FUNCTIONS: &[&str] = &[
    // String concatenation: treats NULLs as empty strings.
    "concat",
    "concat_ws",
    // sprintf-like formatting: NULL args become empty.
    "format",
    // Current time: no inputs, always returns a value.
    "now",
    "transaction_timestamp",
    "statement_timestamp",
    "clock_timestamp",
    "timeofday",
    "current_timestamp",
    "current_date",
    "localtime",
    "localtimestamp",
    // Session info: always returns a value.
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
    // Random values: always return a value.
    "random",
    "gen_random_uuid",
    "setseed",
    // Sequence functions: return bigint or error, never NULL.
    "nextval",
    "currval",
    "lastval",
    "setval",
    // Transaction ID.
    "txid_current",
    "txid_current_if_assigned",
    // Array constructor: always returns an array (possibly empty).
    "array_cat",
    "array_append",
    "array_prepend",
    // COALESCE-like: handled as separate AST nodes, but if called as function:
    "coalesce",
    "greatest",
    "least",
];

/// Returns true if a pg_catalog non-strict function is guaranteed to never
/// return NULL.
pub(crate) fn is_not_null_nonstrict(name: &str) -> bool {
    NOT_NULL_NONSTRICT_PG_CATALOG_FUNCTIONS.contains(&name)
}
