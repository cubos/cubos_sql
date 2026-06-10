use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// Bool expressions (AND, OR, NOT) — PG uses COERCION_ASSIGNMENT for args
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn infer_bool_expr(
    expr: &protobuf::BoolExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let Ctx { snapshot, .. } = ctx;
    // PG names the failing argument after the operator (`argument of NOT must
    // be type boolean, not type X`, likewise AND / OR).
    let label = match protobuf::BoolExprType::try_from(expr.boolop) {
        Ok(protobuf::BoolExprType::NotExpr) => "NOT",
        Ok(protobuf::BoolExprType::OrExpr) => "OR",
        _ => "AND",
    };
    let mut any_nullable = false;
    for arg in &expr.args {
        match infer_expr(arg, ctx, params, TypeGoal::assignment(oid::BOOL)) {
            Ok(t) => any_nullable = any_nullable || t.nullable,
            // Rewrite a coerce-to-bool mismatch to PG's exact wording; other
            // errors keep their own message.
            Err(e) => {
                if !matches!(e, AnalyzeError::TypeMismatch { .. }) {
                    return Err(e);
                }
                let mut params2 = params.clone();
                let actual_oid = infer_expr(arg, ctx, &mut params2, TypeGoal::NONE)
                    .map(|t| t.type_oid)
                    .unwrap_or(oid::UNKNOWN);
                let actual_pg = crate::ddl::util::format_type_for_message(snapshot, actual_oid);
                let span = crate::error::node_location(arg)
                    .and_then(crate::error::SourceSpan::from_node_qname);
                return Err(crate::error::RawError::invalid(
                    format!("argument of {label} must be type boolean, not type {actual_pg}"),
                    span,
                    None,
                )
                .with_primary_label(format!("this is {actual_pg}, expected boolean"))
                .finalize_implicit());
            }
        }
    }
    Ok(ExprType::scalar(oid::BOOL, any_nullable))
}

// ──────────────────────────────────────────────────────────────────────────────
// COALESCE — two-pass (PG chapter 10.5)
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn infer_coalesce(
    expr: &protobuf::CoalesceExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let Ctx { snapshot, .. } = ctx;
    // Pass 1: infer all args bottom-up. Bare string literals stay UNKNOWN —
    // exactly like PG's `select_common_type` — and get coerced (and their
    // content validated) under the resolved common type in pass 2, so
    // `COALESCE(int_col, '42')` is integer and `COALESCE(int_col, 'x')`
    // fails with PG's `invalid input syntax for type integer: "x"`.
    let mut types = Vec::new();
    let mut all_nullable = true;

    for arg in &expr.args {
        let t = infer_expr(arg, ctx, params, TypeGoal::NONE)?;
        types.push(t.type_oid);
        if !t.nullable {
            all_nullable = false;
        }
    }

    // All non-UNKNOWN branches must share a common type, otherwise PG
    // rejects with `could not convert type X to Y`.
    let concrete_types: Vec<PgTypeOid> = types
        .iter()
        .copied()
        .filter(|&t| t != oid::UNKNOWN)
        .collect();
    let type_oid = if concrete_types.is_empty() {
        // All branches are UNKNOWN → PG §10.5 defaults to the preferred type
        // of the string category (usually `text`). Derived from the catalog so
        // we stay honest: no hardcoded OID here.
        snapshot
            .preferred_type_in_category(TypCategory::String)
            .unwrap_or(oid::UNKNOWN)
    } else {
        // Resolve over the *full* arg list (unknowns included): the
        // all-identical fast path that preserves domains must see a NULL
        // branch — `COALESCE(d, NULL)` is the base type, `COALESCE(d, d)`
        // stays `d`.
        coerce::find_common_type(&types, snapshot).ok_or_else(|| {
            // PG (SQLSTATE 42804): `COALESCE types A and B cannot be
            // matched`. PG reports the COALESCE args in source order
            // (first then last), the *opposite* of CASE which orders the
            // last branch first. We use `Invalid` to keep
            // `TypeMismatch::Display`'s generic prefix from leaking in
            // front of PG's exact wording.
            // Report base type names — PG resolves COALESCE over the domain's
            // base, so its wording says `text`, not the domain `email`.
            let first = crate::ddl::util::format_type_for_message(
                snapshot,
                snapshot.unwrap_domain(concrete_types[0]),
            );
            let last = crate::ddl::util::format_type_for_message(
                snapshot,
                snapshot.unwrap_domain(concrete_types[concrete_types.len() - 1]),
            );
            crate::error::RawError::invalid(
                format!("COALESCE types {first} and {last} cannot be matched"),
                None,
                Some(format!(
                    "add an explicit cast so the branches share a type, e.g. `expr::{last}`"
                )),
            )
            .finalize_implicit()
        })?
    };

    // Pass 2: back-fill UNKNOWN args with the resolved common type. Literal
    // content rejections propagate (PG raises them from this coercion).
    if type_oid != oid::UNKNOWN {
        for (i, arg) in expr.args.iter().enumerate() {
            if types[i] == oid::UNKNOWN {
                swallow_unless_literal(infer_expr(arg, ctx, params, TypeGoal::implicit(type_oid)))?;
            }
        }
    }

    // A `$param` directly inside COALESCE is, by construction, expected to be
    // nullable — otherwise the COALESCE would be pointless. Override with
    // `$param!` to force non-null.
    for arg in &expr.args {
        if let Some(node::Node::ParamRef(p)) = arg.node.as_ref() {
            params.infer_nullable(p.number, true);
        }
    }

    Ok(ExprType::scalar(type_oid, all_nullable))
}

// ──────────────────────────────────────────────────────────────────────────────
// CASE — two-pass (PG chapter 10.5)
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn infer_case(
    expr: &protobuf::CaseExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let Ctx { snapshot, .. } = ctx;
    // Pass 1: infer WHEN conditions with BOOL goal, results with NONE.
    let mut types = Vec::new();
    let mut any_branch_nullable = false;

    for arg in &expr.args {
        if let Some(node::Node::CaseWhen(when)) = arg.node.as_ref() {
            // WHEN condition must be boolean. On a coerce-to-bool mismatch,
            // rewrite to PG's exact wording (`argument of CASE/WHEN must be
            // type boolean, not type X`) the same way the WHERE clause does;
            // other errors carry their own message and propagate as-is.
            if let Some(cond) = &when.expr
                && let Err(e) = infer_expr(cond, ctx, params, TypeGoal::assignment(oid::BOOL))
            {
                if !matches!(e, AnalyzeError::TypeMismatch { .. }) {
                    return Err(e);
                }
                let mut params2 = params.clone();
                let actual_oid = infer_expr(cond, ctx, &mut params2, TypeGoal::NONE)
                    .map(|t| t.type_oid)
                    .unwrap_or(oid::UNKNOWN);
                let actual_pg = crate::ddl::util::format_type_for_message(snapshot, actual_oid);
                let span = crate::error::node_location(cond)
                    .and_then(crate::error::SourceSpan::from_node_qname);
                return Err(crate::error::RawError::invalid(
                    format!("argument of CASE/WHEN must be type boolean, not type {actual_pg}"),
                    span,
                    None,
                )
                .with_primary_label(format!("this is {actual_pg}, expected boolean"))
                .finalize_implicit());
            }
            // THEN result. Untyped string literals stay UNKNOWN for branch
            // reconciliation (PG's `select_common_type` behavior); their
            // content is validated under the resolved common type in pass 2,
            // so `CASE … THEN 1 ELSE 'x' END` fails with PG's `invalid input
            // syntax for type integer: "x"` while `… ELSE '2' END` is fine.
            if let Some(result) = &when.result {
                let t = infer_expr(result, ctx, params, TypeGoal::NONE)?;
                types.push(t.type_oid);
                any_branch_nullable = any_branch_nullable || t.nullable;
            }
        }
    }

    // ELSE clause.
    if let Some(defresult) = &expr.defresult {
        let t = infer_expr(defresult, ctx, params, TypeGoal::NONE)?;
        types.push(t.type_oid);
        any_branch_nullable = any_branch_nullable || t.nullable;
    } else {
        // PG adds an implicit `ELSE NULL`, and that NULL participates in
        // common-type resolution — it's what keeps `CASE WHEN c THEN
        // domain_col END` from preserving the domain (the all-same-type
        // fast path requires *every* input identical).
        types.push(oid::UNKNOWN);
        any_branch_nullable = true;
    }

    // All non-UNKNOWN branches must share a common type, otherwise PG
    // rejects with `could not convert type X to Y`.
    let concrete_types: Vec<PgTypeOid> = types
        .iter()
        .copied()
        .filter(|&t| t != oid::UNKNOWN)
        .collect();
    let type_oid = if concrete_types.is_empty() {
        // All branches are UNKNOWN → PG §10.5 defaults to the preferred type
        // of the string category (usually `text`). Derived from the catalog so
        // we stay honest: no hardcoded OID here.
        snapshot
            .preferred_type_in_category(TypCategory::String)
            .unwrap_or(oid::UNKNOWN)
    } else {
        // Full branch list (unknowns included) — see the COALESCE note: the
        // implicit `ELSE NULL` must defeat the domain-preserving fast path.
        coerce::find_common_type(&types, snapshot).ok_or_else(|| {
            // PG: `CASE types A and B cannot be matched` — last branch
            // first, candidate type from prior branches second. Report base
            // type names (domains are resolved over their base).
            let last = crate::ddl::util::format_type_for_message(
                snapshot,
                snapshot.unwrap_domain(concrete_types[concrete_types.len() - 1]),
            );
            let first = crate::ddl::util::format_type_for_message(
                snapshot,
                snapshot.unwrap_domain(concrete_types[0]),
            );
            crate::error::RawError::invalid(
                format!("CASE types {last} and {first} cannot be matched"),
                None,
                Some(format!(
                    "add an explicit cast so the branches share a type, e.g. `expr::{first}`"
                )),
            )
            .finalize_implicit()
        })?
    };

    // Pass 2: back-fill UNKNOWN result branches with the common type.
    // Literal content rejections propagate (PG raises them from this
    // coercion).
    if type_oid != oid::UNKNOWN {
        let mut type_idx = 0;
        for arg in &expr.args {
            if let Some(node::Node::CaseWhen(when)) = arg.node.as_ref()
                && let Some(result) = &when.result
            {
                if types.get(type_idx) == Some(&oid::UNKNOWN) {
                    swallow_unless_literal(infer_expr(
                        result,
                        ctx,
                        params,
                        TypeGoal::implicit(type_oid),
                    ))?;
                }
                type_idx += 1;
            }
        }
        if let Some(defresult) = &expr.defresult
            && types.get(type_idx) == Some(&oid::UNKNOWN)
        {
            swallow_unless_literal(infer_expr(
                defresult,
                ctx,
                params,
                TypeGoal::implicit(type_oid),
            ))?;
        }
    }

    Ok(ExprType::scalar(type_oid, any_branch_nullable))
}
