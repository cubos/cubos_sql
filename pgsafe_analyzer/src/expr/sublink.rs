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
