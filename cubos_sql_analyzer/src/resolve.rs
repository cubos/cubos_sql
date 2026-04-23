//! Top-level query analysis: lex the SQL template, parse it, walk the AST,
//! and produce an [`AnalyzedQuery`] combining lexer positions with inferred
//! types.

use std::collections::HashMap;

use pg_query::protobuf::{self, JoinType, SetOperation, node};

use crate::error::AnalyzeError;
use crate::expr::{self, TypeGoal};
use crate::functions;
use crate::nullability::{self, NullabilityContext};
use crate::param::LexOutput;
use crate::param_collector::ParamCollector;
use crate::qualified_name::QualifiedName;
use crate::schema::{SchemaSnapshot, TypeKind};
use crate::scope::{Scope, ScopeColumn};
use crate::type_map::{self, oid};

/// Internal parameter representation produced by [`analyze_static`] before
/// being fused with lexer-side info (name, sql offsets) into [`AnalyzedParam`].
pub(crate) struct ParamInfo {
    pub pg_type_oid: u32,
    pub rust_type: String,
    pub nullable: bool,
    pub domain_rust_type: Option<String>,
    pub enum_rust_type: Option<String>,
    pub cast_type: Option<String>,
}

// ──────────────────────────────────────────────────────────────────────────────
// Public API
// ──────────────────────────────────────────────────────────────────────────────

/// Rust-type mappings for user-defined PostgreSQL types.
///
/// Values are fully-qualified Rust type paths
/// (e.g. `"crate::domains::UserPreferences"`) that will be emitted verbatim
/// into generated code.
///
/// Keys are [`QualifiedName`]s — always schema-qualified. When deserializing
/// from TOML/JSON, keys go through PostgreSQL's identifier-lexer rules: use
/// `public.vector` for pgvector's `vector` type, or
/// `"My Schema"."My Table"` to escape identifiers containing special
/// characters. See [`QualifiedName`] for the full quoting grammar.
///
/// Unqualified names like `"vector"` are rejected: always qualify with the
/// schema that owns the type (usually `public` for extensions).
#[derive(Debug, Clone, Default)]
pub struct AnalyzerConfig {
    /// Mappings for JSONB-backed domain types. The value is the Rust struct
    /// that `serde_json` will (de)serialize the JSONB payload to.
    pub domains: HashMap<QualifiedName, String>,
    /// Mappings for enum types. The value is the Rust enum that implements
    /// `ToString` / `FromStr` for the SQL labels.
    pub enums: HashMap<QualifiedName, String>,
    /// Mappings for other custom types (e.g. pgvector's `vector`).
    pub types: HashMap<QualifiedName, String>,
}

/// A single output column of an analyzed query.
#[derive(Debug, Clone)]
pub struct AnalyzedColumn {
    pub name: String,
    pub pg_type_oid: u32,
    pub rust_type: String,
    pub nullable: bool,
    pub domain_rust_type: Option<String>,
    pub enum_rust_type: Option<String>,
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
    pub pg_type_oid: u32,
    pub rust_type: String,
    pub nullable: bool,
    pub domain_rust_type: Option<String>,
    pub enum_rust_type: Option<String>,
    /// PostgreSQL type name for explicit cast (`::jsonb`, `::int8`, …).
    pub cast_type: Option<String>,
}

/// A field inside a spread parameter (`$..name { field1, field2 }`), with
/// inferred type.
#[derive(Debug, Clone)]
pub struct AnalyzedSpreadField {
    pub name: String,
    pub pg_type_oid: u32,
    pub rust_type: String,
    pub nullable: bool,
    pub domain_rust_type: Option<String>,
    pub enum_rust_type: Option<String>,
    pub cast_type: Option<String>,
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
            pg_type_oid: pi.pg_type_oid,
            rust_type: pi.rust_type.clone(),
            nullable: pi.nullable,
            domain_rust_type: pi.domain_rust_type.clone(),
            enum_rust_type: pi.enum_rust_type.clone(),
            cast_type: pi.cast_type.clone(),
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
                pg_type_oid: pi.pg_type_oid,
                rust_type: pi.rust_type.clone(),
                nullable: pi.nullable,
                domain_rust_type: pi.domain_rust_type.clone(),
                enum_rust_type: pi.enum_rust_type.clone(),
                cast_type: pi.cast_type.clone(),
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
    snapshot: &SchemaSnapshot,
    sql: &str,
    config: &AnalyzerConfig,
    param_nullability: &[Option<bool>],
) -> Result<(Vec<AnalyzedColumn>, Vec<ParamInfo>), AnalyzeError> {
    let parsed = pg_query::parse(sql).map_err(|e| AnalyzeError::Parse(e.to_string()))?;

    let stmt = parsed
        .protobuf
        .stmts
        .first()
        .and_then(|s| s.stmt.as_ref())
        .and_then(|n| n.node.as_ref())
        .ok_or_else(|| AnalyzeError::Parse("empty statement".into()))?;

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
        _ => {
            return Err(AnalyzeError::Unsupported(format!(
                "statement type: {:?}",
                std::mem::discriminant(stmt)
            )));
        }
    };

    let columns = raw_columns
        .into_iter()
        .map(|rc| build_column(rc, snapshot, config))
        .collect::<Result<Vec<_>, _>>()?;

    let param_list = match raw_params {
        Some(p) => p,
        None => params.into_sorted()?,
    };
    let params_info = param_list
        .into_iter()
        .map(|(_, type_oid, nullable)| build_param_info(type_oid, nullable, snapshot, config))
        .collect::<Result<Vec<_>, _>>()?;

    Ok((columns, params_info))
}

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Infer an expression type, propagating only `TypeMismatch` errors.
///
/// Other errors (e.g. `UnknownColumn` from correlated subqueries referencing
/// outer scope) are swallowed — they represent pre-existing analyzer
/// limitations, not user errors.
fn infer_expr_propagate_mismatch(
    node: &protobuf::Node,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
    goal: TypeGoal,
) -> Result<(), AnalyzeError> {
    match expr::infer_expr(node, scope, null_ctx, snapshot, params, goal) {
        Ok(_) => Ok(()),
        Err(e @ AnalyzeError::TypeMismatch { .. }) => Err(e),
        Err(_) => Ok(()), // Swallow non-type-mismatch errors.
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Raw output types (before Rust type mapping)
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) struct RawColumn {
    pub name: String,
    pub type_oid: u32,
    pub nullable: bool,
    /// When the column is produced by a call to an SRF / OUT-arg function
    /// that returns `record`, this carries the named output columns. Lets
    /// downstream `(x).field` on a subquery-produced column resolve without
    /// re-running the analyzer — we just look the field up here.
    pub record_fields: Option<Vec<crate::schema::CompositeField>>,
}

/// Return type for analyze_* functions: columns + optional pre-sorted params.
type AnalyzeResult = Result<(Vec<RawColumn>, Option<Vec<(i32, u32, bool)>>), AnalyzeError>;

// ──────────────────────────────────────────────────────────────────────────────
// SELECT
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn analyze_select(
    sel: &protobuf::SelectStmt,
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
) -> AnalyzeResult {
    analyze_select_with_ctes_and_outer(sel, snapshot, params, &HashMap::new(), &[])
}

fn analyze_select_with_ctes(
    sel: &protobuf::SelectStmt,
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
) -> AnalyzeResult {
    analyze_select_with_ctes_and_outer(sel, snapshot, params, outer_ctes, &[])
}

/// Core SELECT analyzer.
///
/// `outer_sources` seeds the initial scope with pre-visible table sources
/// (non-empty only for `LATERAL` subqueries, which inherit the outer FROM
/// clause's scope per PG's LATERAL semantics). Empty for regular SELECTs
/// and CTEs, which start with an empty scope.
fn analyze_select_with_ctes_and_outer(
    sel: &protobuf::SelectStmt,
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
    outer_sources: &[crate::scope::TableSource],
) -> AnalyzeResult {
    // Start with outer CTEs (from parent WITH clause).
    let mut cte_scopes: HashMap<String, Vec<ScopeColumn>> = outer_ctes.clone();

    // Process this SELECT's own CTEs (before UNION check, since WITH wraps UNION).
    if let Some(with) = &sel.with_clause {
        for cte_node in &with.ctes {
            if let Some(node::Node::CommonTableExpr(cte)) = cte_node.node.as_ref() {
                let cte_columns = analyze_cte(cte, snapshot, params, &cte_scopes)?;
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
    // LATERAL subqueries see their enclosing FROM clause's sources as if
    // they were already in scope. We seed those up-front so column/row refs
    // resolve normally through the rest of the SELECT analysis.
    scope.sources.extend(outer_sources.iter().cloned());
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

    // Process WHERE clause — PG uses COERCION_ASSIGNMENT + BOOL goal.
    if let Some(where_clause) = &sel.where_clause {
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
    for group_node in &sel.group_clause {
        let _ = expr::infer_expr(
            group_node,
            &scope,
            &null_ctx,
            snapshot,
            params,
            TypeGoal::NONE,
        );
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

// ──────────────────────────────────────────────────────────────────────────────
// INSERT / UPDATE / DELETE
// ──────────────────────────────────────────────────────────────────────────────

fn analyze_insert(
    ins: &protobuf::InsertStmt,
    snapshot: &SchemaSnapshot,
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
        .ok_or_else(|| AnalyzeError::UnknownRelation(relation.relname.clone()))?;

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
                    for (i, val) in list.items.iter().enumerate() {
                        let goal = col_names
                            .get(i)
                            .and_then(|cn| table.columns.iter().find(|c| &c.name == cn))
                            .map(|tc| TypeGoal::assignment(tc.type_oid))
                            .unwrap_or(TypeGoal::NONE);
                        expr::infer_expr(val, &scope, &null_ctx, snapshot, params, goal)?;

                        // Infer nullable from column definition.
                        if let Some(node::Node::ParamRef(p)) = val.node.as_ref()
                            && let Some(col_name) = col_names.get(i)
                            && let Some(tc) = table.columns.iter().find(|c| &c.name == col_name)
                            && !tc.not_null
                        {
                            params.infer_nullable(p.number, true);
                        }
                    }
                }
            }
        } else {
            // INSERT ... SELECT — analyze the SELECT for param inference.
            let _ = analyze_select(val_sel, snapshot, params);

            // Back-fill ParamRef targets with column types from INSERT columns.
            // We only handle direct ParamRef (not complex expressions like p.id)
            // because the SELECT analysis above already inferred types within
            // its own scope.
            for (i, target) in val_sel.target_list.iter().enumerate() {
                if let Some(node::Node::ResTarget(rt)) = target.node.as_ref()
                    && let Some(val) = &rt.val
                    && let Some(node::Node::ParamRef(p)) = val.node.as_ref()
                    && let Some(col_name) = col_names.get(i)
                    && let Some(tc) = table.columns.iter().find(|c| &c.name == col_name)
                {
                    if params.get(p.number) == oid::UNKNOWN {
                        params.record(p.number, tc.type_oid);
                    }
                    if !tc.not_null {
                        params.infer_nullable(p.number, true);
                    }
                }
            }
        }
    }

    // Resolve RETURNING list.
    let mut ret_scope = Scope::default();
    let ret_null_ctx = NullabilityContext::default();
    ret_scope.add_dml_target(
        &relation.relname,
        crate::qualified_name::QualifiedName::new(&table.schema, &table.name),
        &table.columns,
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
    snapshot: &SchemaSnapshot,
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
        .ok_or_else(|| AnalyzeError::UnknownRelation(relation.relname.clone()))?;

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
        crate::qualified_name::QualifiedName::new(&table.schema, &table.name),
        &table.columns,
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
            let goal = table
                .columns
                .iter()
                .find(|c| c.name == rt.name)
                .map(|tc| TypeGoal::assignment(tc.type_oid))
                .unwrap_or(TypeGoal::NONE);
            expr::infer_expr(val, &scope, &null_ctx, snapshot, params, goal)?;

            // Infer nullable from column definition.
            if let Some(node::Node::ParamRef(p)) = val.node.as_ref()
                && let Some(tc) = table.columns.iter().find(|c| c.name == rt.name)
                && !tc.not_null
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
    snapshot: &SchemaSnapshot,
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
        .ok_or_else(|| AnalyzeError::UnknownRelation(relation.relname.clone()))?;

    let mut scope = Scope::default();
    let null_ctx = NullabilityContext::default();
    scope.add_dml_target(
        &relation.relname,
        crate::qualified_name::QualifiedName::new(&table.schema, &table.name),
        &table.columns,
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
// UNION / INTERSECT / EXCEPT
// ──────────────────────────────────────────────────────────────────────────────

fn analyze_set_operation(
    sel: &protobuf::SelectStmt,
    snapshot: &SchemaSnapshot,
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

    let columns = left_cols
        .into_iter()
        .zip(right_cols)
        .map(|(l, r)| {
            let type_oid = crate::coerce::find_common_type(&[l.type_oid, r.type_oid], snapshot)
                .unwrap_or(l.type_oid);
            RawColumn {
                name: l.name,
                type_oid,
                nullable: l.nullable || r.nullable,
                record_fields: None,
            }
        })
        .collect();

    Ok((columns, None))
}

// ──────────────────────────────────────────────────────────────────────────────
// CTE
// ──────────────────────────────────────────────────────────────────────────────

fn analyze_cte(
    cte: &protobuf::CommonTableExpr,
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
    existing_ctes: &HashMap<String, Vec<ScopeColumn>>,
) -> Result<Vec<ScopeColumn>, AnalyzeError> {
    let cte_query = cte
        .ctequery
        .as_ref()
        .and_then(|n| n.node.as_ref())
        .ok_or_else(|| AnalyzeError::Unsupported("CTE without query".into()))?;

    match cte_query {
        node::Node::SelectStmt(sel) => {
            let (cols, _) = analyze_select_with_ctes(sel, snapshot, params, existing_ctes)?;
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

// ──────────────────────────────────────────────────────────────────────────────
// FROM clause processing
// ──────────────────────────────────────────────────────────────────────────────

fn process_from_clause(
    from_clause: &[protobuf::Node],
    scope: &mut Scope,
    null_ctx: &mut NullabilityContext,
    snapshot: &SchemaSnapshot,
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
    snapshot: &SchemaSnapshot,
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
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
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
    // Args of a top-level SRF call don't see the FROM scope of the enclosing
    // SELECT (same rule PG applies), so use an empty scope.
    let empty_scope = Scope::default();
    let empty_null_ctx = NullabilityContext::default();
    let mut arg_types = Vec::with_capacity(func_call.args.len());
    for arg in &func_call.args {
        let t = expr::infer_expr(
            arg,
            &empty_scope,
            &empty_null_ctx,
            snapshot,
            params,
            crate::expr::TypeGoal::NONE,
        )
        .map(|e| e.type_oid)
        .unwrap_or(oid::UNKNOWN);
        arg_types.push(t);
    }

    let (schema, name) = match func_name_parts.as_slice() {
        [n] => (None, n.as_str()),
        [s, n] => (Some(s.as_str()), n.as_str()),
        _ => {
            return Err(AnalyzeError::UnresolvedFunction(format!(
                "invalid function name in FROM: {func_name_parts:?}"
            )));
        }
    };

    let resolved = functions::resolve_function(snapshot, schema, name, &arg_types, false)?;

    // Build the scope columns.
    let mut cols: Vec<ScopeColumn> = if !resolved.out_args.is_empty() {
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
    } else if let Some(crate::schema::TypeKind::Composite { fields }) =
        snapshot.get_type(resolved.return_type_oid).map(|t| &t.kind)
    {
        fields
            .iter()
            .map(|f| ScopeColumn {
                name: f.name.clone(),
                type_oid: f.type_oid,
                base_not_null: f.not_null,
                table_alias: alias.to_owned(),
                record_fields: None,
            })
            .collect()
    } else {
        vec![ScopeColumn {
            name: name.to_owned(),
            type_oid: resolved.return_type_oid,
            base_not_null: false,
            table_alias: alias.to_owned(),
            record_fields: None,
        }]
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
    snapshot: &SchemaSnapshot,
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
                let nullable = null_ctx.is_nullable(&col.table_alias, col.base_not_null);
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

        // If the expression is a FuncCall returning a `record` and we know
        // its named output columns (TABLE/OUT args), propagate those through
        // the scope so downstream `(alias.col).field` can look them up.
        let record_fields = if let Some(node::Node::FuncCall(fc)) = val.node.as_ref() {
            resolve_funccall_record_fields(fc, snapshot, params)
        } else {
            None
        };

        columns.push(RawColumn {
            name,
            type_oid: expr_type.type_oid,
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
    snapshot: &SchemaSnapshot,
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

    let mut column_types: Vec<Vec<u32>> = vec![Vec::new(); arity];
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
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
) -> Option<Vec<crate::schema::CompositeField>> {
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
        Some(resolved.out_args)
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
// Rust type mapping
// ──────────────────────────────────────────────────────────────────────────────

fn build_column(
    rc: RawColumn,
    snapshot: &SchemaSnapshot,
    config: &AnalyzerConfig,
) -> Result<AnalyzedColumn, AnalyzeError> {
    let (rust_type, domain_rust_type, enum_rust_type) =
        resolve_rust_type(rc.type_oid, snapshot, config)?;

    // Handle nullability annotations (! and ?).
    let (name, nullable) = parse_nullability_annotation(&rc.name, rc.nullable);

    Ok(AnalyzedColumn {
        name,
        pg_type_oid: rc.type_oid,
        rust_type,
        nullable,
        domain_rust_type,
        enum_rust_type,
    })
}

fn build_param_info(
    type_oid: u32,
    nullable: bool,
    snapshot: &SchemaSnapshot,
    config: &AnalyzerConfig,
) -> Result<ParamInfo, AnalyzeError> {
    let (rust_type, domain_rust_type, enum_rust_type) =
        resolve_rust_type(type_oid, snapshot, config)?;

    // Resolve cast_type: unwrap domains to base type, then look up pg_name.
    // Prefer the static type_map (built-in PG types) but fall back to the
    // snapshot's type name for extension-defined types like `vector`.
    let base_oid = snapshot.unwrap_domain(type_oid);
    let cast_type = type_map::from_oid(base_oid)
        .map(|ti| ti.pg_name.to_string())
        .or_else(|| {
            snapshot
                .get_type(base_oid)
                .and_then(|te| te.extension.is_some().then(|| te.name.clone()))
        });

    Ok(ParamInfo {
        pg_type_oid: type_oid,
        rust_type,
        nullable,
        domain_rust_type,
        enum_rust_type,
        cast_type,
    })
}

fn resolve_rust_type(
    type_oid: u32,
    snapshot: &SchemaSnapshot,
    config: &AnalyzerConfig,
) -> Result<(String, Option<String>, Option<String>), AnalyzeError> {
    // Check type kind in snapshot.
    if let Some(te) = snapshot.get_type(type_oid) {
        let qualified_name = QualifiedName::new(&te.schema, &te.name);
        match &te.kind {
            TypeKind::Domain { base_type_oid } => {
                if let Some(rust_path) = config.domains.get(&qualified_name) {
                    // JSONB domain.
                    return Ok((
                        "::serde_json::Value".to_owned(),
                        Some(rust_path.clone()),
                        None,
                    ));
                }
                // Non-JSONB domain: unwrap to base type.
                return resolve_rust_type(*base_type_oid, snapshot, config);
            }
            TypeKind::Enum { .. } => {
                let enum_rt = config.enums.get(&qualified_name).cloned();
                return Ok(("String".to_owned(), None, enum_rt));
            }
            TypeKind::Array { element_type_oid } => {
                let (elem_rt, _, _) = resolve_rust_type(*element_type_oid, snapshot, config)?;
                return Ok((format!("Vec<{elem_rt}>"), None, None));
            }
            _ => {}
        }

        // Check custom types config. Keys must be schema-qualified.
        if let Some(rt) = config.types.get(&qualified_name) {
            return Ok((rt.clone(), None, None));
        }

        // Built-in mapping for types defined by known extensions
        // (e.g. pgvector's `vector` → `pgvector::Vector`).
        if let Some(ext_name) = te.extension.as_deref()
            && let Some(rt) = crate::ddl::extensions::extension_type_rust_type(ext_name, &te.name)
        {
            return Ok((rt.to_owned(), None, None));
        }
    }

    // Static type_map lookup.
    if let Some(info) = type_map::from_oid(type_oid) {
        return Ok((info.rust_type.to_owned(), None, None));
    }

    // Unknown type fallback.
    if type_oid == oid::UNKNOWN {
        return Ok(("String".to_owned(), None, None));
    }

    Err(AnalyzeError::UnknownType {
        oid: type_oid,
        context: format!("OID {type_oid}"),
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
