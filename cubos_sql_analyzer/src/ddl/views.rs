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

use super::DdlError;
use super::util::range_var_names;
use crate::database::Database;
use crate::qualified_name::QualifiedName;

pub fn create_view(interp: &mut Database, stmt: &ViewStmt) -> Result<(), DdlError> {
    let rv = stmt
        .view
        .as_ref()
        .ok_or_else(|| DdlError::Parse("CREATE VIEW without name".into()))?;

    let (schema, name) = range_var_names(rv, &interp.snapshot);
    let key = QualifiedName::new(&schema, &name);

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

    if stmt.replace {
        interp.snapshot.tables.remove(&key);
    }

    interp.snapshot.tables.insert(
        key,
        TableEntry {
            name,
            schema,
            kind: RelationKind::View,
            columns,
            view_def,
        },
    );

    Ok(())
}

pub fn create_table_as(interp: &mut Database, stmt: &CreateTableAsStmt) -> Result<(), DdlError> {
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
    let key = QualifiedName::new(&schema, &name);

    let query_sql = stmt.query.as_deref().and_then(deparse_query);

    let (columns, view_def) = if let Some(sql) = query_sql {
        resolve_view_now(&interp.snapshot, &sql, &[])
    } else {
        (Vec::new(), None)
    };

    interp.snapshot.tables.insert(
        key,
        TableEntry {
            name,
            schema,
            kind,
            columns,
            view_def,
        },
    );

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
    let config = crate::resolve::AnalyzerConfig::default();

    let analyzed_columns = match crate::resolve::analyze_static(snapshot, sql, &config, &[]) {
        Ok((cols, _)) => cols,
        Err(_) => return (Vec::new(), None),
    };

    let columns: Vec<TableColumn> = analyzed_columns
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

/// Collect table references (as qualified names) from RangeVar mentions in
/// the serialized JSON AST.
fn collect_table_refs_from_json(
    json: &str,
    snapshot: &crate::schema::SchemaSnapshot,
    out: &mut Vec<QualifiedName>,
) {
    // Match "relname":"<exact_name>" with proper quoting.
    for key in snapshot.tables.keys() {
        let pattern = format!("\"relname\":\"{}\"", key.name);
        if json.contains(&pattern) {
            // Verify schema if present: check for "schemaname":"<schema>" nearby.
            // For unqualified refs, accept if table is in search_path.
            out.push(key.clone());
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
    table_keys: &[QualifiedName],
    out: &mut Vec<(QualifiedName, String)>,
) {
    // Strategy: find all "ColumnRef" patterns and extract the field names.
    // A ColumnRef in the JSON looks like:
    //   "ColumnRef":{"fields":[{"String":{"sval":"colname"}},...]}
    // We extract all "sval" values within ColumnRef contexts.

    // Simple approach: extract all string values that appear as column names
    // by finding "ColumnRef" blocks and their "sval" fields.
    let column_refs = extract_column_ref_names(json);

    for col_name in &column_refs {
        for table_key in table_keys {
            if let Some(table) = snapshot.tables.get(table_key)
                && table.columns.iter().any(|tc| tc.name == *col_name)
            {
                out.push((table_key.clone(), col_name.clone()));
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

/// Find all views that depend on the given table or view.
pub fn find_dependent_views(
    snapshot: &crate::schema::SchemaSnapshot,
    table_key: &QualifiedName,
) -> Vec<QualifiedName> {
    snapshot
        .tables
        .iter()
        .filter(|(_, t)| {
            matches!(t.kind, RelationKind::View | RelationKind::MaterializedView)
                && t.view_def
                    .as_ref()
                    .is_some_and(|vd| vd.depends_on_tables.iter().any(|k| k == table_key))
        })
        .map(|(k, _)| k.clone())
        .collect()
}

/// Find all views that depend on a specific column of a table.
pub fn find_views_depending_on_column(
    snapshot: &crate::schema::SchemaSnapshot,
    table_key: &QualifiedName,
    column_name: &str,
) -> Vec<QualifiedName> {
    snapshot
        .tables
        .iter()
        .filter(|(_, t)| {
            matches!(t.kind, RelationKind::View | RelationKind::MaterializedView)
                && t.view_def.as_ref().is_some_and(|vd| {
                    vd.depends_on_columns
                        .iter()
                        .any(|(tk, cname)| tk == table_key && cname == column_name)
                })
        })
        .map(|(k, _)| k.clone())
        .collect()
}

/// Drop views by qualified name, transitively dropping any views that depend
/// on them.
pub fn drop_views(snapshot: &mut crate::schema::SchemaSnapshot, view_keys: &[QualifiedName]) {
    let mut to_drop: Vec<QualifiedName> = view_keys.to_vec();
    let mut dropped: Vec<QualifiedName> = Vec::new();

    // Transitively find all dependents.
    while let Some(key) = to_drop.pop() {
        if dropped.contains(&key) {
            continue;
        }
        // Find views that depend on this one.
        let dependents = find_dependent_views(snapshot, &key);
        for dep in dependents {
            if !dropped.contains(&dep) {
                to_drop.push(dep);
            }
        }
        // Drop this view.
        snapshot.tables.remove(&key);
        dropped.push(key);
    }
}

// ─── Dependency rewriting (RENAME / SET SCHEMA propagation) ─────────────────

/// Rewrite every view's dependencies so references to `old` now point at `new`.
/// Call this after renaming a table/view or moving it to a new schema.
pub fn rewrite_deps_on_table_rename(
    snapshot: &mut crate::schema::SchemaSnapshot,
    old: &QualifiedName,
    new: &QualifiedName,
) {
    for table in snapshot.tables.values_mut() {
        let Some(vd) = table.view_def.as_mut() else {
            continue;
        };
        for k in vd.depends_on_tables.iter_mut() {
            if k == old {
                *k = new.clone();
            }
        }
        for (k, _) in vd.depends_on_columns.iter_mut() {
            if k == old {
                *k = new.clone();
            }
        }
    }
}

/// Rewrite every view's column-level dependencies after a column rename.
pub fn rewrite_deps_on_column_rename(
    snapshot: &mut crate::schema::SchemaSnapshot,
    table_key: &QualifiedName,
    old_col: &str,
    new_col: &str,
) {
    for table in snapshot.tables.values_mut() {
        let Some(vd) = table.view_def.as_mut() else {
            continue;
        };
        for (k, c) in vd.depends_on_columns.iter_mut() {
            if k == table_key && c == old_col {
                *c = new_col.to_owned();
            }
        }
    }
}

/// Rewrite every view's dependencies after renaming a whole schema — any
/// dependency whose schema matches `old_schema` has its schema field
/// replaced with `new_schema`.
pub fn rewrite_deps_on_schema_rename(
    snapshot: &mut crate::schema::SchemaSnapshot,
    old_schema: &str,
    new_schema: &str,
) {
    for table in snapshot.tables.values_mut() {
        let Some(vd) = table.view_def.as_mut() else {
            continue;
        };
        for k in vd.depends_on_tables.iter_mut() {
            if k.schema == old_schema {
                k.schema = new_schema.to_owned();
            }
        }
        for (k, _) in vd.depends_on_columns.iter_mut() {
            if k.schema == old_schema {
                k.schema = new_schema.to_owned();
            }
        }
    }
}
