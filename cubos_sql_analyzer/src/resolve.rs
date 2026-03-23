//! Top-level query analysis: parse SQL, walk AST, produce QueryInfo.

use std::collections::HashMap;

use pg_query::protobuf::{self, node, JoinType, SetOperation};

use cubos_sql_core::query_info::{ColumnInfo, ParamInfo, QueryInfo};
use cubos_sql_core::type_map;

use crate::coerce::oid;
use crate::error::AnalyzeError;
use crate::expr;
use crate::nullability::{self, NullabilityContext};
use crate::params::ParamCollector;
use crate::schema::{SchemaSnapshot, TypeKind};
use crate::scope::{Scope, ScopeColumn};

/// Configuration for the analyzer (mirrors the relevant parts of Config).
pub struct AnalyzerConfig {
    pub domains: HashMap<String, String>,
    pub enums: HashMap<String, String>,
    pub types: HashMap<String, String>,
}

/// Analyze a SQL query and return typed parameter and column information.
///
/// This is the main entry point for static analysis. It parses the SQL using
/// `pg_query`, walks the AST to resolve types and nullability, and produces
/// the same `QueryInfo` structure as live introspection.
pub fn analyze(
    snapshot: &SchemaSnapshot,
    sql: &str,
    config: &AnalyzerConfig,
) -> Result<QueryInfo, AnalyzeError> {
    let parsed = pg_query::parse(sql).map_err(|e| AnalyzeError::Parse(e.to_string()))?;

    let stmt = parsed
        .protobuf
        .stmts
        .first()
        .and_then(|s| s.stmt.as_ref())
        .and_then(|n| n.node.as_ref())
        .ok_or_else(|| AnalyzeError::Parse("empty statement".into()))?;

    let mut params = ParamCollector::default();

    let (raw_columns, raw_params) = match stmt {
        node::Node::SelectStmt(sel) => analyze_select(sel, snapshot, &mut params)?,
        node::Node::InsertStmt(ins) => analyze_insert(ins, snapshot, &mut params)?,
        node::Node::UpdateStmt(upd) => analyze_update(upd, snapshot, &mut params)?,
        node::Node::DeleteStmt(del) => analyze_delete(del, snapshot, &mut params)?,
        _ => {
            return Err(AnalyzeError::Unsupported(format!(
                "statement type: {:?}",
                std::mem::discriminant(stmt)
            )))
        }
    };

    // Build final QueryInfo with Rust types.
    let columns = raw_columns
        .into_iter()
        .map(|rc| build_column_info(rc, snapshot, config))
        .collect::<Result<Vec<_>, _>>()?;

    let param_list = match raw_params {
        Some(p) => p,
        None => params.into_sorted()?,
    };
    let params_info = param_list
        .into_iter()
        .map(|(_, type_oid)| build_param_info(type_oid, snapshot, config))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(QueryInfo {
        params: params_info,
        columns,
    })
}

// ──────────────────────────────────────────────────────────────────────────────
// Raw output types (before Rust type mapping)
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) struct RawColumn {
    pub(crate) name: String,
    pub(crate) type_oid: u32,
    pub(crate) nullable: bool,
}

// ──────────────────────────────────────────────────────────────────────────────
// SELECT
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn analyze_select(
    sel: &protobuf::SelectStmt,
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
) -> Result<(Vec<RawColumn>, Option<Vec<(i32, u32)>>), AnalyzeError> {
    analyze_select_with_ctes(sel, snapshot, params, &HashMap::new())
}

fn analyze_select_with_ctes(
    sel: &protobuf::SelectStmt,
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
) -> Result<(Vec<RawColumn>, Option<Vec<(i32, u32)>>), AnalyzeError> {
    // Start with outer CTEs (from parent WITH clause).
    let mut cte_scopes: HashMap<String, Vec<ScopeColumn>> = outer_ctes.clone();

    // Process this SELECT's own CTEs (before UNION check, since WITH wraps UNION).
    if let Some(with) = &sel.with_clause {
        for cte_node in &with.ctes {
            if let Some(node::Node::CommonTableExpr(cte)) = cte_node.node.as_ref() {
                let cte_columns = analyze_cte(cte, snapshot, params, &cte_scopes)?;
                cte_scopes.insert(cte.ctename.clone(), cte_columns);
            }
        }
    }

    // Handle UNION/INTERSECT/EXCEPT.
    if sel.op != SetOperation::SetopNone as i32 {
        return analyze_set_operation(sel, snapshot, params, &cte_scopes);
    }

    let mut scope = Scope::default();
    let mut null_ctx = NullabilityContext::default();

    // Process FROM clause.
    process_from_clause(
        &sel.from_clause,
        &mut scope,
        &mut null_ctx,
        snapshot,
        &cte_scopes,
        params,
    )?;

    // Process WHERE clause (for param inference).
    if let Some(where_clause) = &sel.where_clause {
        let _ = expr::infer_expr(where_clause, &scope, &null_ctx, snapshot, params);
    }

    // Resolve target list (SELECT expressions).
    let columns = resolve_target_list(&sel.target_list, &scope, &null_ctx, snapshot, params)?;

    Ok((columns, None))
}

// ──────────────────────────────────────────────────────────────────────────────
// INSERT / UPDATE / DELETE
// ──────────────────────────────────────────────────────────────────────────────

fn analyze_insert(
    ins: &protobuf::InsertStmt,
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
) -> Result<(Vec<RawColumn>, Option<Vec<(i32, u32)>>), AnalyzeError> {
    let relation = ins
        .relation
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("INSERT without relation".into()))?;

    let table = snapshot
        .resolve_table(
            if relation.schemaname.is_empty() {
                None
            } else {
                Some(&relation.schemaname)
            },
            &relation.relname,
        )
        .ok_or_else(|| AnalyzeError::UnknownRelation(relation.relname.clone()))?;

    // Infer param types from column positions in INSERT ... VALUES.
    let col_names: Vec<String> = ins
        .cols
        .iter()
        .filter_map(|n| {
            if let Some(node::Node::ResTarget(rt)) = n.node.as_ref() {
                Some(rt.name.clone())
            } else {
                None
            }
        })
        .collect();

    // Match $N params in VALUES to column types, or analyze INSERT...SELECT.
    if let Some(select_node) = &ins.select_stmt {
        if let Some(node::Node::SelectStmt(val_sel)) = select_node.node.as_ref() {
            if !val_sel.values_lists.is_empty() {
                // VALUES (...) — match params by position.
                for val_list in &val_sel.values_lists {
                    if let Some(node::Node::List(list)) = val_list.node.as_ref() {
                        for (i, val) in list.items.iter().enumerate() {
                            if let Some(node::Node::ParamRef(p)) = val.node.as_ref() {
                                if let Some(col_name) = col_names.get(i) {
                                    if let Some(tc) =
                                        table.columns.iter().find(|c| &c.name == col_name)
                                    {
                                        params.record(p.number, tc.type_oid);
                                    }
                                }
                            }
                        }
                    }
                }
            } else {
                // INSERT ... SELECT — analyze the SELECT for param inference.
                let _ = analyze_select(val_sel, snapshot, params);

                // Also match SELECT target_list params to INSERT column types by position.
                for (i, target) in val_sel.target_list.iter().enumerate() {
                    if let Some(node::Node::ResTarget(rt)) = target.node.as_ref() {
                        if let Some(val) = &rt.val {
                            if let Some(node::Node::ParamRef(p)) = val.node.as_ref() {
                                if let Some(col_name) = col_names.get(i) {
                                    if let Some(tc) =
                                        table.columns.iter().find(|c| &c.name == col_name)
                                    {
                                        params.record(p.number, tc.type_oid);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Resolve RETURNING list.
    let mut scope = Scope::default();
    let null_ctx = NullabilityContext::default();
    scope.add_table_columns(&relation.relname, &table.columns);

    let columns = resolve_target_list(&ins.returning_list, &scope, &null_ctx, snapshot, params)?;

    Ok((columns, None))
}

fn analyze_update(
    upd: &protobuf::UpdateStmt,
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
) -> Result<(Vec<RawColumn>, Option<Vec<(i32, u32)>>), AnalyzeError> {
    let relation = upd
        .relation
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("UPDATE without relation".into()))?;

    let table = snapshot
        .resolve_table(
            if relation.schemaname.is_empty() {
                None
            } else {
                Some(&relation.schemaname)
            },
            &relation.relname,
        )
        .ok_or_else(|| AnalyzeError::UnknownRelation(relation.relname.clone()))?;

    // Infer param types from SET column = $N.
    for target in &upd.target_list {
        if let Some(node::Node::ResTarget(rt)) = target.node.as_ref() {
            if let Some(val) = &rt.val {
                if let Some(node::Node::ParamRef(p)) = val.node.as_ref() {
                    if let Some(tc) = table.columns.iter().find(|c| c.name == rt.name) {
                        params.record(p.number, tc.type_oid);
                    }
                }
            }
        }
    }

    // Build scope with target table + FROM clause tables.
    let mut scope = Scope::default();
    let mut null_ctx = NullabilityContext::default();
    let alias = relation
        .alias
        .as_ref()
        .map(|a| a.aliasname.as_str())
        .unwrap_or(&relation.relname);
    scope.add_table_columns(alias, &table.columns);

    // Process FROM clause (UPDATE ... FROM ... WHERE ...).
    let empty_ctes = HashMap::new();
    process_from_clause(
        &upd.from_clause,
        &mut scope,
        &mut null_ctx,
        snapshot,
        &empty_ctes,
        params,
    )?;

    if let Some(where_clause) = &upd.where_clause {
        let _ = expr::infer_expr(where_clause, &scope, &null_ctx, snapshot, params);
    }

    let columns = resolve_target_list(&upd.returning_list, &scope, &null_ctx, snapshot, params)?;
    Ok((columns, None))
}

fn analyze_delete(
    del: &protobuf::DeleteStmt,
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
) -> Result<(Vec<RawColumn>, Option<Vec<(i32, u32)>>), AnalyzeError> {
    let relation = del
        .relation
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("DELETE without relation".into()))?;

    let table = snapshot
        .resolve_table(
            if relation.schemaname.is_empty() {
                None
            } else {
                Some(&relation.schemaname)
            },
            &relation.relname,
        )
        .ok_or_else(|| AnalyzeError::UnknownRelation(relation.relname.clone()))?;

    let mut scope = Scope::default();
    let null_ctx = NullabilityContext::default();
    scope.add_table_columns(&relation.relname, &table.columns);

    if let Some(where_clause) = &del.where_clause {
        let _ = expr::infer_expr(where_clause, &scope, &null_ctx, snapshot, params);
    }

    let columns = resolve_target_list(&del.returning_list, &scope, &null_ctx, snapshot, params)?;
    Ok((columns, None))
}

// ──────────────────────────────────────────────────────────────────────────────
// UNION / INTERSECT / EXCEPT
// ──────────────────────────────────────────────────────────────────────────────

fn analyze_set_operation(
    sel: &protobuf::SelectStmt,
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
) -> Result<(Vec<RawColumn>, Option<Vec<(i32, u32)>>), AnalyzeError> {
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

    let columns = left_cols
        .into_iter()
        .zip(right_cols)
        .map(|(l, r)| {
            let type_oid = crate::coerce::find_common_type(&[l.type_oid, r.type_oid], snapshot)
                .unwrap_or(l.type_oid);
            RawColumn {
                name: l.name,
                type_oid,
                nullable: l.nullable || r.nullable,
            }
        })
        .collect();

    Ok((columns, None))
}

// ──────────────────────────────────────────────────────────────────────────────
// CTE
// ──────────────────────────────────────────────────────────────────────────────

fn analyze_cte(
    cte: &protobuf::CommonTableExpr,
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
    existing_ctes: &HashMap<String, Vec<ScopeColumn>>,
) -> Result<Vec<ScopeColumn>, AnalyzeError> {
    let cte_query = cte
        .ctequery
        .as_ref()
        .and_then(|n| n.node.as_ref())
        .ok_or_else(|| AnalyzeError::Unsupported("CTE without query".into()))?;

    match cte_query {
        node::Node::SelectStmt(sel) => {
            let (cols, _) = analyze_select_with_ctes(sel, snapshot, params, existing_ctes)?;
            Ok(cols
                .into_iter()
                .map(|rc| ScopeColumn {
                    name: rc.name,
                    type_oid: rc.type_oid,
                    base_not_null: !rc.nullable,
                    table_alias: cte.ctename.clone(),
                })
                .collect())
        }
        node::Node::InsertStmt(ins) => {
            let (cols, _) = analyze_insert(ins, snapshot, params)?;
            Ok(cols
                .into_iter()
                .map(|rc| ScopeColumn {
                    name: rc.name,
                    type_oid: rc.type_oid,
                    base_not_null: !rc.nullable,
                    table_alias: cte.ctename.clone(),
                })
                .collect())
        }
        node::Node::UpdateStmt(upd) => {
            let (cols, _) = analyze_update(upd, snapshot, params)?;
            Ok(cols
                .into_iter()
                .map(|rc| ScopeColumn {
                    name: rc.name,
                    type_oid: rc.type_oid,
                    base_not_null: !rc.nullable,
                    table_alias: cte.ctename.clone(),
                })
                .collect())
        }
        node::Node::DeleteStmt(del) => {
            let (cols, _) = analyze_delete(del, snapshot, params)?;
            Ok(cols
                .into_iter()
                .map(|rc| ScopeColumn {
                    name: rc.name,
                    type_oid: rc.type_oid,
                    base_not_null: !rc.nullable,
                    table_alias: cte.ctename.clone(),
                })
                .collect())
        }
        _ => Err(AnalyzeError::Unsupported(
            "CTE with unsupported statement type".into(),
        )),
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// FROM clause processing
// ──────────────────────────────────────────────────────────────────────────────

fn process_from_clause(
    from_clause: &[protobuf::Node],
    scope: &mut Scope,
    null_ctx: &mut NullabilityContext,
    snapshot: &SchemaSnapshot,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    for node in from_clause {
        process_from_item(node, scope, null_ctx, snapshot, cte_scopes, params)?;
    }
    Ok(())
}

fn process_from_item(
    node: &protobuf::Node,
    scope: &mut Scope,
    null_ctx: &mut NullabilityContext,
    snapshot: &SchemaSnapshot,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    let inner = node
        .node
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("empty FROM item".into()))?;

    match inner {
        node::Node::RangeVar(rv) => {
            let alias = rv
                .alias
                .as_ref()
                .map(|a| a.aliasname.as_str())
                .unwrap_or(&rv.relname);

            // Check CTEs first.
            if rv.schemaname.is_empty() {
                if let Some(cte_cols) = cte_scopes.get(&rv.relname) {
                    let cols: Vec<ScopeColumn> = cte_cols
                        .iter()
                        .cloned()
                        .map(|mut c| {
                            c.table_alias = alias.to_owned();
                            c
                        })
                        .collect();
                    scope.add_virtual_table(alias, cols);
                    return Ok(());
                }
            }

            let schema = if rv.schemaname.is_empty() {
                None
            } else {
                Some(rv.schemaname.as_str())
            };
            scope.add_table(snapshot, schema, &rv.relname, alias)?;
        }
        node::Node::JoinExpr(join) => {
            // Process left and right sides.
            let left_start = scope.sources.len();
            if let Some(larg) = &join.larg {
                process_from_item(larg, scope, null_ctx, snapshot, cte_scopes, params)?;
            }
            let left_end = scope.sources.len();

            if let Some(rarg) = &join.rarg {
                process_from_item(rarg, scope, null_ctx, snapshot, cte_scopes, params)?;
            }
            let right_end = scope.sources.len();

            // Apply JOIN nullability.
            let join_type = JoinType::try_from(join.jointype).unwrap_or(JoinType::JoinInner);

            match join_type {
                JoinType::JoinLeft => {
                    // Right side becomes nullable.
                    let right_aliases =
                        nullability::collect_aliases(&scope.sources[left_end..right_end]);
                    null_ctx.mark_all_nullable(&right_aliases);
                }
                JoinType::JoinRight => {
                    // Left side becomes nullable.
                    let left_aliases =
                        nullability::collect_aliases(&scope.sources[left_start..left_end]);
                    null_ctx.mark_all_nullable(&left_aliases);
                }
                JoinType::JoinFull => {
                    // Both sides become nullable.
                    let all_aliases =
                        nullability::collect_aliases(&scope.sources[left_start..right_end]);
                    null_ctx.mark_all_nullable(&all_aliases);
                }
                _ => {} // INNER, CROSS: no nullability change
            }
        }
        node::Node::RangeSubselect(sub) => {
            let alias = sub
                .alias
                .as_ref()
                .map(|a| a.aliasname.as_str())
                .unwrap_or("_subquery");

            if let Some(subquery) = &sub.subquery {
                if let Some(node::Node::SelectStmt(sel)) = subquery.node.as_ref() {
                    let (cols, _) = analyze_select(sel, snapshot, params)?;
                    let scope_cols: Vec<ScopeColumn> = cols
                        .into_iter()
                        .map(|rc| ScopeColumn {
                            name: rc.name,
                            type_oid: rc.type_oid,
                            base_not_null: !rc.nullable,
                            table_alias: alias.to_owned(),
                        })
                        .collect();
                    scope.add_virtual_table(alias, scope_cols);
                }
            }
        }
        _ => {
            return Err(AnalyzeError::Unsupported(format!(
                "FROM item type: {:?}",
                std::mem::discriminant(inner)
            )));
        }
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// Target list (SELECT columns)
// ──────────────────────────────────────────────────────────────────────────────

fn resolve_target_list(
    target_list: &[protobuf::Node],
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &SchemaSnapshot,
    params: &mut ParamCollector,
) -> Result<Vec<RawColumn>, AnalyzeError> {
    let mut columns = Vec::new();

    for (i, target) in target_list.iter().enumerate() {
        let rt = match target.node.as_ref() {
            Some(node::Node::ResTarget(rt)) => rt,
            _ => continue,
        };

        let val = match &rt.val {
            Some(v) => v,
            None => continue,
        };

        // Check for SELECT * or t.*.
        if let Some(node::Node::ColumnRef(cr)) = val.node.as_ref() {
            if cr
                .fields
                .iter()
                .any(|f| matches!(f.node.as_ref(), Some(node::Node::AStar(_))))
            {
                // Star expansion.
                let table_filter = cr.fields.iter().find_map(|f| match f.node.as_ref()? {
                    node::Node::String(s) => Some(s.sval.as_str()),
                    _ => None,
                });

                let star_cols: Vec<&ScopeColumn> = if let Some(tbl) = table_filter {
                    scope
                        .sources
                        .iter()
                        .filter(|s| s.alias == tbl)
                        .flat_map(|s| s.columns.iter())
                        .collect()
                } else {
                    scope.all_columns()
                };

                for col in star_cols {
                    let nullable = null_ctx.is_nullable(&col.table_alias, col.base_not_null);
                    columns.push(RawColumn {
                        name: col.name.clone(),
                        type_oid: col.type_oid,
                        nullable,
                    });
                }
                continue;
            }
        }

        let expr_type = expr::infer_expr(val, scope, null_ctx, snapshot, params)?;

        // Determine column name: explicit alias, or inferred from expression.
        let name = if !rt.name.is_empty() {
            rt.name.clone()
        } else {
            infer_column_name(val).unwrap_or_else(|| format!("?column{i}?"))
        };

        columns.push(RawColumn {
            name,
            type_oid: expr_type.type_oid,
            nullable: expr_type.nullable,
        });
    }

    Ok(columns)
}

/// Try to infer a default column name from an expression (for unaliased columns).
fn infer_column_name(node: &protobuf::Node) -> Option<String> {
    match node.node.as_ref()? {
        node::Node::ColumnRef(cr) => {
            // Last string field is the column name.
            expr::extract_string_fields(&cr.fields).pop()
        }
        node::Node::FuncCall(fc) => {
            // Function name.
            expr::extract_string_fields(&fc.funcname).pop()
        }
        _ => None,
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Rust type mapping
// ──────────────────────────────────────────────────────────────────────────────

fn build_column_info(
    rc: RawColumn,
    snapshot: &SchemaSnapshot,
    config: &AnalyzerConfig,
) -> Result<ColumnInfo, AnalyzeError> {
    let (rust_type, domain_rust_type, enum_rust_type) =
        resolve_rust_type(rc.type_oid, snapshot, config)?;

    // Handle nullability annotations (! and ?).
    let (name, nullable) = parse_nullability_annotation(&rc.name, rc.nullable);

    Ok(ColumnInfo {
        name,
        pg_type_oid: rc.type_oid,
        rust_type,
        nullable,
        domain_rust_type,
        enum_rust_type,
    })
}

fn build_param_info(
    type_oid: u32,
    snapshot: &SchemaSnapshot,
    config: &AnalyzerConfig,
) -> Result<ParamInfo, AnalyzeError> {
    let (rust_type, domain_rust_type, enum_rust_type) =
        resolve_rust_type(type_oid, snapshot, config)?;

    Ok(ParamInfo {
        pg_type_oid: type_oid,
        rust_type,
        domain_rust_type,
        enum_rust_type,
    })
}

fn resolve_rust_type(
    type_oid: u32,
    snapshot: &SchemaSnapshot,
    config: &AnalyzerConfig,
) -> Result<(String, Option<String>, Option<String>), AnalyzeError> {
    // Check type kind in snapshot.
    if let Some(te) = snapshot.get_type(type_oid) {
        match &te.kind {
            TypeKind::Domain { base_type_oid } => {
                if let Some(rust_path) = config.domains.get(&te.name) {
                    // JSONB domain.
                    return Ok((
                        "serde_json::Value".to_owned(),
                        Some(rust_path.clone()),
                        None,
                    ));
                }
                // Non-JSONB domain: unwrap to base type.
                return resolve_rust_type(*base_type_oid, snapshot, config);
            }
            TypeKind::Enum { .. } => {
                let enum_rt = config.enums.get(&te.name).cloned();
                return Ok(("String".to_owned(), None, enum_rt));
            }
            TypeKind::Array { element_type_oid } => {
                let (elem_rt, _, _) = resolve_rust_type(*element_type_oid, snapshot, config)?;
                return Ok((format!("Vec<{elem_rt}>"), None, None));
            }
            _ => {}
        }

        // Check custom types config.
        let qualified = format!("{}.{}", te.schema, te.name);
        if let Some(rt) = config
            .types
            .get(&qualified)
            .or_else(|| config.types.get(&te.name))
        {
            return Ok((rt.clone(), None, None));
        }
    }

    // Static type_map lookup.
    if let Some(info) = type_map::from_oid(type_oid) {
        return Ok((info.rust_type.to_owned(), None, None));
    }

    // Unknown type fallback.
    if type_oid == oid::UNKNOWN {
        return Ok(("String".to_owned(), None, None));
    }

    Err(AnalyzeError::UnknownType {
        oid: type_oid,
        context: format!("OID {type_oid}"),
    })
}

fn parse_nullability_annotation(name: &str, auto_nullable: bool) -> (String, bool) {
    if let Some(stripped) = name.strip_suffix('!') {
        (stripped.to_owned(), false)
    } else if let Some(stripped) = name.strip_suffix('?') {
        (stripped.to_owned(), true)
    } else {
        (name.to_owned(), auto_nullable)
    }
}
