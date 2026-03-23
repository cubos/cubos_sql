//! Expression type inference.
//!
//! Walks pg_query AST expression nodes and infers their type (OID) and
//! nullability based on the schema snapshot and current scope.

use pg_query::protobuf::{self, a_const, node};

use crate::coerce::{self, oid};
use crate::error::AnalyzeError;
use crate::functions;
use crate::nullability::NullabilityContext;
use crate::params::ParamCollector;
use crate::schema::SchemaSnapshot;
use crate::scope::Scope;

/// Result of inferring an expression's type.
#[derive(Debug, Clone)]
pub struct ExprType {
    pub type_oid: u32,
    pub nullable: bool,
}

/// Infer the type and nullability of an AST expression node.
pub fn infer_expr(
    node: &protobuf::Node,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let inner = node
        .node
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("empty node".into()))?;

    match inner {
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
            let type_oid = params.get(p.number);
            Ok(ExprType {
                type_oid,
                nullable: true,
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
    }
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
            )))
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
    let inner_type = infer_expr(inner, scope, null_ctx, snapshot, params)?;

    let target_oid = resolve_type_name(cast.type_name.as_ref(), snapshot)?;

    // If inner is a ParamRef, record the target type as a constraint.
    if let Some(node::Node::ParamRef(p)) = inner.node.as_ref() {
        params.record(p.number, target_oid);
    }

    Ok(ExprType {
        type_oid: target_oid,
        nullable: inner_type.nullable,
    })
}

// ──────────────────────────────────────────────────────────────────────────────
// Function calls
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
            )))
        }
    };

    let mut arg_types = Vec::new();
    let mut any_arg_nullable = false;
    for arg in &func.args {
        let t = infer_expr(arg, scope, null_ctx, snapshot, params)?;
        any_arg_nullable = any_arg_nullable || t.nullable;
        arg_types.push(t.type_oid);
    }

    let resolved = functions::resolve_function(snapshot, schema, name, &arg_types, func.agg_star)?;

    let nullable = if resolved.is_aggregate {
        // COUNT(*) is never NULL. All other aggregates (SUM, AVG, etc.)
        // return NULL for empty groups.
        name != "count"
    } else if resolved.is_strict && resolved.schema == "pg_catalog" {
        // pg_catalog strict functions: non-null inputs → non-null output,
        // UNLESS the function is in the known exceptions list.
        if functions::is_nullable_strict_exception(name) {
            true
        } else {
            any_arg_nullable
        }
    } else {
        // Non-strict, or non-pg_catalog: conservatively nullable.
        true
    };

    Ok(ExprType {
        type_oid: resolved.return_type_oid,
        nullable,
    })
}

// ──────────────────────────────────────────────────────────────────────────────
// Operators (A_Expr)
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

    // Comparison operators always return bool.
    let is_comparison = matches!(
        op_name,
        "=" | "<>" | "!=" | "<" | ">" | "<=" | ">=" | "~~" | "~~*" | "!~~" | "!~~*"
    );

    let left = expr
        .lexpr
        .as_ref()
        .map(|n| infer_expr(n, scope, null_ctx, snapshot, params))
        .transpose()?;
    let right = expr
        .rexpr
        .as_ref()
        .map(|n| infer_expr(n, scope, null_ctx, snapshot, params))
        .transpose()?;

    // Record param constraints from context: WHERE col = $1.
    if let (Some(ref l), Some(ref r)) = (&left, &right) {
        if let Some(node::Node::ParamRef(p)) = expr.rexpr.as_ref().and_then(|n| n.node.as_ref()) {
            if l.type_oid != oid::UNKNOWN {
                params.record(p.number, l.type_oid);
            }
        }
        if let Some(node::Node::ParamRef(p)) = expr.lexpr.as_ref().and_then(|n| n.node.as_ref()) {
            if r.type_oid != oid::UNKNOWN {
                params.record(p.number, r.type_oid);
            }
        }
    }

    let any_nullable =
        left.as_ref().is_some_and(|l| l.nullable) || right.as_ref().is_some_and(|r| r.nullable);

    // Operators in the nullable exceptions list are always nullable
    // (e.g., jsonb -> 'key' returns NULL if key doesn't exist).
    let op_always_nullable = functions::is_nullable_operator(op_name);
    let nullable = any_nullable || op_always_nullable;

    if is_comparison {
        return Ok(ExprType {
            type_oid: oid::BOOL,
            nullable,
        });
    }

    // Try operator lookup for type.
    let left_oid = left.as_ref().map(|l| l.type_oid);
    let right_oid = right.as_ref().map(|r| r.type_oid).unwrap_or(oid::UNKNOWN);

    if let Some(op) = snapshot.find_operator(op_name, left_oid, right_oid) {
        return Ok(ExprType {
            type_oid: op.result_type_oid,
            nullable,
        });
    }

    // Fallback: try the || operator as text concatenation.
    if op_name == "||" {
        return Ok(ExprType {
            type_oid: oid::TEXT,
            nullable,
        });
    }

    // For IN, ANY, ALL expressions, return bool.
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
        left_oid, right_oid
    )))
}

// ──────────────────────────────────────────────────────────────────────────────
// Bool expressions (AND, OR, NOT)
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
        let t = infer_expr(arg, scope, null_ctx, snapshot, params)?;
        any_nullable = any_nullable || t.nullable;
    }
    Ok(ExprType {
        type_oid: oid::BOOL,
        nullable: any_nullable,
    })
}

// ──────────────────────────────────────────────────────────────────────────────
// COALESCE
// ──────────────────────────────────────────────────────────────────────────────

fn infer_coalesce(
    expr: &protobuf::CoalesceExpr,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let mut types = Vec::new();
    let mut all_nullable = true;

    for arg in &expr.args {
        let t = infer_expr(arg, scope, null_ctx, snapshot, params)?;
        types.push(t.type_oid);
        if !t.nullable {
            all_nullable = false;
        }
    }

    let type_oid = coerce::find_common_type(&types, snapshot).unwrap_or(oid::TEXT);

    // Record param constraints: if we resolved a concrete type, apply it to any
    // ParamRef args so they get the COALESCE result type.
    if type_oid != oid::UNKNOWN {
        for arg in &expr.args {
            if let Some(node::Node::ParamRef(p)) = arg.node.as_ref() {
                params.record(p.number, type_oid);
            }
        }
    }

    // COALESCE is NOT NULL if any argument is guaranteed NOT NULL.
    Ok(ExprType {
        type_oid,
        nullable: all_nullable,
    })
}

// ──────────────────────────────────────────────────────────────────────────────
// CASE
// ──────────────────────────────────────────────────────────────────────────────

fn infer_case(
    expr: &protobuf::CaseExpr,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let mut types = Vec::new();
    let mut any_branch_nullable = false;

    for arg in &expr.args {
        if let Some(node::Node::CaseWhen(when)) = arg.node.as_ref() {
            if let Some(result) = &when.result {
                let t = infer_expr(result, scope, null_ctx, snapshot, params)?;
                types.push(t.type_oid);
                any_branch_nullable = any_branch_nullable || t.nullable;
            }
        }
    }

    // ELSE clause.
    if let Some(defresult) = &expr.defresult {
        let t = infer_expr(defresult, scope, null_ctx, snapshot, params)?;
        types.push(t.type_oid);
        any_branch_nullable = any_branch_nullable || t.nullable;
    } else {
        // No ELSE → always nullable (returns NULL when no match).
        any_branch_nullable = true;
    }

    let type_oid = coerce::find_common_type(&types, snapshot).unwrap_or(oid::TEXT);

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
            // Scalar subquery: analyze the full subquery to get its output type.
            if let Some(subselect) = &sub.subselect {
                if let Some(node::Node::SelectStmt(sel)) = subselect.node.as_ref() {
                    let (cols, _) = crate::resolve::analyze_select(sel, snapshot, params)?;
                    if let Some(first) = cols.first() {
                        return Ok(ExprType {
                            type_oid: first.type_oid,
                            nullable: true, // scalar subquery is always nullable
                        });
                    }
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

/// Extract string values from a list of nodes (used for ColumnRef.fields,
/// FuncCall.funcname, AExpr.name, TypeName.names).
pub fn extract_string_fields(nodes: &[protobuf::Node]) -> Vec<String> {
    nodes
        .iter()
        .filter_map(|n| match n.node.as_ref()? {
            node::Node::String(s) => Some(s.sval.clone()),
            _ => None,
        })
        .collect()
}

/// Resolve a TypeName to a type OID.
pub fn resolve_type_name(
    type_name: Option<&protobuf::TypeName>,
    snapshot: &SchemaSnapshot,
) -> Result<u32, AnalyzeError> {
    let tn = type_name.ok_or_else(|| AnalyzeError::Unsupported("missing TypeName".into()))?;

    // If type_oid is already set (PG pre-resolved), use it.
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
            )))
        }
    };

    // Check if it's an array type (has array_bounds).
    let is_array = !tn.array_bounds.is_empty();

    let type_entry =
        snapshot
            .resolve_type_by_name(schema, name)
            .ok_or_else(|| AnalyzeError::UnknownType {
                oid: 0,
                context: format!("type name: {}", parts.join(".")),
            })?;

    if is_array {
        // Find the array type for this element type.
        let array_name = format!("_{name}");
        if let Some(arr) = snapshot.resolve_type_by_name(schema, &array_name) {
            return Ok(arr.oid);
        }
        // Fallback: search by element type in type catalog.
        for t in snapshot.types.values() {
            if let crate::schema::TypeKind::Array { element_type_oid } = t.kind {
                if element_type_oid == type_entry.oid {
                    return Ok(t.oid);
                }
            }
        }
    }

    Ok(type_entry.oid)
}
