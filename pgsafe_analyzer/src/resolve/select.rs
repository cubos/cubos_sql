use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// SELECT
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn analyze_select(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> AnalyzeResult {
    analyze_select_with_ctes_and_outer(sel, snapshot, params, &HashMap::new(), &[], &[], &[])
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
        &[],
    )
}

pub(crate) fn analyze_select_with_ctes(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
) -> AnalyzeResult {
    analyze_select_with_ctes_and_outer(sel, snapshot, params, outer_ctes, &[], &[], &[])
}

/// Core SELECT analyzer.
///
/// Three flavours of outer scope, mirroring PG's distinction:
/// - `lateral_sources`: pre-visible aliases for `LATERAL` subqueries —
///   merged into the local FROM scope so the inner query sees them as if
///   they were declared locally.
/// - `correlated_sources`: pre-visible aliases for plain sublinks
///   (`EXISTS`, scalar, `IN`, `ANY`/`ALL`) — only consulted as a fallback
///   when local resolution fails, so an inner alias of the same name
///   shadows the outer one.
/// - `shadowed_sources`: aliases visible in the enclosing FROM but
///   *unreachable* from inside this query (non-LATERAL FROM subquery). Not
///   used for resolution — only to upgrade the diagnostic from a generic
///   missing-column to PG's `invalid reference to FROM-clause entry for
///   table "x"` when the SQL tries to reach across the boundary.
pub(crate) fn analyze_select_with_ctes_and_outer(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
    lateral_sources: &[crate::scope::TableSource],
    correlated_sources: &[crate::scope::TableSource],
    shadowed_sources: &[crate::scope::TableSource],
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
    // shadows correctly. Shadowed: aliases live only as a hint for the
    // diagnostic when the SQL reaches across the boundary.
    scope.sources.extend(lateral_sources.iter().cloned());
    scope
        .outer_sources
        .extend(correlated_sources.iter().cloned());
    scope
        .shadowed_sources
        .extend(shadowed_sources.iter().cloned());
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

    // Process WHERE clause — PG uses COERCION_ASSIGNMENT + BOOL goal, and
    // emits its own wording on mismatch: `argument of WHERE must be type
    // boolean, not type X`. Catch the generic coerce error and rewrite to
    // PG's exact message so `pglite_sanity` matches.
    if let Some(where_clause) = &sel.where_clause {
        // PG rejects aggregate / window function calls inside WHERE
        // (they reference the post-aggregation row, not the pre-aggregation
        // one). Catch these statically before the type pass runs.
        check_no_aggregates_or_windows(where_clause, snapshot, "WHERE")?;
        coerce_bool_clause(
            where_clause,
            expr::Ctx::new(&scope, &null_ctx, snapshot),
            params,
            "WHERE",
        )?;
    }

    // Collect select-list aliases so GROUP BY / ORDER BY can fall back to
    // them when a bare identifier doesn't resolve against the FROM scope.
    // PG accepts `SELECT name AS n FROM t GROUP BY n ORDER BY n`; without
    // this fallback, propagating errors from those walks would regress
    // legitimate queries.
    let select_aliases: std::collections::HashSet<String> = sel
        .target_list
        .iter()
        .filter_map(|t| match t.node.as_ref()? {
            node::Node::ResTarget(rt) if !rt.name.is_empty() => Some(rt.name.clone()),
            _ => None,
        })
        .collect();

    // Process GROUP BY expressions — no type expectation, but we still need
    // to walk them so any parameters referenced are collected and typed
    // and column refs validated. `GroupingSet` nodes (`GROUPING SETS` /
    // `ROLLUP` / `CUBE`) are not real expressions; recurse into their
    // `content` to reach the underlying column references and
    // aggregate-rejection checks.
    for group_node in &sel.group_clause {
        walk_group_clause_node(
            group_node,
            expr::Ctx::new(&scope, &null_ctx, snapshot),
            params,
            &select_aliases,
        )?;
    }

    // Process HAVING clause — same boolean goal as WHERE.
    if let Some(having) = &sel.having_clause {
        coerce_bool_clause(
            having,
            expr::Ctx::new(&scope, &null_ctx, snapshot),
            params,
            "HAVING",
        )?;
    }

    // Process ORDER BY expressions. Sort items are wrapped in `SortBy` nodes
    // — we walk the inner expression so parameters referenced there (e.g.
    // `ORDER BY embedding <=> $embedding`) get their types inferred from
    // operator context and any column refs are validated. A bare
    // identifier may name a select-list alias that isn't in the FROM
    // scope (PG resolution rule); suppress `UndefinedColumn` only in that
    // exact shape so typos still surface.
    for sort_node in &sel.sort_clause {
        if let Some(node::Node::SortBy(sb)) = sort_node.node.as_ref()
            && let Some(inner) = sb.node.as_deref()
            && let Err(e) = expr::infer_expr(
                inner,
                expr::Ctx::new(&scope, &null_ctx, snapshot),
                params,
                TypeGoal::NONE,
            )
            && !is_select_alias_reference(inner, &select_aliases, &e)
        {
            return Err(e);
        }
    }

    // Process LIMIT / OFFSET — PG uses coerce_to_specific_type(INT8OID)
    // with COERCION_ASSIGNMENT, and emits its own wording on mismatch:
    // `argument of LIMIT must be type bigint, not type X` (likewise for
    // OFFSET). Catch the generic coerce error and rewrite to PG's exact
    // message so `pglite_sanity` matches.
    for (limit_node, label) in [(&sel.limit_count, "LIMIT"), (&sel.limit_offset, "OFFSET")] {
        let Some(limit_node) = limit_node else {
            continue;
        };
        // Run the inference; on a coerce-to-int8 mismatch, rewrite to PG's
        // wording. Other errors (undefined column, etc.) propagate
        // verbatim — only TypeMismatch maps to `argument of LIMIT/OFFSET`.
        if let Err(e) = expr::infer_expr(
            limit_node,
            expr::Ctx::new(&scope, &null_ctx, snapshot),
            params,
            TypeGoal::assignment(oid::INT8),
        ) {
            if !matches!(e, AnalyzeError::TypeMismatch { .. }) {
                return Err(e);
            }
            let mut params2 = params.clone();
            let actual_oid = expr::infer_expr(
                limit_node,
                expr::Ctx::new(&scope, &null_ctx, snapshot),
                &mut params2,
                TypeGoal::NONE,
            )
            .map(|t| t.type_oid)
            .unwrap_or(oid::UNKNOWN);
            let actual_pg = crate::ddl::util::format_type_for_message(snapshot, actual_oid);
            let span = crate::error::node_location(limit_node)
                .and_then(crate::error::SourceSpan::from_node_qname);
            return Err(crate::error::RawError::invalid(
                format!("argument of {label} must be type bigint, not type {actual_pg}"),
                span,
                None,
            )
            .finalize_implicit());
        }
    }

    // Resolve target list (SELECT expressions) — no type expectation.
    let columns = resolve_target_list(
        &sel.target_list,
        expr::Ctx::new(&scope, &null_ctx, snapshot),
        params,
    )?;

    // In a grouped query, every projected/HAVING/ORDER BY column must be
    // grouped or aggregated (PG SQLSTATE 42803). Checked after the target list
    // so undefined-column errors surface first, matching PG's order.
    crate::grouping::check_grouping(sel, &scope, snapshot)?;

    Ok((columns, None))
}

/// Walk one entry from `sel.group_clause`, recursing into `GroupingSet`
/// nodes (`GROUPING SETS`/`ROLLUP`/`CUBE`) to reach the underlying
/// expressions. The walk type-checks parameters and rejects aggregates /
/// window calls inside the grouping expressions (PG forbids those too).
pub(crate) fn walk_group_clause_node(
    group_node: &protobuf::Node,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
    select_aliases: &std::collections::HashSet<String>,
) -> Result<(), AnalyzeError> {
    let Ctx {
        scope,
        null_ctx,
        snapshot,
    } = ctx;
    if let Some(node::Node::GroupingSet(gs)) = group_node.node.as_ref() {
        for inner in &gs.content {
            walk_group_clause_node(
                inner,
                expr::Ctx::new(scope, null_ctx, snapshot),
                params,
                select_aliases,
            )?;
        }
        return Ok(());
    }
    check_no_aggregates_or_windows(group_node, snapshot, "GROUP BY")?;
    if let Err(e) = expr::infer_expr(
        group_node,
        expr::Ctx::new(scope, null_ctx, snapshot),
        params,
        TypeGoal::NONE,
    ) && !is_select_alias_reference(group_node, select_aliases, &e)
    {
        return Err(e);
    }
    Ok(())
}

/// Returns `true` when `node` is a bare unqualified `ColumnRef` whose name
/// matches one of `aliases` AND the error that infer_expr raised was an
/// `UndefinedColumn`. Used by GROUP BY / ORDER BY to honor PG's rule that
/// a bare identifier in those clauses may reference a select-list alias
/// that isn't visible in the FROM scope. Any other error (type mismatch,
/// undefined function, etc.) is left to propagate.
pub(crate) fn is_select_alias_reference(
    node: &protobuf::Node,
    aliases: &std::collections::HashSet<String>,
    err: &AnalyzeError,
) -> bool {
    if !matches!(err, AnalyzeError::UndefinedColumn(_)) {
        return false;
    }
    let Some(node::Node::ColumnRef(cr)) = node.node.as_ref() else {
        return false;
    };
    if cr.fields.len() != 1 {
        return false;
    }
    let Some(node::Node::String(s)) = cr.fields[0].node.as_ref() else {
        return false;
    };
    aliases.contains(&s.sval)
}
