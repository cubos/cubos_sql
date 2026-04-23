//! Expression type inference.
//!
//! Walks pg_query AST expression nodes and infers their type (OID) and
//! nullability based on the schema snapshot and current scope.
//!
//! Every expression evaluation receives a [`TypeGoal`] describing the type
//! expected by the enclosing context (e.g. `BOOL` for `WHERE`, `INT8` for
//! `LIMIT`).  When the result is a `ParamRef` whose type is still `UNKNOWN`,
//! the goal type is recorded as a constraint — this is the single mechanism
//! that replaces all ad-hoc parameter recording.  After inference, a
//! compatibility check verifies that the result type can be coerced to the
//! goal under the allowed coercion context.

use pg_query::protobuf::{self, a_const, node};

use crate::coerce::{self, CoercionContext, can_coerce};
use crate::error::AnalyzeError;
use crate::functions;
use crate::nullability::NullabilityContext;
use crate::param_collector::ParamCollector;
use crate::schema::SchemaSnapshot;
use crate::scope::Scope;
use crate::type_map::oid;

// ──────────────────────────────────────────────────────────────────────────────
// TypeGoal
// ──────────────────────────────────────────────────────────────────────────────

/// The type expected by the enclosing context.
///
/// Mirrors PostgreSQL's approach where each clause (`WHERE`, `LIMIT`, `INSERT
/// VALUES`, …) tells the parser "I expect this expression to produce type X
/// with coercion level Y".
#[derive(Debug, Clone, Copy)]
pub(crate) struct TypeGoal {
    pub type_oid: u32,
    pub coercion: CoercionContext,
}

impl TypeGoal {
    /// No type expectation (e.g. SELECT target list).
    pub const NONE: Self = Self {
        type_oid: oid::UNKNOWN,
        coercion: CoercionContext::Implicit,
    };

    /// Expression context — only implicit casts allowed
    /// (operator/function argument matching).
    pub fn implicit(type_oid: u32) -> Self {
        Self {
            type_oid,
            coercion: CoercionContext::Implicit,
        }
    }

    /// Assignment context — implicit + assignment casts allowed
    /// (WHERE, LIMIT, INSERT, UPDATE — matches PG's `COERCION_ASSIGNMENT`).
    pub fn assignment(type_oid: u32) -> Self {
        Self {
            type_oid,
            coercion: CoercionContext::Assignment,
        }
    }

    pub fn has_expectation(&self) -> bool {
        self.type_oid != oid::UNKNOWN
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// ExprType
// ──────────────────────────────────────────────────────────────────────────────

/// Result of inferring an expression's type.
#[derive(Debug, Clone)]
pub(crate) struct ExprType {
    pub type_oid: u32,
    pub nullable: bool,
}

// ──────────────────────────────────────────────────────────────────────────────
// Main entry point
// ──────────────────────────────────────────────────────────────────────────────

/// Infer the type and nullability of an AST expression node.
///
/// `goal` describes the type expected by the enclosing context.  When the
/// expression is a `ParamRef` whose type is still unknown, the goal type is
/// recorded as a constraint.  After inference, the result is checked for
/// compatibility with the goal (raising `TypeMismatch` on failure).
pub(crate) fn infer_expr(
    node: &protobuf::Node,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
    goal: TypeGoal,
) -> Result<ExprType, AnalyzeError> {
    let inner = node
        .node
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("empty node".into()))?;

    let result = match inner {
        node::Node::ColumnRef(col_ref) => infer_column_ref(col_ref, scope, null_ctx),
        node::Node::AConst(a_const) => infer_a_const(a_const),
        node::Node::TypeCast(cast) => infer_type_cast(cast, scope, null_ctx, snapshot, params),
        node::Node::FuncCall(func) => infer_func_call(func, scope, null_ctx, snapshot, params),
        node::Node::AExpr(expr) => infer_a_expr(expr, scope, null_ctx, snapshot, params),
        node::Node::BoolExpr(expr) => infer_bool_expr(expr, scope, null_ctx, snapshot, params),
        node::Node::NullTest(_) => Ok(ExprType {
            type_oid: oid::BOOL,
            nullable: false,
        }),
        node::Node::BooleanTest(_) => Ok(ExprType {
            type_oid: oid::BOOL,
            nullable: false,
        }),
        node::Node::CoalesceExpr(expr) => infer_coalesce(expr, scope, null_ctx, snapshot, params),
        node::Node::CaseExpr(expr) => infer_case(expr, scope, null_ctx, snapshot, params),
        node::Node::SubLink(sub) => infer_sublink(sub, scope, null_ctx, snapshot, params),
        node::Node::ParamRef(p) => {
            params.see(p.number);
            // If the param is still untyped and the context provides a goal,
            // record the goal type — this is our equivalent of PG's
            // p_coerce_param_hook.
            if params.get(p.number) == oid::UNKNOWN && goal.has_expectation() {
                params.record(p.number, goal.type_oid);
            }
            let type_oid = params.get(p.number);
            Ok(ExprType {
                type_oid,
                nullable: params.is_nullable(p.number),
            })
        }
        node::Node::MinMaxExpr(mm) => Ok(ExprType {
            type_oid: mm.minmaxtype,
            nullable: true,
        }),
        _ => Err(AnalyzeError::Unsupported(format!(
            "expression node type not supported: {:?}",
            std::mem::discriminant(inner)
        ))),
    }?;

    // Verify result is compatible with the goal type.
    check_goal_compatibility(&result, &goal, snapshot)?;

    Ok(result)
}

// ──────────────────────────────────────────────────────────────────────────────
// Goal compatibility check
// ──────────────────────────────────────────────────────────────────────────────

/// Verify that `result` can be coerced to `goal` under the allowed coercion
/// context.  Returns `Ok(())` when:
/// - There is no goal expectation (`goal.type_oid == UNKNOWN`).
/// - The result is `UNKNOWN` (untyped literals / unresolved params coerce to
///   anything, per SQL spec).
/// - The types match (after domain unwrapping).
/// - A registered cast exists at the required coercion level.
fn check_goal_compatibility(
    result: &ExprType,
    goal: &TypeGoal,
    snapshot: &SchemaSnapshot,
) -> Result<(), AnalyzeError> {
    if !goal.has_expectation() {
        return Ok(());
    }
    if result.type_oid == oid::UNKNOWN {
        return Ok(());
    }
    if result.type_oid == goal.type_oid {
        return Ok(());
    }
    if can_coerce(result.type_oid, goal.type_oid, goal.coercion, snapshot) {
        return Ok(());
    }
    Err(AnalyzeError::TypeMismatch {
        actual: type_display_name(result.type_oid, snapshot),
        expected: type_display_name(goal.type_oid, snapshot),
        context: format!(
            "cannot coerce {} to {}",
            type_display_name(result.type_oid, snapshot),
            type_display_name(goal.type_oid, snapshot),
        ),
    })
}

fn type_display_name(oid: u32, snapshot: &SchemaSnapshot) -> String {
    snapshot
        .get_type(oid)
        .map(|t| t.name.clone())
        .unwrap_or_else(|| format!("oid:{oid}"))
}

// ──────────────────────────────────────────────────────────────────────────────
// Column references
// ──────────────────────────────────────────────────────────────────────────────

fn infer_column_ref(
    col_ref: &protobuf::ColumnRef,
    scope: &Scope,
    null_ctx: &NullabilityContext,
) -> Result<ExprType, AnalyzeError> {
    let parts = extract_string_fields(&col_ref.fields);

    let (table, column) = match parts.as_slice() {
        [col] => (None, col.as_str()),
        [tbl, col] => (Some(tbl.as_str()), col.as_str()),
        [_schema, tbl, col] => (Some(tbl.as_str()), col.as_str()),
        _ => {
            return Err(AnalyzeError::UnknownColumn(format!(
                "invalid column ref: {:?}",
                parts
            )));
        }
    };

    // Handle SELECT * (AStar node in fields).
    if col_ref
        .fields
        .iter()
        .any(|f| matches!(f.node.as_ref(), Some(node::Node::AStar(_))))
    {
        return Err(AnalyzeError::Unsupported(
            "star expansion in expression context".into(),
        ));
    }

    let col = scope.resolve_column(table, column)?;
    let nullable = null_ctx.is_nullable(&col.table_alias, col.base_not_null);

    Ok(ExprType {
        type_oid: col.type_oid,
        nullable,
    })
}

// ──────────────────────────────────────────────────────────────────────────────
// Literals
// ──────────────────────────────────────────────────────────────────────────────

fn infer_a_const(a_const: &protobuf::AConst) -> Result<ExprType, AnalyzeError> {
    if a_const.isnull {
        return Ok(ExprType {
            type_oid: oid::UNKNOWN,
            nullable: true,
        });
    }

    let type_oid = match &a_const.val {
        Some(a_const::Val::Ival(_)) => oid::INT4,
        Some(a_const::Val::Fval(_)) => oid::NUMERIC,
        Some(a_const::Val::Boolval(_)) => oid::BOOL,
        Some(a_const::Val::Sval(_)) => oid::UNKNOWN, // untyped string literal
        Some(a_const::Val::Bsval(_)) => oid::BYTEA,
        None => oid::UNKNOWN,
    };

    Ok(ExprType {
        type_oid,
        nullable: false,
    })
}

// ──────────────────────────────────────────────────────────────────────────────
// Type casts
// ──────────────────────────────────────────────────────────────────────────────

fn infer_type_cast(
    cast: &protobuf::TypeCast,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let inner = cast
        .arg
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("TypeCast without arg".into()))?;

    let target_oid = resolve_type_name(cast.type_name.as_ref(), snapshot)?;

    // An explicit cast (::type / CAST) overrides type checking — we do NOT
    // check compatibility of the inner expression against the target type.
    // We pass NONE as goal to avoid false TypeMismatch errors (e.g. age::text
    // where int4→text has no implicit cast).
    //
    // For ParamRef, we manually record the cast target type (equivalent to
    // PG's coerce_type handling of Param nodes in explicit cast context).
    let inner_type = infer_expr(inner, scope, null_ctx, snapshot, params, TypeGoal::NONE)?;

    if let Some(node::Node::ParamRef(p)) = inner.node.as_ref()
        && params.get(p.number) == oid::UNKNOWN
    {
        params.record(p.number, target_oid);
    }

    Ok(ExprType {
        type_oid: target_oid,
        nullable: inner_type.nullable,
    })
}

// ──────────────────────────────────────────────────────────────────────────────
// Function calls — two-pass (PG chapter 10.3)
// ──────────────────────────────────────────────────────────────────────────────

fn infer_func_call(
    func: &protobuf::FuncCall,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let func_name_parts = extract_string_fields(&func.funcname);
    let (schema, name) = match func_name_parts.as_slice() {
        [name] => (None, name.as_str()),
        [schema, name] => (Some(schema.as_str()), name.as_str()),
        _ => {
            return Err(AnalyzeError::UnresolvedFunction(format!(
                "invalid function name: {:?}",
                func_name_parts
            )));
        }
    };

    // Pass 1: infer args bottom-up with no goal.
    let mut arg_types = Vec::new();
    let mut any_arg_nullable = false;
    for arg in &func.args {
        let t = infer_expr(arg, scope, null_ctx, snapshot, params, TypeGoal::NONE)?;
        any_arg_nullable = any_arg_nullable || t.nullable;
        arg_types.push(t.type_oid);
    }

    // Resolve function with inferred arg types (UNKNOWN treated as wildcard).
    let resolved = functions::resolve_function(snapshot, schema, name, &arg_types, func.agg_star)?;

    // Pass 2: back-fill UNKNOWN args with expected types from the resolved
    // function signature (equivalent to PG's coerce_func_args).
    for (i, arg) in func.args.iter().enumerate() {
        if arg_types[i] == oid::UNKNOWN
            && let Some(&expected) = resolved.arg_types.get(i)
            && expected != oid::UNKNOWN
        {
            let _ = infer_expr(
                arg,
                scope,
                null_ctx,
                snapshot,
                params,
                TypeGoal::implicit(expected),
            );
        }
    }

    let nullable = if resolved.is_aggregate {
        if name == "count" {
            // COUNT is never NULL (returns 0 for empty input).
            false
        } else if null_ctx.has_group_by {
            any_arg_nullable
        } else {
            // Without GROUP BY, non-COUNT aggregates return NULL for empty tables.
            true
        }
    } else if resolved.is_strict && resolved.schema == "pg_catalog" {
        if functions::is_nullable_strict_exception(name) {
            true
        } else {
            any_arg_nullable
        }
    } else {
        !(!resolved.is_strict
            && resolved.schema == "pg_catalog"
            && functions::is_not_null_nonstrict(name))
    };

    Ok(ExprType {
        type_oid: resolved.return_type_oid,
        nullable,
    })
}

// ──────────────────────────────────────────────────────────────────────────────
// Operators (A_Expr) — two-pass (PG chapter 10.2)
// ──────────────────────────────────────────────────────────────────────────────

fn infer_a_expr(
    expr: &protobuf::AExpr,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let op_name = extract_string_fields(&expr.name).join(".");
    let op_name = op_name.as_str();

    let is_comparison = matches!(
        op_name,
        "=" | "<>" | "!=" | "<" | ">" | "<=" | ">=" | "~~" | "~~*" | "!~~" | "!~~*"
    );

    // Pass 1: infer both sides bottom-up.
    let left = expr
        .lexpr
        .as_ref()
        .map(|n| infer_expr(n, scope, null_ctx, snapshot, params, TypeGoal::NONE))
        .transpose()?;
    let right = expr
        .rexpr
        .as_ref()
        .map(|n| infer_expr(n, scope, null_ctx, snapshot, params, TypeGoal::NONE))
        .transpose()?;

    let left_oid = left.as_ref().map(|l| l.type_oid);
    let right_oid = right.as_ref().map(|r| r.type_oid).unwrap_or(oid::UNKNOWN);

    // PG step 2: if one side is unknown and the other is concrete, assume
    // unknown = the other side's type.  Re-infer to propagate into params.
    if let (Some(l_oid), true) = (left_oid, right_oid == oid::UNKNOWN)
        && l_oid != oid::UNKNOWN
        && let Some(rexpr) = &expr.rexpr
    {
        let _ = infer_expr(
            rexpr,
            scope,
            null_ctx,
            snapshot,
            params,
            TypeGoal::implicit(l_oid),
        );
    }
    if let Some(r) = &right
        && r.type_oid != oid::UNKNOWN
        && left_oid == Some(oid::UNKNOWN)
        && let Some(lexpr) = &expr.lexpr
    {
        let _ = infer_expr(
            lexpr,
            scope,
            null_ctx,
            snapshot,
            params,
            TypeGoal::implicit(r.type_oid),
        );
    }

    // Re-read types after back-fill.
    let left_oid_resolved = expr
        .lexpr
        .as_ref()
        .and_then(|n| match n.node.as_ref() {
            Some(node::Node::ParamRef(p)) => {
                let t = params.get(p.number);
                if t != oid::UNKNOWN { Some(t) } else { left_oid }
            }
            _ => left_oid,
        })
        .or(left_oid);
    let right_oid_resolved = expr
        .rexpr
        .as_ref()
        .map(|n| match n.node.as_ref() {
            Some(node::Node::ParamRef(p)) => {
                let t = params.get(p.number);
                if t != oid::UNKNOWN { t } else { right_oid }
            }
            _ => right_oid,
        })
        .unwrap_or(right_oid);

    let any_nullable =
        left.as_ref().is_some_and(|l| l.nullable) || right.as_ref().is_some_and(|r| r.nullable);
    let op_always_nullable = functions::is_nullable_operator(op_name);
    let nullable = any_nullable || op_always_nullable;

    if is_comparison {
        return Ok(ExprType {
            type_oid: oid::BOOL,
            nullable,
        });
    }

    // Try operator lookup with resolved types.
    if let Some(op) = snapshot.find_operator(op_name, left_oid_resolved, right_oid_resolved) {
        // Pass 2: back-fill still-UNKNOWN sides with operator's expected types.
        if left_oid_resolved == Some(oid::UNKNOWN)
            && let (Some(expected), Some(lexpr)) = (op.left_type_oid, &expr.lexpr)
        {
            let _ = infer_expr(
                lexpr,
                scope,
                null_ctx,
                snapshot,
                params,
                TypeGoal::implicit(expected),
            );
        }
        if right_oid_resolved == oid::UNKNOWN
            && let Some(rexpr) = &expr.rexpr
        {
            let _ = infer_expr(
                rexpr,
                scope,
                null_ctx,
                snapshot,
                params,
                TypeGoal::implicit(op.right_type_oid),
            );
        }
        return Ok(ExprType {
            type_oid: op.result_type_oid,
            nullable,
        });
    }

    // Fallback: || as text concatenation.
    if op_name == "||" {
        // Back-fill UNKNOWN sides as TEXT.
        if left_oid_resolved == Some(oid::UNKNOWN)
            && let Some(lexpr) = &expr.lexpr
        {
            let _ = infer_expr(
                lexpr,
                scope,
                null_ctx,
                snapshot,
                params,
                TypeGoal::implicit(oid::TEXT),
            );
        }
        if right_oid_resolved == oid::UNKNOWN
            && let Some(rexpr) = &expr.rexpr
        {
            let _ = infer_expr(
                rexpr,
                scope,
                null_ctx,
                snapshot,
                params,
                TypeGoal::implicit(oid::TEXT),
            );
        }
        return Ok(ExprType {
            type_oid: oid::TEXT,
            nullable,
        });
    }

    // IN, LIKE, BETWEEN, etc. → bool.
    if matches!(
        protobuf::AExprKind::try_from(expr.kind),
        Ok(protobuf::AExprKind::AexprIn)
            | Ok(protobuf::AExprKind::AexprLike)
            | Ok(protobuf::AExprKind::AexprIlike)
            | Ok(protobuf::AExprKind::AexprBetween)
            | Ok(protobuf::AExprKind::AexprNotBetween)
            | Ok(protobuf::AExprKind::AexprSimilar)
    ) {
        return Ok(ExprType {
            type_oid: oid::BOOL,
            nullable,
        });
    }

    Err(AnalyzeError::UnresolvedOperator(format!(
        "operator '{op_name}' with types {:?}, {:?}",
        left_oid_resolved, right_oid_resolved
    )))
}

// ──────────────────────────────────────────────────────────────────────────────
// Bool expressions (AND, OR, NOT) — PG uses COERCION_ASSIGNMENT for args
// ──────────────────────────────────────────────────────────────────────────────

fn infer_bool_expr(
    expr: &protobuf::BoolExpr,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let mut any_nullable = false;
    for arg in &expr.args {
        let t = infer_expr(
            arg,
            scope,
            null_ctx,
            snapshot,
            params,
            TypeGoal::assignment(oid::BOOL),
        )?;
        any_nullable = any_nullable || t.nullable;
    }
    Ok(ExprType {
        type_oid: oid::BOOL,
        nullable: any_nullable,
    })
}

// ──────────────────────────────────────────────────────────────────────────────
// COALESCE — two-pass (PG chapter 10.5)
// ──────────────────────────────────────────────────────────────────────────────

fn infer_coalesce(
    expr: &protobuf::CoalesceExpr,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    // Pass 1: infer all args bottom-up.
    let mut types = Vec::new();
    let mut all_nullable = true;

    for arg in &expr.args {
        let t = infer_expr(arg, scope, null_ctx, snapshot, params, TypeGoal::NONE)?;
        types.push(t.type_oid);
        if !t.nullable {
            all_nullable = false;
        }
    }

    let type_oid = coerce::find_common_type(&types, snapshot).unwrap_or(oid::TEXT);

    // Pass 2: back-fill UNKNOWN args with the resolved common type.
    if type_oid != oid::UNKNOWN {
        for (i, arg) in expr.args.iter().enumerate() {
            if types[i] == oid::UNKNOWN {
                let _ = infer_expr(
                    arg,
                    scope,
                    null_ctx,
                    snapshot,
                    params,
                    TypeGoal::implicit(type_oid),
                );
            }
        }
    }

    Ok(ExprType {
        type_oid,
        nullable: all_nullable,
    })
}

// ──────────────────────────────────────────────────────────────────────────────
// CASE — two-pass (PG chapter 10.5)
// ──────────────────────────────────────────────────────────────────────────────

fn infer_case(
    expr: &protobuf::CaseExpr,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    // Pass 1: infer WHEN conditions with BOOL goal, results with NONE.
    let mut types = Vec::new();
    let mut any_branch_nullable = false;

    for arg in &expr.args {
        if let Some(node::Node::CaseWhen(when)) = arg.node.as_ref() {
            // WHEN condition must be boolean.
            if let Some(cond) = &when.expr {
                let _ = infer_expr(
                    cond,
                    scope,
                    null_ctx,
                    snapshot,
                    params,
                    TypeGoal::assignment(oid::BOOL),
                );
            }
            // THEN result.
            if let Some(result) = &when.result {
                let t = infer_expr(result, scope, null_ctx, snapshot, params, TypeGoal::NONE)?;
                types.push(t.type_oid);
                any_branch_nullable = any_branch_nullable || t.nullable;
            }
        }
    }

    // ELSE clause.
    if let Some(defresult) = &expr.defresult {
        let t = infer_expr(defresult, scope, null_ctx, snapshot, params, TypeGoal::NONE)?;
        types.push(t.type_oid);
        any_branch_nullable = any_branch_nullable || t.nullable;
    } else {
        any_branch_nullable = true;
    }

    let type_oid = coerce::find_common_type(&types, snapshot).unwrap_or(oid::TEXT);

    // Pass 2: back-fill UNKNOWN result branches with the common type.
    if type_oid != oid::UNKNOWN {
        let mut type_idx = 0;
        for arg in &expr.args {
            if let Some(node::Node::CaseWhen(when)) = arg.node.as_ref()
                && let Some(result) = &when.result
            {
                if types.get(type_idx) == Some(&oid::UNKNOWN) {
                    let _ = infer_expr(
                        result,
                        scope,
                        null_ctx,
                        snapshot,
                        params,
                        TypeGoal::implicit(type_oid),
                    );
                }
                type_idx += 1;
            }
        }
        if let Some(defresult) = &expr.defresult
            && types.get(type_idx) == Some(&oid::UNKNOWN)
        {
            let _ = infer_expr(
                defresult,
                scope,
                null_ctx,
                snapshot,
                params,
                TypeGoal::implicit(type_oid),
            );
        }
    }

    Ok(ExprType {
        type_oid,
        nullable: any_branch_nullable,
    })
}

// ──────────────────────────────────────────────────────────────────────────────
// Subqueries (SubLink)
// ──────────────────────────────────────────────────────────────────────────────

fn infer_sublink(
    sub: &protobuf::SubLink,
    _scope: &Scope,
    _null_ctx: &NullabilityContext,
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let sub_type = protobuf::SubLinkType::try_from(sub.sub_link_type)
        .unwrap_or(protobuf::SubLinkType::ExprSublink);

    match sub_type {
        protobuf::SubLinkType::ExistsSublink => Ok(ExprType {
            type_oid: oid::BOOL,
            nullable: false,
        }),
        protobuf::SubLinkType::ExprSublink => {
            if let Some(subselect) = &sub.subselect
                && let Some(node::Node::SelectStmt(sel)) = subselect.node.as_ref()
            {
                let (cols, _) = crate::resolve::analyze_select(sel, snapshot, params)?;
                if let Some(first) = cols.first() {
                    let guaranteed_one_row =
                        sel.group_clause.is_empty() && has_aggregate_target(&sel.target_list);
                    let nullable = if guaranteed_one_row {
                        first.nullable
                    } else {
                        true
                    };
                    return Ok(ExprType {
                        type_oid: first.type_oid,
                        nullable,
                    });
                }
            }
            Ok(ExprType {
                type_oid: oid::UNKNOWN,
                nullable: true,
            })
        }
        protobuf::SubLinkType::AnySublink | protobuf::SubLinkType::AllSublink => Ok(ExprType {
            type_oid: oid::BOOL,
            nullable: true,
        }),
        _ => Err(AnalyzeError::Unsupported(format!(
            "sublink type: {:?}",
            sub_type
        ))),
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Check if any target in a SELECT's target list contains an aggregate function call.
fn has_aggregate_target(target_list: &[protobuf::Node]) -> bool {
    target_list.iter().any(|node| {
        if let Some(node::Node::ResTarget(res)) = node.node.as_ref()
            && let Some(val) = &res.val
        {
            return node_contains_aggregate(val);
        }
        false
    })
}

/// Recursively check if a node is or contains an aggregate function call.
fn node_contains_aggregate(node: &protobuf::Node) -> bool {
    match node.node.as_ref() {
        Some(node::Node::FuncCall(func)) => {
            if func.agg_star || func.agg_order.iter().len() > 0 {
                return true;
            }
            let names = extract_string_fields(&func.funcname);
            let name = names.last().map(|s| s.as_str()).unwrap_or("");
            matches!(
                name,
                "count"
                    | "sum"
                    | "avg"
                    | "min"
                    | "max"
                    | "array_agg"
                    | "string_agg"
                    | "bool_and"
                    | "bool_or"
                    | "every"
                    | "json_agg"
                    | "jsonb_agg"
                    | "json_object_agg"
                    | "jsonb_object_agg"
                    | "bit_and"
                    | "bit_or"
            )
        }
        Some(node::Node::SubLink(_)) => false,
        Some(node::Node::AExpr(expr)) => {
            expr.lexpr
                .as_ref()
                .is_some_and(|n| node_contains_aggregate(n))
                || expr
                    .rexpr
                    .as_ref()
                    .is_some_and(|n| node_contains_aggregate(n))
        }
        Some(node::Node::TypeCast(cast)) => cast
            .arg
            .as_ref()
            .is_some_and(|n| node_contains_aggregate(n)),
        Some(node::Node::CoalesceExpr(c)) => c.args.iter().any(node_contains_aggregate),
        Some(node::Node::CaseExpr(c)) => {
            c.args.iter().any(|n| {
                if let Some(node::Node::CaseWhen(w)) = n.node.as_ref() {
                    w.result
                        .as_ref()
                        .is_some_and(|r| node_contains_aggregate(r))
                } else {
                    false
                }
            }) || c
                .defresult
                .as_ref()
                .is_some_and(|n| node_contains_aggregate(n))
        }
        Some(node::Node::BoolExpr(b)) => b.args.iter().any(node_contains_aggregate),
        Some(node::Node::NullTest(t)) => t.arg.as_ref().is_some_and(|n| node_contains_aggregate(n)),
        _ => false,
    }
}

/// Extract string values from a list of nodes.
pub(crate) fn extract_string_fields(nodes: &[protobuf::Node]) -> Vec<String> {
    nodes
        .iter()
        .filter_map(|n| match n.node.as_ref()? {
            node::Node::String(s) => Some(s.sval.clone()),
            _ => None,
        })
        .collect()
}

/// Resolve a TypeName to a type OID.
fn resolve_type_name(
    type_name: Option<&protobuf::TypeName>,
    snapshot: &SchemaSnapshot,
) -> Result<u32, AnalyzeError> {
    let tn = type_name.ok_or_else(|| AnalyzeError::Unsupported("missing TypeName".into()))?;

    if tn.type_oid != 0 {
        return Ok(tn.type_oid);
    }

    let parts = extract_string_fields(&tn.names);
    let (schema, name) = match parts.as_slice() {
        [name] => (None, name.as_str()),
        [schema, name] => (Some(schema.as_str()), name.as_str()),
        _ => {
            return Err(AnalyzeError::Unsupported(format!(
                "complex type name: {:?}",
                parts
            )));
        }
    };

    let is_array = !tn.array_bounds.is_empty();

    let type_entry =
        snapshot
            .resolve_type_by_name(schema, name)
            .ok_or_else(|| AnalyzeError::UnknownType {
                oid: 0,
                context: format!("type name: {}", parts.join(".")),
            })?;

    if is_array {
        let array_name = format!("_{name}");
        if let Some(arr) = snapshot.resolve_type_by_name(schema, &array_name) {
            return Ok(arr.oid);
        }
        for t in snapshot.types.values() {
            if let crate::schema::TypeKind::Array { element_type_oid } = t.kind
                && element_type_oid == type_entry.oid
            {
                return Ok(t.oid);
            }
        }
    }

    Ok(type_entry.oid)
}
