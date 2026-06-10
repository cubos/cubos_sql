use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// Subqueries (SubLink)
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn infer_sublink(
    sub: &protobuf::SubLink,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let Ctx {
        scope, snapshot, ..
    } = ctx;
    let sub_type = protobuf::SubLinkType::try_from(sub.sub_link_type)
        .unwrap_or(protobuf::SubLinkType::ExprSublink);

    match sub_type {
        protobuf::SubLinkType::ExistsSublink => {
            // Walk the subselect to collect any params referenced inside —
            // without this, `EXISTS(SELECT 1 FROM t WHERE x = $p1)` would
            // drop `$p1` from the param list entirely. Outer scope is
            // seeded so correlated refs (`outer.col`) resolve correctly
            // and feed types into the param resolver.
            if let Some(subselect) = &sub.subselect
                && let Some(node::Node::SelectStmt(sel)) = subselect.node.as_ref()
            {
                let _ = crate::resolve::analyze_correlated_select(sel, snapshot, params, scope)?;
            }
            Ok(ExprType::scalar(oid::BOOL, false))
        }
        protobuf::SubLinkType::ExprSublink => {
            if let Some(subselect) = &sub.subselect
                && let Some(node::Node::SelectStmt(sel)) = subselect.node.as_ref()
            {
                let (cols, _) =
                    crate::resolve::analyze_correlated_select(sel, snapshot, params, scope)?;
                if let Some(first) = cols.first() {
                    let guaranteed_one_row = sel.group_clause.is_empty()
                        && has_aggregate_target(&sel.target_list, snapshot);
                    let nullable = if guaranteed_one_row {
                        first.nullable
                    } else {
                        true
                    };
                    return Ok(ExprType::scalar(first.type_oid, nullable));
                }
            }
            Ok(ExprType::scalar(oid::UNKNOWN, true))
        }
        protobuf::SubLinkType::AnySublink | protobuf::SubLinkType::AllSublink => {
            // Walk the subselect so params inside `col = ANY(SELECT …)` /
            // `col = ALL(SELECT …)` are collected with the right types.
            if let Some(subselect) = &sub.subselect
                && let Some(node::Node::SelectStmt(sel)) = subselect.node.as_ref()
            {
                let (cols, _) =
                    crate::resolve::analyze_correlated_select(sel, snapshot, params, scope)?;

                // Arity check: `lhs IN (SELECT …)` / `lhs = ANY(SELECT …)`
                // requires the LHS and the subquery to match column counts.
                // PG rejects mismatches with `subquery has too many columns`
                // or `subquery has too few columns`.
                let lhs_arity = sub
                    .testexpr
                    .as_ref()
                    .map(|n| match n.node.as_ref() {
                        Some(node::Node::RowExpr(r)) => r.args.len(),
                        _ => 1,
                    })
                    .unwrap_or(1);
                if lhs_arity != cols.len() {
                    let pg_msg = if cols.len() < lhs_arity {
                        "subquery has too few columns"
                    } else {
                        "subquery has too many columns"
                    };
                    return Err(AnalyzeError::Invalid(format!(
                        "{pg_msg} (subquery has {}, lhs has {lhs_arity})",
                        cols.len(),
                    )));
                }

                // Resolve the comparison operator between each LHS expression
                // and the matching subquery column. PG applies the same
                // operator-resolution rules here as for a plain `a OP b`, so
                // `int_col IN (SELECT text_col …)` is rejected with
                // `operator does not exist: integer = text`. The LHS lives in
                // `testexpr` and is *only* reachable through this SubLink, so we
                // must walk it here — that also pins LHS params/columns.
                let lhs_nodes: Vec<&protobuf::Node> =
                    match sub.testexpr.as_deref().and_then(|n| n.node.as_ref()) {
                        Some(node::Node::RowExpr(r)) => r.args.iter().collect(),
                        _ => sub.testexpr.as_deref().into_iter().collect(),
                    };
                // `oper_name` is `=` for `IN`, or the written operator for
                // `<op> ANY/ALL`. Default to `=` if the parser left it empty.
                let op_name = {
                    let joined = extract_string_fields(&sub.oper_name).join(".");
                    if joined.is_empty() {
                        "=".to_string()
                    } else {
                        joined
                    }
                };
                for (lhs_node, col) in lhs_nodes.iter().zip(cols.iter()) {
                    let lhs = infer_expr(lhs_node, ctx, params, TypeGoal::NONE)?;
                    let l_oid = lhs.type_oid;
                    let r_oid = col.type_oid;
                    // An UNKNOWN side (bare literal / unpinned param) is coerced
                    // by PG to its peer — pin params and skip the rejection.
                    if l_oid == oid::UNKNOWN {
                        if r_oid != oid::UNKNOWN {
                            coerce_unknown_to(
                                lhs_node,
                                ctx,
                                params,
                                snapshot.unwrap_domain(r_oid),
                            )?;
                        }
                        continue;
                    }
                    if r_oid == oid::UNKNOWN {
                        continue;
                    }
                    if snapshot
                        .find_operator(&op_name, Some(l_oid), r_oid)
                        .is_none()
                    {
                        let left_pg = crate::ddl::util::format_type_for_message(snapshot, l_oid);
                        let right_pg = crate::ddl::util::format_type_for_message(snapshot, r_oid);
                        return Err(AnalyzeError::UndefinedOperator(format!(
                            "operator does not exist: {left_pg} {op_name} {right_pg}"
                        )));
                    }
                }
            }
            Ok(ExprType::scalar(oid::BOOL, true))
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
                let (cols, _) =
                    crate::resolve::analyze_correlated_select(sel, snapshot, params, scope)?;
                if let Some(first) = cols.first() {
                    elem_oid = first.type_oid;
                }
            }
            let array_oid = snapshot.array_type_of(elem_oid).unwrap_or(oid::UNKNOWN);
            Ok(ExprType::scalar(array_oid, false))
        }
        _ => Err(AnalyzeError::Unsupported(format!(
            "sublink type: {:?}",
            sub_type
        ))),
    }
}
