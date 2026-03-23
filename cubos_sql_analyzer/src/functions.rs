//! Function and aggregate resolution.

use crate::coerce::oid;
use crate::error::AnalyzeError;
use crate::schema::{FunctionEntry, SchemaSnapshot};

/// Resolved function call result.
pub struct ResolvedFunction {
    pub return_type_oid: u32,
    pub schema: String,
    pub is_aggregate: bool,
    pub is_set_returning: bool,
    pub is_strict: bool,
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
pub fn resolve_function(
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
            schema: "pg_catalog".into(),
            is_aggregate: true,
            is_set_returning: false,
            is_strict: true,
        });
    }

    let candidates = snapshot.find_functions(schema, name);
    if candidates.is_empty() {
        return Err(AnalyzeError::UnresolvedFunction(format!(
            "function {name} not found"
        )));
    }

    // Phase 1: exact match on arg count and types.
    if let Some(f) = find_exact_match(&candidates, arg_types) {
        return Ok(make_resolved(f));
    }

    // Phase 2: match with implicit casts.
    if let Some(f) = find_cast_match(&candidates, arg_types, snapshot) {
        return Ok(make_resolved(f));
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

fn find_exact_match<'a>(
    candidates: &[&'a FunctionEntry],
    arg_types: &[u32],
) -> Option<&'a FunctionEntry> {
    candidates
        .iter()
        .find(|f| f.arg_types == arg_types)
        .copied()
}

fn find_cast_match<'a>(
    candidates: &[&'a FunctionEntry],
    arg_types: &[u32],
    snapshot: &SchemaSnapshot,
) -> Option<&'a FunctionEntry> {
    candidates
        .iter()
        .filter(|f| f.arg_types.len() == arg_types.len())
        .find(|f| {
            f.arg_types
                .iter()
                .zip(arg_types.iter())
                .all(|(&expected, &actual)| {
                    expected == actual || snapshot.has_implicit_cast(actual, expected)
                })
        })
        .copied()
}

fn make_resolved(f: &FunctionEntry) -> ResolvedFunction {
    ResolvedFunction {
        return_type_oid: f.agg_final_type_oid.unwrap_or(f.return_type_oid),
        schema: f.schema.clone(),
        is_aggregate: f.is_aggregate,
        is_set_returning: f.is_set_returning,
        is_strict: f.is_strict,
    }
}

/// Returns true if a pg_catalog strict function is known to possibly return NULL
/// even with non-null inputs.
pub fn is_nullable_strict_exception(name: &str) -> bool {
    NULLABLE_STRICT_PG_CATALOG_FUNCTIONS.contains(&name)
}

/// Returns true if an operator can return NULL with non-null inputs.
pub fn is_nullable_operator(name: &str) -> bool {
    NULLABLE_PG_CATALOG_OPERATORS.contains(&name)
}
