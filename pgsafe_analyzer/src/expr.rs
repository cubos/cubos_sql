//! Expression type inference.
//!
//! Walks pg_query AST expression nodes and infers their type (OID) and
//! nullability based on the schema snapshot and current scope.
//!
//! Every expression evaluation receives a [`TypeGoal`] describing the type
//! expected by the enclosing context (e.g. `BOOL` for `WHERE`, `INT8` for
//! `LIMIT`).  When the result is a `ParamRef` whose type is still `UNKNOWN`,
//! the goal type is recorded as a constraint — this is the single mechanism
//! that replaces all ad-hoc parameter recording.  After inference, a
//! compatibility check verifies that the result type can be coerced to the
//! goal under the allowed coercion context.

use pg_query::protobuf::{self, a_const, node};

use crate::coerce::{self, CoercionContext, can_coerce};
use crate::error::AnalyzeError;
use crate::functions;
use crate::functions::OutArg;
use crate::nullability::NullabilityContext;
use crate::oid::PgTypeOid;
use crate::param_collector::ParamCollector;
use crate::pg_catalog::{PgCatalog, TypCategory, TypType, oid};
use crate::scope::Scope;

// ──────────────────────────────────────────────────────────────────────────────
// TypeGoal
// ──────────────────────────────────────────────────────────────────────────────

/// The type expected by the enclosing context.
///
/// Mirrors PostgreSQL's approach where each clause (`WHERE`, `LIMIT`, `INSERT
/// VALUES`, …) tells the parser "I expect this expression to produce type X
/// with coercion level Y".
#[derive(Debug, Clone)]
pub(crate) struct TypeGoal {
    pub type_oid: PgTypeOid,
    pub coercion: CoercionContext,
    /// Optional byte range (post-lex SQL) covering the *source* of this
    /// expectation — the column being assigned, the other side of a
    /// comparison, etc. When present, surfaces as a secondary label in
    /// `TypeMismatch` diagnostics ("expected here").
    pub source_span: Option<crate::error::SourceSpan>,
    /// When the expectation comes from a named column (INSERT VALUES,
    /// UPDATE SET), the column's name. Used to produce PG's exact wording:
    /// `column "X" is of type Y but expression is of type Z`.
    pub source_col_name: Option<String>,
}

impl TypeGoal {
    /// No type expectation (e.g. SELECT target list).
    pub const NONE: Self = Self {
        type_oid: oid::UNKNOWN,
        coercion: CoercionContext::Implicit,
        source_span: None,
        source_col_name: None,
    };

    /// Expression context — only implicit casts allowed
    /// (operator/function argument matching).
    pub fn implicit(type_oid: PgTypeOid) -> Self {
        Self {
            type_oid,
            coercion: CoercionContext::Implicit,
            source_span: None,
            source_col_name: None,
        }
    }

    /// Assignment context — implicit + assignment casts allowed
    /// (WHERE, LIMIT, INSERT, UPDATE — matches PG's `COERCION_ASSIGNMENT`).
    pub fn assignment(type_oid: PgTypeOid) -> Self {
        Self {
            type_oid,
            coercion: CoercionContext::Assignment,
            source_span: None,
            source_col_name: None,
        }
    }

    /// Attach a `source_span` (the range that establishes this expectation,
    /// e.g. the column reference being assigned to). Used to render a
    /// secondary label in type-mismatch diagnostics.
    pub fn with_source(mut self, span: crate::error::SourceSpan) -> Self {
        self.source_span = Some(span);
        self
    }

    /// Attach the name of the column whose type drove this goal. Used by
    /// `check_goal_compatibility` to render PG's exact wording for
    /// INSERT/UPDATE assignments.
    pub fn with_source_column(mut self, name: impl Into<String>) -> Self {
        self.source_col_name = Some(name.into());
        self
    }

    pub fn has_expectation(&self) -> bool {
        self.type_oid != oid::UNKNOWN
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// ExprType
// ──────────────────────────────────────────────────────────────────────────────

/// Result of inferring an expression's type.
///
/// `record_fields` is populated for anonymous records whose attribute list is
/// statically determinable (e.g. `ROW(1, 'x')` produces `Some([(f1, int4),
/// (f2, text)])`). Mirrors PostgreSQL's typmod + RecordCacheArray: when the
/// shape can't be determined (opaque `RETURNS RECORD`, UNION of mismatched
/// shapes, casts to `record`/`text`), the field is `None` and the value
/// behaves like a typmod=-1 dynamic record.
#[derive(Debug, Clone)]
pub(crate) struct ExprType {
    pub type_oid: PgTypeOid,
    pub nullable: bool,
    /// `pg_attribute.atttypmod`-shaped type modifier carried through the
    /// inference. `None` (PG's `-1`) is the default; only column refs, direct
    /// casts, and uniform CASE/UNION branches actually propagate a value.
    /// Functions, operators, and aggregates strip it (PG matching).
    pub typmod: Option<i32>,
    /// `pg_collation.oid` carried through the inference. `None` for non-
    /// collatable types and untagged sites; `Some` only when a real
    /// collation is pinned (column attcollation, explicit `COLLATE "x"`,
    /// or propagation through a binary op / case branch). Operator and
    /// function outputs typically clear this — PG's full collation
    /// derivation rules ("explicit > implicit > none") are approximated
    /// here as "left-or-right wins" in binary contexts.
    pub collation: Option<crate::oid::PgCollationOid>,
    pub record_fields: Option<Vec<RecordField>>,
}

/// One element of an anonymous record's static shape, as it flows through
/// inference. Recursive via `ty: ExprType` — nested rows like
/// `ROW(1, ROW(2, 3))` survive without a special `nested_fields` channel.
///
/// SRF / OUT-arg outputs live as [`OutArg`]. The `from_*` constructor below
/// bridges that form into the expression-side shape used during inference.
/// Composite-type fields are read directly from `pg_attribute` via
/// [`PgCatalog::composite_fields_of`] in the call sites that need them.
#[derive(Debug, Clone)]
pub(crate) struct RecordField {
    pub name: String,
    pub ty: ExprType,
}

impl RecordField {
    /// Convert an SRF / OUT-arg field into the expression form.
    pub fn from_out_arg(a: &OutArg) -> Self {
        Self {
            name: a.name.clone(),
            ty: ExprType::scalar(a.type_oid, !a.not_null),
        }
    }

    pub fn from_out_args(args: &[OutArg]) -> Vec<Self> {
        args.iter().map(Self::from_out_arg).collect()
    }
}

impl ExprType {
    /// Construct a scalar (non-record) ExprType. The vast majority of call
    /// sites use this; only ROW constructors and shape-propagating helpers
    /// build with `record_fields: Some(...)`.
    pub fn scalar(type_oid: PgTypeOid, nullable: bool) -> Self {
        Self {
            type_oid,
            nullable,
            typmod: None,
            collation: None,
            record_fields: None,
        }
    }

    /// Construct a scalar with a known `pg_attribute.atttypmod` value. Used
    /// by `infer_column_ref` and `infer_type_cast` to thread the modifier
    /// through the inference chain.
    pub fn scalar_with_typmod(type_oid: PgTypeOid, nullable: bool, typmod: Option<i32>) -> Self {
        Self {
            type_oid,
            nullable,
            typmod,
            collation: None,
            record_fields: None,
        }
    }

    /// Construct a scalar with a known typmod *and* collation. Used by
    /// `infer_column_ref` (column attcollation) and the `CollateClause`
    /// arm of `infer_expr` (explicit decoration overrides the inferred
    /// collation regardless of source).
    pub fn scalar_with_collation(
        type_oid: PgTypeOid,
        nullable: bool,
        typmod: Option<i32>,
        collation: Option<crate::oid::PgCollationOid>,
    ) -> Self {
        Self {
            type_oid,
            nullable,
            typmod,
            collation,
            record_fields: None,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Context-rule validation
// ──────────────────────────────────────────────────────────────────────────────

/// Kind of function call an expression tree contains. Used to enforce PG's
/// placement rules (`no aggregate in WHERE`, `no window function in WHERE`,
/// `no nested aggregates`).
#[derive(Default, Debug, Clone, Copy)]
pub(crate) struct FuncKindPresence {
    pub has_aggregate: bool,
    pub has_window: bool,
}

/// Walk an expression AST and report whether it contains aggregate calls
/// (`COUNT(*)`, `SUM(x)`, …) or window function calls (`RANK() OVER …`),
/// without resolving anything against the schema. Used up-front by clauses
/// that forbid those constructs (WHERE, GROUP BY, JOIN ON, HAVING for the
/// nested-agg case).
pub(crate) fn detect_func_kinds(node: &protobuf::Node, snapshot: &PgCatalog) -> FuncKindPresence {
    let mut out = FuncKindPresence::default();
    walk(node, snapshot, &mut out);
    out
}

fn walk(node: &protobuf::Node, snapshot: &PgCatalog, out: &mut FuncKindPresence) {
    let Some(inner) = node.node.as_ref() else {
        return;
    };
    match inner {
        node::Node::FuncCall(fc) => {
            if fc.over.is_some() {
                out.has_window = true;
            } else {
                // Aggregate check via pg_proc.is_aggregate — resolved by name
                // against the snapshot.
                let parts = extract_string_fields(&fc.funcname);
                let (schema, name) = match parts.as_slice() {
                    [n] => (None, n.as_str()),
                    [s, n] => (Some(s.as_str()), n.as_str()),
                    _ => (None, ""),
                };
                if !name.is_empty() {
                    let candidates = snapshot.find_functions(schema, name);
                    if candidates
                        .iter()
                        .any(|f| matches!(f.prokind, crate::pg_catalog::ProKind::Aggregate))
                    {
                        out.has_aggregate = true;
                    }
                }
            }
            for arg in &fc.args {
                walk(arg, snapshot, out);
            }
            if let Some(f) = &fc.agg_filter {
                walk(f, snapshot, out);
            }
            for o in &fc.agg_order {
                walk(o, snapshot, out);
            }
        }
        node::Node::AExpr(e) => {
            if let Some(l) = &e.lexpr {
                walk(l, snapshot, out);
            }
            if let Some(r) = &e.rexpr {
                walk(r, snapshot, out);
            }
        }
        node::Node::BoolExpr(b) => {
            for a in &b.args {
                walk(a, snapshot, out);
            }
        }
        node::Node::NullTest(t) => {
            if let Some(a) = &t.arg {
                walk(a, snapshot, out);
            }
        }
        node::Node::BooleanTest(t) => {
            if let Some(a) = &t.arg {
                walk(a, snapshot, out);
            }
        }
        node::Node::CoalesceExpr(c) => {
            for a in &c.args {
                walk(a, snapshot, out);
            }
        }
        node::Node::CaseExpr(c) => {
            for w in &c.args {
                walk(w, snapshot, out);
            }
            if let Some(d) = &c.defresult {
                walk(d, snapshot, out);
            }
        }
        node::Node::CaseWhen(w) => {
            if let Some(e) = &w.expr {
                walk(e, snapshot, out);
            }
            if let Some(r) = &w.result {
                walk(r, snapshot, out);
            }
        }
        node::Node::TypeCast(c) => {
            if let Some(a) = &c.arg {
                walk(a, snapshot, out);
            }
        }
        node::Node::List(l) => {
            for i in &l.items {
                walk(i, snapshot, out);
            }
        }
        node::Node::SubLink(_) => {
            // Do NOT descend into subqueries — a SubLink is its own scope
            // and aggregates/windows inside it belong to that scope, not
            // the one we're validating.
        }
        _ => {}
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Main entry point
// ──────────────────────────────────────────────────────────────────────────────

/// Infer the type and nullability of an AST expression node.
///
/// `goal` describes the type expected by the enclosing context.  When the
/// expression is a `ParamRef` whose type is still unknown, the goal type is
/// recorded as a constraint.  After inference, the result is checked for
/// compatibility with the goal (raising `TypeMismatch` on failure).
pub(crate) fn infer_expr(
    node: &protobuf::Node,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    goal: TypeGoal,
) -> Result<ExprType, AnalyzeError> {
    let inner = node
        .node
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("empty node".into()))?;

    let result = match inner {
        node::Node::ColumnRef(col_ref) => infer_column_ref(col_ref, scope, null_ctx, snapshot),
        node::Node::AConst(a_const) => infer_a_const(a_const),
        node::Node::TypeCast(cast) => infer_type_cast(cast, scope, null_ctx, snapshot, params),
        node::Node::FuncCall(func) => infer_func_call(func, scope, null_ctx, snapshot, params),
        node::Node::GroupingFunc(g) => {
            // `GROUPING(expr, …)` — returns int4 indicating which of the
            // listed expressions are *missing* from the current grouping
            // set. Always defined → NOT NULL. Walk the args so params get
            // typed and column refs / typos surface as errors.
            for arg in &g.args {
                infer_expr(arg, scope, null_ctx, snapshot, params, TypeGoal::NONE)?;
            }
            Ok(ExprType::scalar(oid::INT4, false))
        }
        node::Node::AExpr(expr) => infer_a_expr(expr, scope, null_ctx, snapshot, params),
        node::Node::BoolExpr(expr) => infer_bool_expr(expr, scope, null_ctx, snapshot, params),
        node::Node::NullTest(t) => {
            if let Some(arg) = &t.arg {
                infer_expr(arg, scope, null_ctx, snapshot, params, TypeGoal::NONE)?;
            }
            Ok(ExprType::scalar(oid::BOOL, false))
        }
        node::Node::BooleanTest(t) => {
            if let Some(arg) = &t.arg {
                infer_expr(arg, scope, null_ctx, snapshot, params, TypeGoal::NONE)?;
            }
            Ok(ExprType::scalar(oid::BOOL, false))
        }
        node::Node::CoalesceExpr(expr) => infer_coalesce(expr, scope, null_ctx, snapshot, params),
        node::Node::CaseExpr(expr) => infer_case(expr, scope, null_ctx, snapshot, params),
        node::Node::SubLink(sub) => infer_sublink(sub, scope, null_ctx, snapshot, params),
        node::Node::ParamRef(p) => {
            params.see(p.number);
            // If the param is still untyped and the context provides a goal,
            // record the goal type — this is our equivalent of PG's
            // p_coerce_param_hook.
            if params.get(p.number) == oid::UNKNOWN && goal.has_expectation() {
                params.record(p.number, goal.type_oid);
            }
            let type_oid = params.get(p.number);
            Ok(ExprType::scalar(type_oid, params.is_nullable(p.number)))
        }
        node::Node::MinMaxExpr(mm) => {
            // `GREATEST`/`LEAST` are non-strict: they skip NULL args and
            // return NULL only when every arg is NULL. pg_query's AST
            // doesn't fill in `minmaxtype` without full parse analysis —
            // we resolve the common type from the args and track per-arg
            // nullability.
            let mut arg_oids = Vec::with_capacity(mm.args.len());
            let mut all_nullable = true;
            let mut any_arg = false;
            for arg in &mm.args {
                let t = infer_expr(arg, scope, null_ctx, snapshot, params, TypeGoal::NONE)?;
                arg_oids.push(t.type_oid);
                if !t.nullable {
                    all_nullable = false;
                }
                any_arg = true;
            }
            let resolved_type = match PgTypeOid::new(mm.minmaxtype) {
                Some(t) if t != oid::UNKNOWN => t,
                _ => crate::coerce::find_common_type(&arg_oids, snapshot).unwrap_or(oid::UNKNOWN),
            };
            // GREATEST/LEAST over ≥1 NOT NULL arg are never NULL.
            Ok(ExprType::scalar(resolved_type, !any_arg || all_nullable))
        }
        node::Node::AIndirection(ind) => infer_indirection(ind, scope, null_ctx, snapshot, params),
        node::Node::AArrayExpr(arr) => infer_array_expr(arr, scope, null_ctx, snapshot, params),
        node::Node::RowExpr(row) => {
            // `ROW(a, b, …)` constructs an anonymous composite. The ROW
            // value itself is never NULL — empty `ROW()` still yields a
            // record.
            //
            // When the enclosing context expects a registered composite
            // type of matching arity (UPDATE composite_col = ROW(...) or
            // INSERT INTO t (composite_col) VALUES (ROW(...))), we type
            // each element against the composite's declared field type so
            // params get pinned correctly and the result adopts the goal's
            // OID — exactly what PG does in `coerce_record_to_complex`.
            // Otherwise the ROW types as the pseudo `record` with shape
            // captured statically so downstream operators/indirection can
            // see through.
            let composite_goal = if goal.has_expectation() {
                let target = snapshot.unwrap_domain(goal.type_oid);
                snapshot.get_type(target).and_then(|te| {
                    if te.typtype == TypType::Composite
                        && let Some(relid) = te.typrelid
                    {
                        let fields = snapshot.attributes_of(relid).to_vec();
                        if fields.len() == row.args.len() {
                            Some((target, fields))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                })
            } else {
                None
            };

            if let Some((composite_oid, composite_fields)) = composite_goal {
                let mut any_nullable = false;
                for (arg, field) in row.args.iter().zip(composite_fields.iter()) {
                    let t = infer_expr(
                        arg,
                        scope,
                        null_ctx,
                        snapshot,
                        params,
                        TypeGoal::assignment(field.atttypid),
                    )?;
                    any_nullable = any_nullable || t.nullable;
                }
                // ROW value is never NULL; element NULLs are tracked
                // inside the composite, not at the outer site.
                let _ = any_nullable;
                return Ok(ExprType::scalar(composite_oid, false));
            }

            // PG names anonymous ROW elements `f1`, `f2`, ... by position.
            // The element's full ExprType (with any nested record shape)
            // goes straight onto the field — recursion handled by ExprType.
            // For each bare `$N` element, mark the param as
            // indeterminate-required: PG refuses to default these to text
            // (`SELECT ROW($1)` raises `could not determine data type of
            // parameter $1`). The marker is harmless if a later inference
            // site (ROW=ROW back-fill, composite-cast pre-pass, …) pins
            // the param to a concrete type.
            let mut fields = Vec::with_capacity(row.args.len());
            for (i, arg) in row.args.iter().enumerate() {
                let ty = infer_expr(arg, scope, null_ctx, snapshot, params, TypeGoal::NONE)?;
                if let Some(node::Node::ParamRef(p)) = arg.node.as_ref() {
                    params.mark_indeterminate_required(p.number);
                }
                fields.push(RecordField {
                    name: format!("f{}", i + 1),
                    ty,
                });
            }
            Ok(ExprType {
                type_oid: oid::RECORD,
                nullable: false,
                typmod: None,
                collation: None,
                record_fields: Some(fields),
            })
        }
        node::Node::SetToDefault(_) => {
            // `DEFAULT` placeholder in INSERT VALUES / UPDATE SET. The actual
            // default expression lives on the column definition and is
            // trusted to produce a valid value of the column's type, so we
            // adopt the assignment goal here. Nullability defers to the
            // goal's NOT NULL reasoning in the caller.
            Ok(ExprType::scalar(
                if goal.has_expectation() {
                    goal.type_oid
                } else {
                    oid::UNKNOWN
                },
                false,
            ))
        }
        node::Node::CollateClause(c) => {
            // `expr COLLATE "x"` is metadata-only — it changes how the
            // surrounding operator compares strings, not the result type or
            // nullability. Forward the goal so a `$param COLLATE "x"`
            // placeholder still picks up its expected type.
            let arg = c
                .arg
                .as_ref()
                .ok_or_else(|| AnalyzeError::Internal("CollateClause without arg".into()))?;
            // PG rejects unknown collation names up front; mirror that.
            // `collname` is a list of identifier nodes (`["pg_catalog", "C"]`
            // when fully qualified, just `["C"]` otherwise).
            let parts: Vec<&str> = c
                .collname
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
                    return Err(AnalyzeError::Invalid("malformed COLLATE clause".into()));
                }
            };
            let resolved_collation = if parts.is_empty() {
                None
            } else {
                let r = snapshot.resolve_collation(schema, name).ok_or_else(|| {
                    AnalyzeError::Invalid(format!("collation \"{name}\" does not exist"))
                })?;
                Some(r.oid)
            };
            let result = infer_expr(arg, scope, null_ctx, snapshot, params, goal)?;
            // PG rejects `COLLATE` on non-string-category types with
            // `collations are not supported by type X`. Accept UNKNOWN
            // (untyped literal/param) — the parser already coerces it
            // through the surrounding goal.
            if result.type_oid != oid::UNKNOWN {
                let base = snapshot.unwrap_domain(result.type_oid);
                let category = snapshot
                    .get_type(base)
                    .map(|t| t.typcategory)
                    .unwrap_or(TypCategory::UserDefined);
                if category != TypCategory::String {
                    // PG renders the bare type name here (search-path
                    // aware) — keep the SQL-standard aliases for built-ins
                    // (`int4` → `integer`) but drop the schema prefix for
                    // user types so a `public.address` column reads the
                    // same way PG would: `... by type address`.
                    let formatted = crate::ddl::util::format_type_for_message(snapshot, base);
                    let type_name = match formatted.rsplit_once('.') {
                        Some((_, bare)) => bare.to_owned(),
                        None => formatted,
                    };
                    return Err(AnalyzeError::Invalid(format!(
                        "collations are not supported by type {type_name}"
                    )));
                }
            }
            // Explicit COLLATE overrides whatever collation was inherited
            // from the inner expression (PG's "explicit" derivation tier).
            return Ok(ExprType::scalar_with_collation(
                result.type_oid,
                result.nullable,
                result.typmod,
                resolved_collation,
            ));
        }
        _ => Err(AnalyzeError::Unsupported(format!(
            "expression node type not supported: {:?}",
            std::mem::discriminant(inner)
        ))),
    }?;

    // Verify result is compatible with the goal type. Pass the location
    // of the offending expression so a `TypeMismatch` carries a snippet.
    check_goal_compatibility(&result, &goal, snapshot, crate::error::node_location(node))?;

    Ok(result)
}

// ──────────────────────────────────────────────────────────────────────────────
// Goal compatibility check
// ──────────────────────────────────────────────────────────────────────────────

/// Verify that `result` can be coerced to `goal` under the allowed coercion
/// context.  Returns `Ok(())` when:
/// - There is no goal expectation (`goal.type_oid == UNKNOWN`).
/// - The result is `UNKNOWN` (untyped literals / unresolved params coerce to
///   anything, per SQL spec).
/// - The types match (after domain unwrapping).
/// - A registered cast exists at the required coercion level.
fn check_goal_compatibility(
    result: &ExprType,
    goal: &TypeGoal,
    snapshot: &PgCatalog,
    location: Option<i32>,
) -> Result<(), AnalyzeError> {
    if !goal.has_expectation() {
        return Ok(());
    }
    if result.type_oid == oid::UNKNOWN {
        return Ok(());
    }
    if result.type_oid == goal.type_oid {
        return Ok(());
    }
    if can_coerce(result.type_oid, goal.type_oid, goal.coercion, snapshot) {
        return Ok(());
    }
    // PG uses a distinct wording when the source is the pseudo `record`
    // type and the target is a registered composite (e.g. assigning
    // `ROW($p1, $p2)` to an `address` column with the wrong arity). Mirror
    // it so pg_sanity's prefix check passes — the rest of the cases keep
    // the generic `cannot coerce` form.
    if result.type_oid == oid::RECORD
        && let Some(target_te) = snapshot.get_type(goal.type_oid)
        && target_te.typtype == TypType::Composite
    {
        // PG renders the bare composite name (search-path aware) here, not
        // the schema-qualified form `format_type_for_message` would produce.
        return Err(AnalyzeError::Invalid(format!(
            "cannot cast type record to {}",
            target_te.typname
        )));
    }
    // PG's user-facing type names (e.g. `int4` → `integer`, `bool` →
    // `boolean`) — these appear verbatim in the message so the sanity
    // prefix match works.
    let actual_pg = crate::ddl::util::format_type_for_message(snapshot, result.type_oid);
    let expected_pg = crate::ddl::util::format_type_for_message(snapshot, goal.type_oid);

    // Internal short forms kept around so the introspection-style
    // `actual`/`expected` fields on `TypeMismatch` still carry the OID's
    // type name (what tests/macros use).
    let actual = type_display_name(result.type_oid, snapshot);
    let expected = type_display_name(goal.type_oid, snapshot);

    // PG-verbatim message. When the expectation comes from a named column
    // (INSERT VALUES, UPDATE SET), use PG's exact wording so pg_sanity
    // passes; otherwise fall back to the generic `cannot coerce` form.
    let context = match &goal.source_col_name {
        Some(col) => format!(
            "column \"{col}\" is of type {expected_pg} but expression is of type {actual_pg}"
        ),
        None => format!("cannot coerce {actual_pg} to {expected_pg}"),
    };

    let primary_span = location.and_then(|loc| {
        // `from_node_token` covers identifiers, numeric literals, and
        // quoted strings — TypeMismatch can fire on any of those.
        crate::error::SourceSpan::from_node_token(loc)
            .or_else(|| crate::error::SourceSpan::from_location(loc))
    });

    // When the goal carries a `source_span` (the column being assigned, the
    // operand setting the expectation, …), surface it as a secondary label
    // so the diagnostic shows both sides.
    let secondary = goal
        .source_span
        .map(|s| crate::error::DiagnosticLabel::new(s, format!("expected {expected_pg} here")));

    Err(crate::error::RawError::type_mismatch(
        actual,
        expected,
        &actual_pg,
        &expected_pg,
        context,
        primary_span,
        secondary,
        None,
    )
    .finalize_implicit())
}

fn type_display_name(oid: PgTypeOid, snapshot: &PgCatalog) -> String {
    snapshot
        .get_type(oid)
        .map(|t| t.typname.clone())
        .unwrap_or_else(|| format!("oid:{}", oid.get()))
}

/// Return `text` when `node` is an untyped string literal (`'x'`) and its
/// inferred type is still UNKNOWN. Used by constructs that need to treat a
/// bare string constant as text for type-compatibility checks — NULLIF,
/// CASE / COALESCE / ARRAY[...] branch merging, UNION column reconciliation.
pub(crate) fn unknown_literal_as_text(
    node: Option<&protobuf::Node>,
    inferred_oid: PgTypeOid,
) -> PgTypeOid {
    if inferred_oid != oid::UNKNOWN {
        return inferred_oid;
    }
    let is_string_literal = node.is_some_and(|n| {
        matches!(
            n.node.as_ref(),
            Some(node::Node::AConst(ac))
                if !ac.isnull && matches!(ac.val, Some(a_const::Val::Sval(_)))
        )
    });
    if is_string_literal {
        oid::TEXT
    } else {
        oid::UNKNOWN
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Column references
// ──────────────────────────────────────────────────────────────────────────────

fn infer_column_ref(
    col_ref: &protobuf::ColumnRef,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &PgCatalog,
) -> Result<ExprType, AnalyzeError> {
    // Star expansion in expression context. `alias.*` in PG becomes the
    // composite type of the relation referenced by `alias`. `*` alone
    // (no qualifier) could expand to a ROW of every visible source but
    // the semantic is ambiguous enough that we leave it unsupported.
    let has_star = col_ref
        .fields
        .iter()
        .any(|f| matches!(f.node.as_ref(), Some(node::Node::AStar(_))));
    if has_star {
        return infer_star_ref(col_ref, scope, snapshot);
    }

    let parts = extract_string_fields(&col_ref.fields);

    let (table, column) = match parts.as_slice() {
        [col] => (None, col.as_str()),
        [tbl, col] => (Some(tbl.as_str()), col.as_str()),
        [_schema, tbl, col] => (Some(tbl.as_str()), col.as_str()),
        _ => {
            return Err(AnalyzeError::UndefinedColumn(format!(
                "invalid column ref: {:?}",
                parts
            )));
        }
    };

    match scope.resolve_column(
        table,
        column,
        crate::error::SourceSpan::from_node_qname(col_ref.location),
    ) {
        Ok(col) => {
            let nullable = null_ctx.is_nullable(&col.table_alias, &col.name, col.base_not_null);
            Ok(ExprType {
                type_oid: col.type_oid,
                nullable,
                typmod: col.typmod,
                // The column's `attcollation` (if any) flows out as-is. PG
                // never overrides it implicitly — only an explicit
                // `COLLATE "x"` decoration on the surrounding expression
                // does.
                collation: col.collation,
                // Carry the column's record shape forward so downstream
                // `(col).field` indirection and ROW-vs-shape coercion can
                // see through to the field types.
                record_fields: col.record_fields.clone(),
            })
        }
        Err(e) => {
            // PG row-reference fallback: a single unqualified identifier can
            // name a whole row from the FROM clause (`SELECT u FROM users u`
            // or `(u).name`). Only kick in when the column lookup failed AND
            // the identifier matches a table alias in scope — otherwise we'd
            // shadow legitimate UndefinedColumn errors.
            if table.is_none()
                && let Some(src) = scope.find_source(column)
                && let Some(qn) = src.source_qn.as_ref()
                && let Some(nsoid) = snapshot.namespace_oid(&qn.schema)
                && let Some(&composite_oid) = snapshot.type_by_qname.get(&(nsoid, qn.name.clone()))
            {
                return Ok(ExprType::scalar(composite_oid, false));
            }
            Err(e)
        }
    }
}

/// Resolve `alias.*` (or `schema.alias.*`) to the composite type of the
/// underlying relation. The composite is the per-table `TypeEntry` that
/// `create_table` registers alongside the table — same OID that a call site
/// like `row_to_json(alias.*)` would see at runtime.
fn infer_star_ref(
    col_ref: &protobuf::ColumnRef,
    scope: &Scope,
    snapshot: &PgCatalog,
) -> Result<ExprType, AnalyzeError> {
    // The alias/relname qualifying the star is the last String field before
    // AStar. For `t.*` it's index 0; for `schema.t.*` it's index 1.
    let alias = col_ref
        .fields
        .iter()
        .rev()
        .skip_while(|f| !matches!(f.node.as_ref(), Some(node::Node::AStar(_))))
        .nth(1)
        .and_then(|f| match f.node.as_ref()? {
            node::Node::String(s) => Some(s.sval.as_str()),
            _ => None,
        })
        .ok_or_else(|| {
            AnalyzeError::Unsupported("unqualified * has no relation — use alias.* instead".into())
        })?;

    let source = scope.find_source(alias).ok_or_else(|| {
        AnalyzeError::UndefinedTable(format!("missing FROM-clause entry for table \"{alias}\""))
    })?;

    // Real tables / views resolve to their backing composite type so calls
    // like `row_to_json(t.*)` see the registered row OID. CTE and subquery
    // sources have no `source_qn` — PG composes an anonymous row type at
    // planning time, so we surface `pg_catalog.record` with the source's
    // columns threaded as the record shape. The shape lets downstream
    // `(t.*).field` indirection still resolve, and the `record` OID lines
    // up with what PG's wire-protocol Describe reports for these queries.
    if let Some(qn) = source.source_qn.as_ref() {
        let composite_oid = snapshot
            .namespace_oid(&qn.schema)
            .and_then(|nsoid| {
                snapshot
                    .type_by_qname
                    .get(&(nsoid, qn.name.clone()))
                    .copied()
            })
            .ok_or_else(|| {
                AnalyzeError::UndefinedType(format!(
                    "internal: no composite type registered for relation {qn}"
                ))
            })?;
        return Ok(ExprType::scalar(composite_oid, false));
    }

    let fields: Vec<RecordField> = source
        .columns
        .iter()
        .map(|c| RecordField {
            name: c.name.clone(),
            ty: ExprType {
                type_oid: c.type_oid,
                nullable: !c.base_not_null,
                typmod: c.typmod,
                collation: c.collation,
                record_fields: c.record_fields.clone(),
            },
        })
        .collect();
    Ok(ExprType {
        type_oid: oid::RECORD,
        nullable: false,
        typmod: None,
        collation: None,
        record_fields: Some(fields),
    })
}

// ──────────────────────────────────────────────────────────────────────────────
// Indirection (`(expr).field`, `(expr)[i]`)
// ──────────────────────────────────────────────────────────────────────────────

/// Resolve `(expr).field1.field2…` chains. Each step either names a field in
/// a composite (String) or subscripts an array / `jsonb` (`AIndices`).
/// Array subscripting handles both element access (`arr[n]`) and slicing
/// (`arr[1:3]`, which keeps the array type). `jsonb` / `json` subscripting
/// (`data['key']`, `data[0]`, chained) yields `jsonb` at every step.
fn infer_indirection(
    ind: &protobuf::AIndirection,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let arg = ind
        .arg
        .as_deref()
        .ok_or_else(|| AnalyzeError::Unsupported("indirection without arg".into()))?;

    // Two shortcut paths for `record`-typed args whose fields aren't stored
    // in a composite `TypeEntry`:
    //
    // 1. `(func(...)).field` — direct FuncCall with `out_args` (TABLE/OUT).
    // 2. `(alias.col).field` — ColumnRef whose scope entry carries
    //    `record_fields` (populated when the subquery's target expr was a
    //    FuncCall with out_args).
    //
    // Consume leading String steps against those named fields; fall through
    // to the generic walker for any remaining steps (e.g. nested composite
    // unwrap, subscript on a scalar out_arg).
    let from_direct_funccall = if let Some(node::Node::FuncCall(fc)) = arg.node.as_ref() {
        resolve_funccall_out_args(fc, scope, null_ctx, snapshot, params)?
    } else {
        None
    };
    let from_column_record = if from_direct_funccall.is_none() {
        if let Some(node::Node::ColumnRef(cr)) = arg.node.as_ref() {
            column_ref_record_fields(cr, scope)
        } else {
            None
        }
    } else {
        None
    };

    let leading_fields = from_direct_funccall
        .as_ref()
        .or(from_column_record.as_ref());
    let (start_step, mut current) = if let Some(fields) = leading_fields {
        let mut idx = 0usize;
        let mut current = None;
        while idx < ind.indirection.len() {
            let Some(node::Node::String(s)) = ind.indirection[idx].node.as_ref() else {
                break;
            };
            let field = fields.iter().find(|f| f.name == s.sval).ok_or_else(|| {
                AnalyzeError::UndefinedColumn(format!(
                    "could not identify column \"{}\" in record data type",
                    s.sval
                ))
            })?;
            current = Some(field.ty.clone());
            idx += 1;
        }
        (idx, current)
    } else {
        (0, None)
    };

    let mut current = match current.take() {
        Some(c) => c,
        None => infer_expr(arg, scope, null_ctx, snapshot, params, TypeGoal::NONE)?,
    };

    // Detect the `(alias).field` shape: arg is a single-identifier ColumnRef
    // whose identifier is a relation alias in scope (not a column). PG emits
    // `column alias.field does not exist` for this case (whereas
    // `(c.col).field` produces `column "field" not found in data type T`).
    // The alias hint only applies to the first indirection step — chained
    // accesses past that point are no longer at the relation boundary.
    let arg_is_bare_alias: Option<&str> = if let Some(node::Node::ColumnRef(cr)) = arg.node.as_ref()
    {
        let parts = extract_string_fields(&cr.fields);
        match parts.as_slice() {
            [single] if scope.find_source(single).is_some() => {
                cr.fields.iter().find_map(|f| match f.node.as_ref()? {
                    node::Node::String(s) => Some(s.sval.as_str()),
                    _ => None,
                })
            }
            _ => None,
        }
    } else {
        None
    };

    for (idx, step) in ind.indirection.iter().enumerate().skip(start_step) {
        match step.node.as_ref() {
            Some(node::Node::String(s)) => {
                let alias_hint = if idx == start_step {
                    arg_is_bare_alias
                } else {
                    None
                };
                current = resolve_composite_field(&current, &s.sval, snapshot, alias_hint)?;
            }
            Some(node::Node::AIndices(ai)) => {
                // `jsonb` subscripting (PG 14+): `data['key']`, `data[0]`,
                // chained. Each non-slice step yields `jsonb` and is always
                // nullable — a missing key/index produces NULL. Only `jsonb`
                // has a subscript handler in PG; the plain `json` type does
                // not, so it falls through to the array path below, which
                // rejects non-array types with PG's wording.
                let jsonb_base_oid = snapshot.unwrap_domain(current.type_oid);
                let current_is_jsonb = snapshot
                    .get_type(jsonb_base_oid)
                    .is_some_and(|t| t.typname == "jsonb");
                if current_is_jsonb {
                    if ai.is_slice {
                        // PG's jsonb subscript handler rejects slices —
                        // verbatim message for the sanity prefix match.
                        return Err(AnalyzeError::Unsupported(
                            "jsonb subscript does not support slices".into(),
                        ));
                    }
                    // The subscript key is coerced by PG to `text` (an
                    // object key) or `int4` (an array index) — both are
                    // accepted, so infer the bounds without forcing a goal.
                    for bound in [&ai.lidx, &ai.uidx].into_iter().flatten() {
                        infer_expr(bound, scope, null_ctx, snapshot, params, TypeGoal::NONE)?;
                    }
                    let jsonb_oid = snapshot
                        .resolve_type_by_name(None, "jsonb")
                        .map(|j| j.oid)
                        .unwrap_or(jsonb_base_oid);
                    current = ExprType::scalar(jsonb_oid, true);
                    continue;
                }

                // Walk both bounds with an int4 goal so params and column
                // refs inside `arr[lo:hi]` / `arr[i]` get typed and
                // validated. Track nullability so slice results propagate
                // NULL from any NULL bound.
                let mut any_bound_nullable = false;
                for bound in [&ai.lidx, &ai.uidx].into_iter().flatten() {
                    let t = infer_expr(
                        bound,
                        scope,
                        null_ctx,
                        snapshot,
                        params,
                        TypeGoal::assignment(oid::INT4),
                    )?;
                    any_bound_nullable = any_bound_nullable || t.nullable;
                }

                if ai.is_slice {
                    let type_entry = snapshot.get_type(current.type_oid).ok_or_else(|| {
                        AnalyzeError::UndefinedType(format!(
                            "internal: array slice over unknown type OID {}",
                            current.type_oid.get()
                        ))
                    })?;
                    if type_entry.typcategory != TypCategory::Array {
                        return Err(AnalyzeError::Unsupported(format!(
                            "cannot subscript type {} because it does not support subscripting",
                            crate::ddl::util::format_type_for_message(snapshot, current.type_oid,)
                        )));
                    }
                    // `arr[lo:hi]` keeps the array type. Result is NULL iff
                    // the array is NULL or any bound is NULL — out-of-range
                    // bounds yield an empty (non-null) array.
                    current =
                        ExprType::scalar(current.type_oid, current.nullable || any_bound_nullable);
                } else {
                    // Adjacent non-slice subscripts (`arr[i][j]`) keep the
                    // array type for all but the last step. PG accepts an
                    // arbitrary number of subscripts on any array (multi-dim
                    // arrays collapse into the same type OID), so we mirror
                    // that by reducing to the element type only when no
                    // further `[…]` step follows in the same chain.
                    let next_is_subscript = ind.indirection.get(idx + 1).is_some_and(|s| {
                        matches!(
                            s.node.as_ref(),
                            Some(node::Node::AIndices(next)) if !next.is_slice
                        )
                    });
                    if next_is_subscript {
                        current = ExprType::scalar(current.type_oid, true);
                    } else {
                        // `arr[i]` is always nullable (out-of-bounds → NULL,
                        // even with non-null array and non-null index).
                        current = resolve_array_element(&current, snapshot)?;
                    }
                }
            }
            _ => {
                return Err(AnalyzeError::Unsupported(format!(
                    "unsupported indirection step: {:?}",
                    step.node.as_ref().map(std::mem::discriminant)
                )));
            }
        }
    }

    Ok(current)
}

/// Look up `record_fields` for a `ColumnRef` that resolves to a scope column
/// carrying named output columns (set when its producing expression was a
/// FuncCall with `out_args`). Returns `None` if the ref doesn't resolve or
/// the column isn't a record.
fn column_ref_record_fields(cr: &protobuf::ColumnRef, scope: &Scope) -> Option<Vec<RecordField>> {
    let parts = extract_string_fields(&cr.fields);
    let (table, column) = match parts.as_slice() {
        [col] => (None, col.as_str()),
        [tbl, col] => (Some(tbl.as_str()), col.as_str()),
        [_schema, tbl, col] => (Some(tbl.as_str()), col.as_str()),
        _ => return None,
    };
    let col = scope.resolve_column(table, column, None).ok()?;
    col.record_fields.clone()
}

/// If `fc` names a function with declared `out_args` (TABLE/OUT args),
/// return them so indirection steps can match against named output columns.
/// Returns `Ok(None)` when the function has no out_args — the caller should
/// fall back to generic composite/record handling.
fn resolve_funccall_out_args(
    fc: &protobuf::FuncCall,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> Result<Option<Vec<RecordField>>, AnalyzeError> {
    let parts = extract_string_fields(&fc.funcname);
    let (schema, name) = match parts.as_slice() {
        [n] => (None, n.as_str()),
        [s, n] => (Some(s.as_str()), n.as_str()),
        _ => return Ok(None),
    };

    // Infer arg types against the caller's scope so column refs in the
    // arguments resolve to concrete types — needed for polymorphic
    // substitution (`anyelement` → element-of-array etc.) when the function
    // has polymorphic out args like `_pg_expandarray(anyarray) RETURNS
    // (x anyelement, n int)`.
    let mut arg_types = Vec::with_capacity(fc.args.len());
    for arg in &fc.args {
        let t = infer_expr(arg, scope, null_ctx, snapshot, params, TypeGoal::NONE)
            .map(|e| e.type_oid)
            .unwrap_or(oid::UNKNOWN);
        arg_types.push(t);
    }

    let resolved =
        match crate::functions::resolve_function(snapshot, schema, name, &arg_types, false, None) {
            Ok(r) => r,
            Err(_) => return Ok(None),
        };
    if resolved.out_args.is_empty() {
        Ok(None)
    } else {
        Ok(Some(RecordField::from_out_args(&resolved.out_args)))
    }
}

/// Look up `field_name` inside a composite type's field list. The resulting
/// nullability is the combination of the enclosing value being nullable AND
/// the field's own `not_null` declaration — either one being nullable makes
/// the access nullable.
///
/// When the enclosing value carries an inline `record_fields` shape (e.g.
/// `(ROW(1, 'x'::text)).f2`), we use that directly — no snapshot lookup,
/// since pseudo `record` has no `TypeKind::Composite` to consult.
///
/// `relation_alias` is `Some(alias)` when the indirection's argument was a
/// bare relation reference (`(alias).field` form). PG emits a different
/// error wording in that case — `column alias.field does not exist` — so
/// the analyzer mirrors it to keep `pg_sanity` aligned. For chained or
/// composite-column accesses (`(c.col).field`, `((c).x).field`), pass
/// `None` and the wording switches to PG's `column "f" not found in data
/// type T`.
fn resolve_composite_field(
    current: &ExprType,
    field_name: &str,
    snapshot: &PgCatalog,
    relation_alias: Option<&str>,
) -> Result<ExprType, AnalyzeError> {
    if let Some(shape) = current.record_fields.as_deref() {
        let field = shape.iter().find(|f| f.name == field_name).ok_or_else(|| {
            AnalyzeError::UndefinedColumn(format!(
                "could not identify column \"{field_name}\" in record data type"
            ))
        })?;
        // Field's full ExprType (including any nested record shape) is
        // already on `field.ty`; just OR the enclosing nullability in.
        return Ok(ExprType {
            type_oid: field.ty.type_oid,
            nullable: current.nullable || field.ty.nullable,
            typmod: field.ty.typmod,
            collation: field.ty.collation,
            record_fields: field.ty.record_fields.clone(),
        });
    }

    // Domain-over-composite needs unwrapping to see the composite fields.
    let base_oid = snapshot.unwrap_domain(current.type_oid);
    let type_entry = snapshot.get_type(base_oid).ok_or_else(|| {
        AnalyzeError::UndefinedType(format!(
            "internal: composite field access .{field_name} over unknown type OID {}",
            base_oid.get()
        ))
    })?;

    let pg_type_name = crate::ddl::util::format_type_for_message(snapshot, base_oid);
    let Some(relid) = type_entry.typrelid else {
        return Err(AnalyzeError::Unsupported(format!(
            "column notation .{field_name} applied to type {pg_type_name}, \
             which is not a composite type"
        )));
    };
    if type_entry.typtype != TypType::Composite {
        return Err(AnalyzeError::Unsupported(format!(
            "column notation .{field_name} applied to type {pg_type_name}, \
             which is not a composite type"
        )));
    }
    let fields = snapshot.attributes_of(relid);
    let field = fields
        .iter()
        .find(|f| f.attname == field_name)
        .ok_or_else(|| {
            let msg = if let Some(alias) = relation_alias {
                format!(
                    "column {} does not exist",
                    crate::qualified_name::QualifiedName::new(alias, field_name),
                )
            } else {
                format!(
                    "column \"{field_name}\" not found in data type {}",
                    type_entry.typname
                )
            };
            AnalyzeError::UndefinedColumn(msg)
        })?;

    Ok(ExprType::scalar(
        field.atttypid,
        current.nullable || !field.attnotnull,
    ))
}

/// `ARRAY[expr1, expr2, …]` literal — result type is the common element type
/// promoted to its array. Empty arrays fall back to `UNKNOWN` so that the
/// enclosing cast (`ARRAY[]::text[]`) takes over.
fn infer_array_expr(
    arr: &protobuf::AArrayExpr,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    if arr.elements.is_empty() {
        return Ok(ExprType::scalar(oid::UNKNOWN, false));
    }
    let mut element_types = Vec::with_capacity(arr.elements.len());
    let mut any_nullable = false;
    for elem in &arr.elements {
        let t = infer_expr(elem, scope, null_ctx, snapshot, params, TypeGoal::NONE)?;
        element_types.push(t.type_oid);
        any_nullable |= t.nullable;
    }
    let common = match coerce::find_common_type(&element_types, snapshot) {
        Some(t) => t,
        None => {
            // PG: `ARRAY types <X> and <Y> cannot be matched`. Use the
            // first two distinct concrete types in the message so the
            // diagnostic is stable regardless of ordering tie-breaks.
            let mut concrete: Vec<PgTypeOid> = element_types
                .iter()
                .copied()
                .filter(|&t| t != oid::UNKNOWN)
                .collect();
            concrete.dedup();
            let names: Vec<String> = concrete
                .iter()
                .take(2)
                .map(|&t| crate::ddl::util::format_type_for_message(snapshot, t))
                .collect();
            // PG (SQLSTATE 42804) emits this exactly as `ARRAY types A and
            // B cannot be matched`. We keep the same wording so the
            // `pglite_sanity` mirror passes; demote to `Invalid` so the
            // generic `type mismatch: …` prefix from `TypeMismatch::Display`
            // doesn't leak in front of it.
            return Err(AnalyzeError::Invalid(format!(
                "ARRAY types {} and {} cannot be matched",
                names.first().map(String::as_str).unwrap_or("?"),
                names.get(1).map(String::as_str).unwrap_or("?"),
            )));
        }
    };
    // PG collapses array dimensions into the same type OID:
    // `ARRAY[ARRAY[1,2], ARRAY[3,4]]` is `int4[]`, not `int4[][]`. So if the
    // common element type is already an array, reuse it instead of trying to
    // wrap it (`array_type_of` on an array type returns `None`).
    let common_is_array = snapshot
        .get_type(common)
        .is_some_and(|t| t.typcategory == TypCategory::Array);
    let array_oid = if common_is_array {
        common
    } else {
        snapshot.array_type_of(common).unwrap_or(oid::UNKNOWN)
    };
    // An ARRAY[...] constructor is never NULL itself — it's always at least
    // an empty array. Element nullability is tracked separately by Rust's
    // `Option<T>` inside `Vec<T>`.
    let _ = any_nullable;
    Ok(ExprType::scalar(array_oid, false))
}

/// `arr[i]` — the result is an element of the array. Nullable because SQL
/// subscripts out of bounds return NULL rather than erroring.
fn resolve_array_element(
    current: &ExprType,
    snapshot: &PgCatalog,
) -> Result<ExprType, AnalyzeError> {
    let type_entry = snapshot.get_type(current.type_oid).ok_or_else(|| {
        AnalyzeError::UndefinedType(format!(
            "internal: array subscript over unknown type OID {}",
            current.type_oid.get()
        ))
    })?;
    // PG (SQLSTATE 42804) rejects subscripting a type with no subscript
    // handler with this verbatim wording; keep it exact for the sanity
    // prefix match. `jsonb` is handled before reaching here.
    let not_subscriptable = || {
        AnalyzeError::Unsupported(format!(
            "cannot subscript type {} because it does not support subscripting",
            crate::ddl::util::format_type_for_message(snapshot, current.type_oid)
        ))
    };
    let Some(elem) = type_entry.typelem else {
        return Err(not_subscriptable());
    };
    if type_entry.typcategory != TypCategory::Array {
        return Err(not_subscriptable());
    }
    Ok(ExprType::scalar(elem, true))
}

// ──────────────────────────────────────────────────────────────────────────────
// Literals
// ──────────────────────────────────────────────────────────────────────────────

fn infer_a_const(a_const: &protobuf::AConst) -> Result<ExprType, AnalyzeError> {
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

fn infer_type_cast(
    cast: &protobuf::TypeCast,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
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
    let inner_type = infer_expr(inner, scope, null_ctx, snapshot, params, inner_goal)?;

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

// ──────────────────────────────────────────────────────────────────────────────
// Function calls — two-pass (PG chapter 10.3)
// ──────────────────────────────────────────────────────────────────────────────

fn infer_func_call(
    func: &protobuf::FuncCall,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
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

    // `WITHIN GROUP (ORDER BY …)` marks an ordered-set aggregate. PG forbids
    // combining it with `OVER` or `DISTINCT`; reject those before going
    // further so the error message points at the actual conflict instead of
    // a misleading overload-resolution failure.
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

    // Pass 1: infer args bottom-up with no goal.
    let mut arg_types = Vec::new();
    let mut arg_nullable = Vec::with_capacity(func.args.len());
    let mut any_arg_nullable = false;
    for arg in &func.args {
        let t = infer_expr(arg, scope, null_ctx, snapshot, params, TypeGoal::NONE)?;
        any_arg_nullable = any_arg_nullable || t.nullable;
        arg_nullable.push(t.nullable);
        arg_types.push(t.type_oid);
    }

    // For ordered-set aggregates, append the types of the `WITHIN GROUP
    // (ORDER BY …)` expressions to the arg list so overload resolution sees
    // the full signature (PG records both direct args and ordered args in
    // `pg_proc.proargtypes`). Each item is a `SortBy` wrapping the actual
    // sort expression in `node`.
    let direct_arg_count = arg_types.len();
    if func.agg_within_group {
        for order_item in &func.agg_order {
            let sort_inner = match order_item.node.as_ref() {
                Some(node::Node::SortBy(sb)) => sb.node.as_deref(),
                _ => Some(order_item),
            };
            if let Some(inner) = sort_inner {
                let t = infer_expr(inner, scope, null_ctx, snapshot, params, TypeGoal::NONE)?;
                any_arg_nullable = any_arg_nullable || t.nullable;
                arg_nullable.push(t.nullable);
                arg_types.push(t.type_oid);
            }
        }
    }

    // Resolve function with inferred arg types (UNKNOWN treated as wildcard).
    let resolved = functions::resolve_function(
        snapshot,
        schema,
        name,
        &arg_types,
        func.agg_star,
        crate::error::SourceSpan::from_node_qname(func.location),
    )?;

    // PG forbids aggregates / window functions nested inside aggregate
    // arguments (`SUM(COUNT(*))`). We catch it here, after resolution,
    // using the AST of each arg.
    if resolved.is_aggregate {
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
    }

    // Pass 2: back-fill UNKNOWN args with expected types from the resolved
    // function signature (equivalent to PG's coerce_func_args). Only the
    // direct args correspond to `func.args`; ordered args (when
    // `agg_within_group` is set) come from `func.agg_order` and are walked
    // separately below.
    for (i, arg) in func.args.iter().enumerate() {
        if i >= direct_arg_count {
            break;
        }
        if arg_types[i] == oid::UNKNOWN
            && let Some(&expected) = resolved.arg_types.get(i)
            && expected != oid::UNKNOWN
        {
            let _ = infer_expr(
                arg,
                scope,
                null_ctx,
                snapshot,
                params,
                TypeGoal::implicit(expected),
            );
        }
    }

    // Walk aggregate modifiers so any `$N` placeholders they contain get
    // their types inferred and any column refs are validated. FILTER must
    // be bool (like a WHERE clause), per-aggregate ORDER BY items have no
    // specific goal, and the WINDOW `OVER (…)` clause is walked separately
    // below. None of these positions can reference a select-list alias —
    // they're all row-level — so propagating errors here matches PG.
    if let Some(filter) = &func.agg_filter {
        infer_expr(
            filter,
            scope,
            null_ctx,
            snapshot,
            params,
            TypeGoal::implicit(oid::BOOL),
        )?;
    }
    // Per-aggregate `ORDER BY` (e.g. `array_agg(x ORDER BY y)`). For
    // ordered-set aggregates (`WITHIN GROUP`) the sort expressions were
    // already inferred above as part of the arg list, so skip to avoid
    // double inference / param recording. Items are `SortBy` nodes —
    // unwrap to the inner expression before inferring.
    if !func.agg_within_group {
        for order_item in &func.agg_order {
            if let Some(node::Node::SortBy(sb)) = order_item.node.as_ref()
                && let Some(inner) = sb.node.as_deref()
            {
                infer_expr(inner, scope, null_ctx, snapshot, params, TypeGoal::NONE)?;
            }
        }
    }
    if let Some(over) = &func.over {
        for item in &over.partition_clause {
            infer_expr(item, scope, null_ctx, snapshot, params, TypeGoal::NONE)?;
        }
        for item in &over.order_clause {
            // Window `ORDER BY` items are also `SortBy` nodes; unwrap.
            if let Some(node::Node::SortBy(sb)) = item.node.as_ref()
                && let Some(inner) = sb.node.as_deref()
            {
                infer_expr(inner, scope, null_ctx, snapshot, params, TypeGoal::NONE)?;
            }
        }
        if let Some(start) = &over.start_offset {
            infer_expr(start, scope, null_ctx, snapshot, params, TypeGoal::NONE)?;
        }
        if let Some(end) = &over.end_offset {
            infer_expr(end, scope, null_ctx, snapshot, params, TypeGoal::NONE)?;
        }
    }

    // Per-arg nullability read by `concat_ws` (NULL separator propagates)
    // and `lag(col, offset, default)` / `lead(col, offset, default)` (a
    // non-null default replaces the boundary NULL).
    let arg_is_nullable = |i: usize| arg_nullable.get(i).copied().unwrap_or(false);

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
    let value_window_nullable = || -> bool {
        match name {
            "lag" | "lead" if func.args.len() >= 3 => arg_is_nullable(0) || arg_is_nullable(2),
            _ => true,
        }
    };

    let nullable = if is_value_window {
        value_window_nullable()
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
            any_arg_nullable
        } else {
            // Without GROUP BY, non-COUNT aggregates return NULL for empty tables.
            true
        }
    } else if resolved.is_strict && resolved.schema == "pg_catalog" {
        if functions::is_nullable_strict_exception(name) {
            true
        } else {
            any_arg_nullable
        }
    } else if resolved.schema == "pg_catalog" && name == "concat_ws" {
        // `concat_ws(sep, …)` is non-strict for the variadic args (NULLs are
        // skipped), but a NULL separator makes the whole result NULL.
        arg_is_nullable(0)
    } else {
        !(!resolved.is_strict
            && resolved.schema == "pg_catalog"
            && functions::is_not_null_nonstrict(name))
    };

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

// ──────────────────────────────────────────────────────────────────────────────
// Operators (A_Expr) — two-pass (PG chapter 10.2)
// ──────────────────────────────────────────────────────────────────────────────

fn infer_a_expr(
    expr: &protobuf::AExpr,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let op_name = extract_string_fields(&expr.name).join(".");
    let op_name = op_name.as_str();

    // `NULLIF(v1, v2)` — represented as an AExpr with op_name "=" and a
    // special kind. PG defines it as `CASE WHEN v1 = v2 THEN NULL ELSE v1 END`
    // (src/backend/parser/parse_expr.c:transformAExprNullIf), so the result
    // type is v1's type and the expression is always nullable. The generic
    // path below would return `bool` (from the `=` operator's result type),
    // silently corrupting the result column, so handle it up front.
    if matches!(
        protobuf::AExprKind::try_from(expr.kind),
        Ok(protobuf::AExprKind::AexprNullif)
    ) {
        // Both arms are inferred with NONE goal: a concrete-but-incompatible
        // RHS would otherwise trip the generic `cannot coerce X to Y` error
        // from the implicit goal before we get to the NULLIF-specific check
        // below, swallowing the chance to emit PG's exact wording.
        let left = expr
            .lexpr
            .as_ref()
            .map(|n| infer_expr(n, scope, null_ctx, snapshot, params, TypeGoal::NONE))
            .transpose()?;
        let left_oid = left.as_ref().map(|l| l.type_oid).unwrap_or(oid::UNKNOWN);
        let right = expr
            .rexpr
            .as_ref()
            .map(|n| infer_expr(n, scope, null_ctx, snapshot, params, TypeGoal::NONE))
            .transpose()?;
        let right_oid = right.as_ref().map(|r| r.type_oid).unwrap_or(oid::UNKNOWN);

        // Back-fill UNKNOWN side with the concrete side via implicit goal so
        // params and bare unknowns get pinned. Errors here are non-fatal: a
        // genuinely incompatible pair falls through to the operator check.
        let left_oid_final = if left_oid == oid::UNKNOWN && right_oid != oid::UNKNOWN {
            expr.lexpr
                .as_ref()
                .and_then(|n| {
                    infer_expr(
                        n,
                        scope,
                        null_ctx,
                        snapshot,
                        params,
                        TypeGoal::implicit(right_oid),
                    )
                    .ok()
                    .map(|t| t.type_oid)
                })
                .unwrap_or(left_oid)
        } else {
            left_oid
        };
        let right_oid_final = if right_oid == oid::UNKNOWN && left_oid_final != oid::UNKNOWN {
            expr.rexpr
                .as_ref()
                .and_then(|n| {
                    infer_expr(
                        n,
                        scope,
                        null_ctx,
                        snapshot,
                        params,
                        TypeGoal::implicit(left_oid_final),
                    )
                    .ok()
                    .map(|t| t.type_oid)
                })
                .unwrap_or(right_oid)
        } else {
            right_oid
        };

        // Validate: `=` must be defined between the two types. For untyped
        // string literals (`'x'`), treat them as `text` during the check so
        // `NULLIF(int, 'x')` still surfaces a type clash (analyzer is more
        // strict than PG here, which validates the literal at parse_analyze
        // time and would emit `invalid input syntax for type integer`).
        let left_for_check = unknown_literal_as_text(expr.lexpr.as_deref(), left_oid_final);
        let right_for_check = unknown_literal_as_text(expr.rexpr.as_deref(), right_oid_final);
        if left_for_check != oid::UNKNOWN
            && right_for_check != oid::UNKNOWN
            && snapshot
                .find_operator("=", Some(left_for_check), right_for_check)
                .is_none()
        {
            // PG's wording is `operator does not exist: A = B`. We append the
            // NULLIF context as a suffix so the macro caller still sees that
            // it was a NULLIF-shape mismatch.
            let l = crate::ddl::util::format_type_for_message(snapshot, left_for_check);
            let r = crate::ddl::util::format_type_for_message(snapshot, right_for_check);
            return Err(AnalyzeError::Invalid(format!(
                "operator does not exist: {l} = {r} \
                 (NULLIF types {l} and {r} cannot be matched)"
            )));
        }

        // Result type is the first arg's type (never bool). If the first arg
        // is UNKNOWN and the second is concrete, use the second as a fallback
        // so the result isn't a bare UNKNOWN dangling into the output.
        let result_oid = if left_oid_final != oid::UNKNOWN {
            left_oid_final
        } else {
            right_oid_final
        };
        return Ok(ExprType::scalar(result_oid, true));
    }

    // `expr IS [NOT] DISTINCT FROM other` — shares op_name "=" with ordinary
    // equality but PG guarantees the result is ALWAYS bool NOT NULL (the
    // whole point of the construct is NULL-aware comparison). Handled up
    // front so operand nullability doesn't bleed into the result.
    if matches!(
        protobuf::AExprKind::try_from(expr.kind),
        Ok(protobuf::AExprKind::AexprDistinct) | Ok(protobuf::AExprKind::AexprNotDistinct)
    ) {
        let left = expr
            .lexpr
            .as_ref()
            .map(|n| infer_expr(n, scope, null_ctx, snapshot, params, TypeGoal::NONE))
            .transpose()?;
        let left_oid = left.as_ref().map(|l| l.type_oid).unwrap_or(oid::UNKNOWN);
        let rhs_goal = if left_oid != oid::UNKNOWN {
            TypeGoal::implicit(left_oid)
        } else {
            TypeGoal::NONE
        };
        let _ = expr
            .rexpr
            .as_ref()
            .map(|n| infer_expr(n, scope, null_ctx, snapshot, params, rhs_goal))
            .transpose()?;
        return Ok(ExprType::scalar(oid::BOOL, false));
    }

    // `expr [NOT] BETWEEN lo AND hi` (and the SYM variants) — rexpr is a
    // `Node::List` holding the two bounds. The generic Pass 1 below walks
    // rexpr as a single expression, hits the `_` fallback for List, and
    // silently drops any `$N` placeholders inside. Handle it up front: infer
    // the lhs first, then re-enter each bound with the lhs type as the
    // inference goal so param OIDs resolve correctly.
    if matches!(
        protobuf::AExprKind::try_from(expr.kind),
        Ok(protobuf::AExprKind::AexprBetween)
            | Ok(protobuf::AExprKind::AexprNotBetween)
            | Ok(protobuf::AExprKind::AexprBetweenSym)
            | Ok(protobuf::AExprKind::AexprNotBetweenSym)
    ) {
        let left = expr
            .lexpr
            .as_ref()
            .map(|n| infer_expr(n, scope, null_ctx, snapshot, params, TypeGoal::NONE))
            .transpose()?;
        let left_oid = left.as_ref().map(|l| l.type_oid).unwrap_or(oid::UNKNOWN);

        let mut any_bound_nullable = false;
        if let Some(rexpr) = &expr.rexpr
            && let Some(node::Node::List(list)) = rexpr.node.as_ref()
        {
            let goal = if left_oid != oid::UNKNOWN {
                TypeGoal::implicit(left_oid)
            } else {
                TypeGoal::NONE
            };
            for item in &list.items {
                let t = infer_expr(item, scope, null_ctx, snapshot, params, goal.clone())?;
                any_bound_nullable = any_bound_nullable || t.nullable;
            }
        }

        let any_nullable = left.as_ref().is_some_and(|l| l.nullable) || any_bound_nullable;
        return Ok(ExprType::scalar(oid::BOOL, any_nullable));
    }

    // col IN ($1, $2, ...) / col NOT IN (...): rexpr is a Node::List whose
    // items need to be inferred with the left side's type as the goal so any
    // untyped params inside the list get their OID resolved. The generic
    // Pass 1 below calls `infer_expr` on the List node itself, which hits the
    // `_` fallback and silently errors (swallowed by the WHERE-clause helper).
    if matches!(
        protobuf::AExprKind::try_from(expr.kind),
        Ok(protobuf::AExprKind::AexprIn)
    ) {
        let left = expr
            .lexpr
            .as_ref()
            .map(|n| infer_expr(n, scope, null_ctx, snapshot, params, TypeGoal::NONE))
            .transpose()?;
        let left_oid = left.as_ref().map(|l| l.type_oid).unwrap_or(oid::UNKNOWN);

        let mut any_right_nullable = false;
        if let Some(rexpr) = &expr.rexpr
            && let Some(node::Node::List(list)) = rexpr.node.as_ref()
        {
            let goal = if left_oid != oid::UNKNOWN {
                TypeGoal::implicit(left_oid)
            } else {
                TypeGoal::NONE
            };
            for item in &list.items {
                let t = infer_expr(item, scope, null_ctx, snapshot, params, goal.clone())?;
                any_right_nullable = any_right_nullable || t.nullable;
            }
        }

        let any_nullable = left.as_ref().is_some_and(|l| l.nullable) || any_right_nullable;
        return Ok(ExprType::scalar(oid::BOOL, any_nullable));
    }

    // col = ANY($arr) / col = ALL($arr): lexpr is scalar, rexpr is array.
    // The generic back-fill below would assign the wrong type (element ↔ array
    // confusion), so we handle it first and return early.
    if matches!(
        protobuf::AExprKind::try_from(expr.kind),
        Ok(protobuf::AExprKind::AexprOpAny) | Ok(protobuf::AExprKind::AexprOpAll)
    ) {
        let left = expr
            .lexpr
            .as_ref()
            .map(|n| infer_expr(n, scope, null_ctx, snapshot, params, TypeGoal::NONE))
            .transpose()?;
        let right = expr
            .rexpr
            .as_ref()
            .map(|n| infer_expr(n, scope, null_ctx, snapshot, params, TypeGoal::NONE))
            .transpose()?;

        let left_oid = left.as_ref().map(|l| l.type_oid).unwrap_or(oid::UNKNOWN);
        let right_oid = right.as_ref().map(|r| r.type_oid).unwrap_or(oid::UNKNOWN);

        // left is concrete T, right is unknown → right must be T[].
        if left_oid != oid::UNKNOWN
            && right_oid == oid::UNKNOWN
            && let Some(arr_oid) = snapshot.array_type_of(left_oid)
            && let Some(rexpr) = &expr.rexpr
        {
            let _ = infer_expr(
                rexpr,
                scope,
                null_ctx,
                snapshot,
                params,
                TypeGoal::implicit(arr_oid),
            );
        }

        // right is concrete T[], left is unknown → left must be the element type T.
        if right_oid != oid::UNKNOWN
            && left_oid == oid::UNKNOWN
            && let Some(elem_oid) = snapshot.get_type(right_oid).and_then(|t| {
                if t.typcategory == TypCategory::Array {
                    t.typelem
                } else {
                    None
                }
            })
            && let Some(lexpr) = &expr.lexpr
        {
            let _ = infer_expr(
                lexpr,
                scope,
                null_ctx,
                snapshot,
                params,
                TypeGoal::implicit(elem_oid),
            );
        }

        let any_nullable =
            left.as_ref().is_some_and(|l| l.nullable) || right.as_ref().is_some_and(|r| r.nullable);
        return Ok(ExprType::scalar(oid::BOOL, any_nullable));
    }

    // Record-record comparison pre-pass.
    //
    // `ROW(a, b) = ROW(c, d)` and the implicit `(a, b) = (c, d)` both parse
    // as AExpr with two RowExpr children. The generic AExpr resolver below
    // can't handle them: `find_operator` looks for a `record OP record`
    // overload but neither side carries enough type info for params to be
    // pinned, so `$p1`/`$p2` fall through as text. Instead, walk both rows
    // once to collect shapes, then back-fill each ROW element with the
    // peer's concrete OID as a goal — exactly mirroring how PG types
    // each component before reaching the row-compare operator.
    if matches!(op_name, "=" | "<>" | "<" | ">" | "<=" | ">=")
        && let (Some(lexpr), Some(rexpr)) = (expr.lexpr.as_deref(), expr.rexpr.as_deref())
        && let (Some(node::Node::RowExpr(lrow)), Some(node::Node::RowExpr(rrow))) =
            (lexpr.node.as_ref(), rexpr.node.as_ref())
    {
        // PG (parse_analyze): `unequal number of entries in row expressions`
        // when the two ROWs have different arity. Catch it up front so the
        // back-fill loop below can assume aligned positions.
        if lrow.args.len() != rrow.args.len() {
            return Err(AnalyzeError::Invalid(
                "unequal number of entries in row expressions".to_owned(),
            ));
        }
        // Pass 1: collect element types for each side with no goal.
        let mut left_types = Vec::with_capacity(lrow.args.len());
        let mut right_types = Vec::with_capacity(rrow.args.len());
        let mut any_nullable = false;
        for la in &lrow.args {
            let t = infer_expr(la, scope, null_ctx, snapshot, params, TypeGoal::NONE)?;
            any_nullable = any_nullable || t.nullable;
            left_types.push(t);
        }
        for ra in &rrow.args {
            let t = infer_expr(ra, scope, null_ctx, snapshot, params, TypeGoal::NONE)?;
            any_nullable = any_nullable || t.nullable;
            right_types.push(t);
        }

        // Pass 2: back-fill — when one side is concrete and the other is
        // UNKNOWN at the same position, re-walk the unknown side with the
        // concrete OID as goal so embedded params get pinned.
        for (i, (l, r)) in left_types.iter().zip(right_types.iter()).enumerate() {
            if l.type_oid != oid::UNKNOWN && r.type_oid == oid::UNKNOWN {
                let _ = infer_expr(
                    &rrow.args[i],
                    scope,
                    null_ctx,
                    snapshot,
                    params,
                    TypeGoal::implicit(l.type_oid),
                );
            } else if r.type_oid != oid::UNKNOWN && l.type_oid == oid::UNKNOWN {
                let _ = infer_expr(
                    &lrow.args[i],
                    scope,
                    null_ctx,
                    snapshot,
                    params,
                    TypeGoal::implicit(r.type_oid),
                );
            }
        }

        return Ok(ExprType::scalar(oid::BOOL, any_nullable));
    }

    // ROW(...) compared against a sub-SELECT: PG counts columns at the
    // subquery boundary (the inner ROW stays a single record column), so
    // the LHS arity must equal the subquery's column count. Mirror PG's
    // `subquery has too few/many columns` for the mismatch case.
    if matches!(op_name, "=" | "<>" | "<" | ">" | "<=" | ">=")
        && let (Some(lexpr), Some(rexpr)) = (expr.lexpr.as_deref(), expr.rexpr.as_deref())
        && let (Some(node::Node::RowExpr(lrow)), Some(node::Node::SubLink(sub))) =
            (lexpr.node.as_ref(), rexpr.node.as_ref())
        && matches!(
            protobuf::SubLinkType::try_from(sub.sub_link_type),
            Ok(protobuf::SubLinkType::ExprSublink)
        )
        && let Some(subselect) = sub.subselect.as_ref()
        && let Some(node::Node::SelectStmt(sel)) = subselect.node.as_ref()
    {
        for la in &lrow.args {
            let _ = infer_expr(la, scope, null_ctx, snapshot, params, TypeGoal::NONE);
        }
        let (cols, _) = crate::resolve::analyze_correlated_select(sel, snapshot, params, scope)?;
        if cols.len() != lrow.args.len() {
            let pg_msg = if cols.len() < lrow.args.len() {
                "subquery has too few columns"
            } else {
                "subquery has too many columns"
            };
            return Err(AnalyzeError::Invalid(format!(
                "{pg_msg} (subquery has {}, lhs has {})",
                cols.len(),
                lrow.args.len(),
            )));
        }
        return Ok(ExprType::scalar(oid::BOOL, true));
    }

    // Pass 1: infer both sides bottom-up.
    let left = expr
        .lexpr
        .as_ref()
        .map(|n| infer_expr(n, scope, null_ctx, snapshot, params, TypeGoal::NONE))
        .transpose()?;
    let right = expr
        .rexpr
        .as_ref()
        .map(|n| infer_expr(n, scope, null_ctx, snapshot, params, TypeGoal::NONE))
        .transpose()?;

    let left_oid = left.as_ref().map(|l| l.type_oid);
    let right_oid = right.as_ref().map(|r| r.type_oid).unwrap_or(oid::UNKNOWN);

    // PG step 2: if one side is unknown and the other is concrete, assume
    // unknown = the other side's type.  Re-infer to propagate into params.
    if let (Some(l_oid), true) = (left_oid, right_oid == oid::UNKNOWN)
        && l_oid != oid::UNKNOWN
        && let Some(rexpr) = &expr.rexpr
    {
        let _ = infer_expr(
            rexpr,
            scope,
            null_ctx,
            snapshot,
            params,
            TypeGoal::implicit(l_oid),
        );
    }
    if let Some(r) = &right
        && r.type_oid != oid::UNKNOWN
        && left_oid == Some(oid::UNKNOWN)
        && let Some(lexpr) = &expr.lexpr
    {
        let _ = infer_expr(
            lexpr,
            scope,
            null_ctx,
            snapshot,
            params,
            TypeGoal::implicit(r.type_oid),
        );
    }

    // Re-read types after back-fill.
    let left_oid_resolved = expr
        .lexpr
        .as_ref()
        .and_then(|n| match n.node.as_ref() {
            Some(node::Node::ParamRef(p)) => {
                let t = params.get(p.number);
                if t != oid::UNKNOWN { Some(t) } else { left_oid }
            }
            _ => left_oid,
        })
        .or(left_oid);
    let right_oid_resolved = expr
        .rexpr
        .as_ref()
        .map(|n| match n.node.as_ref() {
            Some(node::Node::ParamRef(p)) => {
                let t = params.get(p.number);
                if t != oid::UNKNOWN { t } else { right_oid }
            }
            _ => right_oid,
        })
        .unwrap_or(right_oid);

    let any_nullable =
        left.as_ref().is_some_and(|l| l.nullable) || right.as_ref().is_some_and(|r| r.nullable);
    let op_always_nullable = functions::is_nullable_operator(op_name);
    let nullable = any_nullable || op_always_nullable;

    // Try operator lookup with resolved types.
    if let Some(op) = snapshot.find_operator(op_name, left_oid_resolved, right_oid_resolved) {
        // Pass 2: back-fill still-UNKNOWN sides with operator's expected types.
        if left_oid_resolved == Some(oid::UNKNOWN)
            && let (Some(expected), Some(lexpr)) = (op.left_type_oid, &expr.lexpr)
        {
            let _ = infer_expr(
                lexpr,
                scope,
                null_ctx,
                snapshot,
                params,
                TypeGoal::implicit(expected),
            );
        }
        if right_oid_resolved == oid::UNKNOWN
            && let Some(rexpr) = &expr.rexpr
        {
            let _ = infer_expr(
                rexpr,
                scope,
                null_ctx,
                snapshot,
                params,
                TypeGoal::implicit(op.right_type_oid),
            );
        }
        return Ok(ExprType::scalar(op.result_type_oid, nullable));
    }

    // `find_operator` fails in two semantically different ways:
    //   * both operand types are UNKNOWN → PG `indeterminate_datatype` (42P18),
    //     e.g. `$1 + $2` with no context, or `NULL = NULL` where no candidate
    //     can be picked. The user fix is to cast one side, not to blame the
    //     operator itself.
    //   * at least one side is concrete → PG `undefined_function` / operator
    //     (42883): the operator really doesn't exist for these types.
    let left_unknown = left_oid_resolved.map(|o| o == oid::UNKNOWN).unwrap_or(true);
    let right_unknown = right_oid_resolved == oid::UNKNOWN;
    if left_unknown && right_unknown {
        let span = (expr.location >= 0)
            .then(|| crate::error::SourceSpan::at_length(expr.location as usize, op_name.len()));
        return Err(crate::error::RawError::indeterminate_type(
            format!("could not determine data type of operator {op_name}"),
            span,
            Some("add an explicit type cast to one side, e.g. `expr::int4`".into()),
        )
        .finalize_implicit());
    }
    // PG (SQLSTATE 42883): `operator does not exist: <left> <op> <right>`.
    // Use PG's user-facing type names (`integer`, `bigint`, …) so the
    // sanity-check prefix match passes.
    let left_pg = crate::ddl::util::format_type_for_message(
        snapshot,
        left_oid_resolved.unwrap_or(oid::UNKNOWN),
    );
    let right_pg = crate::ddl::util::format_type_for_message(snapshot, right_oid_resolved);
    // `AExpr.location` points at the operator token; cover its length
    // so the caret spans the operator symbol/name.
    let span = (expr.location >= 0)
        .then(|| crate::error::SourceSpan::at_length(expr.location as usize, op_name.len()));
    Err(crate::error::RawError::undefined_operator(
        format!("operator does not exist: {left_pg} {op_name} {right_pg}"),
        span,
        None,
    )
    .finalize_implicit())
}

// ──────────────────────────────────────────────────────────────────────────────
// Bool expressions (AND, OR, NOT) — PG uses COERCION_ASSIGNMENT for args
// ──────────────────────────────────────────────────────────────────────────────

fn infer_bool_expr(
    expr: &protobuf::BoolExpr,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    // PG names the failing argument after the operator (`argument of NOT must
    // be type boolean, not type X`, likewise AND / OR).
    let label = match protobuf::BoolExprType::try_from(expr.boolop) {
        Ok(protobuf::BoolExprType::NotExpr) => "NOT",
        Ok(protobuf::BoolExprType::OrExpr) => "OR",
        _ => "AND",
    };
    let mut any_nullable = false;
    for arg in &expr.args {
        match infer_expr(
            arg,
            scope,
            null_ctx,
            snapshot,
            params,
            TypeGoal::assignment(oid::BOOL),
        ) {
            Ok(t) => any_nullable = any_nullable || t.nullable,
            // Rewrite a coerce-to-bool mismatch to PG's exact wording; other
            // errors keep their own message.
            Err(e) => {
                if !matches!(e, AnalyzeError::TypeMismatch { .. }) {
                    return Err(e);
                }
                let mut params2 = params.clone();
                let actual_oid =
                    infer_expr(arg, scope, null_ctx, snapshot, &mut params2, TypeGoal::NONE)
                        .map(|t| t.type_oid)
                        .unwrap_or(oid::UNKNOWN);
                let actual_pg = crate::ddl::util::format_type_for_message(snapshot, actual_oid);
                return Err(AnalyzeError::Invalid(format!(
                    "argument of {label} must be type boolean, not type {actual_pg}"
                )));
            }
        }
    }
    Ok(ExprType::scalar(oid::BOOL, any_nullable))
}

// ──────────────────────────────────────────────────────────────────────────────
// COALESCE — two-pass (PG chapter 10.5)
// ──────────────────────────────────────────────────────────────────────────────

fn infer_coalesce(
    expr: &protobuf::CoalesceExpr,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    // Pass 1: infer all args bottom-up. Bare string literals in UNKNOWN slots
    // are reinterpreted as `text` so `COALESCE(int_col, 'x')` rejects instead
    // of silently coercing the literal under the concrete branch's type.
    let mut types = Vec::new();
    let mut all_nullable = true;

    for arg in &expr.args {
        let t = infer_expr(arg, scope, null_ctx, snapshot, params, TypeGoal::NONE)?;
        types.push(unknown_literal_as_text(Some(arg), t.type_oid));
        if !t.nullable {
            all_nullable = false;
        }
    }

    // All non-UNKNOWN branches must share a common type, otherwise PG
    // rejects with `could not convert type X to Y`.
    let concrete_types: Vec<PgTypeOid> = types
        .iter()
        .copied()
        .filter(|&t| t != oid::UNKNOWN)
        .collect();
    let type_oid = if concrete_types.is_empty() {
        // All branches are UNKNOWN → PG §10.5 defaults to the preferred type
        // of the string category (usually `text`). Derived from the catalog so
        // we stay honest: no hardcoded OID here.
        snapshot
            .preferred_type_in_category(TypCategory::String)
            .unwrap_or(oid::UNKNOWN)
    } else {
        coerce::find_common_type(&concrete_types, snapshot).ok_or_else(|| {
            // PG (SQLSTATE 42804): `COALESCE types A and B cannot be
            // matched`. PG reports the COALESCE args in source order
            // (first then last), the *opposite* of CASE which orders the
            // last branch first. We use `Invalid` to keep
            // `TypeMismatch::Display`'s generic prefix from leaking in
            // front of PG's exact wording.
            let first = crate::ddl::util::format_type_for_message(snapshot, concrete_types[0]);
            let last = crate::ddl::util::format_type_for_message(
                snapshot,
                concrete_types[concrete_types.len() - 1],
            );
            AnalyzeError::Invalid(format!(
                "COALESCE types {first} and {last} cannot be matched"
            ))
        })?
    };

    // Pass 2: back-fill UNKNOWN args with the resolved common type.
    if type_oid != oid::UNKNOWN {
        for (i, arg) in expr.args.iter().enumerate() {
            if types[i] == oid::UNKNOWN {
                let _ = infer_expr(
                    arg,
                    scope,
                    null_ctx,
                    snapshot,
                    params,
                    TypeGoal::implicit(type_oid),
                );
            }
        }
    }

    // A `$param` directly inside COALESCE is, by construction, expected to be
    // nullable — otherwise the COALESCE would be pointless. Override with
    // `$param!` to force non-null.
    for arg in &expr.args {
        if let Some(node::Node::ParamRef(p)) = arg.node.as_ref() {
            params.infer_nullable(p.number, true);
        }
    }

    Ok(ExprType::scalar(type_oid, all_nullable))
}

// ──────────────────────────────────────────────────────────────────────────────
// CASE — two-pass (PG chapter 10.5)
// ──────────────────────────────────────────────────────────────────────────────

fn infer_case(
    expr: &protobuf::CaseExpr,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    // Pass 1: infer WHEN conditions with BOOL goal, results with NONE.
    let mut types = Vec::new();
    let mut any_branch_nullable = false;

    for arg in &expr.args {
        if let Some(node::Node::CaseWhen(when)) = arg.node.as_ref() {
            // WHEN condition must be boolean. On a coerce-to-bool mismatch,
            // rewrite to PG's exact wording (`argument of CASE/WHEN must be
            // type boolean, not type X`) the same way the WHERE clause does;
            // other errors carry their own message and propagate as-is.
            if let Some(cond) = &when.expr
                && let Err(e) = infer_expr(
                    cond,
                    scope,
                    null_ctx,
                    snapshot,
                    params,
                    TypeGoal::assignment(oid::BOOL),
                )
            {
                if !matches!(e, AnalyzeError::TypeMismatch { .. }) {
                    return Err(e);
                }
                let mut params2 = params.clone();
                let actual_oid = infer_expr(
                    cond,
                    scope,
                    null_ctx,
                    snapshot,
                    &mut params2,
                    TypeGoal::NONE,
                )
                .map(|t| t.type_oid)
                .unwrap_or(oid::UNKNOWN);
                let actual_pg = crate::ddl::util::format_type_for_message(snapshot, actual_oid);
                return Err(AnalyzeError::Invalid(format!(
                    "argument of CASE/WHEN must be type boolean, not type {actual_pg}"
                )));
            }
            // THEN result. Untyped string literals are reinterpreted as
            // `text` for branch reconciliation — PG's common-type rules
            // compare literal syntax against the concrete branch's type
            // and reject mismatches like `CASE … THEN 1 ELSE 'x' END`.
            if let Some(result) = &when.result {
                let t = infer_expr(result, scope, null_ctx, snapshot, params, TypeGoal::NONE)?;
                types.push(unknown_literal_as_text(Some(result), t.type_oid));
                any_branch_nullable = any_branch_nullable || t.nullable;
            }
        }
    }

    // ELSE clause.
    if let Some(defresult) = &expr.defresult {
        let t = infer_expr(defresult, scope, null_ctx, snapshot, params, TypeGoal::NONE)?;
        types.push(unknown_literal_as_text(Some(defresult), t.type_oid));
        any_branch_nullable = any_branch_nullable || t.nullable;
    } else {
        any_branch_nullable = true;
    }

    // All non-UNKNOWN branches must share a common type, otherwise PG
    // rejects with `could not convert type X to Y`.
    let concrete_types: Vec<PgTypeOid> = types
        .iter()
        .copied()
        .filter(|&t| t != oid::UNKNOWN)
        .collect();
    let type_oid = if concrete_types.is_empty() {
        // All branches are UNKNOWN → PG §10.5 defaults to the preferred type
        // of the string category (usually `text`). Derived from the catalog so
        // we stay honest: no hardcoded OID here.
        snapshot
            .preferred_type_in_category(TypCategory::String)
            .unwrap_or(oid::UNKNOWN)
    } else {
        coerce::find_common_type(&concrete_types, snapshot).ok_or_else(|| {
            // PG: `CASE types A and B cannot be matched` — last branch
            // first, candidate type from prior branches second.
            let last = crate::ddl::util::format_type_for_message(
                snapshot,
                concrete_types[concrete_types.len() - 1],
            );
            let first = crate::ddl::util::format_type_for_message(snapshot, concrete_types[0]);
            AnalyzeError::Invalid(format!("CASE types {last} and {first} cannot be matched"))
        })?
    };

    // Pass 2: back-fill UNKNOWN result branches with the common type.
    if type_oid != oid::UNKNOWN {
        let mut type_idx = 0;
        for arg in &expr.args {
            if let Some(node::Node::CaseWhen(when)) = arg.node.as_ref()
                && let Some(result) = &when.result
            {
                if types.get(type_idx) == Some(&oid::UNKNOWN) {
                    let _ = infer_expr(
                        result,
                        scope,
                        null_ctx,
                        snapshot,
                        params,
                        TypeGoal::implicit(type_oid),
                    );
                }
                type_idx += 1;
            }
        }
        if let Some(defresult) = &expr.defresult
            && types.get(type_idx) == Some(&oid::UNKNOWN)
        {
            let _ = infer_expr(
                defresult,
                scope,
                null_ctx,
                snapshot,
                params,
                TypeGoal::implicit(type_oid),
            );
        }
    }

    Ok(ExprType::scalar(type_oid, any_branch_nullable))
}

// ──────────────────────────────────────────────────────────────────────────────
// Subqueries (SubLink)
// ──────────────────────────────────────────────────────────────────────────────

fn infer_sublink(
    sub: &protobuf::SubLink,
    scope: &Scope,
    _null_ctx: &NullabilityContext,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let sub_type = protobuf::SubLinkType::try_from(sub.sub_link_type)
        .unwrap_or(protobuf::SubLinkType::ExprSublink);

    match sub_type {
        protobuf::SubLinkType::ExistsSublink => {
            // Walk the subselect to collect any params referenced inside —
            // without this, `EXISTS(SELECT 1 FROM t WHERE x = $p1)` would
            // drop `$p1` from the param list entirely. Outer scope is
            // seeded so correlated refs (`outer.col`) resolve correctly
            // and feed types into the param resolver.
            if let Some(subselect) = &sub.subselect
                && let Some(node::Node::SelectStmt(sel)) = subselect.node.as_ref()
            {
                let _ = crate::resolve::analyze_correlated_select(sel, snapshot, params, scope)?;
            }
            Ok(ExprType::scalar(oid::BOOL, false))
        }
        protobuf::SubLinkType::ExprSublink => {
            if let Some(subselect) = &sub.subselect
                && let Some(node::Node::SelectStmt(sel)) = subselect.node.as_ref()
            {
                let (cols, _) =
                    crate::resolve::analyze_correlated_select(sel, snapshot, params, scope)?;
                if let Some(first) = cols.first() {
                    let guaranteed_one_row =
                        sel.group_clause.is_empty() && has_aggregate_target(&sel.target_list);
                    let nullable = if guaranteed_one_row {
                        first.nullable
                    } else {
                        true
                    };
                    return Ok(ExprType::scalar(first.type_oid, nullable));
                }
            }
            Ok(ExprType::scalar(oid::UNKNOWN, true))
        }
        protobuf::SubLinkType::AnySublink | protobuf::SubLinkType::AllSublink => {
            // Walk the subselect so params inside `col = ANY(SELECT …)` /
            // `col = ALL(SELECT …)` are collected with the right types.
            if let Some(subselect) = &sub.subselect
                && let Some(node::Node::SelectStmt(sel)) = subselect.node.as_ref()
            {
                let (cols, _) =
                    crate::resolve::analyze_correlated_select(sel, snapshot, params, scope)?;

                // Arity check: `lhs IN (SELECT …)` / `lhs = ANY(SELECT …)`
                // requires the LHS and the subquery to match column counts.
                // PG rejects mismatches with `subquery has too many columns`
                // or `subquery has too few columns`.
                let lhs_arity = sub
                    .testexpr
                    .as_ref()
                    .map(|n| match n.node.as_ref() {
                        Some(node::Node::RowExpr(r)) => r.args.len(),
                        _ => 1,
                    })
                    .unwrap_or(1);
                if lhs_arity != cols.len() {
                    let pg_msg = if cols.len() < lhs_arity {
                        "subquery has too few columns"
                    } else {
                        "subquery has too many columns"
                    };
                    return Err(AnalyzeError::Invalid(format!(
                        "{pg_msg} (subquery has {}, lhs has {lhs_arity})",
                        cols.len(),
                    )));
                }
            }
            Ok(ExprType::scalar(oid::BOOL, true))
        }
        protobuf::SubLinkType::ArraySublink => {
            // `ARRAY(SELECT expr FROM …)` — returns an array of the subquery's
            // first output column. The array itself is always NOT NULL (an
            // empty result produces `{}`, not NULL), even though individual
            // elements may be nullable.
            let mut elem_oid = oid::UNKNOWN;
            if let Some(subselect) = &sub.subselect
                && let Some(node::Node::SelectStmt(sel)) = subselect.node.as_ref()
            {
                let (cols, _) =
                    crate::resolve::analyze_correlated_select(sel, snapshot, params, scope)?;
                if let Some(first) = cols.first() {
                    elem_oid = first.type_oid;
                }
            }
            let array_oid = snapshot.array_type_of(elem_oid).unwrap_or(oid::UNKNOWN);
            Ok(ExprType::scalar(array_oid, false))
        }
        _ => Err(AnalyzeError::Unsupported(format!(
            "sublink type: {:?}",
            sub_type
        ))),
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Check if any target in a SELECT's target list contains an aggregate function call.
fn has_aggregate_target(target_list: &[protobuf::Node]) -> bool {
    target_list.iter().any(|node| {
        if let Some(node::Node::ResTarget(res)) = node.node.as_ref()
            && let Some(val) = &res.val
        {
            return node_contains_aggregate(val);
        }
        false
    })
}

/// Recursively check if a node is or contains an aggregate function call.
fn node_contains_aggregate(node: &protobuf::Node) -> bool {
    match node.node.as_ref() {
        Some(node::Node::FuncCall(func)) => {
            if func.agg_star || func.agg_order.iter().len() > 0 {
                return true;
            }
            let names = extract_string_fields(&func.funcname);
            let name = names.last().map(|s| s.as_str()).unwrap_or("");
            matches!(
                name,
                "count"
                    | "sum"
                    | "avg"
                    | "min"
                    | "max"
                    | "array_agg"
                    | "string_agg"
                    | "bool_and"
                    | "bool_or"
                    | "every"
                    | "json_agg"
                    | "jsonb_agg"
                    | "json_object_agg"
                    | "jsonb_object_agg"
                    | "bit_and"
                    | "bit_or"
            )
        }
        Some(node::Node::SubLink(_)) => false,
        Some(node::Node::AExpr(expr)) => {
            expr.lexpr
                .as_ref()
                .is_some_and(|n| node_contains_aggregate(n))
                || expr
                    .rexpr
                    .as_ref()
                    .is_some_and(|n| node_contains_aggregate(n))
        }
        Some(node::Node::TypeCast(cast)) => cast
            .arg
            .as_ref()
            .is_some_and(|n| node_contains_aggregate(n)),
        Some(node::Node::CoalesceExpr(c)) => c.args.iter().any(node_contains_aggregate),
        Some(node::Node::CaseExpr(c)) => {
            c.args.iter().any(|n| {
                if let Some(node::Node::CaseWhen(w)) = n.node.as_ref() {
                    w.result
                        .as_ref()
                        .is_some_and(|r| node_contains_aggregate(r))
                } else {
                    false
                }
            }) || c
                .defresult
                .as_ref()
                .is_some_and(|n| node_contains_aggregate(n))
        }
        Some(node::Node::BoolExpr(b)) => b.args.iter().any(node_contains_aggregate),
        Some(node::Node::NullTest(t)) => t.arg.as_ref().is_some_and(|n| node_contains_aggregate(n)),
        _ => false,
    }
}

/// Extract string values from a list of nodes.
pub(crate) fn extract_string_fields(nodes: &[protobuf::Node]) -> Vec<String> {
    nodes
        .iter()
        .filter_map(|n| match n.node.as_ref()? {
            node::Node::String(s) => Some(s.sval.clone()),
            _ => None,
        })
        .collect()
}

/// Resolve a TypeName to a type OID.
fn resolve_type_name(
    type_name: Option<&protobuf::TypeName>,
    snapshot: &PgCatalog,
) -> Result<PgTypeOid, AnalyzeError> {
    let tn = type_name.ok_or_else(|| AnalyzeError::Unsupported("missing TypeName".into()))?;

    if let Some(oid) = PgTypeOid::new(tn.type_oid) {
        return Ok(oid);
    }

    let parts = extract_string_fields(&tn.names);
    let (schema, name) = match parts.as_slice() {
        [name] => (None, name.as_str()),
        [schema, name] => (Some(schema.as_str()), name.as_str()),
        _ => {
            return Err(AnalyzeError::Unsupported(format!(
                "complex type name: {:?}",
                parts
            )));
        }
    };

    let is_array = !tn.array_bounds.is_empty();

    let type_entry = snapshot.resolve_type_by_name(schema, name).ok_or_else(|| {
        // Build a snippet + "did you mean" hint for the unknown type name.
        let hint = crate::suggest::suggest_similar(name, snapshot.visible_type_names(schema))
            .map(|c| format!("did you mean \"{c}\"?"));
        let span = crate::error::SourceSpan::from_node_qname(tn.location);
        let qualified = parts.join(".");
        crate::error::RawError {
            kind: AnalyzeError::UndefinedType(format!("type \"{qualified}\" does not exist")),
            primary: span.map(|s| crate::error::DiagnosticLabel::new(s, "type does not exist")),
            secondaries: Vec::new(),
            hint,
        }
        .finalize_implicit()
    })?;

    if is_array {
        let array_name = format!("_{name}");
        if let Some(arr) = snapshot.resolve_type_by_name(schema, &array_name) {
            return Ok(arr.oid);
        }
        if let Some(arr_oid) = snapshot.array_type_of(type_entry.oid) {
            return Ok(arr_oid);
        }
    }

    Ok(type_entry.oid)
}
