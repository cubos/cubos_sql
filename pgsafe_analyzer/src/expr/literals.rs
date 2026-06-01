use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// Literals
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn infer_a_const(a_const: &protobuf::AConst) -> Result<ExprType, AnalyzeError> {
    if a_const.isnull {
        return Ok(ExprType::scalar(oid::UNKNOWN, true));
    }

    let type_oid = match &a_const.val {
        Some(a_const::Val::Ival(_)) => oid::INT4,
        Some(a_const::Val::Fval(_)) => oid::NUMERIC,
        Some(a_const::Val::Boolval(_)) => oid::BOOL,
        Some(a_const::Val::Sval(_)) => oid::UNKNOWN, // untyped string literal
        Some(a_const::Val::Bsval(_)) => oid::BYTEA,
        None => oid::UNKNOWN,
    };

    Ok(ExprType::scalar(type_oid, false))
}

// ──────────────────────────────────────────────────────────────────────────────
// Type casts
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn infer_type_cast(
    cast: &protobuf::TypeCast,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let Ctx { snapshot, .. } = ctx;
    let inner = cast
        .arg
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("TypeCast without arg".into()))?;

    let target_oid = resolve_type_name(cast.type_name.as_ref(), snapshot)?;

    // An explicit cast (::type / CAST) overrides type checking — we do NOT
    // check compatibility of the inner expression against the target type.
    // The inner expression is normally inferred with NONE to avoid false
    // TypeMismatch errors (e.g. age::text where int4→text has no implicit
    // cast). The one exception is a `ROW(...)::composite` shape: PG uses
    // the cast target as the composite goal so each ROW element gets
    // pinned against the matching field type — without that propagation,
    // params inside the ROW would remain indeterminate. Mirror it.
    let inner_goal = match (
        inner.node.as_ref(),
        snapshot
            .get_type(snapshot.unwrap_domain(target_oid))
            .map(|t| t.typtype),
    ) {
        (Some(node::Node::RowExpr(_)), Some(TypType::Composite)) => {
            TypeGoal::assignment(target_oid)
        }
        _ => TypeGoal::NONE,
    };
    let inner_type = infer_expr(inner, ctx, params, inner_goal)?;

    if let Some(node::Node::ParamRef(p)) = inner.node.as_ref()
        && params.get(p.number) == oid::UNKNOWN
    {
        params.record(p.number, target_oid);
    }

    // PG rejects an explicit cast with no legal path (e.g. boolean → double
    // precision) at parse time — `cannot cast type X to Y`. Mirror that, but
    // only after the inner expression's own type is known.
    if !coerce::can_cast_explicit(inner_type.type_oid, target_oid, snapshot) {
        let from = crate::ddl::util::format_type_for_message(snapshot, inner_type.type_oid);
        let to = crate::ddl::util::format_type_for_message(snapshot, target_oid);
        return Err(AnalyzeError::Invalid(format!(
            "cannot cast type {from} to {to}"
        )));
    }

    // PG: an explicit cast `x::T(n)` carries the target's typmod through.
    // When the cast omits typmods (`x::T`), keep the operand's typmod only
    // when the type OID is unchanged — coercing across types strips it.
    let target_typmod = match cast.type_name.as_ref() {
        Some(tn) if !tn.typmods.is_empty() => {
            crate::typmod::encode(snapshot, target_oid, &tn.typmods)
                .map_err(|e| AnalyzeError::Invalid(e.to_string()))?
        }
        _ if target_oid == inner_type.type_oid => inner_type.typmod,
        _ => None,
    };

    Ok(ExprType::scalar_with_typmod(
        target_oid,
        inner_type.nullable,
        target_typmod,
    ))
}
