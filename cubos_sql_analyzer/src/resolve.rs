//! Top-level query analysis: lex the SQL template, parse it, walk the AST,
//! and produce an [`AnalyzedQuery`] combining lexer positions with inferred
//! types.

use std::collections::HashMap;

use pg_query::protobuf::{self, CmdType, JoinType, SetOperation, node};

use crate::error::AnalyzeError;
use crate::expr::{self, TypeGoal};
use crate::functions;
use crate::grouping;
use crate::nullability::{self, NullabilityContext};
use crate::oid::PgTypeOid;
use crate::param::LexOutput;
use crate::param_collector::ParamCollector;
use crate::pg_catalog::{PgCatalog, TypCategory, TypType, oid};
use crate::scope::{Scope, ScopeColumn};
use crate::types::Type;

/// Internal parameter representation produced by [`analyze_static`] before
/// being fused with lexer-side info (name, sql offsets) into [`AnalyzedParam`].
pub(crate) struct ParamInfo {
    pub pg_type: Type,
    pub nullable: bool,
}

// ──────────────────────────────────────────────────────────────────────────────
// Public API
// ──────────────────────────────────────────────────────────────────────────────

/// A single output column of an analyzed query.
#[derive(Debug, Clone)]
pub struct AnalyzedColumn {
    pub name: String,
    pub pg_type: Type,
    pub nullable: bool,
}

/// A named query parameter (`$name`) with lexer position plus inferred type.
#[derive(Debug, Clone)]
pub struct AnalyzedParam {
    /// Parameter name without the `$` prefix and `?`/`!` suffix.
    pub name: String,
    /// Byte offsets in [`AnalyzedQuery::sql`] immediately after each `$N`
    /// placeholder for this parameter. Used by code generators to insert
    /// type casts (e.g. `::jsonb`). A param referenced multiple times has
    /// multiple offsets.
    pub sql_offsets: Vec<usize>,
    pub pg_type: Type,
    pub nullable: bool,
}

/// A field inside a spread parameter (`$..name { field1, field2 }`), with
/// inferred type.
#[derive(Debug, Clone)]
pub struct AnalyzedSpreadField {
    pub name: String,
    pub pg_type: Type,
    pub nullable: bool,
}

/// A spread parameter (`$..name { ... }`) with its offset in the rewritten SQL
/// and the typed field list.
#[derive(Debug, Clone)]
pub struct AnalyzedSpread {
    pub name: String,
    /// Byte offset in [`AnalyzedQuery::sql`] where the expanded
    /// `($N, $M, ...), ...` placeholders should be inserted.
    pub offset: usize,
    pub fields: Vec<AnalyzedSpreadField>,
}

/// The full result of analyzing a SQL query template.
#[derive(Debug, Clone)]
pub struct AnalyzedQuery {
    /// SQL rewritten with positional placeholders (`$1`, `$2`, …). Spread
    /// tokens are removed; the caller must expand them at each spread's
    /// [`AnalyzedSpread::offset`].
    pub sql: String,
    pub params: Vec<AnalyzedParam>,
    pub spreads: Vec<AnalyzedSpread>,
    pub columns: Vec<AnalyzedColumn>,
}

/// Build a "sample" SQL for analysis when the query contains spreads.
///
/// Replaces each spread insertion point with a single row of positional
/// placeholders numbered after the last regular parameter. Field mapping is
/// mandatory for spreads, so `fields.len()` gives the column count.
pub(crate) fn build_spread_sample_sql(lex_output: &LexOutput) -> String {
    let base_sql = &lex_output.sql;
    let num_regular_params = lex_output.params.len();
    let mut result = String::with_capacity(base_sql.len() + 64);
    let mut last_offset = 0;
    let mut param_counter = num_regular_params;

    for spread in &lex_output.spreads {
        result.push_str(&base_sql[last_offset..spread.offset]);
        let fields = spread.fields.as_ref().expect("spread must have fields");
        result.push('(');
        for (i, _) in fields.iter().enumerate() {
            if i > 0 {
                result.push_str(", ");
            }
            param_counter += 1;
            result.push('$');
            result.push_str(&param_counter.to_string());
        }
        result.push(')');
        last_offset = spread.offset;
    }

    result.push_str(&base_sql[last_offset..]);
    result
}

pub(crate) fn fuse(
    lex_output: LexOutput,
    columns: Vec<AnalyzedColumn>,
    info_params: Vec<ParamInfo>,
) -> AnalyzedQuery {
    let LexOutput {
        sql,
        params: lex_params,
        spreads: lex_spreads,
    } = lex_output;

    let num_regular = lex_params.len();

    // Regular params: zip lex params with the first N inferred params.
    let mut params = Vec::with_capacity(num_regular);
    for (p, pi) in lex_params
        .into_iter()
        .zip(info_params.iter().take(num_regular))
    {
        params.push(AnalyzedParam {
            name: p.name,
            sql_offsets: p.sql_offsets,
            pg_type: pi.pg_type.clone(),
            nullable: pi.nullable,
        });
    }

    // Spread fields: consume the remaining inferred params in order.
    let mut spread_param_cursor = num_regular;
    let mut spreads = Vec::with_capacity(lex_spreads.len());
    for spread in lex_spreads {
        let lex_fields = spread.fields.expect("spread must have fields");
        let mut fields = Vec::with_capacity(lex_fields.len());
        for lf in lex_fields {
            let pi = &info_params[spread_param_cursor];
            fields.push(AnalyzedSpreadField {
                name: lf.name,
                pg_type: pi.pg_type.clone(),
                nullable: pi.nullable,
            });
            spread_param_cursor += 1;
        }
        spreads.push(AnalyzedSpread {
            name: spread.name,
            offset: spread.offset,
            fields,
        });
    }

    AnalyzedQuery {
        sql,
        params,
        spreads,
        columns,
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Internal static analyzer
// ──────────────────────────────────────────────────────────────────────────────

/// Parse `sql` with `pg_query`, walk the AST, and produce the resolved output
/// columns and parameter type information.
///
/// `param_nullability` seeds explicit `$foo?`/`$foo!` annotations indexed by
/// 1-based positional parameter index minus one.
pub(crate) fn analyze_static(
    snapshot: &PgCatalog,
    sql: &str,
    param_nullability: &[Option<bool>],
) -> Result<(Vec<AnalyzedColumn>, Vec<ParamInfo>), AnalyzeError> {
    let (raw_columns, raw_params) = analyze_raw(snapshot, sql, param_nullability)?;

    let columns = raw_columns
        .into_iter()
        .map(|mut rc| {
            // PG resolves any `unknown`-typed top-level output column (bare
            // string literal, NULL, untyped param that stayed unresolved) to
            // `text` before sending it to the client. `analyze_raw` is also
            // used for view-column analysis, which needs the raw OID, so apply
            // the coercion only here at the statement boundary.
            if rc.type_oid == oid::UNKNOWN {
                rc.type_oid = oid::TEXT;
            }
            build_column(rc, snapshot)
        })
        .collect::<Result<Vec<_>, _>>()?;

    let params_info = raw_params
        .into_iter()
        .map(|(_, type_oid, nullable)| build_param_info(type_oid, nullable, snapshot))
        .collect::<Result<Vec<_>, _>>()?;

    Ok((columns, params_info))
}

/// A positional parameter slot: `(position, type_oid, nullable)`. Shared by
/// the analyzer internals that thread params through overload resolution
/// before they are merged with lexer-side info.
pub(crate) type RawParam = (i32, PgTypeOid, bool);

/// Lower-level analyzer entry point: returns the raw columns (keyed by OID)
/// and sorted param list without converting to [`Type`]. Used by the DDL
/// view handling, which only needs OIDs to rebuild catalog entries.
pub(crate) fn analyze_raw(
    snapshot: &PgCatalog,
    sql: &str,
    param_nullability: &[Option<bool>],
) -> Result<(Vec<RawColumn>, Vec<RawParam>), AnalyzeError> {
    let parsed = pg_query::parse(sql).map_err(|e| AnalyzeError::Parse(e.to_string()))?;

    let stmt = parsed
        .protobuf
        .stmts
        .first()
        .and_then(|s| s.stmt.as_ref())
        .and_then(|n| n.node.as_ref())
        .ok_or_else(|| AnalyzeError::Parse("empty statement".into()))?;

    analyze_raw_node(snapshot, stmt, param_nullability)
}

/// Same as [`analyze_raw`] but consumes a pre-parsed AST node directly. View
/// reanalysis uses this to skip the deparse → reparse round-trip: the stored
/// AST + binding side-table already gives us the post-RENAME tree.
pub(crate) fn analyze_raw_node(
    snapshot: &PgCatalog,
    stmt: &node::Node,
    param_nullability: &[Option<bool>],
) -> Result<(Vec<RawColumn>, Vec<RawParam>), AnalyzeError> {
    let mut params = ParamCollector::default();

    // Seed explicit nullable annotations from lexer ($foo? / $foo! syntax).
    for (i, &nullable) in param_nullability.iter().enumerate() {
        if let Some(explicit) = nullable {
            params.set_nullable((i + 1) as i32, explicit);
        }
    }

    let (raw_columns, raw_params) = match stmt {
        node::Node::SelectStmt(sel) => analyze_select(sel, snapshot, &mut params)?,
        node::Node::InsertStmt(ins) => analyze_insert(ins, snapshot, &mut params)?,
        node::Node::UpdateStmt(upd) => analyze_update(upd, snapshot, &mut params)?,
        node::Node::DeleteStmt(del) => analyze_delete(del, snapshot, &mut params)?,
        node::Node::MergeStmt(merge) => analyze_merge(merge, snapshot, &mut params)?,
        _ => {
            return Err(AnalyzeError::Unsupported(format!(
                "statement type: {:?}",
                std::mem::discriminant(stmt)
            )));
        }
    };

    let raw_params = match raw_params {
        Some(p) => p,
        None => params.into_sorted()?,
    };

    Ok((raw_columns, raw_params))
}

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Infer an expression type, propagating only `TypeMismatch` errors.
///
/// Other errors (e.g. `UndefinedColumn` from correlated subqueries referencing
/// outer scope) are swallowed — they represent pre-existing analyzer
/// limitations, not user errors.
/// Returns true when `node` is a bare SQL `NULL` (`AConst` with `isnull` set
/// and no concrete `Val`). Typed NULLs like `NULL::int` become
/// `TypeCast { arg: AConst NULL, typename: int }` — we don't treat those as
/// unconditionally NULL because PG allows `SET col = NULL::t` to perform an
/// assignment the caller has explicitly typed.
fn is_sql_null_literal(node: &protobuf::Node) -> bool {
    matches!(
        node.node.as_ref(),
        Some(node::Node::AConst(c)) if c.isnull
    )
}

/// `true` for the literal `DEFAULT` keyword used in INSERT VALUES /
/// UPDATE SET. Mirrors PG's `SetToDefault` AST node.
fn is_set_to_default(node: &protobuf::Node) -> bool {
    matches!(node.node.as_ref(), Some(node::Node::SetToDefault(_)))
}

/// Reject aggregate / window function calls in a context where PG forbids
/// them. Matches PG's `aggregate functions are not allowed in WHERE` /
/// `window functions are not allowed in WHERE` errors. `context` goes into
/// the error message (e.g. `"WHERE"`, `"GROUP BY"`, `"JOIN ON"`).
fn check_no_aggregates_or_windows(
    node: &protobuf::Node,
    snapshot: &PgCatalog,
    context: &str,
) -> Result<(), AnalyzeError> {
    let kinds = expr::detect_func_kinds(node, snapshot);
    if kinds.has_aggregate {
        return Err(AnalyzeError::Invalid(format!(
            "aggregate functions are not allowed in {context}"
        )));
    }
    if kinds.has_window {
        return Err(AnalyzeError::Invalid(format!(
            "window functions are not allowed in {context}"
        )));
    }
    Ok(())
}

fn infer_expr_propagate_mismatch(
    node: &protobuf::Node,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    goal: TypeGoal,
) -> Result<(), AnalyzeError> {
    match expr::infer_expr(node, scope, null_ctx, snapshot, params, goal) {
        Ok(_) => Ok(()),
        Err(
            e @ (AnalyzeError::TypeMismatch { .. }
            | AnalyzeError::UndefinedOperator(_)
            | AnalyzeError::IndeterminateType(_)
            | AnalyzeError::Invalid(_)),
        ) => Err(e),
        Err(_) => Ok(()), // Swallow non-user-facing errors.
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Raw output types (before Rust type mapping)
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub(crate) struct RawColumn {
    pub name: String,
    pub type_oid: PgTypeOid,
    pub nullable: bool,
    /// Named-field structure when this column holds a record. Sourced from
    /// SRF out_args, ROW constructors, or propagated through subqueries.
    /// Used both to surface `Type::AnonymousRecord` in the final output and
    /// to feed downstream `(x).field` resolution via the scope.
    pub record_fields: Option<Vec<crate::expr::RecordField>>,
}

/// Return type for analyze_* functions: columns + optional pre-sorted params.
type AnalyzeResult = Result<(Vec<RawColumn>, Option<Vec<(i32, PgTypeOid, bool)>>), AnalyzeError>;

// ──────────────────────────────────────────────────────────────────────────────
// SELECT
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn analyze_select(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> AnalyzeResult {
    analyze_select_with_ctes_and_outer(sel, snapshot, params, &HashMap::new(), &[], &[])
}

/// Like [`analyze_select`] but seeds the initial scope with `outer_sources`
/// as **correlated** references — a subquery's column lookup tries its local
/// FROM first and only falls back to these outer sources when nothing
/// matched. Used for `EXISTS (...)`, scalar sublinks, and `IN (SELECT ...)`,
/// where PG's lexical rule says inner aliases shadow outer ones.
pub(crate) fn analyze_correlated_select(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_scope: &crate::scope::Scope,
) -> AnalyzeResult {
    analyze_select_with_ctes_and_outer(
        sel,
        snapshot,
        params,
        &HashMap::new(),
        &[],
        &outer_scope.sources,
    )
}

fn analyze_select_with_ctes(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
) -> AnalyzeResult {
    analyze_select_with_ctes_and_outer(sel, snapshot, params, outer_ctes, &[], &[])
}

/// Core SELECT analyzer.
///
/// Two flavours of outer scope, mirroring PG's distinction:
/// - `lateral_sources`: pre-visible aliases for `LATERAL` subqueries —
///   merged into the local FROM scope so the inner query sees them as if
///   they were declared locally.
/// - `correlated_sources`: pre-visible aliases for plain sublinks
///   (`EXISTS`, scalar, `IN`, `ANY`/`ALL`) — only consulted as a fallback
///   when local resolution fails, so an inner alias of the same name
///   shadows the outer one.
fn analyze_select_with_ctes_and_outer(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
    lateral_sources: &[crate::scope::TableSource],
    correlated_sources: &[crate::scope::TableSource],
) -> AnalyzeResult {
    // Start with outer CTEs (from parent WITH clause).
    let mut cte_scopes: HashMap<String, Vec<ScopeColumn>> = outer_ctes.clone();

    // Process this SELECT's own CTEs (before UNION check, since WITH wraps UNION).
    if let Some(with) = &sel.with_clause {
        for cte_node in &with.ctes {
            if let Some(node::Node::CommonTableExpr(cte)) = cte_node.node.as_ref() {
                let cte_columns = analyze_cte(cte, with.recursive, snapshot, params, &cte_scopes)?;
                cte_scopes.insert(cte.ctename.clone(), cte_columns);
            }
        }
    }

    // Handle UNION/INTERSECT/EXCEPT.
    if sel.op != SetOperation::SetopNone as i32 {
        return analyze_set_operation(sel, snapshot, params, &cte_scopes);
    }

    // Handle `VALUES (…), (…), …` — a `SelectStmt` without a FROM/target
    // list, carrying rows in `values_lists`. Column types are derived from
    // the first row; names default to `column1`/`column2`/… (PG convention)
    // and are typically overridden by a `AS alias(col1, col2)` column list
    // at the RangeSubselect that wraps the VALUES.
    if !sel.values_lists.is_empty() {
        return Ok((
            analyze_values_lists(&sel.values_lists, snapshot, params)?,
            None,
        ));
    }

    let mut scope = Scope::default();
    // LATERAL: outer aliases live as if locally declared (visible to `*`,
    // can be referenced unqualified, etc.). Correlated sublinks: outer
    // aliases are only a fallback so an inner alias of the same name
    // shadows correctly.
    scope.sources.extend(lateral_sources.iter().cloned());
    scope
        .outer_sources
        .extend(correlated_sources.iter().cloned());
    let mut null_ctx = NullabilityContext::default();
    null_ctx.has_group_by = !sel.group_clause.is_empty();

    // Process FROM clause.
    process_from_clause(
        &sel.from_clause,
        &mut scope,
        &mut null_ctx,
        snapshot,
        &cte_scopes,
        params,
    )?;

    // Expand `GROUPING SETS` / `ROLLUP` / `CUBE`: promote columns that
    // some grouping set omits to nullable, and remember whether any
    // grouping set is empty (drives aggregate-result nullability).
    let expansion = grouping::expand_grouping_sets(&sel.group_clause, &scope);
    null_ctx.grouping_omitted = expansion.omitted;
    null_ctx.has_empty_grouping_set = expansion.has_empty_set;

    // Process WHERE clause — PG uses COERCION_ASSIGNMENT + BOOL goal.
    if let Some(where_clause) = &sel.where_clause {
        // PG rejects aggregate / window function calls inside WHERE
        // (they reference the post-aggregation row, not the pre-aggregation
        // one). Catch these statically before the type pass runs.
        check_no_aggregates_or_windows(where_clause, snapshot, "WHERE")?;
        infer_expr_propagate_mismatch(
            where_clause,
            &scope,
            &null_ctx,
            snapshot,
            params,
            TypeGoal::assignment(oid::BOOL),
        )?;
    }

    // Process GROUP BY expressions — no type expectation, but we still need
    // to walk them so any parameters referenced are collected and typed.
    // `GroupingSet` nodes (`GROUPING SETS`/`ROLLUP`/`CUBE`) are not real
    // expressions; recurse into their `content` to reach the underlying
    // column references and aggregate-rejection checks.
    for group_node in &sel.group_clause {
        walk_group_clause_node(group_node, &scope, &null_ctx, snapshot, params)?;
    }

    // Process HAVING clause — same boolean goal as WHERE.
    if let Some(having) = &sel.having_clause {
        infer_expr_propagate_mismatch(
            having,
            &scope,
            &null_ctx,
            snapshot,
            params,
            TypeGoal::assignment(oid::BOOL),
        )?;
    }

    // Process ORDER BY expressions. Sort items are wrapped in `SortBy` nodes
    // — we walk the inner expression so parameters referenced there (e.g.
    // `ORDER BY embedding <=> $embedding`) get their types inferred from
    // operator context.
    for sort_node in &sel.sort_clause {
        if let Some(node::Node::SortBy(sb)) = sort_node.node.as_ref()
            && let Some(inner) = sb.node.as_deref()
        {
            let _ = expr::infer_expr(inner, &scope, &null_ctx, snapshot, params, TypeGoal::NONE);
        }
    }

    // Process LIMIT / OFFSET — PG uses coerce_to_specific_type(INT8OID)
    // with COERCION_ASSIGNMENT.
    for limit_node in [&sel.limit_count, &sel.limit_offset].into_iter().flatten() {
        infer_expr_propagate_mismatch(
            limit_node,
            &scope,
            &null_ctx,
            snapshot,
            params,
            TypeGoal::assignment(oid::INT8),
        )?;
    }

    // Resolve target list (SELECT expressions) — no type expectation.
    let columns = resolve_target_list(&sel.target_list, &scope, &null_ctx, snapshot, params)?;

    Ok((columns, None))
}

/// Walk one entry from `sel.group_clause`, recursing into `GroupingSet`
/// nodes (`GROUPING SETS`/`ROLLUP`/`CUBE`) to reach the underlying
/// expressions. The walk type-checks parameters and rejects aggregates /
/// window calls inside the grouping expressions (PG forbids those too).
fn walk_group_clause_node(
    group_node: &protobuf::Node,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    if let Some(node::Node::GroupingSet(gs)) = group_node.node.as_ref() {
        for inner in &gs.content {
            walk_group_clause_node(inner, scope, null_ctx, snapshot, params)?;
        }
        return Ok(());
    }
    check_no_aggregates_or_windows(group_node, snapshot, "GROUP BY")?;
    let _ = expr::infer_expr(
        group_node,
        scope,
        null_ctx,
        snapshot,
        params,
        TypeGoal::NONE,
    );
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// INSERT / UPDATE / DELETE
// ──────────────────────────────────────────────────────────────────────────────

fn analyze_insert(
    ins: &protobuf::InsertStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> AnalyzeResult {
    let relation = ins
        .relation
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("INSERT without relation".into()))?;

    let table = snapshot
        .resolve_table(
            if relation.schemaname.is_empty() {
                None
            } else {
                Some(&relation.schemaname)
            },
            &relation.relname,
        )
        .ok_or_else(|| {
            AnalyzeError::UndefinedTable(format!(
                "relation \"{}\" does not exist",
                relation.relname
            ))
        })?;

    // Infer param types from column positions in INSERT ... VALUES.
    let col_names: Vec<String> = ins
        .cols
        .iter()
        .filter_map(|n| {
            if let Some(node::Node::ResTarget(rt)) = n.node.as_ref() {
                Some(rt.name.clone())
            } else {
                None
            }
        })
        .collect();

    let table_oid = table.oid;
    let table_relname = table.relname.clone();
    let table_nsname = snapshot
        .namespace_name(table.relnamespace)
        .map(str::to_owned)
        .unwrap_or_default();
    let table_attrs = snapshot.attributes_of(table_oid).to_vec();

    // Validate every column mentioned in the INSERT target list exists on the
    // table. PostgreSQL rejects unknown columns with a clear error; without
    // this check the analyzer would silently treat the corresponding `$N`
    // parameter as text via the UNKNOWN fallback, masking a real bug in the
    // caller's SQL.
    for col in &col_names {
        if !table_attrs.iter().any(|c| &c.attname == col) {
            return Err(AnalyzeError::UndefinedColumn(format!(
                "column \"{col}\" of relation \"{}\" does not exist",
                table_relname,
            )));
        }
    }

    // Build a minimal scope for expressions within VALUES (no table in scope
    // for VALUES, but we need scope for possible subqueries/functions).
    let scope = Scope::default();
    let null_ctx = NullabilityContext::default();

    // Match $N params in VALUES to column types, or analyze INSERT...SELECT.
    if let Some(select_node) = &ins.select_stmt
        && let Some(node::Node::SelectStmt(val_sel)) = select_node.node.as_ref()
    {
        if !val_sel.values_lists.is_empty() {
            // VALUES (...) — infer each value with the column's type as goal.
            for val_list in &val_sel.values_lists {
                if let Some(node::Node::List(list)) = val_list.node.as_ref() {
                    // Arity check: the VALUES row must match the declared
                    // column list (or, when no column list is given, the
                    // full table width).
                    let expected_len = if col_names.is_empty() {
                        table_attrs.len()
                    } else {
                        col_names.len()
                    };
                    if list.items.len() != expected_len {
                        return Err(AnalyzeError::Invalid(format!(
                            "INSERT into `{}` expects {expected_len} values per row, \
                             got {} in one row",
                            table_relname,
                            list.items.len(),
                        )));
                    }
                    for (i, val) in list.items.iter().enumerate() {
                        let target_col = col_names
                            .get(i)
                            .and_then(|cn| table_attrs.iter().find(|c| &c.attname == cn));
                        if let Some(tc) = target_col
                            && tc.attnotnull
                            && is_sql_null_literal(val)
                        {
                            return Err(AnalyzeError::Invalid(format!(
                                "cannot insert NULL into NOT NULL column `{}.{}`",
                                table_relname, tc.attname,
                            )));
                        }
                        if let Some(tc) = target_col
                            && tc.attgenerated.is_some()
                            && !is_set_to_default(val)
                        {
                            return Err(AnalyzeError::Invalid(format!(
                                "cannot insert a non-DEFAULT value into generated column `{}.{}`",
                                table_relname, tc.attname,
                            )));
                        }
                        let goal = target_col
                            .map(|tc| TypeGoal::assignment(tc.atttypid))
                            .unwrap_or(TypeGoal::NONE);
                        expr::infer_expr(val, &scope, &null_ctx, snapshot, params, goal)?;

                        if let Some(node::Node::ParamRef(p)) = val.node.as_ref()
                            && let Some(tc) = target_col
                            && !tc.attnotnull
                        {
                            params.infer_nullable(p.number, true);
                        }
                    }
                }
            }
        } else {
            let expected_len = if col_names.is_empty() {
                table_attrs.len()
            } else {
                col_names.len()
            };
            if val_sel.target_list.len() != expected_len {
                return Err(AnalyzeError::Invalid(format!(
                    "INSERT into `{}` expects {expected_len} columns, \
                     SELECT produces {}",
                    table_relname,
                    val_sel.target_list.len(),
                )));
            }
            let _ = analyze_select(val_sel, snapshot, params);

            for (i, target) in val_sel.target_list.iter().enumerate() {
                if let Some(node::Node::ResTarget(rt)) = target.node.as_ref()
                    && let Some(val) = &rt.val
                    && let Some(node::Node::ParamRef(p)) = val.node.as_ref()
                    && let Some(col_name) = col_names.get(i)
                    && let Some(tc) = table_attrs.iter().find(|c| &c.attname == col_name)
                {
                    if params.get(p.number) == oid::UNKNOWN {
                        params.record(p.number, tc.atttypid);
                    }
                    if !tc.attnotnull {
                        params.infer_nullable(p.number, true);
                    }
                }
            }
        }
    }

    // `ON CONFLICT (…) DO UPDATE SET …` / `DO NOTHING`.
    //
    // DO UPDATE exposes a virtual `EXCLUDED` relation holding the proposed
    // row. We model it in scope as a second alias over the target table:
    // the columns share names and types, and nullability follows the real
    // columns because PG rejects an INSERT that violates NOT NULL before
    // the conflict handler runs.
    if let Some(on_conflict) = &ins.on_conflict_clause {
        let mut conflict_scope = Scope::default();
        let target_qn = crate::qualified_name::QualifiedName::new(&table_nsname, &table_relname);
        conflict_scope.add_dml_target(&relation.relname, target_qn.clone(), &table_attrs);
        conflict_scope.add_dml_target("excluded", target_qn, &table_attrs);
        let conflict_null_ctx = NullabilityContext::default();
        for set_item in &on_conflict.target_list {
            if let Some(node::Node::ResTarget(rt)) = set_item.node.as_ref()
                && let Some(val) = &rt.val
            {
                let goal = table_attrs
                    .iter()
                    .find(|c| c.attname == rt.name)
                    .map(|tc| TypeGoal::assignment(tc.atttypid))
                    .unwrap_or(TypeGoal::NONE);
                let _ = expr::infer_expr(
                    val,
                    &conflict_scope,
                    &conflict_null_ctx,
                    snapshot,
                    params,
                    goal,
                );
            }
        }
        if let Some(where_clause) = &on_conflict.where_clause {
            let _ = expr::infer_expr(
                where_clause,
                &conflict_scope,
                &conflict_null_ctx,
                snapshot,
                params,
                TypeGoal::implicit(oid::BOOL),
            );
        }
    }

    // Resolve RETURNING list.
    let mut ret_scope = Scope::default();
    let ret_null_ctx = NullabilityContext::default();
    ret_scope.add_dml_target(
        &relation.relname,
        crate::qualified_name::QualifiedName::new(&table_nsname, &table_relname),
        &table_attrs,
    );

    let columns = resolve_target_list(
        &ins.returning_list,
        &ret_scope,
        &ret_null_ctx,
        snapshot,
        params,
    )?;

    Ok((columns, None))
}

fn analyze_update(
    upd: &protobuf::UpdateStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> AnalyzeResult {
    let relation = upd
        .relation
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("UPDATE without relation".into()))?;

    let table = snapshot
        .resolve_table(
            if relation.schemaname.is_empty() {
                None
            } else {
                Some(&relation.schemaname)
            },
            &relation.relname,
        )
        .ok_or_else(|| {
            AnalyzeError::UndefinedTable(format!(
                "relation \"{}\" does not exist",
                relation.relname
            ))
        })?;

    let table_oid = table.oid;
    let table_relname = table.relname.clone();
    let table_nsname = snapshot
        .namespace_name(table.relnamespace)
        .map(str::to_owned)
        .unwrap_or_default();
    let table_attrs = snapshot.attributes_of(table_oid).to_vec();

    // Build scope with target table + FROM clause tables.
    let mut scope = Scope::default();
    let mut null_ctx = NullabilityContext::default();
    let alias = relation
        .alias
        .as_ref()
        .map(|a| a.aliasname.as_str())
        .unwrap_or(&relation.relname);
    scope.add_dml_target(
        alias,
        crate::qualified_name::QualifiedName::new(&table_nsname, &table_relname),
        &table_attrs,
    );

    // Process FROM clause (UPDATE ... FROM ... WHERE ...).
    let empty_ctes = HashMap::new();
    process_from_clause(
        &upd.from_clause,
        &mut scope,
        &mut null_ctx,
        snapshot,
        &empty_ctes,
        params,
    )?;

    // Infer param types from SET column = expr — assignment context.
    for target in &upd.target_list {
        if let Some(node::Node::ResTarget(rt)) = target.node.as_ref()
            && let Some(val) = &rt.val
        {
            // Same reasoning as analyze_insert: reject unknown columns up
            // front instead of letting the parameter fall back to text via
            // the UNKNOWN path.
            let tc = table_attrs
                .iter()
                .find(|c| c.attname == rt.name)
                .ok_or_else(|| {
                    AnalyzeError::UndefinedColumn(format!(
                        "column \"{}\" of relation \"{}\" does not exist",
                        rt.name, table_relname,
                    ))
                })?;
            // Catch `UPDATE … SET not_null_col = NULL` statically — PG
            // raises a runtime `null value in column … violates not-null
            // constraint` error, and we can do better by failing the macro
            // at compile time.
            if tc.attnotnull && is_sql_null_literal(val) {
                return Err(AnalyzeError::Invalid(format!(
                    "cannot assign NULL to NOT NULL column `{}.{}`",
                    table_relname, tc.attname,
                )));
            }
            if tc.attgenerated.is_some() && !is_set_to_default(val) {
                return Err(AnalyzeError::Invalid(format!(
                    "generated column `{}.{}` can only be updated to DEFAULT",
                    table_relname, tc.attname,
                )));
            }
            let goal = TypeGoal::assignment(tc.atttypid);
            expr::infer_expr(val, &scope, &null_ctx, snapshot, params, goal)?;

            if let Some(node::Node::ParamRef(p)) = val.node.as_ref()
                && !tc.attnotnull
            {
                params.infer_nullable(p.number, true);
            }
        }
    }

    // WHERE — BOOL goal with assignment coercion.
    if let Some(where_clause) = &upd.where_clause {
        infer_expr_propagate_mismatch(
            where_clause,
            &scope,
            &null_ctx,
            snapshot,
            params,
            TypeGoal::assignment(oid::BOOL),
        )?;
    }

    let columns = resolve_target_list(&upd.returning_list, &scope, &null_ctx, snapshot, params)?;
    Ok((columns, None))
}

fn analyze_delete(
    del: &protobuf::DeleteStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> AnalyzeResult {
    let relation = del
        .relation
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("DELETE without relation".into()))?;

    let table = snapshot
        .resolve_table(
            if relation.schemaname.is_empty() {
                None
            } else {
                Some(&relation.schemaname)
            },
            &relation.relname,
        )
        .ok_or_else(|| {
            AnalyzeError::UndefinedTable(format!(
                "relation \"{}\" does not exist",
                relation.relname
            ))
        })?;

    let table_relname = table.relname.clone();
    let table_nsname = snapshot
        .namespace_name(table.relnamespace)
        .map(str::to_owned)
        .unwrap_or_default();
    let table_attrs = snapshot.attributes_of(table.oid).to_vec();

    let mut scope = Scope::default();
    let null_ctx = NullabilityContext::default();
    scope.add_dml_target(
        &relation.relname,
        crate::qualified_name::QualifiedName::new(&table_nsname, &table_relname),
        &table_attrs,
    );

    // WHERE — BOOL goal with assignment coercion.
    if let Some(where_clause) = &del.where_clause {
        infer_expr_propagate_mismatch(
            where_clause,
            &scope,
            &null_ctx,
            snapshot,
            params,
            TypeGoal::assignment(oid::BOOL),
        )?;
    }

    let columns = resolve_target_list(&del.returning_list, &scope, &null_ctx, snapshot, params)?;
    Ok((columns, None))
}

// ──────────────────────────────────────────────────────────────────────────────
// MERGE (PG 15+)
// ──────────────────────────────────────────────────────────────────────────────

fn analyze_merge(
    merge: &protobuf::MergeStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> AnalyzeResult {
    let relation = merge
        .relation
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("MERGE without relation".into()))?;

    let table = snapshot
        .resolve_table(
            if relation.schemaname.is_empty() {
                None
            } else {
                Some(&relation.schemaname)
            },
            &relation.relname,
        )
        .ok_or_else(|| {
            AnalyzeError::UndefinedTable(format!(
                "relation \"{}\" does not exist",
                relation.relname
            ))
        })?;

    let table_oid = table.oid;
    let table_relname = table.relname.clone();
    let table_nsname = snapshot
        .namespace_name(table.relnamespace)
        .map(str::to_owned)
        .unwrap_or_default();
    let table_attrs = snapshot.attributes_of(table_oid).to_vec();
    let target_alias = relation
        .alias
        .as_ref()
        .map(|a| a.aliasname.as_str())
        .unwrap_or(&relation.relname)
        .to_owned();
    let target_qn = crate::qualified_name::QualifiedName::new(&table_nsname, &table_relname);

    // Process the optional `WITH` clause first (CTEs visible to source +
    // ON + every WHEN branch).
    let mut cte_scopes: HashMap<String, Vec<ScopeColumn>> = HashMap::new();
    if let Some(with) = &merge.with_clause {
        for cte_node in &with.ctes {
            if let Some(node::Node::CommonTableExpr(cte)) = cte_node.node.as_ref() {
                let cte_columns = analyze_cte(cte, with.recursive, snapshot, params, &cte_scopes)?;
                cte_scopes.insert(cte.ctename.clone(), cte_columns);
            }
        }
    }

    // Build the scope shared by `ON`, every `WHEN ... THEN` action, and the
    // `RETURNING` list: target table on the left, source on the right (so
    // the source side is on the nullable arm of an outer-style join in
    // `WHEN NOT MATCHED` rows, but PG handles that NULL semantically — we
    // just need both visible for type/parameter inference).
    let mut scope = Scope::default();
    scope.add_dml_target(&target_alias, target_qn.clone(), &table_attrs);

    let mut null_ctx = NullabilityContext::default();
    if let Some(source_relation) = &merge.source_relation {
        process_from_item(
            source_relation,
            &mut scope,
            &mut null_ctx,
            snapshot,
            &cte_scopes,
            params,
        )?;
    }

    if let Some(join_condition) = &merge.join_condition {
        infer_expr_propagate_mismatch(
            join_condition,
            &scope,
            &null_ctx,
            snapshot,
            params,
            TypeGoal::assignment(oid::BOOL),
        )?;
    }

    for when_node in &merge.merge_when_clauses {
        if let Some(node::Node::MergeWhenClause(when)) = when_node.node.as_ref() {
            walk_merge_when_clause(
                when,
                &scope,
                &null_ctx,
                snapshot,
                params,
                &table_attrs,
                &table_relname,
            )?;
        }
    }

    // RETURNING (PG 17+) sees the target table only — `merge_action()` and
    // source columns are also visible at runtime, but NULL-vs-not depends
    // on which branch fired. Following the existing UPDATE/DELETE style,
    // we project against the target with its base nullability.
    let mut ret_scope = Scope::default();
    ret_scope.add_dml_target(&target_alias, target_qn, &table_attrs);
    let ret_null_ctx = NullabilityContext::default();
    let columns = resolve_target_list(
        &merge.returning_list,
        &ret_scope,
        &ret_null_ctx,
        snapshot,
        params,
    )?;
    Ok((columns, None))
}

fn walk_merge_when_clause(
    when: &protobuf::MergeWhenClause,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    table_attrs: &[crate::pg_catalog::PgAttribute],
    table_relname: &str,
) -> Result<(), AnalyzeError> {
    if let Some(condition) = &when.condition {
        infer_expr_propagate_mismatch(
            condition,
            scope,
            null_ctx,
            snapshot,
            params,
            TypeGoal::assignment(oid::BOOL),
        )?;
    }

    let cmd = CmdType::try_from(when.command_type).unwrap_or(CmdType::Undefined);
    match cmd {
        CmdType::CmdUpdate => {
            // `UPDATE SET col = expr [, ...]` — each entry is a `ResTarget`
            // with `name = column` and `val = expression`. Validate the
            // column exists, then walk the value with an assignment goal.
            for set_item in &when.target_list {
                let Some(node::Node::ResTarget(rt)) = set_item.node.as_ref() else {
                    continue;
                };
                let Some(val) = &rt.val else { continue };
                let tc = table_attrs
                    .iter()
                    .find(|c| c.attname == rt.name)
                    .ok_or_else(|| {
                        AnalyzeError::UndefinedColumn(format!(
                            "column \"{}\" of relation \"{}\" does not exist",
                            rt.name, table_relname,
                        ))
                    })?;
                if tc.attnotnull && is_sql_null_literal(val) {
                    return Err(AnalyzeError::Invalid(format!(
                        "cannot assign NULL to NOT NULL column `{}.{}`",
                        table_relname, tc.attname,
                    )));
                }
                if tc.attgenerated.is_some() && !is_set_to_default(val) {
                    return Err(AnalyzeError::Invalid(format!(
                        "generated column `{}.{}` can only be updated to DEFAULT",
                        table_relname, tc.attname,
                    )));
                }
                expr::infer_expr(
                    val,
                    scope,
                    null_ctx,
                    snapshot,
                    params,
                    TypeGoal::assignment(tc.atttypid),
                )?;
                if let Some(node::Node::ParamRef(p)) = val.node.as_ref()
                    && !tc.attnotnull
                {
                    params.infer_nullable(p.number, true);
                }
            }
        }
        CmdType::CmdInsert => {
            // `INSERT (cols...) VALUES (vals...)` — `target_list` holds the
            // column names (each as a `ResTarget` with `name`), `values`
            // holds the parallel value expressions. When `target_list` is
            // empty PG implies the full attribute list.
            let col_names: Vec<String> = when
                .target_list
                .iter()
                .filter_map(|n| match n.node.as_ref()? {
                    node::Node::ResTarget(rt) if !rt.name.is_empty() => Some(rt.name.clone()),
                    _ => None,
                })
                .collect();
            let target_attrs: Vec<&crate::pg_catalog::PgAttribute> = if col_names.is_empty() {
                table_attrs.iter().collect()
            } else {
                col_names
                    .iter()
                    .map(|name| {
                        table_attrs
                            .iter()
                            .find(|c| &c.attname == name)
                            .ok_or_else(|| {
                                AnalyzeError::UndefinedColumn(format!(
                                    "column \"{}\" of relation \"{}\" does not exist",
                                    name, table_relname,
                                ))
                            })
                    })
                    .collect::<Result<_, _>>()?
            };
            for (i, val) in when.values.iter().enumerate() {
                let target_col = target_attrs.get(i).copied();
                if let Some(tc) = target_col {
                    if tc.attnotnull && is_sql_null_literal(val) {
                        return Err(AnalyzeError::Invalid(format!(
                            "cannot insert NULL into NOT NULL column `{}.{}`",
                            table_relname, tc.attname,
                        )));
                    }
                    if tc.attgenerated.is_some() && !is_set_to_default(val) {
                        return Err(AnalyzeError::Invalid(format!(
                            "cannot insert a non-DEFAULT value into generated column `{}.{}`",
                            table_relname, tc.attname,
                        )));
                    }
                }
                let goal = target_col
                    .map(|tc| TypeGoal::assignment(tc.atttypid))
                    .unwrap_or(TypeGoal::NONE);
                expr::infer_expr(val, scope, null_ctx, snapshot, params, goal)?;
                if let Some(node::Node::ParamRef(p)) = val.node.as_ref()
                    && let Some(tc) = target_col
                    && !tc.attnotnull
                {
                    params.infer_nullable(p.number, true);
                }
            }
        }
        CmdType::CmdDelete | CmdType::CmdNothing => {
            // No target / value expressions to walk beyond the optional
            // `AND condition` already handled above.
        }
        _ => {
            return Err(AnalyzeError::Unsupported(format!(
                "MERGE WHEN command type {:?} is not supported",
                cmd
            )));
        }
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// UNION / INTERSECT / EXCEPT
// ──────────────────────────────────────────────────────────────────────────────

fn analyze_set_operation(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
) -> AnalyzeResult {
    let left = sel
        .larg
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("UNION without left side".into()))?;
    let right = sel
        .rarg
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("UNION without right side".into()))?;

    let (left_cols, _) = analyze_select_with_ctes(left, snapshot, params, cte_scopes)?;
    let (right_cols, _) = analyze_select_with_ctes(right, snapshot, params, cte_scopes)?;

    if left_cols.len() != right_cols.len() {
        return Err(AnalyzeError::Unsupported(
            "UNION branches have different column counts".into(),
        ));
    }

    let mut columns = Vec::with_capacity(left_cols.len());
    for (l, r) in left_cols.into_iter().zip(right_cols) {
        // When both sides carry concrete types (not UNKNOWN), their common
        // type must exist — PG rejects `SELECT 1 UNION SELECT 'x'` with
        // `UNION types integer and text cannot be matched`.
        let common = crate::coerce::find_common_type(&[l.type_oid, r.type_oid], snapshot);
        let both_concrete = l.type_oid != oid::UNKNOWN && r.type_oid != oid::UNKNOWN;
        let type_oid = match (common, both_concrete) {
            (Some(t), _) => t,
            (None, true) => {
                return Err(AnalyzeError::TypeMismatch {
                    actual: type_oid_name(r.type_oid, snapshot),
                    expected: type_oid_name(l.type_oid, snapshot),
                    context: format!("UNION column `{}`", l.name),
                });
            }
            (None, false) => l.type_oid,
        };
        columns.push(RawColumn {
            name: l.name,
            type_oid,
            nullable: l.nullable || r.nullable,
            record_fields: None,
        });
    }

    Ok((columns, None))
}

/// Render a type OID as `schema.name` for user-facing errors.
fn type_oid_name(oid: PgTypeOid, snapshot: &PgCatalog) -> String {
    snapshot
        .get_type(oid)
        .map(|t| {
            let ns = snapshot.namespace_name(t.typnamespace).unwrap_or("?");
            format!("{ns}.{}", t.typname)
        })
        .unwrap_or_else(|| format!("unknown({})", oid.get()))
}

// ──────────────────────────────────────────────────────────────────────────────
// CTE
// ──────────────────────────────────────────────────────────────────────────────

fn analyze_cte(
    cte: &protobuf::CommonTableExpr,
    with_recursive: bool,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    existing_ctes: &HashMap<String, Vec<ScopeColumn>>,
) -> Result<Vec<ScopeColumn>, AnalyzeError> {
    let cte_query = cte
        .ctequery
        .as_ref()
        .and_then(|n| n.node.as_ref())
        .ok_or_else(|| AnalyzeError::Unsupported("CTE without query".into()))?;

    // `WITH RECURSIVE` — the recursive branch references the CTE by name, so
    // we have to seed the scope before analyzing it. pg_query's AST doesn't
    // set `cterecursive` on individual CTEs without full parse analysis, so
    // we rely on the enclosing `WithClause.recursive` flag (true when the
    // user wrote `WITH RECURSIVE`) plus the UNION shape of the inner query.
    // We (1) analyze the seed arm alone to type the CTE's columns,
    // (2) register those columns in a temporary scope, (3) analyze the
    // recursive arm against that scope, (4) unify the two arms' column
    // types via `find_common_type` — matching PG's common-type resolution.
    if with_recursive
        && let node::Node::SelectStmt(sel) = cte_query
        && sel.op != SetOperation::SetopNone as i32
        && let (Some(larg), Some(rarg)) = (sel.larg.as_ref(), sel.rarg.as_ref())
    {
        let (seed_cols, _) = analyze_select_with_ctes(larg, snapshot, params, existing_ctes)?;
        let seed_cols = apply_cte_column_aliases(seed_cols, &cte.aliascolnames);

        // Register the CTE against its seed types so the recursive arm can
        // resolve `FROM t`.
        let mut scopes_with_self = existing_ctes.clone();
        let self_scope: Vec<ScopeColumn> = seed_cols
            .iter()
            .cloned()
            .map(|rc| ScopeColumn {
                name: rc.name,
                type_oid: rc.type_oid,
                base_not_null: !rc.nullable,
                table_alias: cte.ctename.clone(),
                record_fields: rc.record_fields,
            })
            .collect();
        scopes_with_self.insert(cte.ctename.clone(), self_scope);

        let (rec_cols, _) = analyze_select_with_ctes(rarg, snapshot, params, &scopes_with_self)?;
        if seed_cols.len() != rec_cols.len() {
            return Err(AnalyzeError::Unsupported(
                "recursive CTE branches have different column counts".into(),
            ));
        }

        let mut unified: Vec<ScopeColumn> = seed_cols
            .into_iter()
            .zip(rec_cols)
            .map(|(s, r)| {
                let type_oid = crate::coerce::find_common_type(&[s.type_oid, r.type_oid], snapshot)
                    .unwrap_or(s.type_oid);
                ScopeColumn {
                    name: s.name,
                    type_oid,
                    // Either arm producing NULL makes the column nullable.
                    base_not_null: !(s.nullable || r.nullable),
                    table_alias: cte.ctename.clone(),
                    record_fields: s.record_fields,
                }
            })
            .collect();
        append_search_cycle_columns(cte, &mut unified, snapshot);
        return Ok(unified);
    }

    match cte_query {
        node::Node::SelectStmt(sel) => {
            let (cols, _) = analyze_select_with_ctes(sel, snapshot, params, existing_ctes)?;
            let cols = apply_cte_column_aliases(cols, &cte.aliascolnames);
            Ok(cols
                .into_iter()
                .map(|rc| ScopeColumn {
                    name: rc.name,
                    type_oid: rc.type_oid,
                    base_not_null: !rc.nullable,
                    table_alias: cte.ctename.clone(),
                    record_fields: rc.record_fields,
                })
                .collect())
        }
        node::Node::InsertStmt(ins) => {
            let (cols, _) = analyze_insert(ins, snapshot, params)?;
            Ok(cols
                .into_iter()
                .map(|rc| ScopeColumn {
                    name: rc.name,
                    type_oid: rc.type_oid,
                    base_not_null: !rc.nullable,
                    table_alias: cte.ctename.clone(),
                    record_fields: rc.record_fields,
                })
                .collect())
        }
        node::Node::UpdateStmt(upd) => {
            let (cols, _) = analyze_update(upd, snapshot, params)?;
            Ok(cols
                .into_iter()
                .map(|rc| ScopeColumn {
                    name: rc.name,
                    type_oid: rc.type_oid,
                    base_not_null: !rc.nullable,
                    table_alias: cte.ctename.clone(),
                    record_fields: rc.record_fields,
                })
                .collect())
        }
        node::Node::DeleteStmt(del) => {
            let (cols, _) = analyze_delete(del, snapshot, params)?;
            Ok(cols
                .into_iter()
                .map(|rc| ScopeColumn {
                    name: rc.name,
                    type_oid: rc.type_oid,
                    base_not_null: !rc.nullable,
                    table_alias: cte.ctename.clone(),
                    record_fields: rc.record_fields,
                })
                .collect())
        }
        _ => Err(AnalyzeError::Unsupported(
            "CTE with unsupported statement type".into(),
        )),
    }
}

/// Append synthetic columns introduced by `SEARCH BREADTH/DEPTH FIRST BY
/// … SET col` and `CYCLE … SET mark USING path` clauses on a recursive
/// CTE. PG defines each clause as adding one or two named, NOT NULL
/// columns to the CTE's output:
///
/// - `SEARCH BFS BY k SET ord` → `ord record NOT NULL` (a row of
///   `(integer, k...)` PG materializes during recursion).
/// - `CYCLE k SET is_cycle USING path` → `is_cycle <mark_type> NOT NULL`
///   (defaults to bool when the user didn't specify `TO/DEFAULT`) and
///   `path record[] NOT NULL` (an array of `(k...)` rows).
///
/// Without this, downstream `SELECT id, ord, is_cycle, path FROM cte`
/// fails with `column "ord" does not exist`.
fn append_search_cycle_columns(
    cte: &protobuf::CommonTableExpr,
    cols: &mut Vec<ScopeColumn>,
    snapshot: &PgCatalog,
) {
    if let Some(search) = cte.search_clause.as_ref()
        && !search.search_seq_column.is_empty()
    {
        cols.push(ScopeColumn {
            name: search.search_seq_column.clone(),
            type_oid: oid::RECORD,
            base_not_null: true,
            table_alias: cte.ctename.clone(),
            record_fields: None,
        });
    }
    if let Some(cycle) = cte.cycle_clause.as_ref() {
        if !cycle.cycle_mark_column.is_empty() {
            // PG: when `TO/DEFAULT` are omitted the mark column is bool;
            // otherwise the AST exposes the inferred type via `cycle_mark_type`.
            let mark_oid = PgTypeOid::new(cycle.cycle_mark_type).unwrap_or(oid::BOOL);
            cols.push(ScopeColumn {
                name: cycle.cycle_mark_column.clone(),
                type_oid: mark_oid,
                base_not_null: true,
                table_alias: cte.ctename.clone(),
                record_fields: None,
            });
        }
        if !cycle.cycle_path_column.is_empty() {
            // The path column is `record[]` — let `array_type_of(RECORD)`
            // walk the snapshot's `pg_type.typarray` link instead of
            // hardcoding the OID, mirroring how PG resolves the
            // automatic `_record` array type.
            let path_oid = snapshot.array_type_of(oid::RECORD).unwrap_or(oid::UNKNOWN);
            cols.push(ScopeColumn {
                name: cycle.cycle_path_column.clone(),
                type_oid: path_oid,
                base_not_null: true,
                table_alias: cte.ctename.clone(),
                record_fields: None,
            });
        }
    }
}

/// Rename `cols` using the `aliascolnames` from `WITH name(col1, col2) AS …`
/// if present. PG uses positional matching; if the CTE has fewer aliases
/// than columns, the trailing columns keep their inner names.
fn apply_cte_column_aliases(cols: Vec<RawColumn>, aliases: &[protobuf::Node]) -> Vec<RawColumn> {
    if aliases.is_empty() {
        return cols;
    }
    let names: Vec<String> = aliases
        .iter()
        .filter_map(|n| match n.node.as_ref()? {
            node::Node::String(s) => Some(s.sval.clone()),
            _ => None,
        })
        .collect();
    cols.into_iter()
        .enumerate()
        .map(|(i, c)| RawColumn {
            name: names.get(i).cloned().unwrap_or(c.name),
            type_oid: c.type_oid,
            nullable: c.nullable,
            record_fields: c.record_fields,
        })
        .collect()
}

// ──────────────────────────────────────────────────────────────────────────────
// FROM clause processing
// ──────────────────────────────────────────────────────────────────────────────

fn process_from_clause(
    from_clause: &[protobuf::Node],
    scope: &mut Scope,
    null_ctx: &mut NullabilityContext,
    snapshot: &PgCatalog,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    for node in from_clause {
        process_from_item(node, scope, null_ctx, snapshot, cte_scopes, params)?;
    }
    Ok(())
}

fn process_from_item(
    node: &protobuf::Node,
    scope: &mut Scope,
    null_ctx: &mut NullabilityContext,
    snapshot: &PgCatalog,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    let inner = node
        .node
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("empty FROM item".into()))?;

    match inner {
        node::Node::RangeVar(rv) => {
            let alias = rv
                .alias
                .as_ref()
                .map(|a| a.aliasname.as_str())
                .unwrap_or(&rv.relname);

            // Check CTEs first.
            if rv.schemaname.is_empty()
                && let Some(cte_cols) = cte_scopes.get(&rv.relname)
            {
                let cols: Vec<ScopeColumn> = cte_cols
                    .iter()
                    .cloned()
                    .map(|mut c| {
                        c.table_alias = alias.to_owned();
                        c
                    })
                    .collect();
                scope.add_virtual_table(alias, cols);
                return Ok(());
            }

            let schema = if rv.schemaname.is_empty() {
                None
            } else {
                Some(rv.schemaname.as_str())
            };
            scope.add_table(snapshot, schema, &rv.relname, alias)?;
        }
        node::Node::JoinExpr(join) => {
            // Process left and right sides.
            let left_start = scope.sources.len();
            if let Some(larg) = &join.larg {
                process_from_item(larg, scope, null_ctx, snapshot, cte_scopes, params)?;
            }
            let left_end = scope.sources.len();

            if let Some(rarg) = &join.rarg {
                process_from_item(rarg, scope, null_ctx, snapshot, cte_scopes, params)?;
            }
            let right_end = scope.sources.len();

            // Apply JOIN nullability. Fail loudly on unknown join kinds rather
            // than defaulting to INNER, which would silently produce wrong
            // nullability for outer joins the parser couldn't classify.
            let join_type = JoinType::try_from(join.jointype)
                .map_err(|_| AnalyzeError::UnsupportedJoinType(join.jointype))?;

            match join_type {
                JoinType::JoinLeft => {
                    let right_aliases =
                        nullability::collect_aliases(&scope.sources[left_end..right_end]);
                    null_ctx.mark_all_nullable(&right_aliases);
                }
                JoinType::JoinRight => {
                    let left_aliases =
                        nullability::collect_aliases(&scope.sources[left_start..left_end]);
                    null_ctx.mark_all_nullable(&left_aliases);
                }
                JoinType::JoinFull => {
                    let all_aliases =
                        nullability::collect_aliases(&scope.sources[left_start..right_end]);
                    null_ctx.mark_all_nullable(&all_aliases);
                }
                JoinType::JoinInner => {} // No nullability change.
                other => return Err(AnalyzeError::UnsupportedJoinType(other as i32)),
            }
        }
        node::Node::RangeSubselect(sub) => {
            let alias = sub
                .alias
                .as_ref()
                .map(|a| a.aliasname.as_str())
                .unwrap_or("_subquery");

            // `AS foo(a, b, c)` overrides the subquery's own output names.
            // Common in information_schema views that rename columns at the
            // FROM boundary instead of in the SELECT list.
            let col_aliases: Vec<String> = sub
                .alias
                .as_ref()
                .map(|a| {
                    a.colnames
                        .iter()
                        .filter_map(|n| match n.node.as_ref()? {
                            node::Node::String(s) => Some(s.sval.clone()),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();

            if let Some(subquery) = &sub.subquery
                && let Some(node::Node::SelectStmt(sel)) = subquery.node.as_ref()
            {
                // A LATERAL subquery inherits the visible FROM items to its
                // left — including the enclosing SELECT's scope we already
                // built — so column refs like `s.oid` inside
                // `JOIN LATERAL (… s.oid …)` resolve properly.
                let lateral_sources: Vec<_> = if sub.lateral {
                    scope.sources.clone()
                } else {
                    Vec::new()
                };
                let (cols, _) = analyze_select_with_ctes_and_outer(
                    sel,
                    snapshot,
                    params,
                    cte_scopes,
                    &lateral_sources,
                    &[],
                )?;
                let mut scope_cols: Vec<ScopeColumn> = cols
                    .into_iter()
                    .map(|rc| ScopeColumn {
                        name: rc.name,
                        type_oid: rc.type_oid,
                        base_not_null: !rc.nullable,
                        table_alias: alias.to_owned(),
                        record_fields: rc.record_fields,
                    })
                    .collect();
                for (i, alias_name) in col_aliases.iter().enumerate() {
                    if let Some(c) = scope_cols.get_mut(i) {
                        c.name = alias_name.clone();
                    }
                }
                scope.add_virtual_table(alias, scope_cols);
            }
        }
        node::Node::RangeFunction(rf) => {
            process_range_function(rf, scope, snapshot, params)?;
        }
        node::Node::RangeTableSample(ts) => {
            // `TABLESAMPLE` only changes how rows are picked at runtime —
            // it does not affect the relation's column shape or
            // nullability. Pass through to the wrapped `relation` and
            // ignore method/args/repeatable.
            let relation = ts.relation.as_ref().ok_or_else(|| {
                AnalyzeError::Unsupported("RangeTableSample without relation".into())
            })?;
            return process_from_item(relation, scope, null_ctx, snapshot, cte_scopes, params);
        }
        _ => {
            return Err(AnalyzeError::Unsupported(format!(
                "FROM item type: {:?}",
                std::mem::discriminant(inner)
            )));
        }
    }
    Ok(())
}

/// `FROM func(args)` — resolve the SRF and populate `scope` with its output
/// columns. Handles three cases:
/// - Function has `out_args` (TABLE/OUT) → one scope column per out_arg.
/// - Function returns a registered composite type → expand the composite's
///   fields as scope columns.
/// - Otherwise (scalar or plain `record`) → a single scope column named after
///   the function, typed with its return OID.
///
/// Also honors `WITH ORDINALITY` by adding a trailing `ordinality BIGINT NOT NULL`
/// column when the flag is set.
fn process_range_function(
    rf: &protobuf::RangeFunction,
    scope: &mut Scope,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    // PG always treats a function-call FROM item as LATERAL: the
    // `LATERAL` keyword on a `RangeFunction` is a noise word, because
    // the args can already refer to earlier FROM items implicitly. So
    // we copy the visible sources unconditionally, not just when
    // `rf.lateral` is set. We also propagate `outer_sources` so SRF
    // args inside a correlated sublink can reach aliases bound by the
    // enclosing query (e.g. `pg_stats_ext` does
    // `(SELECT … FROM unnest(s.stxkeys) …)` where `s` is from the outer
    // FROM).
    let arg_scope_sources = scope.sources.clone();
    let arg_scope_outer = scope.outer_sources.clone();
    let _ = rf.lateral;
    // Each entry in `functions` is a 2-element `List` — [FuncCall, coldeflist].
    // We support only the simple form: a single function call, no explicit
    // column definitions. `ROWS FROM (…)` with multiple functions or user-
    // supplied coldeflists are rarer and fall through to Unsupported so we
    // don't silently lose column shape.
    let list = rf
        .functions
        .first()
        .and_then(|n| n.node.as_ref())
        .and_then(|n| {
            if let node::Node::List(l) = n {
                Some(l)
            } else {
                None
            }
        })
        .ok_or_else(|| AnalyzeError::Unsupported("RangeFunction without function call".into()))?;

    let func_call_node = list
        .items
        .first()
        .ok_or_else(|| AnalyzeError::Unsupported("RangeFunction function list is empty".into()))?;
    let func_call = match func_call_node.node.as_ref() {
        Some(node::Node::FuncCall(fc)) => fc,
        _ => {
            return Err(AnalyzeError::Unsupported(
                "RangeFunction item is not a FuncCall".into(),
            ));
        }
    };

    // Alias: `FROM f() AS t(col1, col2)` gives aliases both for the relation
    // and for its columns. Fall back to the function's last name component.
    let func_name_parts = expr::extract_string_fields(&func_call.funcname);
    let default_alias = func_name_parts
        .last()
        .cloned()
        .unwrap_or_else(|| "_srf".into());
    let alias_owned = rf
        .alias
        .as_ref()
        .map(|a| a.aliasname.clone())
        .unwrap_or_else(|| default_alias.clone());
    let alias = alias_owned.as_str();
    let col_aliases: Vec<String> = rf
        .alias
        .as_ref()
        .map(|a| {
            a.colnames
                .iter()
                .filter_map(|n| match n.node.as_ref()? {
                    node::Node::String(s) => Some(s.sval.clone()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();

    // Infer arg types so overload resolution can pick the right function.
    // Non-LATERAL SRF args can't see the enclosing FROM; LATERAL args can.
    let mut arg_scope = Scope::default();
    arg_scope.sources.extend(arg_scope_sources);
    arg_scope.outer_sources.extend(arg_scope_outer);
    let empty_null_ctx = NullabilityContext::default();
    let mut arg_types = Vec::with_capacity(func_call.args.len());
    let mut arg_nullable = Vec::with_capacity(func_call.args.len());
    for arg in &func_call.args {
        let (t, n) = match expr::infer_expr(
            arg,
            &arg_scope,
            &empty_null_ctx,
            snapshot,
            params,
            crate::expr::TypeGoal::NONE,
        ) {
            Ok(e) => (e.type_oid, e.nullable),
            // `FROM a, f(a.col)` without LATERAL — PG rejects with `invalid
            // reference to FROM-clause entry for table "a"`. The scope we
            // built above is empty precisely so this fails; don't let the
            // old `.unwrap_or(UNKNOWN)` swallow it.
            Err(e @ AnalyzeError::UndefinedColumn(_)) if !rf.lateral => return Err(e),
            Err(_) => (oid::UNKNOWN, true),
        };
        arg_types.push(t);
        arg_nullable.push(n);
    }
    let any_arg_nullable = arg_nullable.iter().any(|&n| n);

    let (schema, name) = match func_name_parts.as_slice() {
        [n] => (None, n.as_str()),
        [s, n] => (Some(s.as_str()), n.as_str()),
        _ => {
            return Err(AnalyzeError::UndefinedFunction(format!(
                "invalid function name in FROM: {func_name_parts:?}"
            )));
        }
    };

    // `unnest(arr1, arr2, …)` in FROM is a special PG-only multi-array form
    // (parsed as a regular FuncCall but transformed by PG into ROWS FROM
    // (unnest(arr1), unnest(arr2), …)). Each argument contributes one column
    // of its element type, aligned row-wise (zip with NULL-padding).
    let is_pg_unnest = (schema.is_none() || schema == Some("pg_catalog")) && name == "unnest";
    let mut cols: Vec<ScopeColumn> = if is_pg_unnest && arg_types.len() > 1 {
        let mut col_specs = Vec::with_capacity(arg_types.len());
        for (i, (&type_oid, &nullable)) in arg_types.iter().zip(arg_nullable.iter()).enumerate() {
            let type_entry =
                snapshot
                    .get_type(type_oid)
                    .ok_or_else(|| AnalyzeError::UndefinedType {
                        oid: type_oid.get(),
                        context: format!("unnest argument {}", i + 1),
                    })?;
            let elem = (type_entry.typcategory == TypCategory::Array)
                .then_some(type_entry.typelem)
                .flatten()
                .ok_or_else(|| AnalyzeError::TypeMismatch {
                    actual: type_entry.typname.clone(),
                    expected: "array".into(),
                    context: format!("unnest argument {} must be an array", i + 1),
                })?;
            // Multi-arg unnest is strict: each output column is NOT NULL iff
            // the corresponding input array is NOT NULL (a NULL array yields
            // a single NULL row, not zero rows). Out-of-bounds positions are
            // padded with NULLs because the arrays may have different
            // lengths, so each column is conservatively nullable when any
            // *other* arg is shorter — but we can only see that at runtime.
            // Match PG's behavior: NOT NULL only when the array itself is.
            col_specs.push(ScopeColumn {
                name: "unnest".to_owned(),
                type_oid: elem,
                base_not_null: !nullable,
                table_alias: alias.to_owned(),
                record_fields: None,
            });
        }
        col_specs
    } else {
        let resolved = functions::resolve_function(snapshot, schema, name, &arg_types, false)?;

        // Build the scope columns.
        if !resolved.out_args.is_empty() {
            resolved
                .out_args
                .iter()
                .map(|f| ScopeColumn {
                    name: f.name.clone(),
                    type_oid: f.type_oid,
                    base_not_null: f.not_null,
                    table_alias: alias.to_owned(),
                    record_fields: None,
                })
                .collect()
        } else if let Some(typrelid) = snapshot.get_type(resolved.return_type_oid).and_then(|t| {
            (t.typtype == TypType::Composite)
                .then_some(t.typrelid)
                .flatten()
        }) {
            snapshot
                .attributes_of(typrelid)
                .iter()
                .map(|f| ScopeColumn {
                    name: f.attname.clone(),
                    type_oid: f.atttypid,
                    base_not_null: f.attnotnull,
                    table_alias: alias.to_owned(),
                    record_fields: None,
                })
                .collect()
        } else {
            // Strict pg_catalog SRFs (e.g. `unnest`) propagate NOT NULL from
            // their arguments — `FROM unnest(int4[] NOT NULL)` produces
            // NOT NULL int4 elements, just like `SELECT unnest(arr)` in
            // the projection.
            let strict_not_null =
                resolved.is_strict && resolved.schema == "pg_catalog" && !any_arg_nullable;
            vec![ScopeColumn {
                name: name.to_owned(),
                type_oid: resolved.return_type_oid,
                base_not_null: strict_not_null,
                table_alias: alias.to_owned(),
                record_fields: None,
            }]
        }
    };

    // WITH ORDINALITY appends a trailing BIGINT NOT NULL row number. Do this
    // before the alias override so `AS t(val, ord)` can rename the ordinality
    // column too.
    if rf.ordinality {
        cols.push(ScopeColumn {
            name: "ordinality".into(),
            type_oid: oid::INT8,
            base_not_null: true,
            table_alias: alias.to_owned(),
            record_fields: None,
        });
    }

    // User-supplied column aliases override the names above, in order.
    for (i, alias_name) in col_aliases.iter().enumerate() {
        if let Some(c) = cols.get_mut(i) {
            c.name = alias_name.clone();
        }
    }

    scope.add_virtual_table(alias, cols);
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// Target list (SELECT columns)
// ──────────────────────────────────────────────────────────────────────────────

fn resolve_target_list(
    target_list: &[protobuf::Node],
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> Result<Vec<RawColumn>, AnalyzeError> {
    let mut columns = Vec::new();

    for (i, target) in target_list.iter().enumerate() {
        let rt = match target.node.as_ref() {
            Some(node::Node::ResTarget(rt)) => rt,
            _ => continue,
        };

        let val = match &rt.val {
            Some(v) => v,
            None => continue,
        };

        // Check for SELECT * or t.*.
        if let Some(node::Node::ColumnRef(cr)) = val.node.as_ref()
            && cr
                .fields
                .iter()
                .any(|f| matches!(f.node.as_ref(), Some(node::Node::AStar(_))))
        {
            // Star expansion.
            let table_filter = cr.fields.iter().find_map(|f| match f.node.as_ref()? {
                node::Node::String(s) => Some(s.sval.as_str()),
                _ => None,
            });

            let star_cols: Vec<&ScopeColumn> = if let Some(tbl) = table_filter {
                scope
                    .sources
                    .iter()
                    .filter(|s| s.alias == tbl)
                    .flat_map(|s| s.columns.iter())
                    .collect()
            } else {
                scope.all_columns()
            };

            for col in star_cols {
                let nullable = null_ctx.is_nullable(&col.table_alias, &col.name, col.base_not_null);
                columns.push(RawColumn {
                    name: col.name.clone(),
                    type_oid: col.type_oid,
                    nullable,
                    record_fields: col.record_fields.clone(),
                });
            }
            continue;
        }

        // No type expectation for SELECT expressions.
        let expr_type = expr::infer_expr(val, scope, null_ctx, snapshot, params, TypeGoal::NONE)?;

        // Determine column name: explicit alias, or inferred from expression.
        let name = if !rt.name.is_empty() {
            rt.name.clone()
        } else {
            infer_column_name(val).unwrap_or_else(|| format!("_column{i}_"))
        };

        // Inferred shape from the expression (ROW(...), nested indirection,
        // column propagation) takes priority. As a fallback, if the target
        // expression is a direct FuncCall with TABLE/OUT args, lift those so
        // downstream `(alias.col).field` can look them up — this covers the
        // SRF-as-target-list case where the expression itself is the call.
        let record_fields = expr_type.record_fields.or_else(|| {
            if let Some(node::Node::FuncCall(fc)) = val.node.as_ref() {
                resolve_funccall_record_fields(fc, snapshot, params)
            } else {
                None
            }
        });

        // Bare string literals are carried as `text` at the target-list
        // boundary — this matches PG's `select_common_type` behavior at
        // the SELECT output level, and (more importantly) gives UNION /
        // subquery reconciliation a concrete type to compare against so
        // `SELECT 1 UNION SELECT 'x'` fails instead of silently coercing.
        let type_oid = expr::unknown_literal_as_text(Some(val), expr_type.type_oid);

        columns.push(RawColumn {
            name,
            type_oid,
            nullable: expr_type.nullable,
            record_fields,
        });
    }

    Ok(columns)
}

/// Analyze a `VALUES (…), (…)` list. Each row must have the same arity;
/// column types are unified across rows via `coerce::find_common_type`.
/// Nullability is `true` if any row's element at that position is nullable.
fn analyze_values_lists(
    values_lists: &[protobuf::Node],
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> Result<Vec<RawColumn>, AnalyzeError> {
    // Each entry in `values_lists` is a `List` of per-column expressions for
    // one row. An empty VALUES list would be a grammar error in PG, but we
    // guard anyway for robustness.
    let first = values_lists
        .iter()
        .find_map(|n| match n.node.as_ref()? {
            node::Node::List(l) => Some(l),
            _ => None,
        })
        .ok_or_else(|| AnalyzeError::Unsupported("empty VALUES list".into()))?;

    let arity = first.items.len();
    let empty_scope = Scope::default();
    let empty_null = NullabilityContext::default();

    let mut column_types: Vec<Vec<PgTypeOid>> = vec![Vec::new(); arity];
    let mut column_nullable: Vec<bool> = vec![false; arity];

    for row_node in values_lists {
        let Some(node::Node::List(row)) = row_node.node.as_ref() else {
            continue;
        };
        for (i, item) in row.items.iter().enumerate() {
            if i >= arity {
                break;
            }
            let t = expr::infer_expr(
                item,
                &empty_scope,
                &empty_null,
                snapshot,
                params,
                TypeGoal::NONE,
            )?;
            column_types[i].push(t.type_oid);
            column_nullable[i] |= t.nullable;
        }
    }

    let columns = (0..arity)
        .map(|i| RawColumn {
            name: format!("column{}", i + 1),
            type_oid: crate::coerce::find_common_type(&column_types[i], snapshot)
                .unwrap_or(oid::UNKNOWN),
            nullable: column_nullable[i],
            record_fields: None,
        })
        .collect();

    Ok(columns)
}

/// Look up the named output columns (TABLE/OUT args) of `fc`, if any.
/// Used by the target-list walker so a `SELECT _pg_expandarray(…) AS x`
/// records the field list on the produced column.
fn resolve_funccall_record_fields(
    fc: &protobuf::FuncCall,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> Option<Vec<crate::expr::RecordField>> {
    let parts = expr::extract_string_fields(&fc.funcname);
    let (schema, name) = match parts.as_slice() {
        [n] => (None, n.as_str()),
        [s, n] => (Some(s.as_str()), n.as_str()),
        _ => return None,
    };
    // Args inferred in an empty scope — we only need their types to drive
    // overload resolution, and FuncCall args don't see the enclosing FROM.
    let empty_scope = Scope::default();
    let empty_null = NullabilityContext::default();
    let mut arg_types = Vec::with_capacity(fc.args.len());
    for a in &fc.args {
        let t = expr::infer_expr(
            a,
            &empty_scope,
            &empty_null,
            snapshot,
            params,
            TypeGoal::NONE,
        )
        .map(|e| e.type_oid)
        .unwrap_or(oid::UNKNOWN);
        arg_types.push(t);
    }
    let resolved = functions::resolve_function(snapshot, schema, name, &arg_types, false).ok()?;
    if resolved.out_args.is_empty() {
        None
    } else {
        Some(crate::expr::RecordField::from_out_args(&resolved.out_args))
    }
}

/// Try to infer a default column name from an expression (for unaliased columns).
fn infer_column_name(node: &protobuf::Node) -> Option<String> {
    match node.node.as_ref()? {
        node::Node::ColumnRef(cr) => {
            // Last string field is the column name.
            expr::extract_string_fields(&cr.fields).pop()
        }
        node::Node::FuncCall(fc) => {
            // Function name.
            expr::extract_string_fields(&fc.funcname).pop()
        }
        _ => None,
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Type resolution
// ──────────────────────────────────────────────────────────────────────────────

fn build_column(rc: RawColumn, snapshot: &PgCatalog) -> Result<AnalyzedColumn, AnalyzeError> {
    let pg_type = resolve_type_with_shape(rc.type_oid, rc.record_fields.as_deref(), snapshot)?;

    // Handle nullability annotations (! and ?).
    let (name, nullable) = parse_nullability_annotation(&rc.name, rc.nullable);

    Ok(AnalyzedColumn {
        name,
        pg_type,
        nullable,
    })
}

/// Like [`resolve_type`] but lets the caller override the structural shape
/// when the OID is the pseudo `record` type (typmod -1 in PG terms). When
/// `shape` is `Some` and the OID is `record`, we build a `Type::AnonymousRecord`
/// from the shape recursively. Otherwise falls through to the OID-only path.
fn resolve_type_with_shape(
    type_oid: PgTypeOid,
    shape: Option<&[crate::expr::RecordField]>,
    snapshot: &PgCatalog,
) -> Result<Type, AnalyzeError> {
    if type_oid == oid::RECORD
        && let Some(fields) = shape
    {
        let mut out = Vec::with_capacity(fields.len());
        for f in fields {
            out.push(crate::types::RecordField {
                name: f.name.clone(),
                ty: resolve_type_with_shape(
                    f.ty.type_oid,
                    f.ty.record_fields.as_deref(),
                    snapshot,
                )?,
                nullable: f.ty.nullable,
            });
        }
        return Ok(Type::AnonymousRecord { fields: out });
    }
    resolve_type(type_oid, snapshot)
}

fn build_param_info(
    type_oid: PgTypeOid,
    nullable: bool,
    snapshot: &PgCatalog,
) -> Result<ParamInfo, AnalyzeError> {
    let pg_type = resolve_type(type_oid, snapshot)?;
    Ok(ParamInfo { pg_type, nullable })
}

/// Build the PG-facing [`Type`] for an OID, recursing through Domain/Array
/// wrappers. Unknown OIDs (pseudo `UNKNOWN` included) are surfaced as
/// [`Type::Basic`] named `pg_catalog.unknown` so consumers can fall back to
/// `String` without the analyzer having to know about Rust.
fn resolve_type(type_oid: PgTypeOid, snapshot: &PgCatalog) -> Result<Type, AnalyzeError> {
    if let Some(te) = snapshot.get_type(type_oid) {
        let schema = snapshot
            .namespace_name(te.typnamespace)
            .map(str::to_owned)
            .unwrap_or_else(|| "pg_catalog".to_owned());
        let name = te.typname.clone();
        let extension = snapshot.extension_of_type(type_oid).map(str::to_owned);

        // Arrays first: in PG, `_int4` is typtype=Base + typcategory=Array +
        // typelem=int4. They aren't a separate `typtype`.
        if te.typcategory == TypCategory::Array
            && let Some(elem) = te.typelem
        {
            let element = resolve_type(elem, snapshot)?;
            return Ok(Type::Array {
                element: Box::new(element),
            });
        }

        match te.typtype {
            TypType::Domain => {
                let base_oid = te.typbasetype.ok_or_else(|| AnalyzeError::UndefinedType {
                    oid: type_oid.get(),
                    context: "domain base type".into(),
                })?;
                let base = resolve_type(base_oid, snapshot)?;
                return Ok(Type::Domain {
                    schema,
                    name,
                    base: Box::new(base),
                    extension,
                });
            }
            TypType::Enum => {
                let labels = snapshot
                    .enum_labels_of(type_oid)
                    .into_iter()
                    .map(str::to_owned)
                    .collect();
                return Ok(Type::Enum {
                    schema,
                    name,
                    labels,
                    extension,
                });
            }
            TypType::Range | TypType::Multirange => {
                let subtype_oid = snapshot
                    .pg_range
                    .get(&type_oid)
                    .map(|r| r.rngsubtype)
                    .unwrap_or(oid::UNKNOWN);
                let subtype = resolve_type(subtype_oid, snapshot)?;
                return Ok(Type::Range {
                    schema,
                    name,
                    subtype: Box::new(subtype),
                    extension,
                });
            }
            TypType::Composite => {
                let attrs = if let Some(relid) = te.typrelid {
                    snapshot.attributes_of(relid).to_vec()
                } else {
                    Vec::new()
                };
                let mut out = Vec::with_capacity(attrs.len());
                for f in &attrs {
                    out.push(crate::types::RecordField {
                        name: f.attname.clone(),
                        ty: resolve_type(f.atttypid, snapshot)?,
                        nullable: !f.attnotnull,
                    });
                }
                return Ok(Type::AnonymousRecord { fields: out });
            }
            TypType::Base | TypType::Pseudo => {
                return Ok(Type::Basic {
                    schema,
                    name,
                    extension,
                });
            }
        }
    }

    // Fallback for the pseudo UNKNOWN OID when not present in the snapshot.
    if type_oid == oid::UNKNOWN {
        return Ok(Type::Basic {
            schema: "pg_catalog".to_owned(),
            name: "unknown".to_owned(),
            extension: None,
        });
    }

    Err(AnalyzeError::UndefinedType {
        oid: type_oid.get(),
        context: format!("OID {}", type_oid.get()),
    })
}

fn parse_nullability_annotation(name: &str, auto_nullable: bool) -> (String, bool) {
    if let Some(stripped) = name.strip_suffix('!') {
        (stripped.to_owned(), false)
    } else if let Some(stripped) = name.strip_suffix('?') {
        (stripped.to_owned(), true)
    } else {
        (name.to_owned(), auto_nullable)
    }
}
