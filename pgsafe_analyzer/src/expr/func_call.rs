use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// Function calls — two-pass (PG chapter 10.3)
// ──────────────────────────────────────────────────────────────────────────────

/// Inferred argument types collected in Pass 1, before overload resolution.
struct FuncArgs {
    /// Type OID of each argument (UNKNOWN for untyped literals/params).
    types: Vec<PgTypeOid>,
    /// Per-argument nullability, read later by `concat_ws`/`lag`/`lead`.
    nullable: Vec<bool>,
    /// `true` if any argument is nullable.
    any_nullable: bool,
    /// Number of *direct* args (`func.args`); for ordered-set aggregates the
    /// `WITHIN GROUP (ORDER BY …)` exprs are appended to `types` after these.
    direct_count: usize,
}

pub(crate) fn infer_func_call(
    func: &protobuf::FuncCall,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let Ctx {
        null_ctx, snapshot, ..
    } = ctx;
    let func_name_parts = extract_string_fields(&func.funcname);
    let (schema, name) = match func_name_parts.as_slice() {
        [name] => (None, name.as_str()),
        [schema, name] => (Some(schema.as_str()), name.as_str()),
        _ => {
            return Err(AnalyzeError::UndefinedFunction(format!(
                "invalid function name: {:?}",
                func_name_parts
            )));
        }
    };

    validate_within_group(func)?;

    // Pass 1: infer argument types bottom-up.
    let args = collect_arg_types(func, ctx, params)?;

    // Resolve function with inferred arg types (UNKNOWN treated as wildcard).
    let resolved = functions::resolve_function(
        snapshot,
        schema,
        name,
        &args.types,
        func.agg_star,
        crate::error::SourceSpan::from_node_qname(func.location),
    )?;

    if resolved.is_aggregate {
        check_no_nested_aggregates(func, snapshot)?;
    }

    // PG's OVER-clause placement rules (parse_func.c): a true window
    // function (`prokind = 'w'`) is only callable with an OVER clause, and
    // OVER itself is only attachable to window functions and aggregates.
    // Messages verbatim; PG renders the name as written (qualified iff the
    // call was qualified).
    let written_name = func_name_parts.join(".");
    if resolved.is_window && func.over.is_none() {
        return Err(crate::error::RawError::invalid(
            format!("window function {written_name} requires an OVER clause"),
            crate::error::SourceSpan::from_node_qname(func.location),
            Some("add `OVER ()` (or a window definition) after the call".into()),
        )
        .finalize_implicit());
    }
    if func.over.is_some() && !resolved.is_window && !resolved.is_aggregate {
        return Err(crate::error::RawError::invalid(
            format!(
                "OVER specified, but {written_name} is not a window function nor an aggregate function"
            ),
            crate::error::SourceSpan::from_node_qname(func.location),
            None,
        )
        .finalize_implicit());
    }

    // Pass 2: back-fill UNKNOWN args from the resolved signature.
    backfill_func_args(func, &args, &resolved, ctx, params)?;

    // Walk aggregate / window modifiers so embedded params and column refs
    // are inferred and validated.
    walk_func_modifiers(func, ctx, params)?;

    let nullable = resolve_func_nullability(func, name, &resolved, null_ctx, &args);

    // SRFs / OUT-arg functions carry a static row shape — propagate it as
    // `record_fields` so downstream `(call(...)).field` / `(scope_col).field`
    // indirection sees the named columns with their substituted polymorphic
    // types (e.g. `_pg_expandarray(oid[]).x` → `oid`, not `anyelement`).
    let record_fields = if resolved.out_args.is_empty() {
        None
    } else {
        Some(RecordField::from_out_args(&resolved.out_args))
    };
    Ok(ExprType {
        type_oid: resolved.return_type_oid,
        nullable,
        // Functions / aggregates / window calls never propagate the
        // argument's typmod (PG matching: `lower(varchar(20))` returns
        // varchar, not varchar(20)).
        typmod: None,
        // Collation derivation through function calls is PG's most
        // intricate area (see "collation derivation" in the docs). For
        // the common case of `lower(text_col)` / `upper(text_col)` the
        // input collation flows through, but exhaustive support
        // requires the per-function `proargcollation`/`procollation`
        // we don't model. Conservatively drop collation through
        // calls — the compiler still propagates COLLATE-decorated
        // column refs for the surrounding context.
        collation: None,
        record_fields,
    })
}

/// `WITHIN GROUP (ORDER BY …)` marks an ordered-set aggregate. PG forbids
/// combining it with `OVER` or `DISTINCT`; reject those up front so the error
/// points at the actual conflict instead of a misleading overload-resolution
/// failure.
fn validate_within_group(func: &protobuf::FuncCall) -> Result<(), AnalyzeError> {
    if func.agg_within_group {
        if func.over.is_some() {
            return Err(AnalyzeError::Invalid(
                "WITHIN GROUP cannot be used with OVER".into(),
            ));
        }
        if func.agg_distinct {
            return Err(AnalyzeError::Invalid(
                "DISTINCT is not implemented for ordered-set aggregates".into(),
            ));
        }
    }
    Ok(())
}

/// Pass 1: infer every direct argument with no goal, then (for ordered-set
/// aggregates) append the `WITHIN GROUP (ORDER BY …)` expression types so
/// overload resolution sees the full signature — PG records both direct args
/// and ordered args in `pg_proc.proargtypes`.
fn collect_arg_types(
    func: &protobuf::FuncCall,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<FuncArgs, AnalyzeError> {
    let mut types = Vec::with_capacity(func.args.len());
    let mut nullable = Vec::with_capacity(func.args.len());
    let mut any_nullable = false;
    for arg in &func.args {
        let t = infer_expr(arg, ctx, params, TypeGoal::NONE)?;
        any_nullable = any_nullable || t.nullable;
        nullable.push(t.nullable);
        types.push(t.type_oid);
    }

    let direct_count = types.len();
    if func.agg_within_group {
        for order_item in &func.agg_order {
            // Each item is a `SortBy` wrapping the actual sort expression.
            let sort_inner = match order_item.node.as_ref() {
                Some(node::Node::SortBy(sb)) => sb.node.as_deref(),
                _ => Some(order_item),
            };
            if let Some(inner) = sort_inner {
                let t = infer_expr(inner, ctx, params, TypeGoal::NONE)?;
                any_nullable = any_nullable || t.nullable;
                nullable.push(t.nullable);
                types.push(t.type_oid);
            }
        }
    }

    Ok(FuncArgs {
        types,
        nullable,
        any_nullable,
        direct_count,
    })
}

/// PG forbids aggregates / window functions nested inside aggregate arguments
/// (`SUM(COUNT(*))`). Catch it after resolution, using each arg's AST.
fn check_no_nested_aggregates(
    func: &protobuf::FuncCall,
    snapshot: &PgCatalog,
) -> Result<(), AnalyzeError> {
    for arg in &func.args {
        let kinds = detect_func_kinds(arg, snapshot);
        if kinds.has_aggregate {
            return Err(AnalyzeError::Invalid(
                "aggregate function calls cannot be nested".into(),
            ));
        }
        if kinds.has_window {
            return Err(AnalyzeError::Invalid(
                "aggregate function calls cannot contain window function calls".into(),
            ));
        }
    }
    Ok(())
}

/// Pass 2: back-fill UNKNOWN direct args with the expected types from the
/// resolved signature (equivalent to PG's `coerce_func_args`). Only the direct
/// args correspond to `func.args`; ordered args (`agg_within_group`) come from
/// `func.agg_order` and are handled by [`walk_func_modifiers`].
fn backfill_func_args(
    func: &protobuf::FuncCall,
    args: &FuncArgs,
    resolved: &functions::ResolvedFunction,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    for (i, arg) in func.args.iter().enumerate() {
        if i >= args.direct_count {
            break;
        }
        if args.types[i] == oid::UNKNOWN
            && let Some(&expected) = resolved.arg_types.get(i)
            && expected != oid::UNKNOWN
        {
            // Speculative re-walk: ordinary failures are swallowed, but a
            // literal-content rejection is the parse-time error PG itself
            // raises from this argument coercion (`sqrt('x')`).
            swallow_unless_literal(infer_expr(arg, ctx, params, TypeGoal::implicit(expected)))?;
        }
    }
    Ok(())
}

/// Walk aggregate modifiers so any `$N` placeholders they contain get their
/// types inferred and column refs validated. FILTER must be bool (like a WHERE
/// clause), per-aggregate ORDER BY items have no specific goal, and the WINDOW
/// `OVER (…)` clause's expressions are walked too. None of these positions can
/// reference a select-list alias — they're all row-level — so propagating
/// errors here matches PG.
fn walk_func_modifiers(
    func: &protobuf::FuncCall,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    if let Some(filter) = &func.agg_filter {
        // FILTER is a boolean clause like WHERE; on a coerce-to-bool
        // mismatch PG names the clause: `argument of FILTER must be type
        // boolean, not type X`. Other errors propagate as-is.
        if let Err(e) = infer_expr(filter, ctx, params, TypeGoal::assignment(oid::BOOL)) {
            if !matches!(e, AnalyzeError::TypeMismatch { .. }) {
                return Err(e);
            }
            let mut params2 = params.clone();
            let actual_oid = infer_expr(filter, ctx, &mut params2, TypeGoal::NONE)
                .map(|t| t.type_oid)
                .unwrap_or(oid::UNKNOWN);
            let actual_pg = crate::ddl::util::format_type_for_message(ctx.snapshot, actual_oid);
            let span = crate::error::node_location(filter)
                .and_then(crate::error::SourceSpan::from_node_qname);
            return Err(crate::error::RawError::invalid(
                format!("argument of FILTER must be type boolean, not type {actual_pg}"),
                span,
                None,
            )
            .with_primary_label(format!("this is {actual_pg}, expected boolean"))
            .finalize_implicit());
        }
    }
    // Per-aggregate `ORDER BY` (e.g. `array_agg(x ORDER BY y)`). For
    // ordered-set aggregates (`WITHIN GROUP`) the sort expressions were
    // already inferred in Pass 1 as part of the arg list, so skip to avoid
    // double inference / param recording. Items are `SortBy` nodes — unwrap
    // to the inner expression before inferring.
    if !func.agg_within_group {
        for order_item in &func.agg_order {
            if let Some(node::Node::SortBy(sb)) = order_item.node.as_ref()
                && let Some(inner) = sb.node.as_deref()
            {
                infer_expr(inner, ctx, params, TypeGoal::NONE)?;
            }
        }
    }
    if let Some(over) = &func.over {
        for item in &over.partition_clause {
            infer_expr(item, ctx, params, TypeGoal::NONE)?;
        }
        for item in &over.order_clause {
            // Window `ORDER BY` items are also `SortBy` nodes; unwrap.
            if let Some(node::Node::SortBy(sb)) = item.node.as_ref()
                && let Some(inner) = sb.node.as_deref()
            {
                infer_expr(inner, ctx, params, TypeGoal::NONE)?;
            }
        }
        if let Some(start) = &over.start_offset {
            infer_expr(start, ctx, params, TypeGoal::NONE)?;
        }
        if let Some(end) = &over.end_offset {
            infer_expr(end, ctx, params, TypeGoal::NONE)?;
        }
    }
    Ok(())
}

/// Decide whether a function/aggregate/window call's result is nullable.
///
/// Covers value-window edge NULLs (`lag`/`lead`/…), aggregate emptiness
/// (FILTER, empty grouping sets, GROUP BY presence), strict-function
/// propagation, and the `concat_ws` separator special case.
fn resolve_func_nullability(
    func: &protobuf::FuncCall,
    name: &str,
    resolved: &functions::ResolvedFunction,
    null_ctx: &NullabilityContext,
    args: &FuncArgs,
) -> bool {
    let arg_is_nullable = |i: usize| args.nullable.get(i).copied().unwrap_or(false);

    // Value window functions (`lag`/`lead`/`first_value`/`last_value`/
    // `nth_value`) can return NULL at partition/frame edges even when the
    // source column is NOT NULL — `lag(title) OVER (ORDER BY id)` produces
    // NULL for the first row of each partition. A 3-arg `lag(col, offset,
    // default)`/`lead(...)` replaces the boundary NULL with `default`, so
    // the result is only nullable when the source column or the default
    // themselves are nullable.
    let is_value_window = func.over.is_some()
        && matches!(
            name,
            "lag" | "lead" | "first_value" | "last_value" | "nth_value"
        );

    if is_value_window {
        match name {
            "lag" | "lead" if func.args.len() >= 3 => arg_is_nullable(0) || arg_is_nullable(2),
            _ => true,
        }
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
        } else if null_ctx.has_empty_grouping_set {
            // GROUPING SETS / ROLLUP / CUBE include an empty grouping set
            // (or `GROUP BY ()` does explicitly). For that row the aggregate
            // sees the whole input — and an empty input still produces NULL
            // for non-COUNT aggregates.
            true
        } else if null_ctx.has_group_by {
            args.any_nullable
        } else {
            // Without GROUP BY, non-COUNT aggregates return NULL for empty tables.
            true
        }
    } else if resolved.is_strict && resolved.schema == "pg_catalog" {
        if functions::is_nullable_strict_exception(name) {
            true
        } else {
            args.any_nullable
        }
    } else if resolved.schema == "pg_catalog" && name == "concat_ws" {
        // `concat_ws(sep, …)` is non-strict for the variadic args (NULLs are
        // skipped), but a NULL separator makes the whole result NULL.
        arg_is_nullable(0)
    } else {
        !(!resolved.is_strict
            && resolved.schema == "pg_catalog"
            && functions::is_not_null_nonstrict(name))
    }
}
