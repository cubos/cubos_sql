use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// UNION / INTERSECT / EXCEPT
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn analyze_set_operation(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
) -> AnalyzeResult {
    let left = sel
        .larg
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("UNION without left side".into()))?;
    let right = sel
        .rarg
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("UNION without right side".into()))?;

    let (left_cols, _) = analyze_select_with_ctes(left, snapshot, params, cte_scopes)?;
    let (right_cols, _) = analyze_select_with_ctes(right, snapshot, params, cte_scopes)?;

    if left_cols.len() != right_cols.len() {
        return Err(AnalyzeError::Unsupported(
            "UNION branches have different column counts".into(),
        ));
    }

    let mut columns = Vec::with_capacity(left_cols.len());
    for (l, r) in left_cols.into_iter().zip(right_cols) {
        // When both sides carry concrete types (not UNKNOWN), their common
        // type must exist — PG rejects `SELECT 1 UNION SELECT 'x'` with
        // `UNION types integer and text cannot be matched`.
        let common = crate::coerce::find_common_type(&[l.type_oid, r.type_oid], snapshot);
        let both_concrete = l.type_oid != oid::UNKNOWN && r.type_oid != oid::UNKNOWN;
        let type_oid = match (common, both_concrete) {
            (Some(t), _) => t,
            (None, true) => {
                // PG (SQLSTATE 42804): `UNION types A and B cannot be
                // matched`. Use `Invalid` to keep
                // `TypeMismatch::Display`'s generic prefix from leaking.
                let a = crate::ddl::util::format_type_for_message(snapshot, l.type_oid);
                let b = crate::ddl::util::format_type_for_message(snapshot, r.type_oid);
                return Err(crate::error::RawError::invalid(
                    format!(
                        "UNION types {a} and {b} cannot be matched (column `{}`)",
                        l.name,
                    ),
                    None,
                    Some(format!(
                        "cast both sides to a common type, e.g. `{}::{a}`",
                        l.name,
                    )),
                )
                .finalize_implicit());
            }
            (None, false) => l.type_oid,
        };
        let typmod = if l.typmod == r.typmod { l.typmod } else { None };
        // UNION arms only carry collation forward when both sides agree
        // — same shape as the typmod merge above. Mirrors PG's collation
        // derivation rule that conflicting branches produce an
        // indeterminate (None) collation.
        let collation = if l.collation == r.collation {
            l.collation
        } else {
            None
        };
        columns.push(RawColumn {
            name: l.name,
            type_oid,
            nullable: l.nullable || r.nullable,
            typmod,
            collation,
            record_fields: None,
        });
    }

    Ok((columns, None))
}
