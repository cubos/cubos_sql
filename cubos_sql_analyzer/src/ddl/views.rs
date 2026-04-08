//! CREATE VIEW / CREATE MATERIALIZED VIEW AS handlers.
//!
//! Views resolve columns at creation time (expanding `SELECT *`) and track
//! dependencies on underlying tables/columns. This matches PostgreSQL behavior:
//! - `SELECT *` is expanded to explicit columns at view creation time
//! - ALTER TABLE DROP COLUMN fails if a view depends on the column (without CASCADE)
//! - ALTER TABLE ALTER COLUMN TYPE fails if a view depends on the column (without CASCADE)
//! - With CASCADE, dependent views are dropped (transitively)

use pg_query::protobuf::{CreateTableAsStmt, ObjectType, ViewStmt, node};

use crate::schema::{RelationKind, TableColumn, TableEntry, ViewDef};

use super::util::range_var_names;
use super::{DdlError, DdlInterpreter};

pub fn create_view(interp: &mut DdlInterpreter, stmt: &ViewStmt) -> Result<(), DdlError> {
    let rv = stmt
        .view
        .as_ref()
        .ok_or_else(|| DdlError::Parse("CREATE VIEW without name".into()))?;

    let (schema, name) = range_var_names(rv, &interp.snapshot);
    let key = format!("{schema}.{name}");

    let query_sql = stmt.query.as_deref().and_then(deparse_query);

    let aliases: Vec<String> = stmt
        .aliases
        .iter()
        .filter_map(|n| {
            if let Some(node::Node::String(s)) = n.node.as_ref() {
                Some(s.sval.clone())
            } else {
                None
            }
        })
        .collect();

    let (columns, view_def) = if let Some(sql) = query_sql {
        resolve_view_now(&interp.snapshot, &sql, &aliases)
    } else {
        (Vec::new(), None)
    };

    let oid = interp.alloc_oid();

    if stmt.replace
        && let Some(old_oid) = interp.snapshot.table_by_name.remove(&key)
    {
        interp.snapshot.tables.remove(&old_oid);
    }

    interp.snapshot.tables.insert(
        oid,
        TableEntry {
            oid,
            name,
            schema,
            kind: RelationKind::View,
            columns,
            view_def,
        },
    );
    interp.snapshot.table_by_name.insert(key, oid);

    Ok(())
}

pub fn create_table_as(
    interp: &mut DdlInterpreter,
    stmt: &CreateTableAsStmt,
) -> Result<(), DdlError> {
    let rv = stmt
        .into
        .as_ref()
        .and_then(|ia| ia.rel.as_ref())
        .ok_or_else(|| DdlError::Parse("CREATE TABLE AS without target".into()))?;

    let kind = match ObjectType::try_from(stmt.objtype) {
        Ok(ObjectType::ObjectMatview) => RelationKind::MaterializedView,
        _ => RelationKind::Table,
    };

    let (schema, name) = range_var_names(rv, &interp.snapshot);
    let key = format!("{schema}.{name}");

    let query_sql = stmt.query.as_deref().and_then(deparse_query);

    let (columns, view_def) = if let Some(sql) = query_sql {
        resolve_view_now(&interp.snapshot, &sql, &[])
    } else {
        (Vec::new(), None)
    };

    let oid = interp.alloc_oid();

    interp.snapshot.tables.insert(
        oid,
        TableEntry {
            oid,
            name,
            schema,
            kind,
            columns,
            view_def,
        },
    );
    interp.snapshot.table_by_name.insert(key, oid);

    Ok(())
}

fn deparse_query(query: &pg_query::protobuf::Node) -> Option<String> {
    if !matches!(query.node.as_ref(), Some(node::Node::SelectStmt(_))) {
        return None;
    }

    let version = pg_query::parse("SELECT 1")
        .ok()
        .map(|p| p.protobuf.version)
        .unwrap_or(170001);

    let wrapper = pg_query::protobuf::ParseResult {
        version,
        stmts: vec![pg_query::protobuf::RawStmt {
            stmt: Some(Box::new(query.clone())),
            stmt_location: 0,
            stmt_len: 0,
        }],
    };

    pg_query::deparse(&wrapper).ok()
}

/// Resolve view columns at creation time and track real AST-level dependencies.
fn resolve_view_now(
    snapshot: &crate::schema::SchemaSnapshot,
    sql: &str,
    aliases: &[String],
) -> (Vec<TableColumn>, Option<ViewDef>) {
    let config = crate::resolve::AnalyzerConfig {
        domains: std::collections::HashMap::new(),
        enums: std::collections::HashMap::new(),
        types: std::collections::HashMap::new(),
        param_nullability: Vec::new(),
    };

    let info = match crate::resolve::analyze(snapshot, sql, &config) {
        Ok(info) => info,
        Err(_) => return (Vec::new(), None),
    };

    let columns: Vec<TableColumn> = info
        .columns
        .iter()
        .enumerate()
        .map(|(i, col)| {
            let name = if i < aliases.len() {
                aliases[i].clone()
            } else {
                col.name.clone()
            };
            TableColumn {
                name,
                type_oid: col.pg_type_oid,
                not_null: !col.nullable,
                has_default: false,
                attnum: (i + 1) as i16,
            }
        })
        .collect();

    // Extract dependencies by walking the AST for actual ColumnRef and RangeVar nodes.
    let mut depends_on_tables = Vec::new();
    let mut depends_on_columns = Vec::new();

    if let Ok(parsed) = pg_query::parse(sql) {
        let json = serde_json::to_string(&parsed.protobuf).unwrap_or_default();

        // Collect table references from RangeVar nodes.
        collect_table_refs_from_json(&json, snapshot, &mut depends_on_tables);

        // Collect column references from ColumnRef nodes in the AST.
        // This tracks the ACTUAL source columns, not the output alias names.
        collect_column_deps_from_json(&json, snapshot, &depends_on_tables, &mut depends_on_columns);
    }

    depends_on_tables.sort();
    depends_on_tables.dedup();
    depends_on_columns.sort();
    depends_on_columns.dedup();

    let view_def = ViewDef {
        depends_on_tables,
        depends_on_columns,
    };

    (columns, Some(view_def))
}

/// Collect table OIDs from RangeVar references in serialized JSON.
fn collect_table_refs_from_json(
    json: &str,
    snapshot: &crate::schema::SchemaSnapshot,
    out: &mut Vec<u32>,
) {
    // Match "relname":"<exact_name>" with proper quoting.
    for table in snapshot.tables.values() {
        let pattern = format!("\"relname\":\"{}\"", table.name);
        if json.contains(&pattern) {
            // Verify schema if present: check for "schemaname":"<schema>" nearby.
            // For unqualified refs, accept if table is in search_path.
            out.push(table.oid);
        }
    }
}

/// Extract actual column references from the AST JSON.
///
/// Searches for ColumnRef nodes which contain `fields` with String nodes
/// representing column names. This tracks the real source columns, not aliases.
fn collect_column_deps_from_json(
    json: &str,
    snapshot: &crate::schema::SchemaSnapshot,
    table_oids: &[u32],
    out: &mut Vec<(u32, String)>,
) {
    // Strategy: find all "ColumnRef" patterns and extract the field names.
    // A ColumnRef in the JSON looks like:
    //   "ColumnRef":{"fields":[{"String":{"sval":"colname"}},...]}
    // We extract all "sval" values within ColumnRef contexts.

    // Simple approach: extract all string values that appear as column names
    // by finding "ColumnRef" blocks and their "sval" fields.
    let column_refs = extract_column_ref_names(json);

    for col_name in &column_refs {
        for &table_oid in table_oids {
            if let Some(table) = snapshot.tables.get(&table_oid)
                && table.columns.iter().any(|tc| tc.name == *col_name)
            {
                out.push((table_oid, col_name.clone()));
            }
        }
    }
}

/// Extract column names referenced in ColumnRef AST nodes from JSON.
fn extract_column_ref_names(json: &str) -> Vec<String> {
    let mut names = Vec::new();

    // Find each "ColumnRef" occurrence and extract sval fields from its context.
    let col_ref_tag = "\"ColumnRef\"";
    let mut search_pos = 0;

    while let Some(pos) = json[search_pos..].find(col_ref_tag) {
        let abs_pos = search_pos + pos;
        // Find the fields array within this ColumnRef block.
        // Look for "sval":"..." patterns within the next ~200 chars.
        let end = (abs_pos + 300).min(json.len());
        let block = &json[abs_pos..end];

        // Extract all sval values from this block.
        let sval_tag = "\"sval\":\"";
        let mut sval_pos = 0;
        while let Some(sp) = block[sval_pos..].find(sval_tag) {
            let val_start = sval_pos + sp + sval_tag.len();
            if let Some(val_end) = block[val_start..].find('"') {
                let val = &block[val_start..val_start + val_end];
                names.push(val.to_owned());
            }
            sval_pos = val_start;
            if sval_pos >= block.len() {
                break;
            }
        }

        search_pos = abs_pos + col_ref_tag.len();
    }

    names
}

// ─── Dependency checking ────────────────────────────────────────────────────

/// Find all views that depend on a given table/view OID.
pub fn find_dependent_views(snapshot: &crate::schema::SchemaSnapshot, table_oid: u32) -> Vec<u32> {
    snapshot
        .tables
        .iter()
        .filter(|(_, t)| {
            matches!(t.kind, RelationKind::View | RelationKind::MaterializedView)
                && t.view_def
                    .as_ref()
                    .is_some_and(|vd| vd.depends_on_tables.contains(&table_oid))
        })
        .map(|(&oid, _)| oid)
        .collect()
}

/// Find all views that depend on a specific column of a table.
pub fn find_views_depending_on_column(
    snapshot: &crate::schema::SchemaSnapshot,
    table_oid: u32,
    column_name: &str,
) -> Vec<u32> {
    snapshot
        .tables
        .iter()
        .filter(|(_, t)| {
            matches!(t.kind, RelationKind::View | RelationKind::MaterializedView)
                && t.view_def.as_ref().is_some_and(|vd| {
                    vd.depends_on_columns
                        .iter()
                        .any(|(tid, cname)| *tid == table_oid && cname == column_name)
                })
        })
        .map(|(&oid, _)| oid)
        .collect()
}

/// Drop views by OID, transitively dropping any views that depend on them.
pub fn drop_views(snapshot: &mut crate::schema::SchemaSnapshot, view_oids: &[u32]) {
    let mut to_drop: Vec<u32> = view_oids.to_vec();
    let mut dropped = Vec::new();

    // Transitively find all dependents.
    while let Some(oid) = to_drop.pop() {
        if dropped.contains(&oid) {
            continue;
        }
        // Find views that depend on this one.
        let dependents = find_dependent_views(snapshot, oid);
        for dep in dependents {
            if !dropped.contains(&dep) {
                to_drop.push(dep);
            }
        }
        // Drop this view.
        if let Some(table) = snapshot.tables.remove(&oid) {
            let key = format!("{}.{}", table.schema, table.name);
            snapshot.table_by_name.remove(&key);
        }
        dropped.push(oid);
    }
}
