//! PG-verbatim error constructors — the single home for the analyzer's
//! error-message contract.
//!
//! Every message the analyzer can emit for a query PG would also reject
//! must *start with* PG's server-side wording (see `pg_sanity`); these
//! constructors define each wording exactly once, annotated with the
//! SQLSTATE PostgreSQL uses, so the contract is greppable and a future
//! oracle upgrade can compare SQLSTATEs too.
//!
//! Two families intentionally live elsewhere:
//! - literal *input-syntax* messages (`malformed … literal`, out-of-range,
//!   …) are owned by [`crate::literal_input`], next to the validators that
//!   produce them — except the shared `invalid input syntax for type …`
//!   template, defined here as [`invalid_input_syntax_for_type`];
//! - clause-coercion wording (`argument of WHERE must be type boolean …`)
//!   is owned by [`crate::clause`], keyed by `ClauseKind`.

use crate::error::{AnalyzeError, RawError, SourceSpan};

/// `invalid input syntax for type T: "content"` — SQLSTATE 22P02
/// (`invalid_text_representation`), PG's shared input-function template.
/// `type_msg_name` is the *input function's* type-name string, which
/// differs from `format_type` for the timestamp family (`timestamptz` →
/// `timestamp with time zone`). Returns a plain `String`: the
/// [`crate::literal_input`] validators compose their errors as strings and
/// the caller attaches span/kind.
pub(crate) fn invalid_input_syntax_for_type(type_msg_name: &str, content: &str) -> String {
    format!("invalid input syntax for type {type_msg_name}: \"{content}\"")
}

/// `operator does not exist: <left> <op> <right>` — SQLSTATE 42883
/// (`undefined_function`). `left`/`right` are PG-rendered type names
/// (`format_type_for_message`).
pub(crate) fn operator_does_not_exist(
    left: &str,
    op: &str,
    right: &str,
    span: Option<SourceSpan>,
) -> RawError {
    RawError::undefined_operator(
        format!("operator does not exist: {left} {op} {right}"),
        span,
        None,
    )
}

/// `operator is not unique: <left> <op> <right>` — SQLSTATE 42725
/// (`ambiguous_function`): several overloads survived every resolution
/// tiebreak.
pub(crate) fn operator_is_not_unique(
    left: &str,
    op: &str,
    right: &str,
    span: Option<SourceSpan>,
) -> RawError {
    RawError::invalid(
        format!("operator is not unique: {left} {op} {right}"),
        span,
        Some("add an explicit type cast to one side, e.g. `expr::int4`".into()),
    )
}

/// `function name(types) is not unique` — SQLSTATE 42725.
pub(crate) fn function_is_not_unique(
    qualified_name: &str,
    arg_list: &str,
    span: Option<SourceSpan>,
) -> RawError {
    RawError::invalid(
        format!("function {qualified_name}({arg_list}) is not unique"),
        span,
        Some("add explicit type casts to the arguments to select one overload".into()),
    )
}

/// `GROUP BY position N is not in select list` / `ORDER BY position N is
/// not in select list` — SQLSTATE 42P10 (`invalid_column_reference`).
pub(crate) fn position_not_in_select_list(
    clause: &str,
    position: i64,
    span: Option<SourceSpan>,
) -> RawError {
    RawError::invalid(
        format!("{clause} position {position} is not in select list"),
        span,
        None,
    )
}

/// `for SELECT DISTINCT, ORDER BY expressions must appear in select list`
/// — SQLSTATE 42P10.
pub(crate) fn distinct_order_by_not_in_select_list(span: Option<SourceSpan>) -> RawError {
    RawError::invalid(
        "for SELECT DISTINCT, ORDER BY expressions must appear in select list".to_string(),
        span,
        None,
    )
}

/// `table name "u" specified more than once` — SQLSTATE 42712
/// (`duplicate_alias`).
pub(crate) fn duplicate_table_alias(alias: &str) -> RawError {
    RawError::invalid(
        format!("table name \"{alias}\" specified more than once"),
        None,
        None,
    )
}

/// `table "t" has N columns available but M columns specified` — SQLSTATE
/// 42P10: a FROM column-alias list longer than the relation's width.
pub(crate) fn too_many_column_aliases(alias: &str, available: usize, specified: usize) -> RawError {
    RawError::invalid(
        format!(
            "table \"{alias}\" has {available} columns available but {specified} columns specified"
        ),
        None,
        None,
    )
}

/// `VALUES lists must all be the same length` — SQLSTATE 42601
/// (`syntax_error`).
pub(crate) fn values_lists_length(first_arity: usize, row_arity: usize) -> RawError {
    RawError::invalid(
        "VALUES lists must all be the same length".to_string(),
        None,
        Some(format!(
            "the first row has {first_arity} column(s), a later row has {row_arity}"
        )),
    )
}

/// `window "w" does not exist` — SQLSTATE 42704 (`undefined_object`): a
/// named-window reference with no matching WINDOW-clause definition.
pub(crate) fn window_does_not_exist(name: &str) -> RawError {
    RawError::invalid(
        format!("window \"{name}\" does not exist"),
        None,
        Some("define it in a WINDOW clause, e.g. `WINDOW w AS (ORDER BY …)`".into()),
    )
}

/// `column "x" specified in USING clause does not exist in left table`
/// (or `right table`) — SQLSTATE 42703 (`undefined_column`).
pub(crate) fn using_column_missing(column: &str, side: &str) -> RawError {
    RawError::invalid(
        format!("column \"{column}\" specified in USING clause does not exist in {side} table"),
        None,
        None,
    )
}

/// `JOIN/USING types X and Y cannot be matched` — SQLSTATE 42804
/// (`datatype_mismatch`).
pub(crate) fn join_using_types_mismatch(left: &str, right: &str) -> RawError {
    RawError::invalid(
        format!("JOIN/USING types {left} and {right} cannot be matched"),
        None,
        None,
    )
}

/// `operator does not exist: X = Y (NULLIF types X and Y cannot be
/// matched)` — SQLSTATE 42883: NULLIF resolves `=` over its arguments, so
/// PG reports the operator-lookup failure; the parenthesized tail is our
/// extra detail (allowed by the prefix contract).
pub(crate) fn nullif_types_mismatch(left: &str, right: &str) -> AnalyzeError {
    AnalyzeError::Invalid(format!(
        "operator does not exist: {left} = {right} \
         (NULLIF types {left} and {right} cannot be matched)"
    ))
}

/// `{construct} types A and B cannot be matched` — SQLSTATE 42804. The
/// construct label and argument order are the caller's: CASE reports the
/// *last* branch first, COALESCE/GREATEST/UNION report source order, and
/// UNION appends a column suffix through `extra`.
pub(crate) fn types_cannot_be_matched(
    construct: &str,
    first: &str,
    second: &str,
    extra: &str,
    hint: Option<String>,
) -> RawError {
    RawError::invalid(
        format!("{construct} types {first} and {second} cannot be matched{extra}"),
        None,
        hint,
    )
}

/// `each UNION query must have the same number of columns` (likewise
/// INTERSECT / EXCEPT) — SQLSTATE 42601.
pub(crate) fn set_op_column_count(op_label: &str, left: usize, right: usize) -> RawError {
    RawError::invalid(
        format!("each {op_label} query must have the same number of columns"),
        None,
        Some(format!(
            "the left side produces {left} column(s), the right side {right}"
        )),
    )
}

/// `could not find array type for data type T` — SQLSTATE 42704: typing a
/// bare parameter as `T[]` when no such array type exists (T is itself an
/// array).
pub(crate) fn no_array_type_for(type_name: &str) -> AnalyzeError {
    AnalyzeError::Invalid(format!(
        "could not find array type for data type {type_name}"
    ))
}

/// `recursive query "r" column N has type X in non-recursive term but type
/// Y overall` — SQLSTATE 42804: PG fixes a recursive CTE's column types
/// from the non-recursive term alone.
pub(crate) fn recursive_query_column_type(
    cte_name: &str,
    column: usize,
    seed_type: &str,
    overall_type: &str,
) -> RawError {
    RawError::invalid(
        format!(
            "recursive query \"{cte_name}\" column {column} has type {seed_type} in \
             non-recursive term but type {overall_type} overall"
        ),
        None,
        Some(format!(
            "cast the non-recursive term's column to {overall_type}"
        )),
    )
}
