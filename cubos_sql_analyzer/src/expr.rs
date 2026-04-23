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
use crate::schema::{SchemaSnapshot, oid};
use crate::scope::Scope;

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
        node::Node::ColumnRef(col_ref) => infer_column_ref(col_ref, scope, null_ctx, snapshot),
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
        node::Node::AIndirection(ind) => infer_indirection(ind, scope, null_ctx, snapshot, params),
        node::Node::AArrayExpr(arr) => infer_array_expr(arr, scope, null_ctx, snapshot, params),
        node::Node::SetToDefault(_) => {
            // `DEFAULT` placeholder in INSERT VALUES / UPDATE SET. The actual
            // default expression lives on the column definition and is
            // trusted to produce a valid value of the column's type, so we
            // adopt the assignment goal here. Nullability defers to the
            // goal's NOT NULL reasoning in the caller.
            Ok(ExprType {
                type_oid: if goal.has_expectation() {
                    goal.type_oid
                } else {
                    oid::UNKNOWN
                },
                nullable: false,
            })
        }
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
    snapshot: &SchemaSnapshot,
) -> Result<ExprType, AnalyzeError> {
    // Star expansion in expression context. `alias.*` in PG becomes the
    // composite type of the relation referenced by `alias`. `*` alone
    // (no qualifier) could expand to a ROW of every visible source but
    // the semantic is ambiguous enough that we leave it unsupported.
    let has_star = col_ref
        .fields
        .iter()
        .any(|f| matches!(f.node.as_ref(), Some(node::Node::AStar(_))));
    if has_star {
        return infer_star_ref(col_ref, scope, snapshot);
    }

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

    match scope.resolve_column(table, column) {
        Ok(col) => {
            let nullable = null_ctx.is_nullable(&col.table_alias, col.base_not_null);
            Ok(ExprType {
                type_oid: col.type_oid,
                nullable,
            })
        }
        Err(e) => {
            // PG row-reference fallback: a single unqualified identifier can
            // name a whole row from the FROM clause (`SELECT u FROM users u`
            // or `(u).name`). Only kick in when the column lookup failed AND
            // the identifier matches a table alias in scope — otherwise we'd
            // shadow legitimate UnknownColumn errors.
            if table.is_none()
                && let Some(src) = scope.find_source(column)
                && let Some(qn) = src.source_qn.as_ref()
                && let Some(&composite_oid) = snapshot.type_by_name.get(qn)
            {
                return Ok(ExprType {
                    type_oid: composite_oid,
                    nullable: false,
                });
            }
            Err(e)
        }
    }
}

/// Resolve `alias.*` (or `schema.alias.*`) to the composite type of the
/// underlying relation. The composite is the per-table `TypeEntry` that
/// `create_table` registers alongside the table — same OID that a call site
/// like `row_to_json(alias.*)` would see at runtime.
fn infer_star_ref(
    col_ref: &protobuf::ColumnRef,
    scope: &Scope,
    snapshot: &SchemaSnapshot,
) -> Result<ExprType, AnalyzeError> {
    // The alias/relname qualifying the star is the last String field before
    // AStar. For `t.*` it's index 0; for `schema.t.*` it's index 1.
    let alias = col_ref
        .fields
        .iter()
        .rev()
        .skip_while(|f| !matches!(f.node.as_ref(), Some(node::Node::AStar(_))))
        .nth(1)
        .and_then(|f| match f.node.as_ref()? {
            node::Node::String(s) => Some(s.sval.as_str()),
            _ => None,
        })
        .ok_or_else(|| {
            AnalyzeError::Unsupported("unqualified * has no relation — use alias.* instead".into())
        })?;

    let source = scope
        .find_source(alias)
        .ok_or_else(|| AnalyzeError::UnknownRelation(format!("no table named {alias} in scope")))?;

    let qn = source.source_qn.as_ref().ok_or_else(|| {
        AnalyzeError::Unsupported(format!(
            "cannot use {alias}.* here: {alias} is a CTE or subquery, not a real relation"
        ))
    })?;

    let composite_oid =
        snapshot
            .type_by_name
            .get(qn)
            .copied()
            .ok_or_else(|| AnalyzeError::UnknownType {
                oid: 0,
                context: format!("composite type for {qn}"),
            })?;

    // A row value from a real relation is never NULL (it exists as soon as
    // the row is produced); individual fields may be null, but the composite
    // value itself isn't.
    Ok(ExprType {
        type_oid: composite_oid,
        nullable: false,
    })
}

// ──────────────────────────────────────────────────────────────────────────────
// Indirection (`(expr).field`, `(expr)[i]`)
// ──────────────────────────────────────────────────────────────────────────────

/// Resolve `(expr).field1.field2…` chains. Each step either names a field in
/// a composite (String) or subscripts an array (`AIndices`). Array subscript
/// support is intentionally limited to the common `arr[n]` → element type
/// case; more exotic slicing (`arr[1:3]`) falls back to unsupported so we
/// don't silently return the wrong shape.
fn infer_indirection(
    ind: &protobuf::AIndirection,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let arg = ind
        .arg
        .as_deref()
        .ok_or_else(|| AnalyzeError::Unsupported("indirection without arg".into()))?;

    // Two shortcut paths for `record`-typed args whose fields aren't stored
    // in a composite `TypeEntry`:
    //
    // 1. `(func(...)).field` — direct FuncCall with `out_args` (TABLE/OUT).
    // 2. `(alias.col).field` — ColumnRef whose scope entry carries
    //    `record_fields` (populated when the subquery's target expr was a
    //    FuncCall with out_args).
    //
    // Consume leading String steps against those named fields; fall through
    // to the generic walker for any remaining steps (e.g. nested composite
    // unwrap, subscript on a scalar out_arg).
    let from_direct_funccall = if let Some(node::Node::FuncCall(fc)) = arg.node.as_ref() {
        resolve_funccall_out_args(fc, snapshot, params)?
    } else {
        None
    };
    let from_column_record = if from_direct_funccall.is_none() {
        if let Some(node::Node::ColumnRef(cr)) = arg.node.as_ref() {
            column_ref_record_fields(cr, scope)
        } else {
            None
        }
    } else {
        None
    };

    let leading_fields = from_direct_funccall
        .as_ref()
        .or(from_column_record.as_ref());
    let (start_step, mut current) = if let Some(fields) = leading_fields {
        let mut idx = 0usize;
        let mut current = None;
        while idx < ind.indirection.len() {
            let Some(node::Node::String(s)) = ind.indirection[idx].node.as_ref() else {
                break;
            };
            let field = fields
                .iter()
                .find(|f| f.name == s.sval)
                .ok_or_else(|| AnalyzeError::UnknownColumn(format!("record field {}", s.sval)))?;
            current = Some(ExprType {
                type_oid: field.type_oid,
                nullable: !field.not_null,
            });
            idx += 1;
        }
        (idx, current)
    } else {
        (0, None)
    };

    let mut current = match current.take() {
        Some(c) => c,
        None => infer_expr(arg, scope, null_ctx, snapshot, params, TypeGoal::NONE)?,
    };

    for step in ind.indirection.iter().skip(start_step) {
        match step.node.as_ref() {
            Some(node::Node::String(s)) => {
                current = resolve_composite_field(&current, &s.sval, snapshot)?;
            }
            Some(node::Node::AIndices(ai)) => {
                // Plain subscript `arr[i]`: both bounds absent except `uidx`
                // alone means PG's shorthand for a single element.
                let is_slice = ai.is_slice;
                if is_slice {
                    return Err(AnalyzeError::Unsupported(
                        "array slice indirection (arr[a:b]) not supported".into(),
                    ));
                }
                current = resolve_array_element(&current, snapshot)?;
            }
            _ => {
                return Err(AnalyzeError::Unsupported(format!(
                    "unsupported indirection step: {:?}",
                    step.node.as_ref().map(std::mem::discriminant)
                )));
            }
        }
    }

    Ok(current)
}

/// Look up `record_fields` for a `ColumnRef` that resolves to a scope column
/// carrying named output columns (set when its producing expression was a
/// FuncCall with `out_args`). Returns `None` if the ref doesn't resolve or
/// the column isn't a record.
fn column_ref_record_fields(
    cr: &protobuf::ColumnRef,
    scope: &Scope,
) -> Option<Vec<crate::schema::CompositeField>> {
    let parts = extract_string_fields(&cr.fields);
    let (table, column) = match parts.as_slice() {
        [col] => (None, col.as_str()),
        [tbl, col] => (Some(tbl.as_str()), col.as_str()),
        [_schema, tbl, col] => (Some(tbl.as_str()), col.as_str()),
        _ => return None,
    };
    let col = scope.resolve_column(table, column).ok()?;
    col.record_fields.clone()
}

/// If `fc` names a function with declared `out_args` (TABLE/OUT args),
/// return them so indirection steps can match against named output columns.
/// Returns `Ok(None)` when the function has no out_args — the caller should
/// fall back to generic composite/record handling.
fn resolve_funccall_out_args(
    fc: &protobuf::FuncCall,
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
) -> Result<Option<Vec<crate::schema::CompositeField>>, AnalyzeError> {
    let parts = extract_string_fields(&fc.funcname);
    let (schema, name) = match parts.as_slice() {
        [n] => (None, n.as_str()),
        [s, n] => (Some(s.as_str()), n.as_str()),
        _ => return Ok(None),
    };

    // Infer arg types in an empty scope so overload resolution can run.
    // Function args inside a scalar call don't see a FROM scope in the
    // typical use sites of this helper.
    let empty_scope = Scope::default();
    let empty_null_ctx = NullabilityContext::default();
    let mut arg_types = Vec::with_capacity(fc.args.len());
    for arg in &fc.args {
        let t = infer_expr(
            arg,
            &empty_scope,
            &empty_null_ctx,
            snapshot,
            params,
            TypeGoal::NONE,
        )
        .map(|e| e.type_oid)
        .unwrap_or(oid::UNKNOWN);
        arg_types.push(t);
    }

    let resolved =
        match crate::functions::resolve_function(snapshot, schema, name, &arg_types, false) {
            Ok(r) => r,
            Err(_) => return Ok(None),
        };
    if resolved.out_args.is_empty() {
        Ok(None)
    } else {
        Ok(Some(resolved.out_args))
    }
}

/// Look up `field_name` inside a composite type's field list. The resulting
/// nullability is the combination of the enclosing value being nullable AND
/// the field's own `not_null` declaration — either one being nullable makes
/// the access nullable.
fn resolve_composite_field(
    current: &ExprType,
    field_name: &str,
    snapshot: &SchemaSnapshot,
) -> Result<ExprType, AnalyzeError> {
    // Domain-over-composite needs unwrapping to see the composite fields.
    let base_oid = snapshot.unwrap_domain(current.type_oid);
    let type_entry = snapshot
        .get_type(base_oid)
        .ok_or_else(|| AnalyzeError::UnknownType {
            oid: base_oid,
            context: format!("composite field access .{field_name}"),
        })?;

    let fields = match &type_entry.kind {
        crate::schema::TypeKind::Composite { fields } => fields,
        _ => {
            return Err(AnalyzeError::Unsupported(format!(
                "field access .{field_name} on non-composite type '{}'",
                type_entry.name
            )));
        }
    };

    let field = fields
        .iter()
        .find(|f| f.name == field_name)
        .ok_or_else(|| AnalyzeError::UnknownColumn(format!("{}.{field_name}", type_entry.name)))?;

    Ok(ExprType {
        type_oid: field.type_oid,
        nullable: current.nullable || !field.not_null,
    })
}

/// `ARRAY[expr1, expr2, …]` literal — result type is the common element type
/// promoted to its array. Empty arrays fall back to `UNKNOWN` so that the
/// enclosing cast (`ARRAY[]::text[]`) takes over.
fn infer_array_expr(
    arr: &protobuf::AArrayExpr,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    if arr.elements.is_empty() {
        return Ok(ExprType {
            type_oid: oid::UNKNOWN,
            nullable: false,
        });
    }
    let mut element_types = Vec::with_capacity(arr.elements.len());
    let mut any_nullable = false;
    for elem in &arr.elements {
        let t = infer_expr(elem, scope, null_ctx, snapshot, params, TypeGoal::NONE)?;
        element_types.push(t.type_oid);
        any_nullable |= t.nullable;
    }
    let common = coerce::find_common_type(&element_types, snapshot).unwrap_or(oid::UNKNOWN);
    let array_oid = snapshot.array_type_of(common).unwrap_or(oid::UNKNOWN);
    Ok(ExprType {
        type_oid: array_oid,
        // An ARRAY[...] constructor is never NULL itself — it's always at
        // least an empty array. Element nullability is tracked separately by
        // Rust's `Option<T>` inside `Vec<T>`.
        nullable: {
            let _ = any_nullable;
            false
        },
    })
}

/// `arr[i]` — the result is an element of the array. Nullable because SQL
/// subscripts out of bounds return NULL rather than erroring.
fn resolve_array_element(
    current: &ExprType,
    snapshot: &SchemaSnapshot,
) -> Result<ExprType, AnalyzeError> {
    let type_entry =
        snapshot
            .get_type(current.type_oid)
            .ok_or_else(|| AnalyzeError::UnknownType {
                oid: current.type_oid,
                context: "array subscript".into(),
            })?;
    let elem_oid = match &type_entry.kind {
        crate::schema::TypeKind::Array { element_type_oid } => *element_type_oid,
        _ => {
            return Err(AnalyzeError::Unsupported(format!(
                "subscript on non-array type '{}'",
                type_entry.name
            )));
        }
    };
    Ok(ExprType {
        type_oid: elem_oid,
        nullable: true,
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

    // Walk aggregate modifiers so any `$N` placeholders they contain get
    // their types inferred. FILTER must be bool (like a WHERE clause),
    // per-aggregate ORDER BY items have no specific goal, and the WINDOW
    // `OVER (…)` clause is walked separately below.
    if let Some(filter) = &func.agg_filter {
        let _ = infer_expr(
            filter,
            scope,
            null_ctx,
            snapshot,
            params,
            TypeGoal::implicit(oid::BOOL),
        );
    }
    for order_item in &func.agg_order {
        let _ = infer_expr(
            order_item,
            scope,
            null_ctx,
            snapshot,
            params,
            TypeGoal::NONE,
        );
    }
    if let Some(over) = &func.over {
        for item in &over.partition_clause {
            let _ = infer_expr(item, scope, null_ctx, snapshot, params, TypeGoal::NONE);
        }
        for item in &over.order_clause {
            let _ = infer_expr(item, scope, null_ctx, snapshot, params, TypeGoal::NONE);
        }
        if let Some(start) = &over.start_offset {
            let _ = infer_expr(start, scope, null_ctx, snapshot, params, TypeGoal::NONE);
        }
        if let Some(end) = &over.end_offset {
            let _ = infer_expr(end, scope, null_ctx, snapshot, params, TypeGoal::NONE);
        }
    }

    // Value window functions (`lag`/`lead`/`first_value`/`last_value`/
    // `nth_value`) can return NULL at partition/frame edges even when the
    // source column is NOT NULL — `lag(title) OVER (ORDER BY id)` produces
    // NULL for the first row of each partition. Without this override the
    // analyzer would inherit the strict pg_catalog nullability rule and
    // mark the result as NOT NULL, matching PG with the wrong sign.
    let is_value_window = func.over.is_some()
        && matches!(
            name,
            "lag" | "lead" | "first_value" | "last_value" | "nth_value"
        );

    let nullable = if is_value_window {
        true
    } else if resolved.is_aggregate {
        // A FILTER clause can eliminate every row in the group, leaving the
        // aggregate with an empty set. Every aggregate except COUNT returns
        // NULL for an empty set, so FILTER forces non-COUNT aggregates to
        // nullable even when the source column is NOT NULL and there's a
        // GROUP BY.
        let has_filter = func.agg_filter.is_some();
        if name == "count" {
            // COUNT is never NULL (returns 0 for empty input, even with FILTER).
            false
        } else if has_filter {
            true
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

    // `expr IS [NOT] DISTINCT FROM other` — shares op_name "=" with ordinary
    // equality but PG guarantees the result is ALWAYS bool NOT NULL (the
    // whole point of the construct is NULL-aware comparison). Handle before
    // the generic `is_comparison` branch, which would otherwise let the
    // operand nullability bleed into the result.
    if matches!(
        protobuf::AExprKind::try_from(expr.kind),
        Ok(protobuf::AExprKind::AexprDistinct) | Ok(protobuf::AExprKind::AexprNotDistinct)
    ) {
        let left = expr
            .lexpr
            .as_ref()
            .map(|n| infer_expr(n, scope, null_ctx, snapshot, params, TypeGoal::NONE))
            .transpose()?;
        let left_oid = left.as_ref().map(|l| l.type_oid).unwrap_or(oid::UNKNOWN);
        let rhs_goal = if left_oid != oid::UNKNOWN {
            TypeGoal::implicit(left_oid)
        } else {
            TypeGoal::NONE
        };
        let _ = expr
            .rexpr
            .as_ref()
            .map(|n| infer_expr(n, scope, null_ctx, snapshot, params, rhs_goal))
            .transpose()?;
        return Ok(ExprType {
            type_oid: oid::BOOL,
            nullable: false,
        });
    }

    // `expr [NOT] BETWEEN lo AND hi` (and the SYM variants) — rexpr is a
    // `Node::List` holding the two bounds. The generic Pass 1 below walks
    // rexpr as a single expression, hits the `_` fallback for List, and
    // silently drops any `$N` placeholders inside. Handle it up front: infer
    // the lhs first, then re-enter each bound with the lhs type as the
    // inference goal so param OIDs resolve correctly.
    if matches!(
        protobuf::AExprKind::try_from(expr.kind),
        Ok(protobuf::AExprKind::AexprBetween)
            | Ok(protobuf::AExprKind::AexprNotBetween)
            | Ok(protobuf::AExprKind::AexprBetweenSym)
            | Ok(protobuf::AExprKind::AexprNotBetweenSym)
    ) {
        let left = expr
            .lexpr
            .as_ref()
            .map(|n| infer_expr(n, scope, null_ctx, snapshot, params, TypeGoal::NONE))
            .transpose()?;
        let left_oid = left.as_ref().map(|l| l.type_oid).unwrap_or(oid::UNKNOWN);

        let mut any_bound_nullable = false;
        if let Some(rexpr) = &expr.rexpr
            && let Some(node::Node::List(list)) = rexpr.node.as_ref()
        {
            let goal = if left_oid != oid::UNKNOWN {
                TypeGoal::implicit(left_oid)
            } else {
                TypeGoal::NONE
            };
            for item in &list.items {
                let t = infer_expr(item, scope, null_ctx, snapshot, params, goal)?;
                any_bound_nullable = any_bound_nullable || t.nullable;
            }
        }

        let any_nullable = left.as_ref().is_some_and(|l| l.nullable) || any_bound_nullable;
        return Ok(ExprType {
            type_oid: oid::BOOL,
            nullable: any_nullable,
        });
    }

    // col IN ($1, $2, ...) / col NOT IN (...): rexpr is a Node::List whose
    // items need to be inferred with the left side's type as the goal so any
    // untyped params inside the list get their OID resolved. The generic
    // Pass 1 below calls `infer_expr` on the List node itself, which hits the
    // `_` fallback and silently errors (swallowed by the WHERE-clause helper).
    if matches!(
        protobuf::AExprKind::try_from(expr.kind),
        Ok(protobuf::AExprKind::AexprIn)
    ) {
        let left = expr
            .lexpr
            .as_ref()
            .map(|n| infer_expr(n, scope, null_ctx, snapshot, params, TypeGoal::NONE))
            .transpose()?;
        let left_oid = left.as_ref().map(|l| l.type_oid).unwrap_or(oid::UNKNOWN);

        let mut any_right_nullable = false;
        if let Some(rexpr) = &expr.rexpr
            && let Some(node::Node::List(list)) = rexpr.node.as_ref()
        {
            let goal = if left_oid != oid::UNKNOWN {
                TypeGoal::implicit(left_oid)
            } else {
                TypeGoal::NONE
            };
            for item in &list.items {
                let t = infer_expr(item, scope, null_ctx, snapshot, params, goal)?;
                any_right_nullable = any_right_nullable || t.nullable;
            }
        }

        let any_nullable = left.as_ref().is_some_and(|l| l.nullable) || any_right_nullable;
        return Ok(ExprType {
            type_oid: oid::BOOL,
            nullable: any_nullable,
        });
    }

    // col = ANY($arr) / col = ALL($arr): lexpr is scalar, rexpr is array.
    // The generic back-fill below would assign the wrong type (element ↔ array
    // confusion), so we handle it first and return early.
    if matches!(
        protobuf::AExprKind::try_from(expr.kind),
        Ok(protobuf::AExprKind::AexprOpAny) | Ok(protobuf::AExprKind::AexprOpAll)
    ) {
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

        let left_oid = left.as_ref().map(|l| l.type_oid).unwrap_or(oid::UNKNOWN);
        let right_oid = right.as_ref().map(|r| r.type_oid).unwrap_or(oid::UNKNOWN);

        // left is concrete T, right is unknown → right must be T[].
        if left_oid != oid::UNKNOWN
            && right_oid == oid::UNKNOWN
            && let Some(arr_oid) = snapshot.array_type_of(left_oid)
            && let Some(rexpr) = &expr.rexpr
        {
            let _ = infer_expr(
                rexpr,
                scope,
                null_ctx,
                snapshot,
                params,
                TypeGoal::implicit(arr_oid),
            );
        }

        // right is concrete T[], left is unknown → left must be the element type T.
        if right_oid != oid::UNKNOWN
            && left_oid == oid::UNKNOWN
            && let Some(elem_oid) = snapshot.types.get(&right_oid).and_then(|t| {
                if let crate::schema::TypeKind::Array { element_type_oid } = t.kind {
                    Some(element_type_oid)
                } else {
                    None
                }
            })
            && let Some(lexpr) = &expr.lexpr
        {
            let _ = infer_expr(
                lexpr,
                scope,
                null_ctx,
                snapshot,
                params,
                TypeGoal::implicit(elem_oid),
            );
        }

        let any_nullable =
            left.as_ref().is_some_and(|l| l.nullable) || right.as_ref().is_some_and(|r| r.nullable);
        return Ok(ExprType {
            type_oid: oid::BOOL,
            nullable: any_nullable,
        });
    }

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

    // LIKE, BETWEEN, etc. → bool. (IN is handled earlier, before Pass 1.)
    if matches!(
        protobuf::AExprKind::try_from(expr.kind),
        Ok(protobuf::AExprKind::AexprLike)
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
        protobuf::SubLinkType::ExistsSublink => {
            // Walk the subselect to collect any params referenced inside —
            // without this, `EXISTS(SELECT 1 FROM t WHERE x = $p1)` would
            // drop `$p1` from the param list entirely.
            if let Some(subselect) = &sub.subselect
                && let Some(node::Node::SelectStmt(sel)) = subselect.node.as_ref()
            {
                let _ = crate::resolve::analyze_select(sel, snapshot, params)?;
            }
            Ok(ExprType {
                type_oid: oid::BOOL,
                nullable: false,
            })
        }
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
        protobuf::SubLinkType::AnySublink | protobuf::SubLinkType::AllSublink => {
            // Walk the subselect so params inside `col = ANY(SELECT …)` /
            // `col = ALL(SELECT …)` are collected with the right types.
            if let Some(subselect) = &sub.subselect
                && let Some(node::Node::SelectStmt(sel)) = subselect.node.as_ref()
            {
                let _ = crate::resolve::analyze_select(sel, snapshot, params)?;
            }
            Ok(ExprType {
                type_oid: oid::BOOL,
                nullable: true,
            })
        }
        protobuf::SubLinkType::ArraySublink => {
            // `ARRAY(SELECT expr FROM …)` — returns an array of the subquery's
            // first output column. The array itself is always NOT NULL (an
            // empty result produces `{}`, not NULL), even though individual
            // elements may be nullable.
            let mut elem_oid = oid::UNKNOWN;
            if let Some(subselect) = &sub.subselect
                && let Some(node::Node::SelectStmt(sel)) = subselect.node.as_ref()
            {
                let (cols, _) = crate::resolve::analyze_select(sel, snapshot, params)?;
                if let Some(first) = cols.first() {
                    elem_oid = first.type_oid;
                }
            }
            let array_oid = snapshot.array_type_of(elem_oid).unwrap_or(oid::UNKNOWN);
            Ok(ExprType {
                type_oid: array_oid,
                nullable: false,
            })
        }
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
