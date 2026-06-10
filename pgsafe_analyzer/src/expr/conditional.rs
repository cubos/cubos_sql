use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// Bool expressions (AND, OR, NOT) — PG uses COERCION_ASSIGNMENT for args
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn infer_bool_expr(
    expr: &protobuf::BoolExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    // PG names the failing argument after the operator (`argument of NOT
    // must be type boolean, not type X`, likewise AND / OR) — the shared
    // clause walker owns the wording.
    let kind = match protobuf::BoolExprType::try_from(expr.boolop) {
        Ok(protobuf::BoolExprType::NotExpr) => crate::clause::ClauseKind::Not,
        Ok(protobuf::BoolExprType::OrExpr) => crate::clause::ClauseKind::Or,
        _ => crate::clause::ClauseKind::And,
    };
    let mut any_nullable = false;
    for arg in &expr.args {
        let t = crate::clause::coerce_clause_expr(arg, ctx, params, kind)?;
        any_nullable = any_nullable || t.nullable;
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
                coerce_unknown_to(arg, ctx, params, type_oid)?;
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
    // Simple CASE (`CASE arg WHEN val THEN …`): PG's transformCaseExpr
    // rewrites each WHEN into `CaseTestExpr = val` and resolves the `=`
    // operator — the WHEN values are comparands against the test
    // expression, NOT boolean conditions. Infer the test expression once
    // up front; its type drives the per-WHEN handling below.
    let test_oid = expr
        .arg
        .as_ref()
        .map(|n| infer_expr(n, ctx, params, TypeGoal::NONE))
        .transpose()?
        .map(|t| t.type_oid);

    // Pass 1: infer WHEN conditions (BOOL goal for searched CASE, `=`
    // operator resolution for simple CASE), results with NONE.
    let mut types = Vec::new();
    let mut any_branch_nullable = false;

    for arg in &expr.args {
        if let Some(node::Node::CaseWhen(when)) = arg.node.as_ref() {
            if let (Some(test_oid), Some(cond)) = (test_oid, &when.expr) {
                // Simple CASE: each WHEN value resolves `test = val` — a
                // concrete value only needs an `=` overload (coercing it to
                // the test type would wrongly reject `CASE int_col WHEN
                // 1.5 …`); an UNKNOWN value is assumed to be the test type
                // (pins params, validates literal content; domains compare
                // over their base type).
                let t = infer_expr(cond, ctx, params, TypeGoal::NONE)?;
                if test_oid != oid::UNKNOWN {
                    if t.type_oid == oid::UNKNOWN {
                        infer_expr(
                            cond,
                            ctx,
                            params,
                            TypeGoal::implicit(snapshot.unwrap_domain(test_oid)),
                        )?;
                    } else if snapshot
                        .find_operator("=", Some(test_oid), t.type_oid)
                        .is_none()
                    {
                        let l = crate::ddl::util::format_type_for_message(snapshot, test_oid);
                        let r = crate::ddl::util::format_type_for_message(snapshot, t.type_oid);
                        return Err(AnalyzeError::UndefinedOperator(format!(
                            "operator does not exist: {l} = {r}"
                        )));
                    }
                }
            }
            // Searched CASE: the WHEN condition must be boolean
            // (`argument of CASE/WHEN must be type boolean, not type X`) —
            // wording and ordering live in the shared clause walker.
            else if let Some(cond) = &when.expr {
                crate::clause::coerce_clause_expr(
                    cond,
                    ctx,
                    params,
                    crate::clause::ClauseKind::CaseWhen,
                )?;
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
                    coerce_unknown_to(result, ctx, params, type_oid)?;
                }
                type_idx += 1;
            }
        }
        if let Some(defresult) = &expr.defresult
            && types.get(type_idx) == Some(&oid::UNKNOWN)
        {
            coerce_unknown_to(defresult, ctx, params, type_oid)?;
        }
    }

    Ok(ExprType::scalar(type_oid, any_branch_nullable))
}
