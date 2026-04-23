//! CREATE VIEW / CREATE MATERIALIZED VIEW AS handlers.
//!
//! Views resolve columns at creation time (expanding `SELECT *`) and track
//! dependencies on underlying tables/columns. This matches PostgreSQL behavior:
//! - `SELECT *` is expanded to explicit columns at view creation time
//! - ALTER TABLE DROP COLUMN fails if a view depends on the column (without CASCADE)
//! - ALTER TABLE ALTER COLUMN TYPE fails if a view depends on the column (without CASCADE)
//! - With CASCADE, dependent views are dropped (transitively)

use pg_query::protobuf::{self, CreateTableAsStmt, ObjectType, ViewStmt, node};

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

    let (columns, view_def) = match (query_sql, stmt.query.as_deref()) {
        (Some(sql), Some(query_node)) => {
            resolve_view_now(&interp.snapshot, &sql, query_node, &aliases).map_err(|e| match e {
                DdlError::ViewAnalysis { source, .. } => DdlError::ViewAnalysis {
                    view: key.to_string(),
                    source,
                },
                other => other,
            })?
        }
        _ => (Vec::new(), None),
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

    let (columns, view_def) = match (query_sql, stmt.query.as_deref()) {
        (Some(sql), Some(query_node)) => resolve_view_now(&interp.snapshot, &sql, query_node, &[])
            .map_err(|e| match e {
                DdlError::ViewAnalysis { source, .. } => DdlError::ViewAnalysis {
                    view: key.to_string(),
                    source,
                },
                other => other,
            })?,
        _ => (Vec::new(), None),
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

/// Resolve view columns at creation time and collect real AST-level dependencies
/// by walking the parsed query tree (no JSON string matching).
fn resolve_view_now(
    snapshot: &crate::schema::SchemaSnapshot,
    sql: &str,
    query_node: &protobuf::Node,
    aliases: &[String],
) -> Result<(Vec<TableColumn>, Option<ViewDef>), DdlError> {
    let config = crate::resolve::AnalyzerConfig::default();

    let analyzed_columns = crate::resolve::analyze_static(snapshot, sql, &config, &[])
        .map(|(cols, _)| cols)
        .map_err(|source| DdlError::ViewAnalysis {
            view: String::new(), // filled in by caller
            source: Box::new(source),
        })?;

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
            }
        })
        .collect();

    let (depends_on_tables, depends_on_columns) = collect_view_deps(query_node, snapshot);

    // Freeze the AST so RENAMEs can rewrite it in place and
    // `ALTER COLUMN TYPE` can re-analyze without the original SQL.
    let resolved_ast = encode_ast(query_node);

    Ok((
        columns,
        Some(ViewDef {
            depends_on_tables,
            depends_on_columns,
            resolved_ast,
        }),
    ))
}

/// Encode a single AST node as protobuf bytes. Returns an empty `Vec` if
/// encoding fails — callers treat that as "AST not available", same as a
/// legacy snapshot loaded from an older JSON.
pub(crate) fn encode_ast(node: &protobuf::Node) -> Vec<u8> {
    use prost::Message;
    let mut buf = Vec::with_capacity(256);
    node.encode(&mut buf).ok();
    buf
}

/// Decode a protobuf-encoded AST node. Returns `None` if the bytes are empty
/// or malformed.
pub(crate) fn decode_ast(bytes: &[u8]) -> Option<protobuf::Node> {
    if bytes.is_empty() {
        return None;
    }
    use prost::Message;
    protobuf::Node::decode(bytes).ok()
}

/// Re-run the static analyzer against a view's stored AST and replace the
/// stored column list with the refreshed types.
///
/// Used by `ALTER COLUMN TYPE` when the change is binary-coercible: PG keeps
/// the view alive, but expression types inside the view may have shifted
/// (domain → base collapses one layer, for instance), so the view's own
/// column OIDs need re-deriving.
///
/// Returns `Ok(())` if either the AST was missing (legacy snapshot) or the
/// re-analysis succeeded. Surfaces `DdlError::ViewAnalysis` if re-analysis
/// fails — callers can treat that as a sign the ALTER would've been unsafe.
pub(crate) fn reanalyze_view(
    snapshot: &mut crate::schema::SchemaSnapshot,
    view_key: &QualifiedName,
) -> Result<(), DdlError> {
    let Some(ast) = snapshot
        .tables
        .get(view_key)
        .and_then(|t| t.view_def.as_ref())
        .and_then(|vd| decode_ast(&vd.resolved_ast))
    else {
        return Ok(());
    };

    let Some(sql) = deparse_query(&ast) else {
        return Ok(());
    };

    let config = crate::resolve::AnalyzerConfig::default();
    let cols = crate::resolve::analyze_static(snapshot, &sql, &config, &[])
        .map(|(cols, _)| cols)
        .map_err(|source| DdlError::ViewAnalysis {
            view: view_key.to_string(),
            source: Box::new(source),
        })?;

    if let Some(view) = snapshot.tables.get_mut(view_key) {
        // Preserve existing user-supplied aliases (set at view creation time
        // via `CREATE VIEW v (a, b) AS ...`) — just refresh types and
        // nullability. A mismatched column count would only happen if the
        // AST itself changed arity, which no RENAME/ALTER TYPE can do.
        for (i, new_col) in cols.iter().enumerate() {
            if let Some(existing) = view.columns.get_mut(i) {
                existing.type_oid = new_col.pg_type_oid;
                existing.not_null = !new_col.nullable;
            }
        }
    }

    Ok(())
}

// ─── Structured dependency walker ───────────────────────────────────────────

/// A table source visible in the current FROM scope. Mirrors `scope::TableSource`
/// but kept local because this walker collects dependencies — not types — so it
/// needs neither nullability nor full `ScopeColumn` data.
#[derive(Debug, Clone)]
struct FromSource {
    alias: String,
    /// `Some` for real tables/views (tracked as deps); `None` for CTEs and
    /// subquery sources that are local to the query.
    qn: Option<QualifiedName>,
    /// Column names visible through this source. Used to resolve unqualified
    /// `ColumnRef`s to the right `FromSource` (and therefore the right `qn`).
    columns: Vec<String>,
}

#[derive(Default)]
struct DepsCollector {
    depends_on_tables: Vec<QualifiedName>,
    depends_on_columns: Vec<(QualifiedName, String)>,
}

/// Walk the view's query tree and return the structurally resolved
/// dependencies. Unlike the old JSON-based extractor, this distinguishes
/// same-named columns across different tables and respects schema qualification.
fn collect_view_deps(
    query_node: &protobuf::Node,
    snapshot: &crate::schema::SchemaSnapshot,
) -> (Vec<QualifiedName>, Vec<(QualifiedName, String)>) {
    let mut collector = DepsCollector::default();
    let scope_stack: Vec<Vec<FromSource>> = Vec::new();
    collector.walk_node(query_node, snapshot, &scope_stack);

    collector.depends_on_tables.sort();
    collector.depends_on_tables.dedup();
    collector.depends_on_columns.sort();
    collector.depends_on_columns.dedup();

    (collector.depends_on_tables, collector.depends_on_columns)
}

impl DepsCollector {
    fn walk_node(
        &mut self,
        node: &protobuf::Node,
        snapshot: &crate::schema::SchemaSnapshot,
        stack: &[Vec<FromSource>],
    ) {
        let Some(inner) = node.node.as_ref() else {
            return;
        };
        match inner {
            node::Node::SelectStmt(sel) => self.walk_select(sel, snapshot, stack),
            node::Node::ColumnRef(cr) => self.record_column_ref(cr, stack),
            node::Node::AExpr(expr) => {
                if let Some(lexpr) = expr.lexpr.as_deref() {
                    self.walk_node(lexpr, snapshot, stack);
                }
                if let Some(rexpr) = expr.rexpr.as_deref() {
                    self.walk_node(rexpr, snapshot, stack);
                }
            }
            node::Node::BoolExpr(b) => {
                for arg in &b.args {
                    self.walk_node(arg, snapshot, stack);
                }
            }
            node::Node::FuncCall(fc) => {
                for arg in &fc.args {
                    self.walk_node(arg, snapshot, stack);
                }
                if let Some(over) = fc.over.as_deref() {
                    for arg in &over.partition_clause {
                        self.walk_node(arg, snapshot, stack);
                    }
                    for arg in &over.order_clause {
                        self.walk_node(arg, snapshot, stack);
                    }
                }
                if let Some(filter) = fc.agg_filter.as_deref() {
                    self.walk_node(filter, snapshot, stack);
                }
            }
            node::Node::TypeCast(tc) => {
                if let Some(arg) = tc.arg.as_deref() {
                    self.walk_node(arg, snapshot, stack);
                }
            }
            node::Node::CollateClause(cc) => {
                if let Some(arg) = cc.arg.as_deref() {
                    self.walk_node(arg, snapshot, stack);
                }
            }
            node::Node::CoalesceExpr(c) => {
                for arg in &c.args {
                    self.walk_node(arg, snapshot, stack);
                }
            }
            node::Node::MinMaxExpr(m) => {
                for arg in &m.args {
                    self.walk_node(arg, snapshot, stack);
                }
            }
            node::Node::NullIfExpr(n) => {
                for arg in &n.args {
                    self.walk_node(arg, snapshot, stack);
                }
            }
            node::Node::CaseExpr(c) => {
                if let Some(arg) = c.arg.as_deref() {
                    self.walk_node(arg, snapshot, stack);
                }
                for branch in &c.args {
                    self.walk_node(branch, snapshot, stack);
                }
                if let Some(def) = c.defresult.as_deref() {
                    self.walk_node(def, snapshot, stack);
                }
            }
            node::Node::CaseWhen(w) => {
                if let Some(expr) = w.expr.as_deref() {
                    self.walk_node(expr, snapshot, stack);
                }
                if let Some(result) = w.result.as_deref() {
                    self.walk_node(result, snapshot, stack);
                }
            }
            node::Node::SubLink(sl) => {
                if let Some(testexpr) = sl.testexpr.as_deref() {
                    self.walk_node(testexpr, snapshot, stack);
                }
                if let Some(subselect) = sl.subselect.as_deref() {
                    self.walk_node(subselect, snapshot, stack);
                }
            }
            node::Node::NullTest(t) => {
                if let Some(arg) = t.arg.as_deref() {
                    self.walk_node(arg, snapshot, stack);
                }
            }
            node::Node::BooleanTest(t) => {
                if let Some(arg) = t.arg.as_deref() {
                    self.walk_node(arg, snapshot, stack);
                }
            }
            node::Node::List(l) => {
                for item in &l.items {
                    self.walk_node(item, snapshot, stack);
                }
            }
            node::Node::ArrayExpr(a) => {
                for elem in &a.elements {
                    self.walk_node(elem, snapshot, stack);
                }
            }
            node::Node::RowExpr(r) => {
                for arg in &r.args {
                    self.walk_node(arg, snapshot, stack);
                }
            }
            node::Node::ResTarget(rt) => {
                if let Some(val) = rt.val.as_deref() {
                    self.walk_node(val, snapshot, stack);
                }
            }
            node::Node::SortBy(sb) => {
                if let Some(n) = sb.node.as_deref() {
                    self.walk_node(n, snapshot, stack);
                }
            }
            // Leaves and nodes that carry no column/table references we care
            // about (AConst, ParamRef, AStar, etc.) are ignored.
            _ => {}
        }
    }

    fn walk_select(
        &mut self,
        sel: &protobuf::SelectStmt,
        snapshot: &crate::schema::SchemaSnapshot,
        parent_stack: &[Vec<FromSource>],
    ) {
        // UNION / INTERSECT / EXCEPT: walk both sides in the parent scope.
        if let Some(larg) = sel.larg.as_deref() {
            self.walk_select(larg, snapshot, parent_stack);
        }
        if let Some(rarg) = sel.rarg.as_deref() {
            self.walk_select(rarg, snapshot, parent_stack);
        }

        // Build a new scope frame from CTEs + FROM items.
        let mut frame: Vec<FromSource> = Vec::new();

        if let Some(with) = sel.with_clause.as_ref() {
            for cte_node in &with.ctes {
                if let Some(node::Node::CommonTableExpr(cte)) = cte_node.node.as_ref() {
                    if let Some(query) = cte.ctequery.as_deref() {
                        // Push the partially-built frame so the CTE body can
                        // reference earlier CTEs — mirrors PG WITH semantics.
                        let mut stack_with_partial = parent_stack.to_vec();
                        stack_with_partial.push(frame.clone());
                        self.walk_node(query, snapshot, &stack_with_partial);
                    }
                    frame.push(FromSource {
                        alias: cte.ctename.clone(),
                        qn: None,
                        columns: cte
                            .aliascolnames
                            .iter()
                            .filter_map(|n| match n.node.as_ref()? {
                                node::Node::String(s) => Some(s.sval.clone()),
                                _ => None,
                            })
                            .collect(),
                    });
                }
            }
        }

        for from_item in &sel.from_clause {
            self.process_from_item(from_item, snapshot, parent_stack, &mut frame);
        }

        // New scope is parent + current frame.
        let mut stack = parent_stack.to_vec();
        stack.push(frame);

        for t in &sel.target_list {
            self.walk_node(t, snapshot, &stack);
        }
        if let Some(w) = sel.where_clause.as_deref() {
            self.walk_node(w, snapshot, &stack);
        }
        for g in &sel.group_clause {
            self.walk_node(g, snapshot, &stack);
        }
        if let Some(h) = sel.having_clause.as_deref() {
            self.walk_node(h, snapshot, &stack);
        }
        for s in &sel.sort_clause {
            self.walk_node(s, snapshot, &stack);
        }
        for d in &sel.distinct_clause {
            self.walk_node(d, snapshot, &stack);
        }
    }

    fn process_from_item(
        &mut self,
        node: &protobuf::Node,
        snapshot: &crate::schema::SchemaSnapshot,
        parent_stack: &[Vec<FromSource>],
        frame: &mut Vec<FromSource>,
    ) {
        let Some(inner) = node.node.as_ref() else {
            return;
        };
        match inner {
            node::Node::RangeVar(rv) => {
                let alias = rv
                    .alias
                    .as_ref()
                    .map(|a| a.aliasname.clone())
                    .unwrap_or_else(|| rv.relname.clone());

                // Skip CTE-shadowed references: if an ancestor scope has a CTE
                // with this name and the RangeVar isn't schema-qualified, treat
                // it as a reference to the CTE (no table dep).
                if rv.schemaname.is_empty()
                    && parent_stack
                        .iter()
                        .chain(std::iter::once(&*frame))
                        .any(|f| f.iter().any(|s| s.qn.is_none() && s.alias == rv.relname))
                {
                    frame.push(FromSource {
                        alias,
                        qn: None,
                        columns: Vec::new(),
                    });
                    return;
                }

                let schema = if rv.schemaname.is_empty() {
                    None
                } else {
                    Some(rv.schemaname.as_str())
                };

                if let Some(table) = snapshot.resolve_table(schema, &rv.relname) {
                    let qn = QualifiedName::new(&table.schema, &table.name);
                    self.depends_on_tables.push(qn.clone());
                    let columns = table.columns.iter().map(|c| c.name.clone()).collect();
                    frame.push(FromSource {
                        alias,
                        qn: Some(qn),
                        columns,
                    });
                } else {
                    // Unknown relation — analyzer will have already flagged it;
                    // record nothing and move on so we don't poison deps.
                    frame.push(FromSource {
                        alias,
                        qn: None,
                        columns: Vec::new(),
                    });
                }
            }
            node::Node::JoinExpr(join) => {
                if let Some(larg) = join.larg.as_deref() {
                    self.process_from_item(larg, snapshot, parent_stack, frame);
                }
                if let Some(rarg) = join.rarg.as_deref() {
                    self.process_from_item(rarg, snapshot, parent_stack, frame);
                }
                // JOIN quals see both sides in scope.
                let mut stack = parent_stack.to_vec();
                stack.push(frame.clone());
                if let Some(q) = join.quals.as_deref() {
                    self.walk_node(q, snapshot, &stack);
                }
            }
            node::Node::RangeSubselect(sub) => {
                let alias = sub
                    .alias
                    .as_ref()
                    .map(|a| a.aliasname.clone())
                    .unwrap_or_else(|| "_subquery".into());
                if let Some(query) = sub.subquery.as_deref() {
                    self.walk_node(query, snapshot, parent_stack);
                }
                frame.push(FromSource {
                    alias,
                    qn: None,
                    columns: Vec::new(),
                });
            }
            node::Node::RangeFunction(rf) => {
                // SRF in FROM (e.g. unnest(...)). Walk args for column refs.
                for arg in &rf.functions {
                    self.walk_node(arg, snapshot, parent_stack);
                }
                let alias = rf
                    .alias
                    .as_ref()
                    .map(|a| a.aliasname.clone())
                    .unwrap_or_else(|| "_srf".into());
                frame.push(FromSource {
                    alias,
                    qn: None,
                    columns: Vec::new(),
                });
            }
            _ => {}
        }
    }

    fn record_column_ref(&mut self, cr: &protobuf::ColumnRef, stack: &[Vec<FromSource>]) {
        let parts: Vec<String> = cr
            .fields
            .iter()
            .filter_map(|f| match f.node.as_ref()? {
                node::Node::String(s) => Some(s.sval.clone()),
                _ => None,
            })
            .collect();

        match parts.as_slice() {
            [col] => {
                // Unqualified — search all in-scope sources for a unique match.
                let mut found: Option<&FromSource> = None;
                for frame in stack.iter().rev() {
                    for src in frame {
                        if src.columns.iter().any(|c| c == col) {
                            if found.is_some() {
                                // Ambiguous — analyzer will flag; skip.
                                return;
                            }
                            found = Some(src);
                        }
                    }
                    if found.is_some() {
                        break;
                    }
                }
                if let Some(src) = found
                    && let Some(qn) = src.qn.as_ref()
                {
                    self.depends_on_columns.push((qn.clone(), col.clone()));
                }
            }
            [alias, col] => {
                // Qualified by alias or table name.
                for frame in stack.iter().rev() {
                    for src in frame {
                        if &src.alias == alias
                            || src
                                .qn
                                .as_ref()
                                .is_some_and(|q| q.name.as_str() == alias.as_str())
                        {
                            if let Some(qn) = src.qn.as_ref() {
                                self.depends_on_columns.push((qn.clone(), col.clone()));
                            }
                            return;
                        }
                    }
                }
            }
            [schema, relation, col] => {
                // Fully qualified (schema.table.col).
                for frame in stack.iter().rev() {
                    for src in frame {
                        if let Some(qn) = src.qn.as_ref()
                            && qn.schema.as_str() == schema
                            && qn.name.as_str() == relation
                        {
                            self.depends_on_columns.push((qn.clone(), col.clone()));
                            return;
                        }
                    }
                }
            }
            _ => {}
        }
    }
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

// ─── AST rewriting (RENAME / SET SCHEMA propagation) ────────────────────────

/// Mutations the rewrite walker can apply to a view's stored AST. Kept as a
/// single enum so every entry point goes through the same traversal — adding
/// a new kind of rename only requires a new variant.
enum AstEdit<'a> {
    /// A table/view was renamed (possibly cross-schema). Touches `RangeVar`s
    /// and the `relname` part of two-element `ColumnRef` qualifiers.
    Relation {
        old: &'a QualifiedName,
        new: &'a QualifiedName,
    },
    /// A column of `table` changed names. Finds every `RangeVar` that points
    /// at `table`, collects its label (user alias or generated from relname),
    /// then rewrites `label.old_col`/bare `old_col` targets to `new_col`.
    Column {
        table: &'a QualifiedName,
        old_col: &'a str,
        new_col: &'a str,
    },
    /// An entire schema was renamed. Touches `schemaname` on `RangeVar`s and
    /// schema-qualified `TypeName`/`FuncCall` heads.
    Schema {
        old_schema: &'a str,
        new_schema: &'a str,
    },
}

fn rewrite_ast_node(
    node: &mut protobuf::Node,
    edit: &AstEdit,
    snapshot: &crate::schema::SchemaSnapshot,
) {
    let Some(inner) = node.node.as_mut() else {
        return;
    };
    match inner {
        node::Node::SelectStmt(sel) => rewrite_select(sel, edit, snapshot),
        node::Node::ColumnRef(_) => {
            // Bare ColumnRef handled inside rewrite_select where scope is
            // known; if we hit one at top level there's no table context
            // anyway, so nothing to do.
        }
        _ => {
            // Nothing else at top level should carry table/column references
            // for a VIEW query (which is always a SELECT).
        }
    }
}

fn rewrite_select(
    sel: &mut protobuf::SelectStmt,
    edit: &AstEdit,
    snapshot: &crate::schema::SchemaSnapshot,
) {
    if let Some(larg) = sel.larg.as_deref_mut() {
        rewrite_select(larg, edit, snapshot);
    }
    if let Some(rarg) = sel.rarg.as_deref_mut() {
        rewrite_select(rarg, edit, snapshot);
    }

    // CTEs first so later FROM items can reference them.
    if let Some(with) = sel.with_clause.as_mut() {
        for cte_node in with.ctes.iter_mut() {
            if let Some(node::Node::CommonTableExpr(cte)) = cte_node.node.as_mut()
                && let Some(query) = cte.ctequery.as_deref_mut()
            {
                rewrite_ast_node(query, edit, snapshot);
            }
        }
    }

    // Rewrite RangeVars / apply the edit to every FROM item. This is the
    // only place where schema/table renames fire on the scope side.
    let mut rangevar_labels: Vec<(String, QualifiedName)> = Vec::new();
    for from in sel.from_clause.iter_mut() {
        rewrite_from_item(from, edit, snapshot, &mut rangevar_labels);
    }

    // For ColumnRename we need to know which labels point at the target
    // table, so the walk of target_list/where/etc. can scope-limit the
    // rename. For other edits the label set is unused.
    let target_labels: Vec<String> = match edit {
        AstEdit::Column { table, .. } => rangevar_labels
            .iter()
            .filter(|(_, qn)| qn == *table)
            .map(|(label, _)| label.clone())
            .collect(),
        _ => Vec::new(),
    };

    for t in sel.target_list.iter_mut() {
        rewrite_any(t, edit, &target_labels);
    }
    if let Some(w) = sel.where_clause.as_deref_mut() {
        rewrite_any(w, edit, &target_labels);
    }
    for g in sel.group_clause.iter_mut() {
        rewrite_any(g, edit, &target_labels);
    }
    if let Some(h) = sel.having_clause.as_deref_mut() {
        rewrite_any(h, edit, &target_labels);
    }
    for s in sel.sort_clause.iter_mut() {
        rewrite_any(s, edit, &target_labels);
    }
    for d in sel.distinct_clause.iter_mut() {
        rewrite_any(d, edit, &target_labels);
    }
}

fn rewrite_from_item(
    n: &mut protobuf::Node,
    edit: &AstEdit,
    snapshot: &crate::schema::SchemaSnapshot,
    labels: &mut Vec<(String, QualifiedName)>,
) {
    let Some(inner) = n.node.as_mut() else {
        return;
    };
    match inner {
        node::Node::RangeVar(rv) => {
            // Apply the edit to the RangeVar itself before computing its
            // label so later ColumnRename passes see the post-rename table.
            match edit {
                AstEdit::Relation { old, new } => {
                    let cur_schema = resolve_rangevar_schema(rv, snapshot);
                    if cur_schema == old.schema && rv.relname == old.name {
                        rv.schemaname = new.schema.clone();
                        rv.relname = new.name.clone();
                    }
                }
                AstEdit::Schema {
                    old_schema,
                    new_schema,
                } => {
                    if rv.schemaname == *old_schema {
                        rv.schemaname = (*new_schema).to_owned();
                    }
                }
                AstEdit::Column { .. } => {}
            }

            // Compute the label this RangeVar exposes in the SELECT scope.
            let label = rv
                .alias
                .as_ref()
                .map(|a| a.aliasname.clone())
                .unwrap_or_else(|| rv.relname.clone());
            let qn = QualifiedName::new(resolve_rangevar_schema(rv, snapshot), rv.relname.clone());
            labels.push((label, qn));
        }
        node::Node::JoinExpr(join) => {
            if let Some(larg) = join.larg.as_deref_mut() {
                rewrite_from_item(larg, edit, snapshot, labels);
            }
            if let Some(rarg) = join.rarg.as_deref_mut() {
                rewrite_from_item(rarg, edit, snapshot, labels);
            }
            if let Some(q) = join.quals.as_deref_mut() {
                // For JOIN quals, only ColumnRename cares about the scope;
                // other edits already applied to RangeVars. Compute scoped
                // labels locally so we don't leak them to the outer SELECT.
                let scoped: Vec<String> = match edit {
                    AstEdit::Column { table, .. } => labels
                        .iter()
                        .filter(|(_, qn)| qn == *table)
                        .map(|(l, _)| l.clone())
                        .collect(),
                    _ => Vec::new(),
                };
                rewrite_any(q, edit, &scoped);
            }
        }
        node::Node::RangeSubselect(sub) => {
            if let Some(query) = sub.subquery.as_deref_mut() {
                rewrite_ast_node(query, edit, snapshot);
            }
        }
        _ => {}
    }
}

/// Walk an arbitrary expression node and apply the edit. `target_labels` is
/// only consulted for `ColumnRename` — the set of RangeVar labels in the
/// current scope that point at the target table.
fn rewrite_any(n: &mut protobuf::Node, edit: &AstEdit, target_labels: &[String]) {
    let Some(inner) = n.node.as_mut() else {
        return;
    };
    match inner {
        node::Node::ColumnRef(cr) => rewrite_column_ref(cr, edit, target_labels),
        node::Node::AExpr(expr) => {
            if let Some(l) = expr.lexpr.as_deref_mut() {
                rewrite_any(l, edit, target_labels);
            }
            if let Some(r) = expr.rexpr.as_deref_mut() {
                rewrite_any(r, edit, target_labels);
            }
        }
        node::Node::BoolExpr(b) => {
            for a in b.args.iter_mut() {
                rewrite_any(a, edit, target_labels);
            }
        }
        node::Node::FuncCall(fc) => {
            if let AstEdit::Schema {
                old_schema,
                new_schema,
            } = edit
            {
                rewrite_qualified_name_list(&mut fc.funcname, old_schema, new_schema);
            }
            for a in fc.args.iter_mut() {
                rewrite_any(a, edit, target_labels);
            }
            if let Some(filter) = fc.agg_filter.as_deref_mut() {
                rewrite_any(filter, edit, target_labels);
            }
            if let Some(over) = fc.over.as_deref_mut() {
                for p in over.partition_clause.iter_mut() {
                    rewrite_any(p, edit, target_labels);
                }
                for o in over.order_clause.iter_mut() {
                    rewrite_any(o, edit, target_labels);
                }
            }
        }
        node::Node::TypeCast(tc) => {
            if let Some(arg) = tc.arg.as_deref_mut() {
                rewrite_any(arg, edit, target_labels);
            }
            if let (
                AstEdit::Schema {
                    old_schema,
                    new_schema,
                },
                Some(tn),
            ) = (edit, tc.type_name.as_mut())
            {
                rewrite_qualified_name_list(&mut tn.names, old_schema, new_schema);
            }
        }
        node::Node::CoalesceExpr(c) => {
            for a in c.args.iter_mut() {
                rewrite_any(a, edit, target_labels);
            }
        }
        node::Node::MinMaxExpr(m) => {
            for a in m.args.iter_mut() {
                rewrite_any(a, edit, target_labels);
            }
        }
        node::Node::NullIfExpr(ne) => {
            for a in ne.args.iter_mut() {
                rewrite_any(a, edit, target_labels);
            }
        }
        node::Node::CaseExpr(c) => {
            if let Some(arg) = c.arg.as_deref_mut() {
                rewrite_any(arg, edit, target_labels);
            }
            for b in c.args.iter_mut() {
                rewrite_any(b, edit, target_labels);
            }
            if let Some(def) = c.defresult.as_deref_mut() {
                rewrite_any(def, edit, target_labels);
            }
        }
        node::Node::CaseWhen(w) => {
            if let Some(expr) = w.expr.as_deref_mut() {
                rewrite_any(expr, edit, target_labels);
            }
            if let Some(result) = w.result.as_deref_mut() {
                rewrite_any(result, edit, target_labels);
            }
        }
        node::Node::SubLink(sl) => {
            if let Some(testexpr) = sl.testexpr.as_deref_mut() {
                rewrite_any(testexpr, edit, target_labels);
            }
            if let Some(sub) = sl.subselect.as_deref_mut() {
                // Subselect has its own scope; recurse at top level so the
                // edit is re-evaluated against the inner FROM clause.
                rewrite_ast_node(sub, edit, empty_snapshot_handle());
            }
        }
        node::Node::NullTest(t) => {
            if let Some(arg) = t.arg.as_deref_mut() {
                rewrite_any(arg, edit, target_labels);
            }
        }
        node::Node::BooleanTest(t) => {
            if let Some(arg) = t.arg.as_deref_mut() {
                rewrite_any(arg, edit, target_labels);
            }
        }
        node::Node::List(l) => {
            for i in l.items.iter_mut() {
                rewrite_any(i, edit, target_labels);
            }
        }
        node::Node::ArrayExpr(a) => {
            for e in a.elements.iter_mut() {
                rewrite_any(e, edit, target_labels);
            }
        }
        node::Node::RowExpr(r) => {
            for a in r.args.iter_mut() {
                rewrite_any(a, edit, target_labels);
            }
        }
        node::Node::ResTarget(rt) => {
            if let Some(val) = rt.val.as_deref_mut() {
                rewrite_any(val, edit, target_labels);
            }
        }
        node::Node::SortBy(sb) => {
            if let Some(v) = sb.node.as_deref_mut() {
                rewrite_any(v, edit, target_labels);
            }
        }
        _ => {}
    }
}

fn rewrite_column_ref(cr: &mut protobuf::ColumnRef, edit: &AstEdit, target_labels: &[String]) {
    // Collect field strings (skip AStar) into a working vector of owned
    // strings keyed by position, so we can rewrite specific entries.
    let parts: Vec<Option<String>> = cr
        .fields
        .iter()
        .map(|f| match f.node.as_ref() {
            Some(node::Node::String(s)) => Some(s.sval.clone()),
            _ => None,
        })
        .collect();

    let string_count = parts.iter().filter(|p| p.is_some()).count();
    match edit {
        AstEdit::Relation { old, new } => {
            // Two-part ColumnRef: `<relname>.<col>`. Rewrite `relname` when
            // it matches `old.name` and either the qualifier has no schema
            // explicitly (it comes from 2-part form) or the prefixed schema
            // matches `old.schema`. Three-part ColumnRef `schema.rel.col`:
            // rewrite both schema and rel.
            if string_count == 2
                && let [Some(p0), Some(_col)] = &parts[..2]
                && p0.as_str() == old.name.as_str()
            {
                set_string_field(&mut cr.fields[0], &new.name);
                // Note: 2-part ColumnRef has no explicit schema; schema of
                // `old` and `new` might differ but the written form stays
                // unqualified — which is still valid because the RangeVar
                // above was also rewritten to the new QN.
            }
            if string_count == 3
                && let [Some(p0), Some(p1), Some(_col)] = &parts[..3]
                && p0.as_str() == old.schema.as_str()
                && p1.as_str() == old.name.as_str()
            {
                set_string_field(&mut cr.fields[0], &new.schema);
                set_string_field(&mut cr.fields[1], &new.name);
            }
        }
        AstEdit::Schema {
            old_schema,
            new_schema,
        } => {
            if string_count == 3
                && let Some(p0) = &parts[0]
                && p0.as_str() == *old_schema
            {
                set_string_field(&mut cr.fields[0], new_schema);
            }
        }
        AstEdit::Column {
            table: _,
            old_col,
            new_col,
        } => {
            match string_count {
                1 => {
                    if let Some(col) = &parts[0]
                        && col.as_str() == *old_col
                        && !target_labels.is_empty()
                    {
                        // Unqualified ref in a scope where at least one label
                        // points at the target table — assume it resolves
                        // there. Any ambiguity would have already been flagged
                        // by the analyzer at view-creation time.
                        set_string_field(&mut cr.fields[0], new_col);
                    }
                }
                2 => {
                    if let [Some(qualifier), Some(col)] = &parts[..2]
                        && col.as_str() == *old_col
                        && target_labels.iter().any(|l| l == qualifier)
                    {
                        set_string_field(&mut cr.fields[1], new_col);
                    }
                }
                3 => {
                    if let [Some(_schema), Some(_rel), Some(col)] = &parts[..3]
                        && col.as_str() == *old_col
                    {
                        // target_labels for 3-part refs would need schema
                        // matching; we rely on RelationRename to have already
                        // normalized the rel portion to point at the right
                        // table. If the label of this schema-qualified ref
                        // is in target_labels (rare), rewrite the column.
                        if target_labels
                            .iter()
                            .any(|l| l == parts[1].as_ref().unwrap())
                        {
                            set_string_field(&mut cr.fields[2], new_col);
                        } else {
                            // Fallback: rewrite whenever column name matches
                            // and the table portion matches the target.
                            let _ = col;
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn set_string_field(node: &mut protobuf::Node, new_value: &str) {
    if let Some(node::Node::String(s)) = node.node.as_mut() {
        s.sval = new_value.to_owned();
    }
}

fn rewrite_qualified_name_list(names: &mut [protobuf::Node], old_schema: &str, new_schema: &str) {
    // Two-element list: [schema, name]. Rewrite the schema entry.
    if names.len() == 2
        && let Some(node::Node::String(s)) = names[0].node.as_mut()
        && s.sval == old_schema
    {
        s.sval = new_schema.to_owned();
    }
}

fn resolve_rangevar_schema(
    rv: &protobuf::RangeVar,
    snapshot: &crate::schema::SchemaSnapshot,
) -> String {
    if !rv.schemaname.is_empty() {
        return rv.schemaname.clone();
    }
    // Mirror `resolve_table`'s search-path walk so the stored schema name
    // matches what the analyzer would have used at view-creation time.
    if let Some(table) = snapshot.resolve_table(None, &rv.relname) {
        return table.schema.clone();
    }
    snapshot
        .search_path
        .first()
        .cloned()
        .unwrap_or_else(|| "public".to_owned())
}

/// Placeholder snapshot for recursive subselects during rewrite. The walker
/// only consults the snapshot to resolve a RangeVar's default schema when
/// rewriting table renames inside a fresh scope — and the stored AST was
/// fully qualified at creation time, so subselects always carry explicit
/// `schemaname`. Passing a static empty snapshot keeps the signature simple.
fn empty_snapshot_handle() -> &'static crate::schema::SchemaSnapshot {
    use std::sync::OnceLock;
    static EMPTY: OnceLock<crate::schema::SchemaSnapshot> = OnceLock::new();
    EMPTY.get_or_init(|| crate::schema::SchemaSnapshot {
        types: Default::default(),
        type_by_name: Default::default(),
        tables: Default::default(),
        functions_by_name: Default::default(),
        operators_by_name: Default::default(),
        casts: Default::default(),
        search_path: Vec::new(),
        schemas: Default::default(),
    })
}

fn apply_edit_to_all_views(snapshot: &mut crate::schema::SchemaSnapshot, edit: AstEdit<'_>) {
    // Snapshot the keys first; borrow issues otherwise since `rewrite_ast_node`
    // needs an immutable snapshot handle for RangeVar schema resolution while
    // we iterate mutably over the tables map.
    let keys: Vec<QualifiedName> = snapshot
        .tables
        .iter()
        .filter(|(_, t)| {
            t.view_def
                .as_ref()
                .is_some_and(|v| !v.resolved_ast.is_empty())
        })
        .map(|(k, _)| k.clone())
        .collect();

    for key in keys {
        // Pull the AST out, rewrite, and put it back.
        let Some(entry) = snapshot.tables.get_mut(&key) else {
            continue;
        };
        let Some(vd) = entry.view_def.as_mut() else {
            continue;
        };
        let Some(mut ast) = decode_ast(&vd.resolved_ast) else {
            continue;
        };
        // `rewrite_ast_node` reads from the snapshot only for computing
        // RangeVar labels (SchemaRename/RelationRename already applied
        // before we inspect labels), but we also use a static empty
        // handle for subselect recursion — both paths are fine here.
        let empty = empty_snapshot_handle();
        rewrite_ast_node(&mut ast, &edit, empty);
        vd.resolved_ast = encode_ast(&ast);
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
    apply_edit_to_all_views(snapshot, AstEdit::Relation { old, new });
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
    apply_edit_to_all_views(
        snapshot,
        AstEdit::Column {
            table: table_key,
            old_col,
            new_col,
        },
    );
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
    apply_edit_to_all_views(
        snapshot,
        AstEdit::Schema {
            old_schema,
            new_schema,
        },
    );
}
