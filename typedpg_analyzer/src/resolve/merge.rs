use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// MERGE (PG 15+)
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn analyze_merge(
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
            crate::scope::undefined_table_error(
                snapshot,
                if relation.schemaname.is_empty() {
                    None
                } else {
                    Some(relation.schemaname.as_str())
                },
                &relation.relname,
                crate::error::SourceSpan::from_node_qname(relation.location),
            )
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
    scope.add_dml_target(snapshot, &target_alias, target_qn.clone(), &table_attrs);

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
        expr::infer_expr(
            join_condition,
            expr::Ctx::new(&scope, &null_ctx, snapshot),
            params,
            TypeGoal::assignment(oid::BOOL),
        )?;
    }

    for when_node in &merge.merge_when_clauses {
        if let Some(node::Node::MergeWhenClause(when)) = when_node.node.as_ref() {
            walk_merge_when_clause(
                when,
                expr::Ctx::new(&scope, &null_ctx, snapshot),
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
    ret_scope.add_dml_target(snapshot, &target_alias, target_qn, &table_attrs);
    let ret_null_ctx = NullabilityContext::default();
    let columns = resolve_target_list(
        &merge.returning_list,
        expr::Ctx::new(&ret_scope, &ret_null_ctx, snapshot),
        params,
    )?;
    Ok((columns, None))
}

fn walk_merge_when_clause(
    when: &protobuf::MergeWhenClause,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
    table_attrs: &[crate::pg_catalog::PgAttribute],
    table_relname: &str,
) -> Result<(), AnalyzeError> {
    if let Some(condition) = &when.condition {
        expr::infer_expr(condition, ctx, params, TypeGoal::assignment(oid::BOOL))?;
    }

    let cmd = CmdType::try_from(when.command_type).unwrap_or(CmdType::Undefined);
    match cmd {
        CmdType::CmdUpdate => merge_when_update(when, ctx, params, table_attrs, table_relname),
        CmdType::CmdInsert => merge_when_insert(when, ctx, params, table_attrs, table_relname),
        CmdType::CmdDelete | CmdType::CmdNothing => {
            // No target / value expressions to walk beyond the optional
            // `AND condition` already handled above.
            Ok(())
        }
        _ => Err(AnalyzeError::Unsupported(format!(
            "MERGE WHEN command type {:?} is not supported",
            cmd
        ))),
    }
}

/// `WHEN MATCHED THEN UPDATE SET col = expr [, …]` — each entry is a
/// `ResTarget` with `name = column` and `val = expression`. Validate the
/// column exists, then walk the value with an assignment goal.
fn merge_when_update(
    when: &protobuf::MergeWhenClause,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
    table_attrs: &[crate::pg_catalog::PgAttribute],
    table_relname: &str,
) -> Result<(), AnalyzeError> {
    let snapshot = ctx.snapshot;
    for set_item in &when.target_list {
        let Some(node::Node::ResTarget(rt)) = set_item.node.as_ref() else {
            continue;
        };
        let Some(val) = &rt.val else { continue };
        let tc = table_attrs
            .iter()
            .find(|c| c.attname == rt.name)
            .ok_or_else(|| {
                crate::scope::undefined_dml_column_error(
                    &rt.name,
                    table_relname,
                    table_attrs,
                    crate::error::SourceSpan::from_node_qname(rt.location),
                )
            })?;
        if is_sql_null_literal(val)
            && let Some(err) = null_assignment_error(tc, snapshot, table_relname, "assign")
        {
            return Err(err);
        }
        if let Some(err) = crate::typmod::check_literal_assignment(
            snapshot,
            tc.atttypid,
            snapshot.effective_typmod(tc.atttypid, tc.atttypmod),
            val,
        ) {
            return Err(err);
        }
        if tc.attgenerated.is_some() && !is_set_to_default(val) {
            return Err(AnalyzeError::Invalid(format!(
                "column \"{}\" can only be updated to DEFAULT \
                 (generated column on `{}`)",
                tc.attname, table_relname,
            )));
        }
        if tc.attidentity == Some(AttIdentity::Always) && !is_set_to_default(val) {
            return Err(AnalyzeError::Invalid(format!(
                "column \"{}\" can only be updated to DEFAULT \
                 (identity column on `{}` defined as GENERATED ALWAYS)",
                tc.attname, table_relname,
            )));
        }
        expr::infer_expr(val, ctx, params, TypeGoal::assignment(tc.atttypid))?;
        if let Some(node::Node::ParamRef(p)) = val.node.as_ref()
            && !tc.attnotnull
        {
            params.infer_nullable(p.number, true);
        }
    }
    Ok(())
}

/// `WHEN NOT MATCHED THEN INSERT (cols…) VALUES (vals…)` — `target_list` holds
/// the column names (each a `ResTarget` with `name`), `values` holds the
/// parallel value expressions. An empty `target_list` implies the full
/// attribute list.
fn merge_when_insert(
    when: &protobuf::MergeWhenClause,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
    table_attrs: &[crate::pg_catalog::PgAttribute],
    table_relname: &str,
) -> Result<(), AnalyzeError> {
    let snapshot = ctx.snapshot;
    let res_targets: Vec<&protobuf::ResTarget> = when
        .target_list
        .iter()
        .filter_map(|n| match n.node.as_ref()? {
            node::Node::ResTarget(rt) if !rt.name.is_empty() => Some(rt.as_ref()),
            _ => None,
        })
        .collect();
    let target_attrs: Vec<&crate::pg_catalog::PgAttribute> = if res_targets.is_empty() {
        table_attrs.iter().collect()
    } else {
        res_targets
            .iter()
            .map(|rt| {
                table_attrs
                    .iter()
                    .find(|c| c.attname == rt.name)
                    .ok_or_else(|| {
                        crate::scope::undefined_dml_column_error(
                            &rt.name,
                            table_relname,
                            table_attrs,
                            crate::error::SourceSpan::from_node_qname(rt.location),
                        )
                    })
            })
            .collect::<Result<_, _>>()?
    };
    for (i, val) in when.values.iter().enumerate() {
        let target_col = target_attrs.get(i).copied();
        if let Some(tc) = target_col {
            if is_sql_null_literal(val)
                && let Some(err) = null_assignment_error(tc, snapshot, table_relname, "insert")
            {
                return Err(err);
            }
            if let Some(err) = crate::typmod::check_literal_assignment(
                snapshot,
                tc.atttypid,
                snapshot.effective_typmod(tc.atttypid, tc.atttypmod),
                val,
            ) {
                return Err(err);
            }
            if tc.attgenerated.is_some() && !is_set_to_default(val) {
                return Err(AnalyzeError::Invalid(format!(
                    "cannot insert a non-DEFAULT value into column \"{}\" \
                     (generated column on `{}`)",
                    tc.attname, table_relname,
                )));
            }
            if tc.attidentity == Some(AttIdentity::Always) && !is_set_to_default(val) {
                return Err(AnalyzeError::Invalid(format!(
                    "cannot insert a non-DEFAULT value into column \"{}\" \
                     (identity column on `{}` defined as GENERATED ALWAYS)",
                    tc.attname, table_relname,
                )));
            }
        }
        let goal = target_col
            .map(|tc| TypeGoal::assignment(tc.atttypid))
            .unwrap_or(TypeGoal::NONE);
        expr::infer_expr(val, ctx, params, goal)?;
        if let Some(node::Node::ParamRef(p)) = val.node.as_ref()
            && let Some(tc) = target_col
            && !tc.attnotnull
        {
            params.infer_nullable(p.number, true);
        }
    }
    Ok(())
}
