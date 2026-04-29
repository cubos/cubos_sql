//! CREATE VIEW / CREATE MATERIALIZED VIEW AS handlers.
//!
//! Views resolve columns at creation time (expanding `SELECT *`) and track
//! dependencies on underlying tables/columns. This matches PostgreSQL behavior:
//! - `SELECT *` is expanded to explicit columns at view creation time
//! - ALTER TABLE DROP COLUMN fails if a view depends on the column (without CASCADE)
//! - ALTER TABLE ALTER COLUMN TYPE fails if a view depends on the column (without CASCADE)
//! - With CASCADE, dependent views are dropped (transitively)
//!
//! View → relation/column dependencies are stored in `pg_depend` rows with
//! `deptype = Normal`, `classid = refclassid = PG_CLASS_RELID`, `objid =
//! view_oid`, `refobjid = table_oid`, and `refobjsubid = attnum` (or 0 when
//! the view depends on the whole relation).

use pg_query::protobuf::{self, CreateTableAsStmt, ObjectType, ViewStmt, node};

use crate::oid::{PgClassOid, PgGenericOid, PgNamespaceOid, PgProcOid, PgTypeOid};
use crate::pg_catalog::{
    DepType, PG_CLASS_RELID, PgAttribute, PgClass, PgDepend, PgType, RelKind, TypCategory, TypType,
    ViewBinding,
};

use super::DdlError;
use super::util::ensure_range_var;
use crate::pg_catalog::PgCatalog;

pub fn create_view(interp: &mut PgCatalog, stmt: &ViewStmt) -> Result<(), DdlError> {
    let rv = stmt
        .view
        .as_ref()
        .ok_or_else(|| DdlError::Parse("CREATE VIEW without name".into()))?;

    let (nsoid, name) = ensure_range_var(interp, rv);
    let qn_label = crate::qualified_name::QualifiedName::new(
        interp.namespace_name(nsoid).unwrap_or("?"),
        name.clone(),
    )
    .to_string();

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

    let resolved = match stmt.query.as_deref() {
        Some(query_node) => {
            resolve_view_now(interp, query_node, &aliases).map_err(|e| match e {
                DdlError::ViewAnalysis { source, .. } => DdlError::ViewAnalysis {
                    view: qn_label.clone(),
                    source,
                },
                other => other,
            })?
        }
        None => ResolvedView::default(),
    };

    if stmt.replace
        && let Some(existing_oid) = interp.class_by_qname.get(&(nsoid, name.clone())).copied()
    {
        super::drop::drop_relation_by_oid(interp, existing_oid);
    }

    install_relation(interp, nsoid, name, RelKind::View, resolved);
    Ok(())
}

pub fn create_table_as(interp: &mut PgCatalog, stmt: &CreateTableAsStmt) -> Result<(), DdlError> {
    let rv = stmt
        .into
        .as_ref()
        .and_then(|ia| ia.rel.as_ref())
        .ok_or_else(|| DdlError::Parse("CREATE TABLE AS without target".into()))?;

    let kind = match ObjectType::try_from(stmt.objtype) {
        Ok(ObjectType::ObjectMatview) => RelKind::MaterializedView,
        _ => RelKind::Table,
    };

    let (nsoid, name) = ensure_range_var(interp, rv);
    let qn_label = crate::qualified_name::QualifiedName::new(
        interp.namespace_name(nsoid).unwrap_or("?"),
        name.clone(),
    )
    .to_string();

    let resolved = match stmt.query.as_deref() {
        Some(query_node) => resolve_view_now(interp, query_node, &[]).map_err(|e| match e {
            DdlError::ViewAnalysis { source, .. } => DdlError::ViewAnalysis {
                view: qn_label.clone(),
                source,
            },
            other => other,
        })?,
        None => ResolvedView::default(),
    };

    install_relation(interp, nsoid, name, kind, resolved);
    Ok(())
}

/// Bundle of analyzer outputs that [`install_relation`] needs to wire up a
/// view: column shape, the deparse-time binding side-table, the encoded
/// AST, and the deps to record in `pg_depend`.
#[derive(Default)]
struct ResolvedView {
    columns: Vec<ResolvedColumn>,
    bindings: Vec<ViewBinding>,
    ast: Vec<u8>,
    deps: ViewDeps,
}

/// Build a `pg_class` row + the matching `pg_attribute` rows + composite type
/// + array type, and write the `pg_depend` rows for the view's dependencies.
fn install_relation(
    interp: &mut PgCatalog,
    nsoid: PgNamespaceOid,
    name: String,
    relkind: RelKind,
    resolved: ResolvedView,
) {
    let ResolvedView {
        columns,
        bindings: viewbindings,
        ast: relviewdef,
        deps,
    } = resolved;
    let class_oid = PgClassOid::new(interp.alloc_oid()).expect("alloc_oid is non-zero");
    let composite_oid = PgTypeOid::new(interp.alloc_oid()).expect("alloc_oid is non-zero");
    let array_oid = PgTypeOid::new(interp.alloc_oid()).expect("alloc_oid is non-zero");

    interp.insert_pg_class(PgClass {
        oid: class_oid,
        relname: name.clone(),
        relnamespace: nsoid,
        relkind,
        reltype: Some(composite_oid),
        relviewdef,
        viewbindings,
    });
    for (i, col) in columns.iter().enumerate() {
        interp.insert_pg_attribute(PgAttribute {
            attrelid: class_oid,
            attname: col.name.clone(),
            atttypid: col.type_oid,
            attnum: (i + 1) as i16,
            attnotnull: col.not_null,
            atthasdef: false,
            attgenerated: None,
            atttypmod: col.typmod,
        });
    }
    interp.insert_pg_type(PgType {
        oid: composite_oid,
        typname: name.clone(),
        typnamespace: nsoid,
        typtype: TypType::Composite,
        typcategory: TypCategory::Composite,
        typispreferred: false,
        typrelid: Some(class_oid),
        typelem: None,
        typarray: Some(array_oid),
        typbasetype: None,
        typnotnull: false,
        typtypmod: None,
    });
    interp.insert_pg_type(PgType {
        oid: array_oid,
        typname: format!("_{name}"),
        typnamespace: nsoid,
        typtype: TypType::Base,
        typcategory: TypCategory::Array,
        typispreferred: false,
        typrelid: None,
        typelem: Some(composite_oid),
        typarray: None,
        typbasetype: None,
        typnotnull: false,
        typtypmod: None,
    });

    // Record dependencies in pg_depend.
    let class_obj = PgGenericOid::new(class_oid.get()).unwrap();
    let view_dep = |refobjid: PgClassOid, refobjsubid: i16| PgDepend {
        classid: PG_CLASS_RELID,
        objid: class_obj,
        objsubid: 0,
        refclassid: PG_CLASS_RELID,
        refobjid: PgGenericOid::new(refobjid.get()).unwrap(),
        refobjsubid,
        deptype: DepType::Normal,
    };
    let mut whole_recorded = std::collections::HashSet::new();
    for (refrelid, refattnum) in &deps.column_refs {
        interp.add_dependency(view_dep(*refrelid, *refattnum));
    }
    for refrelid in &deps.relation_refs {
        if whole_recorded.insert(*refrelid) {
            interp.add_dependency(view_dep(*refrelid, 0));
        }
    }
}

#[derive(Clone)]
struct ResolvedColumn {
    name: String,
    type_oid: PgTypeOid,
    typmod: Option<i32>,
    not_null: bool,
}

#[derive(Default, Clone)]
struct ViewDeps {
    /// `(refrelid, attnum)` pairs — one per distinct column dependency.
    column_refs: Vec<(PgClassOid, i16)>,
    /// `refrelid` values — relations the view reads from (whole-row deps).
    relation_refs: Vec<PgClassOid>,
}

/// Resolve view columns at creation time, walk the AST to emit a binding
/// stream (one OID-resolved entry per name slot), and derive the
/// `pg_depend` deps from those bindings.
fn resolve_view_now(
    snapshot: &PgCatalog,
    query_node: &protobuf::Node,
    aliases: &[String],
) -> Result<ResolvedView, DdlError> {
    let inner = query_node
        .node
        .as_ref()
        .ok_or_else(|| DdlError::Parse("CREATE VIEW with empty query node".into()))?;
    let (raw_columns, _) =
        crate::resolve::analyze_raw_node(snapshot, inner, &[]).map_err(|source| {
            DdlError::ViewAnalysis {
                view: String::new(),
                source: Box::new(source),
            }
        })?;

    let columns: Vec<ResolvedColumn> = raw_columns
        .iter()
        .enumerate()
        .map(|(i, col)| {
            let name = if i < aliases.len() {
                aliases[i].clone()
            } else {
                col.name.clone()
            };
            ResolvedColumn {
                name,
                type_oid: col.type_oid,
                typmod: col.typmod,
                not_null: !col.nullable,
            }
        })
        .collect();

    let (bindings, deps) = collect_view_bindings_and_deps(query_node, snapshot);
    let ast = encode_ast(query_node);

    Ok(ResolvedView {
        columns,
        bindings,
        ast,
        deps,
    })
}

/// Encode a single AST node as protobuf bytes.
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

/// Re-run the static analyzer against a view's stored AST and refresh the
/// stored column types. The AST is rewritten in-memory via the bindings
/// side-table before analysis, so any RENAME / SET SCHEMA on a dependency
/// is visible to the analyzer without ever round-tripping through SQL text
/// (no deparse + reparse).
pub(crate) fn reanalyze_view(
    snapshot: &mut PgCatalog,
    view_oid: PgClassOid,
) -> Result<(), DdlError> {
    let Some((ast_bytes, bindings)) = snapshot
        .pg_class
        .get(&view_oid)
        .map(|c| (c.relviewdef.clone(), c.viewbindings.clone()))
        .filter(|(b, _)| !b.is_empty())
    else {
        return Ok(());
    };
    let Some(mut ast) = decode_ast(&ast_bytes) else {
        return Ok(());
    };
    apply_view_bindings(&mut ast, &bindings, snapshot);

    let Some(stmt) = ast.node.as_ref() else {
        return Ok(());
    };

    let label = snapshot
        .pg_class
        .get(&view_oid)
        .map(|c| {
            crate::qualified_name::QualifiedName::new(
                snapshot.namespace_name(c.relnamespace).unwrap_or("?"),
                c.relname.clone(),
            )
            .to_string()
        })
        .unwrap_or_default();

    let (cols, _) = crate::resolve::analyze_raw_node(snapshot, stmt, &[]).map_err(|source| {
        DdlError::ViewAnalysis {
            view: label,
            source: Box::new(source),
        }
    })?;

    if let Some(attrs) = snapshot.pg_attribute.get_mut(&view_oid) {
        for (i, new_col) in cols.iter().enumerate() {
            if let Some(existing) = attrs.get_mut(i) {
                existing.atttypid = new_col.type_oid;
                existing.attnotnull = !new_col.nullable;
            }
        }
    }

    Ok(())
}

// ─── Structured walker: emits bindings + collects deps ──────────────────────

/// A table source visible in the current FROM scope.
#[derive(Debug, Clone)]
struct FromSource {
    alias: String,
    /// `Some` for real relations (tracked as deps); `None` for CTEs and
    /// subquery sources that are local to the query.
    relid: Option<PgClassOid>,
    /// Column names visible through this source.
    columns: Vec<String>,
}

/// Walks a view's parsed AST and emits a deterministic stream of
/// [`ViewBinding`]s — one per name slot, in pre-order. Deps for `pg_depend`
/// are derived from the bindings as a side-effect.
///
/// The applier ([`apply_view_bindings`]) walks the same AST in the same order
/// and consumes the bindings in lockstep — this is how RENAME / SET SCHEMA
/// propagate without ever editing the stored AST: at deparse time the
/// applier rewrites name slots from the OID's *current* catalog name.
#[derive(Default)]
struct ViewWalker {
    bindings: Vec<ViewBinding>,
}

fn collect_view_bindings_and_deps(
    query_node: &protobuf::Node,
    snapshot: &PgCatalog,
) -> (Vec<ViewBinding>, ViewDeps) {
    let mut walker = ViewWalker::default();
    let scope_stack: Vec<Vec<FromSource>> = Vec::new();
    walker.walk_node(query_node, snapshot, &scope_stack);
    let deps = derive_deps_from_bindings(&walker.bindings);
    (walker.bindings, deps)
}

/// Derive `(relation_refs, column_refs)` from emitted bindings — the pieces
/// `install_relation` writes into `pg_depend`.
fn derive_deps_from_bindings(bindings: &[ViewBinding]) -> ViewDeps {
    let mut relation_refs: Vec<PgClassOid> = Vec::new();
    let mut column_refs: Vec<(PgClassOid, i16)> = Vec::new();
    for b in bindings {
        match b {
            ViewBinding::Relation(oid) => relation_refs.push(*oid),
            ViewBinding::Column(rel, attnum) => column_refs.push((*rel, *attnum)),
            _ => {}
        }
    }
    relation_refs.sort();
    relation_refs.dedup();
    column_refs.sort();
    column_refs.dedup();
    ViewDeps {
        column_refs,
        relation_refs,
    }
}

impl ViewWalker {
    fn walk_node(
        &mut self,
        node: &protobuf::Node,
        snapshot: &PgCatalog,
        stack: &[Vec<FromSource>],
    ) {
        let Some(inner) = node.node.as_ref() else {
            return;
        };
        match inner {
            node::Node::SelectStmt(sel) => self.walk_select(sel, snapshot, stack),
            node::Node::ColumnRef(cr) => self.record_column_ref(cr, snapshot, stack),
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
                self.bind_func_name(&fc.funcname, snapshot);
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
                if let Some(tn) = tc.type_name.as_ref() {
                    self.bind_type_name(tn, snapshot);
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
            _ => {}
        }
    }

    fn walk_select(
        &mut self,
        sel: &protobuf::SelectStmt,
        snapshot: &PgCatalog,
        parent_stack: &[Vec<FromSource>],
    ) {
        if let Some(larg) = sel.larg.as_deref() {
            self.walk_select(larg, snapshot, parent_stack);
        }
        if let Some(rarg) = sel.rarg.as_deref() {
            self.walk_select(rarg, snapshot, parent_stack);
        }

        let mut frame: Vec<FromSource> = Vec::new();

        if let Some(with) = sel.with_clause.as_ref() {
            for cte_node in &with.ctes {
                if let Some(node::Node::CommonTableExpr(cte)) = cte_node.node.as_ref() {
                    if let Some(query) = cte.ctequery.as_deref() {
                        let mut stack_with_partial = parent_stack.to_vec();
                        stack_with_partial.push(frame.clone());
                        self.walk_node(query, snapshot, &stack_with_partial);
                    }
                    frame.push(FromSource {
                        alias: cte.ctename.clone(),
                        relid: None,
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
        snapshot: &PgCatalog,
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

                if rv.schemaname.is_empty()
                    && parent_stack
                        .iter()
                        .chain(std::iter::once(&*frame))
                        .any(|f| f.iter().any(|s| s.relid.is_none() && s.alias == rv.relname))
                {
                    // CTE / subquery alias shadowing: keep literal AST text.
                    self.bindings.push(ViewBinding::Unresolved);
                    frame.push(FromSource {
                        alias,
                        relid: None,
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
                    let relid = table.oid;
                    self.bindings.push(ViewBinding::Relation(relid));
                    let columns = snapshot
                        .attributes_of(relid)
                        .iter()
                        .map(|a| a.attname.clone())
                        .collect();
                    frame.push(FromSource {
                        alias,
                        relid: Some(relid),
                        columns,
                    });
                } else {
                    self.bindings.push(ViewBinding::Unresolved);
                    frame.push(FromSource {
                        alias,
                        relid: None,
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
                    relid: None,
                    columns: Vec::new(),
                });
            }
            node::Node::RangeFunction(rf) => {
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
                    relid: None,
                    columns: Vec::new(),
                });
            }
            _ => {}
        }
    }

    fn record_column_ref(
        &mut self,
        cr: &protobuf::ColumnRef,
        snapshot: &PgCatalog,
        stack: &[Vec<FromSource>],
    ) {
        let parts: Vec<String> = cr
            .fields
            .iter()
            .filter_map(|f| match f.node.as_ref()? {
                node::Node::String(s) => Some(s.sval.clone()),
                _ => None,
            })
            .collect();

        let resolved: Option<(PgClassOid, i16)> = match parts.as_slice() {
            [col] => resolve_column_unqualified(col, snapshot, stack),
            [alias, col] => resolve_column_aliased(alias, col, snapshot, stack),
            [schema, relation, col] => {
                resolve_column_qualified(schema, relation, col, snapshot, stack)
            }
            _ => None,
        };
        match resolved {
            Some((relid, attnum)) => self.bindings.push(ViewBinding::Column(relid, attnum)),
            None => self.bindings.push(ViewBinding::Unresolved),
        }
    }

    /// FuncCall.funcname is `[name]` or `[schema, name]`. We look up the
    /// function (any overload) and emit a Function binding with its OID;
    /// the applier uses the proc's namespace + proname to rewrite the
    /// AST literals on deparse.
    fn bind_func_name(&mut self, funcname: &[protobuf::Node], snapshot: &PgCatalog) {
        let parts: Vec<&str> = funcname
            .iter()
            .filter_map(|n| match n.node.as_ref()? {
                node::Node::String(s) => Some(s.sval.as_str()),
                _ => None,
            })
            .collect();
        let (schema, name) = match parts.as_slice() {
            [n] => (None, *n),
            [s, n] => (Some(*s), *n),
            _ => {
                self.bindings.push(ViewBinding::Unresolved);
                return;
            }
        };
        let resolved = snapshot
            .find_functions(schema, name)
            .into_iter()
            .next()
            .map(|p| p.oid);
        match resolved {
            Some(oid) => self.bindings.push(ViewBinding::Function(oid)),
            None => self.bindings.push(ViewBinding::Unresolved),
        }
    }

    /// TypeName.names is `[name]` or `[schema, name]`. Emit a Type binding
    /// when the type resolves; applier rewrites the schema portion.
    fn bind_type_name(&mut self, tn: &protobuf::TypeName, snapshot: &PgCatalog) {
        let parts: Vec<&str> = tn
            .names
            .iter()
            .filter_map(|n| match n.node.as_ref()? {
                node::Node::String(s) => Some(s.sval.as_str()),
                _ => None,
            })
            .collect();
        let (schema, name) = match parts.as_slice() {
            [n] => (None, *n),
            [s, n] => (Some(*s), *n),
            _ => {
                self.bindings.push(ViewBinding::Unresolved);
                return;
            }
        };
        let resolved = snapshot.resolve_type_by_name(schema, name).map(|t| t.oid);
        match resolved {
            Some(oid) => self.bindings.push(ViewBinding::Type(oid)),
            None => self.bindings.push(ViewBinding::Unresolved),
        }
    }
}

fn resolve_column_unqualified(
    col: &str,
    snapshot: &PgCatalog,
    stack: &[Vec<FromSource>],
) -> Option<(PgClassOid, i16)> {
    let mut found: Option<&FromSource> = None;
    for frame in stack.iter().rev() {
        for src in frame {
            if src.columns.iter().any(|c| c == col) {
                if found.is_some() {
                    return None;
                }
                found = Some(src);
            }
        }
        if found.is_some() {
            break;
        }
    }
    let src = found?;
    let relid = src.relid?;
    let attr = snapshot.attribute_by_name(relid, col)?;
    Some((relid, attr.attnum))
}

fn resolve_column_aliased(
    alias: &str,
    col: &str,
    snapshot: &PgCatalog,
    stack: &[Vec<FromSource>],
) -> Option<(PgClassOid, i16)> {
    for frame in stack.iter().rev() {
        for src in frame {
            let qn_name_match = src
                .relid
                .and_then(|r| snapshot.pg_class.get(&r))
                .is_some_and(|c| c.relname.as_str() == alias);
            if src.alias == alias || qn_name_match {
                let relid = src.relid?;
                let attr = snapshot.attribute_by_name(relid, col)?;
                return Some((relid, attr.attnum));
            }
        }
    }
    None
}

fn resolve_column_qualified(
    schema: &str,
    relation: &str,
    col: &str,
    snapshot: &PgCatalog,
    stack: &[Vec<FromSource>],
) -> Option<(PgClassOid, i16)> {
    for frame in stack.iter().rev() {
        for src in frame {
            if let Some(relid) = src.relid
                && let Some(class) = snapshot.pg_class.get(&relid)
                && snapshot
                    .namespace_name(class.relnamespace)
                    .is_some_and(|ns| ns == schema)
                && class.relname.as_str() == relation
                && let Some(attr) = snapshot.attribute_by_name(relid, col)
            {
                return Some((relid, attr.attnum));
            }
        }
    }
    None
}

// ─── Binding applier (rewrites AST in-place using bindings) ─────────────────
//
// Walks the AST in the same pre-order as `ViewWalker`, popping one binding
// per name slot and substituting the literal AST text with the OID's
// *current* catalog name. Called right before deparse / reanalysis so RENAME
// and SET SCHEMA never need to touch the stored AST.
//
// On binding/AST mismatch (e.g. binding count drifts from slot count due to
// a walker bug) the applier silently bails by treating extra slots as
// `Unresolved`. That can't happen with the matched walker pair below, but
// keeps things robust against future divergence.
struct ViewApplier<'a> {
    bindings: &'a [ViewBinding],
    cursor: usize,
    snapshot: &'a PgCatalog,
}

/// Walk `ast` mutably, applying `bindings` in the same pre-order
/// [`ViewWalker`] used to emit them. Each name slot is rewritten from the
/// OID stored in its binding to the catalog's current name.
fn apply_view_bindings(ast: &mut protobuf::Node, bindings: &[ViewBinding], snapshot: &PgCatalog) {
    let mut applier = ViewApplier {
        bindings,
        cursor: 0,
        snapshot,
    };
    applier.walk_node(ast);
}

impl<'a> ViewApplier<'a> {
    fn pop(&mut self) -> ViewBinding {
        let b = self
            .bindings
            .get(self.cursor)
            .copied()
            .unwrap_or(ViewBinding::Unresolved);
        self.cursor += 1;
        b
    }

    fn walk_node(&mut self, node: &mut protobuf::Node) {
        let Some(inner) = node.node.as_mut() else {
            return;
        };
        match inner {
            node::Node::SelectStmt(sel) => self.walk_select(sel),
            node::Node::ColumnRef(cr) => {
                let b = self.pop();
                if let ViewBinding::Column(rel, attnum) = b {
                    self.apply_column_ref(cr, rel, attnum);
                }
            }
            node::Node::AExpr(expr) => {
                if let Some(lexpr) = expr.lexpr.as_deref_mut() {
                    self.walk_node(lexpr);
                }
                if let Some(rexpr) = expr.rexpr.as_deref_mut() {
                    self.walk_node(rexpr);
                }
            }
            node::Node::BoolExpr(b) => {
                for arg in b.args.iter_mut() {
                    self.walk_node(arg);
                }
            }
            node::Node::FuncCall(fc) => {
                let b = self.pop();
                if let ViewBinding::Function(proc_oid) = b {
                    self.apply_func_name(&mut fc.funcname, proc_oid);
                }
                for arg in fc.args.iter_mut() {
                    self.walk_node(arg);
                }
                if let Some(over) = fc.over.as_deref_mut() {
                    for arg in over.partition_clause.iter_mut() {
                        self.walk_node(arg);
                    }
                    for arg in over.order_clause.iter_mut() {
                        self.walk_node(arg);
                    }
                }
                if let Some(filter) = fc.agg_filter.as_deref_mut() {
                    self.walk_node(filter);
                }
            }
            node::Node::TypeCast(tc) => {
                if let Some(arg) = tc.arg.as_deref_mut() {
                    self.walk_node(arg);
                }
                if let Some(tn) = tc.type_name.as_mut() {
                    let b = self.pop();
                    if let ViewBinding::Type(type_oid) = b {
                        self.apply_type_name(tn, type_oid);
                    }
                }
            }
            node::Node::CollateClause(cc) => {
                if let Some(arg) = cc.arg.as_deref_mut() {
                    self.walk_node(arg);
                }
            }
            node::Node::CoalesceExpr(c) => {
                for arg in c.args.iter_mut() {
                    self.walk_node(arg);
                }
            }
            node::Node::MinMaxExpr(m) => {
                for arg in m.args.iter_mut() {
                    self.walk_node(arg);
                }
            }
            node::Node::NullIfExpr(n) => {
                for arg in n.args.iter_mut() {
                    self.walk_node(arg);
                }
            }
            node::Node::CaseExpr(c) => {
                if let Some(arg) = c.arg.as_deref_mut() {
                    self.walk_node(arg);
                }
                for branch in c.args.iter_mut() {
                    self.walk_node(branch);
                }
                if let Some(def) = c.defresult.as_deref_mut() {
                    self.walk_node(def);
                }
            }
            node::Node::CaseWhen(w) => {
                if let Some(expr) = w.expr.as_deref_mut() {
                    self.walk_node(expr);
                }
                if let Some(result) = w.result.as_deref_mut() {
                    self.walk_node(result);
                }
            }
            node::Node::SubLink(sl) => {
                if let Some(testexpr) = sl.testexpr.as_deref_mut() {
                    self.walk_node(testexpr);
                }
                if let Some(subselect) = sl.subselect.as_deref_mut() {
                    self.walk_node(subselect);
                }
            }
            node::Node::NullTest(t) => {
                if let Some(arg) = t.arg.as_deref_mut() {
                    self.walk_node(arg);
                }
            }
            node::Node::BooleanTest(t) => {
                if let Some(arg) = t.arg.as_deref_mut() {
                    self.walk_node(arg);
                }
            }
            node::Node::List(l) => {
                for item in l.items.iter_mut() {
                    self.walk_node(item);
                }
            }
            node::Node::ArrayExpr(a) => {
                for elem in a.elements.iter_mut() {
                    self.walk_node(elem);
                }
            }
            node::Node::RowExpr(r) => {
                for arg in r.args.iter_mut() {
                    self.walk_node(arg);
                }
            }
            node::Node::ResTarget(rt) => {
                if let Some(val) = rt.val.as_deref_mut() {
                    self.walk_node(val);
                }
            }
            node::Node::SortBy(sb) => {
                if let Some(n) = sb.node.as_deref_mut() {
                    self.walk_node(n);
                }
            }
            _ => {}
        }
    }

    fn walk_select(&mut self, sel: &mut protobuf::SelectStmt) {
        if let Some(larg) = sel.larg.as_deref_mut() {
            self.walk_select(larg);
        }
        if let Some(rarg) = sel.rarg.as_deref_mut() {
            self.walk_select(rarg);
        }
        if let Some(with) = sel.with_clause.as_mut() {
            for cte_node in with.ctes.iter_mut() {
                if let Some(node::Node::CommonTableExpr(cte)) = cte_node.node.as_mut()
                    && let Some(query) = cte.ctequery.as_deref_mut()
                {
                    self.walk_node(query);
                }
            }
        }
        for from_item in sel.from_clause.iter_mut() {
            self.process_from_item(from_item);
        }
        for t in sel.target_list.iter_mut() {
            self.walk_node(t);
        }
        if let Some(w) = sel.where_clause.as_deref_mut() {
            self.walk_node(w);
        }
        for g in sel.group_clause.iter_mut() {
            self.walk_node(g);
        }
        if let Some(h) = sel.having_clause.as_deref_mut() {
            self.walk_node(h);
        }
        for s in sel.sort_clause.iter_mut() {
            self.walk_node(s);
        }
        for d in sel.distinct_clause.iter_mut() {
            self.walk_node(d);
        }
    }

    fn process_from_item(&mut self, n: &mut protobuf::Node) {
        let Some(inner) = n.node.as_mut() else {
            return;
        };
        match inner {
            node::Node::RangeVar(rv) => {
                let b = self.pop();
                if let ViewBinding::Relation(relid) = b {
                    self.apply_range_var(rv, relid);
                }
            }
            node::Node::JoinExpr(join) => {
                if let Some(larg) = join.larg.as_deref_mut() {
                    self.process_from_item(larg);
                }
                if let Some(rarg) = join.rarg.as_deref_mut() {
                    self.process_from_item(rarg);
                }
                if let Some(q) = join.quals.as_deref_mut() {
                    self.walk_node(q);
                }
            }
            node::Node::RangeSubselect(sub) => {
                if let Some(query) = sub.subquery.as_deref_mut() {
                    self.walk_node(query);
                }
            }
            node::Node::RangeFunction(rf) => {
                for arg in rf.functions.iter_mut() {
                    self.walk_node(arg);
                }
            }
            _ => {}
        }
    }

    fn apply_range_var(&self, rv: &mut protobuf::RangeVar, relid: PgClassOid) {
        let Some(class) = self.snapshot.pg_class.get(&relid) else {
            return;
        };
        let Some(nsname) = self.snapshot.namespace_name(class.relnamespace) else {
            return;
        };
        // Preserve schema-less form when the original AST had it: only emit
        // the schema portion if the AST already had one.
        if !rv.schemaname.is_empty() {
            rv.schemaname = nsname.to_owned();
        }
        rv.relname = class.relname.clone();
    }

    fn apply_column_ref(&self, cr: &mut protobuf::ColumnRef, relid: PgClassOid, attnum: i16) {
        let Some(class) = self.snapshot.pg_class.get(&relid) else {
            return;
        };
        let Some(attr) = self
            .snapshot
            .attributes_of(relid)
            .iter()
            .find(|a| a.attnum == attnum)
        else {
            return;
        };
        let nsname = self.snapshot.namespace_name(class.relnamespace);

        let segments: Vec<Option<&mut String>> = cr
            .fields
            .iter_mut()
            .map(|f| match f.node.as_mut() {
                Some(node::Node::String(s)) => Some(&mut s.sval),
                _ => None,
            })
            .collect();
        match segments.len() {
            // [col] — bare column. Update terminal.
            1 => {
                if let Some(Some(s)) = segments.into_iter().next() {
                    *s = attr.attname.clone();
                }
            }
            // [alias_or_relname, col] — qualifier could be an alias OR the
            // bare table name. We can't tell without scope; leave the
            // qualifier alone (RENAME ALIAS isn't a thing; RENAME TABLE
            // updates the qualifier only when it spelled the relation
            // name, which we'd need more context for). Update only the
            // terminal.
            2 => {
                let mut iter = segments.into_iter();
                let _ = iter.next();
                if let Some(Some(s)) = iter.next() {
                    *s = attr.attname.clone();
                }
            }
            // [schema, relname, col] — fully qualified. Update all three.
            3 => {
                let mut iter = segments.into_iter();
                if let (Some(Some(s_schema)), Some(Some(s_rel)), Some(Some(s_col))) =
                    (iter.next(), iter.next(), iter.next())
                {
                    if let Some(ns) = nsname {
                        *s_schema = ns.to_owned();
                    }
                    *s_rel = class.relname.clone();
                    *s_col = attr.attname.clone();
                }
            }
            _ => {}
        }
    }

    fn apply_func_name(&self, funcname: &mut [protobuf::Node], proc_oid: PgProcOid) {
        let Some(proc) = self.snapshot.pg_proc.get(&proc_oid) else {
            return;
        };
        // Only update the schema portion (proname stays — RENAME FUNCTION
        // renames an exact overload, not all overloads, so we'd need full
        // type-aware resolution to bind the right one). Schema move via
        // SET SCHEMA does affect every overload, so it's safe to update.
        if funcname.len() == 2
            && let Some(node::Node::String(s)) = funcname[0].node.as_mut()
            && let Some(ns) = self.snapshot.namespace_name(proc.pronamespace)
        {
            s.sval = ns.to_owned();
        }
    }

    fn apply_type_name(&self, tn: &mut protobuf::TypeName, type_oid: PgTypeOid) {
        let Some(t) = self.snapshot.pg_type.get(&type_oid) else {
            return;
        };
        // Same restraint as apply_func_name: only fix the schema portion.
        if tn.names.len() == 2
            && let Some(node::Node::String(s)) = tn.names[0].node.as_mut()
            && let Some(ns) = self.snapshot.namespace_name(t.typnamespace)
        {
            s.sval = ns.to_owned();
        }
    }
}

// ─── Dependency checking ────────────────────────────────────────────────────

/// Find all view OIDs that depend on the given table or view OID.
pub fn find_dependent_views(snapshot: &PgCatalog, relid: PgClassOid) -> Vec<PgClassOid> {
    let mut out: Vec<PgClassOid> = snapshot
        .iter_pg_depend()
        .filter(|d| {
            matches!(d.deptype, DepType::Normal)
                && d.classid == PG_CLASS_RELID
                && d.refclassid == PG_CLASS_RELID
                && d.refobjid.get() == relid.get()
        })
        .filter_map(|d| {
            let obj = PgClassOid::new(d.objid.get())?;
            let class = snapshot.pg_class.get(&obj)?;
            matches!(class.relkind, RelKind::View | RelKind::MaterializedView).then_some(obj)
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Find all view OIDs that depend on a specific column of a relation.
pub fn find_views_depending_on_column(
    snapshot: &PgCatalog,
    relid: PgClassOid,
    column_name: &str,
) -> Vec<PgClassOid> {
    let Some(attr) = snapshot.attribute_by_name(relid, column_name) else {
        return Vec::new();
    };
    let attnum = attr.attnum;
    let mut out: Vec<PgClassOid> = snapshot
        .iter_pg_depend()
        .filter(|d| {
            matches!(d.deptype, DepType::Normal)
                && d.classid == PG_CLASS_RELID
                && d.refclassid == PG_CLASS_RELID
                && d.refobjid.get() == relid.get()
                && d.refobjsubid == attnum
        })
        .filter_map(|d| {
            let obj = PgClassOid::new(d.objid.get())?;
            let class = snapshot.pg_class.get(&obj)?;
            matches!(class.relkind, RelKind::View | RelKind::MaterializedView).then_some(obj)
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Drop views by OID, transitively dropping any views that depend on them.
pub fn drop_views(snapshot: &mut PgCatalog, view_oids: &[PgClassOid]) {
    let mut to_drop: Vec<PgClassOid> = view_oids.to_vec();
    let mut dropped: Vec<PgClassOid> = Vec::new();

    while let Some(oid) = to_drop.pop() {
        if dropped.contains(&oid) {
            continue;
        }
        let dependents = find_dependent_views(snapshot, oid);
        for dep in dependents {
            if !dropped.contains(&dep) {
                to_drop.push(dep);
            }
        }
        super::drop::drop_relation_by_oid(snapshot, oid);
        dropped.push(oid);
    }
}

// (The legacy `AstEdit` machinery — `rewrite_ast_node` / `rewrite_select` /
// `rewrite_from_item` / `rewrite_any` / `rewrite_column_ref` / helpers /
// `apply_edit_to_all_views` — was deleted when the binding side-table
// replaced it. RENAME / SET SCHEMA no longer touch the stored AST;
// deparse-time `apply_view_bindings` rewrites name slots from the OIDs in
// `pg_class.viewbindings`.)

// ─── AST rewriting entry points (called from ALTER handlers) ────────────────
//
// With the OID-resolved binding side-table, these are now no-ops: a rename
// or schema move doesn't change any view's stored AST, because the bindings
// keep pointing at the same OIDs. The applier substitutes current names at
// deparse time. The functions are preserved as named callsites in case we
// reintroduce a side-effect later (e.g. invalidating a cached deparse).

pub fn rewrite_views_on_table_rename(
    _snapshot: &mut PgCatalog,
    _old_schema: &str,
    _old_name: &str,
    _new_schema: &str,
    _new_name: &str,
) {
}

pub fn rewrite_views_on_column_rename(
    _snapshot: &mut PgCatalog,
    _relid: PgClassOid,
    _old_col: &str,
    _new_col: &str,
) {
}

pub fn rewrite_views_on_schema_rename(
    _snapshot: &mut PgCatalog,
    _old_schema: &str,
    _new_schema: &str,
) {
}
