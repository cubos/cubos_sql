//! Read-only queries and lookup helpers for [`PgCatalog`].
//!
//! Hosts the second `impl PgCatalog` block — everything that reads the
//! catalog rows and indexes without mutating them. Includes the PG §10.2
//! operator resolution algorithm, name/OID resolution along the search path,
//! and helpers like [`PgCatalog::attributes_of`] and
//! [`PgCatalog::enum_labels_of`] that consumers use instead of poking at the
//! HashMaps directly.

use crate::oid::{PgClassOid, PgExtensionOid, PgNamespaceOid, PgTypeOid};
use crate::pg_catalog::{
    CastContext, CastMethod, DepType, PG_CLASS_RELID, PG_EXTENSION_RELID, PG_TYPE_RELID,
    PgAttribute, PgCast, PgCatalog, PgClass, PgDepend, PgOperator, PgProc, PgType, TypCategory,
    TypType, oid,
};

/// Result of operator resolution: operand and result OIDs with any
/// polymorphic pseudo-types (`anyelement`, `anycompatiblearray`, …) already
/// substituted by the concrete types of the arguments.
#[derive(Debug, Clone)]
pub struct ResolvedOperator {
    pub left_type_oid: Option<PgTypeOid>,
    pub right_type_oid: PgTypeOid,
    pub result_type_oid: PgTypeOid,
}

const PG_CATALOG_SCHEMA: &str = "pg_catalog";

impl PgCatalog {
    // ── Namespace helpers ───────────────────────────────────────────────

    pub fn namespace_oid(&self, name: &str) -> Option<PgNamespaceOid> {
        self.namespace_by_name.get(name).copied()
    }

    pub fn namespace_name(&self, oid: PgNamespaceOid) -> Option<&str> {
        self.pg_namespace.get(&oid).map(|n| n.nspname.as_str())
    }

    /// OID of the `pg_catalog` schema (looked up once per call). Returns
    /// `None` only on an empty catalog.
    fn pg_catalog_oid(&self) -> Option<PgNamespaceOid> {
        self.namespace_oid(PG_CATALOG_SCHEMA)
    }

    /// `true` if `pg_catalog` appears explicitly on the search path.
    fn search_path_includes_pg_catalog(&self) -> bool {
        match self.pg_catalog_oid() {
            Some(oid) => self.search_path.contains(&oid),
            None => false,
        }
    }

    /// Search-path-aware schema resolution: if `schema` is `Some`, returns
    /// `[that_oid]`; otherwise returns the search path with `pg_catalog`
    /// implicitly prepended (PG §5.9.5).
    fn schemas_for_lookup(&self, schema: Option<&str>) -> Vec<PgNamespaceOid> {
        if let Some(name) = schema {
            return self.namespace_oid(name).into_iter().collect();
        }
        let mut out = Vec::with_capacity(self.search_path.len() + 1);
        if !self.search_path_includes_pg_catalog()
            && let Some(pg_oid) = self.pg_catalog_oid()
        {
            out.push(pg_oid);
        }
        out.extend(self.search_path.iter().copied());
        out
    }

    // ── Relation / type / function lookups ──────────────────────────────

    /// Look up a relation by name, walking the search path when `schema` is
    /// `None`. Mirrors PG §5.9.5 (pg_catalog implicitly searched first).
    pub fn resolve_table(&self, schema: Option<&str>, name: &str) -> Option<&PgClass> {
        for nsoid in self.schemas_for_lookup(schema) {
            if let Some(&class_oid) = self.class_by_qname.get(&(nsoid, name.to_owned()))
                && let Some(class) = self.pg_class.get(&class_oid)
            {
                return Some(class);
            }
        }
        None
    }

    /// Iterate over `relname`s for tables/views/sequences visible in
    /// `schema` (or in the search path when `schema` is `None`). Used to
    /// produce "did you mean ..." hints for `UndefinedTable`.
    pub fn visible_relnames<'a>(
        &'a self,
        schema: Option<&'a str>,
    ) -> impl Iterator<Item = &'a str> + 'a {
        self.schemas_for_lookup(schema)
            .into_iter()
            .flat_map(move |nsoid| {
                self.class_by_qname
                    .iter()
                    .filter(move |((ns, _), _)| *ns == nsoid)
                    .map(|((_, name), _)| name.as_str())
            })
    }

    /// Look up a type by name, walking the search path when `schema` is
    /// `None`.
    pub fn resolve_type_by_name(&self, schema: Option<&str>, name: &str) -> Option<&PgType> {
        for nsoid in self.schemas_for_lookup(schema) {
            if let Some(&type_oid) = self.type_by_qname.get(&(nsoid, name.to_owned()))
                && let Some(t) = self.pg_type.get(&type_oid)
            {
                return Some(t);
            }
        }
        None
    }

    /// Look up a type by OID.
    pub fn get_type(&self, oid: PgTypeOid) -> Option<&PgType> {
        self.pg_type.get(&oid)
    }

    /// Find the OID of the array type whose elements are `element_oid`.
    /// Resolves through `pg_type.typarray`, which PG keeps pointing at the
    /// canonical `_<name>` array; legacy types like `oidvector` /
    /// `int2vector` share `typelem` with `oid`/`int2` but are not pointed
    /// to by anyone's `typarray`, so they're correctly excluded.
    pub fn array_type_of(&self, element_oid: PgTypeOid) -> Option<PgTypeOid> {
        self.pg_type.get(&element_oid).and_then(|t| t.typarray)
    }

    /// Unwrap domains to their base type OID (capped at 32 levels to avoid
    /// pathological cycles in malformed catalogs).
    pub fn unwrap_domain(&self, oid: PgTypeOid) -> PgTypeOid {
        let mut current = oid;
        for _ in 0..32 {
            match self.pg_type.get(&current) {
                Some(t) if t.typtype == TypType::Domain => match t.typbasetype {
                    Some(base) => current = base,
                    None => break,
                },
                _ => break,
            }
        }
        current
    }

    /// Walk the domain chain looking for a `typnotnull` row. Returns the
    /// `typname` of the first domain that forbids NULLs, or `None` if no
    /// domain in the chain has the constraint. Capped at 32 hops, same as
    /// [`unwrap_domain`], to stay safe against malformed catalogs.
    pub fn domain_not_null_name(&self, oid: PgTypeOid) -> Option<&str> {
        let mut current = oid;
        for _ in 0..32 {
            let t = self.pg_type.get(&current)?;
            if t.typtype == TypType::Domain && t.typnotnull {
                return Some(&t.typname);
            }
            if t.typtype == TypType::Domain {
                current = t.typbasetype?;
            } else {
                break;
            }
        }
        None
    }

    /// True when the type chain forces non-nullable semantics on the column,
    /// independent of `pg_attribute.attnotnull`.
    pub fn type_is_not_null(&self, oid: PgTypeOid) -> bool {
        self.domain_not_null_name(oid).is_some()
    }

    /// Resolve the modifier that should apply to a column, given its
    /// `pg_attribute.atttypmod` and the column's type. PG semantics:
    /// `atttypmod` wins when present; otherwise we walk the domain chain
    /// looking for a `typtypmod` to inherit. This way `CREATE DOMAIN d AS
    /// varchar(20); CREATE TABLE t (x d)` produces a column with the right
    /// length even though `parse_column_def` left `atttypmod = None`.
    pub fn effective_typmod(&self, oid: PgTypeOid, atttypmod: Option<i32>) -> Option<i32> {
        if atttypmod.is_some() {
            return atttypmod;
        }
        let mut current = oid;
        for _ in 0..32 {
            let t = self.pg_type.get(&current)?;
            if let Some(v) = t.typtypmod {
                return Some(v);
            }
            if t.typtype == TypType::Domain {
                current = t.typbasetype?;
            } else {
                break;
            }
        }
        None
    }

    /// The preferred type of a given `pg_type.typcategory`. Used when the
    /// analyzer needs to pick a concrete type for an expression whose inputs
    /// are all UNKNOWN (string-category literals default to `text`, numeric
    /// literals to `numeric`, etc.).
    pub fn preferred_type_in_category(&self, category: TypCategory) -> Option<PgTypeOid> {
        self.pg_type
            .values()
            .find(|t| t.typcategory == category && t.typispreferred)
            .map(|t| t.oid)
    }

    /// Check if an implicit cast exists from `source` to `target`.
    pub fn has_implicit_cast(&self, source: PgTypeOid, target: PgTypeOid) -> bool {
        if source == target {
            return true;
        }
        match self.cast_by_pair.get(&(source, target)) {
            Some(&oid) => matches!(
                self.pg_cast.get(&oid),
                Some(PgCast {
                    castcontext: CastContext::Implicit,
                    ..
                })
            ),
            None => false,
        }
    }

    /// Check if `source` is binary-coercible to `target` — the PG rule that
    /// lets `ALTER COLUMN TYPE` skip a table rewrite.
    ///
    /// True when:
    /// - `source == target`
    /// - `source` is a domain whose base type is `target` (one level)
    /// - `pg_cast` has an implicit, binary-method entry from `source` to `target`
    pub fn is_binary_coercible(&self, source: PgTypeOid, target: PgTypeOid) -> bool {
        if source == target {
            return true;
        }
        if let Some(t) = self.pg_type.get(&source)
            && t.typtype == TypType::Domain
            && t.typbasetype == Some(target)
        {
            return true;
        }
        match self.cast_by_pair.get(&(source, target)) {
            Some(&oid) => matches!(
                self.pg_cast.get(&oid),
                Some(PgCast {
                    castcontext: CastContext::Implicit,
                    castmethod: CastMethod::Binary,
                    ..
                })
            ),
            None => false,
        }
    }

    /// Iterate over function names visible from `schema` (or the full
    /// search path when `schema` is `None`). Used for `did you mean` hints
    /// on `UndefinedFunction`.
    /// Iterate over type names visible from `schema` (or the search path
    /// when `schema` is `None`). Used for `did you mean` hints on
    /// `UndefinedType`.
    pub fn visible_type_names<'a>(
        &'a self,
        schema: Option<&'a str>,
    ) -> impl Iterator<Item = &'a str> + 'a {
        self.schemas_for_lookup(schema)
            .into_iter()
            .flat_map(move |nsoid| {
                self.type_by_qname
                    .iter()
                    .filter(move |((ns, _), _)| *ns == nsoid)
                    .map(|((_, name), _)| name.as_str())
            })
    }

    pub fn visible_function_names<'a>(
        &'a self,
        schema: Option<&'a str>,
    ) -> impl Iterator<Item = &'a str> + 'a {
        self.schemas_for_lookup(schema)
            .into_iter()
            .flat_map(move |nsoid| {
                self.proc_by_qname
                    .iter()
                    .filter(move |((ns, _), _)| *ns == nsoid)
                    .map(|((_, name), _)| name.as_str())
            })
    }

    /// Find all functions matching a name, walking the search path when
    /// `schema` is `None`.
    ///
    /// When `schema` is `Some`, only overloads in that schema are returned.
    /// When `None`, overloads from every schema on the search_path (plus
    /// `pg_catalog` if not explicitly listed) are concatenated.
    pub fn find_functions(&self, schema: Option<&str>, name: &str) -> Vec<&PgProc> {
        let mut out = Vec::new();
        for nsoid in self.schemas_for_lookup(schema) {
            if let Some(oids) = self.proc_by_qname.get(&(nsoid, name.to_owned())) {
                for &oid in oids {
                    if let Some(p) = self.pg_proc.get(&oid) {
                        out.push(p);
                    }
                }
            }
        }
        out
    }

    /// Find an operator matching name and operand types.
    ///
    /// Implements the PostgreSQL §10.2 operator type resolution algorithm:
    ///   1. Exact match
    ///   2. Match via implicit casts
    ///   3. UNKNOWN-aware resolution with preferred-type disambiguation
    ///
    /// Candidates are gathered from every schema on the search_path (plus
    /// `pg_catalog` if not listed explicitly).
    pub fn find_operator(
        &self,
        name: &str,
        left_oid: Option<PgTypeOid>,
        right_oid: PgTypeOid,
    ) -> Option<ResolvedOperator> {
        let mut candidate_buf: Vec<&PgOperator> = Vec::new();
        for nsoid in self.schemas_for_lookup(None) {
            if let Some(oids) = self.operator_by_qname.get(&(nsoid, name.to_owned())) {
                for &oid in oids {
                    if let Some(op) = self.pg_operator.get(&oid) {
                        // Skip shell operators — `oprresult = None` means
                        // the implementation hasn't been linked yet and
                        // the operator can't appear in queries.
                        if op.oprresult.is_some() {
                            candidate_buf.push(op);
                        }
                    }
                }
            }
        }
        if candidate_buf.is_empty() {
            return None;
        }
        let candidates = &candidate_buf;

        // PG §10.2 step 3b: unwrap domain types to their base types.
        let left_oid = left_oid.map(|oid| self.unwrap_domain(oid));
        let right_oid = self.unwrap_domain(right_oid);

        // `oprleft = None` means prefix; the field is already an `Option`.
        let op_left = |o: &PgOperator| -> Option<PgTypeOid> { o.oprleft };

        // Step 1: exact match.
        if let Some(&op) = candidates
            .iter()
            .find(|o| op_left(o) == left_oid && o.oprright == right_oid)
        {
            return concretize_operator(op, left_oid, right_oid, self);
        }

        // Step 2: match via implicit casts. More than one candidate can
        // match — PG §10.2 step 3c keeps those with the most exact matches
        // on input types.
        let cast_matches: Vec<&PgOperator> = candidates
            .iter()
            .filter(|o| {
                let left_ok = match (op_left(o), left_oid) {
                    (Some(expected), Some(actual)) => {
                        actual == expected || self.has_implicit_cast(actual, expected)
                    }
                    (None, None) => true,
                    _ => false,
                };
                let right_ok =
                    o.oprright == right_oid || self.has_implicit_cast(right_oid, o.oprright);
                left_ok && right_ok
            })
            .copied()
            .collect();
        let exact_score = |o: &&PgOperator| -> u8 {
            let left_exact = match (op_left(o), left_oid) {
                (Some(e), Some(a)) => (e == a) as u8,
                (None, None) => 1,
                _ => 0,
            };
            let right_exact = (o.oprright == right_oid) as u8;
            left_exact + right_exact
        };
        if let Some(max_score) = cast_matches.iter().map(exact_score).max()
            && let Some(best) = cast_matches
                .iter()
                .find(|o| exact_score(o) == max_score)
                .copied()
        {
            return concretize_operator(best, left_oid, right_oid, self);
        }

        // Step 2b: polymorphic match. Operators declared over pseudo-types
        // (`anyarray || anyarray`, `anycompatible || anycompatiblearray`, …)
        // never appear as exact matches — PG resolves them by checking the
        // shape of the concrete operands against the pseudo-type's
        // constraint, then substitutes the bound types into the result.
        // UNKNOWN actuals are deliberately *not* accepted here — when one
        // side is UNKNOWN we defer to step 3, which prefers a candidate
        // whose known side matches *exactly* (e.g. `text || text` over
        // `anycompatible || anycompatiblearray` for `text || 'foo'`).
        let poly_matches: Vec<&PgOperator> = candidates
            .iter()
            .filter(|o| {
                let left_ok = match (op_left(o), left_oid) {
                    (Some(expected), Some(actual))
                        if crate::functions::is_polymorphic(expected) =>
                    {
                        crate::functions::matches_polymorphic(expected, actual, self)
                    }
                    (Some(expected), Some(actual)) => {
                        expected == actual || self.has_implicit_cast(actual, expected)
                    }
                    (None, None) => true,
                    _ => false,
                };
                let right_ok = if crate::functions::is_polymorphic(o.oprright) {
                    crate::functions::matches_polymorphic(o.oprright, right_oid, self)
                } else {
                    o.oprright == right_oid || self.has_implicit_cast(right_oid, o.oprright)
                };
                let has_any_poly = op_left(o).is_some_and(crate::functions::is_polymorphic)
                    || crate::functions::is_polymorphic(o.oprright);
                has_any_poly && left_ok && right_ok
            })
            .copied()
            .collect();
        // PG tie-break: among polymorphic candidates, pick the most specific
        // signature.
        if !poly_matches.is_empty() {
            let score = |o: &&PgOperator| -> u16 {
                let l = op_left(o)
                    .map(crate::functions::polymorphic_specificity)
                    .unwrap_or(10) as u16;
                let r = crate::functions::polymorphic_specificity(o.oprright) as u16;
                l + r
            };
            if let Some(max_score) = poly_matches.iter().map(&score).max() {
                let best: Vec<&PgOperator> = poly_matches
                    .iter()
                    .filter(|o| score(o) == max_score)
                    .copied()
                    .collect();
                if best.len() == 1 {
                    return concretize_operator(best[0], left_oid, right_oid, self);
                }
            }
        }

        // Step 3 (PG §10.2 step 3): UNKNOWN-aware resolution.
        let left_unknown = left_oid == Some(oid::UNKNOWN);
        let right_unknown = right_oid == oid::UNKNOWN;
        if !left_unknown && !right_unknown {
            return None;
        }

        // When exactly one operand is UNKNOWN and the other is a concrete type
        // `T`, PG resolves the unknown to `T` — so `int4 = NULL`, `bigint > 'x'`
        // and `int4 + NULL` are all valid. Prefer the homogeneous `T OP T`
        // overload directly when it exists: without this, a type with
        // cross-type operators (`int4 = int8`, `int4 = int2`, …) leaves several
        // candidates that the category/`text` fallback below can't
        // disambiguate, so the operator is wrongly reported as missing. The
        // probe is exact (`oprleft == T && oprright == T`), so it only ever
        // *adds* a resolution PG also makes — never changes an existing one.
        if left_unknown ^ right_unknown {
            let known = if right_unknown {
                left_oid
            } else {
                Some(right_oid)
            };
            if let Some(t) = known
                && t != oid::UNKNOWN
                && let Some(&op) = candidates
                    .iter()
                    .find(|o| op_left(o) == Some(t) && o.oprright == t)
            {
                return concretize_operator(op, left_oid, right_oid, self);
            }
        }

        // 3a. Keep candidates where known sides match (exact, implicit cast,
        //     or polymorphic constraint) and UNKNOWN sides are treated as
        //     compatible with anything. Polymorphic positions (`anyenum`,
        //     `anycompatiblearray`, …) accept any actual whose shape matches
        //     the pseudo-type's constraint — `enforce_generic_type_consistency`
        //     binds the concrete type later, and we mirror it here so an
        //     `enum_col = 'literal'` resolves through `anyenum = anyenum`.
        let mut remaining: Vec<&PgOperator> = candidates
            .iter()
            .filter(|o| {
                let left_ok = match (op_left(o), left_oid) {
                    (Some(_), Some(actual)) if actual == oid::UNKNOWN => true,
                    (Some(expected), Some(actual))
                        if crate::functions::is_polymorphic(expected) =>
                    {
                        crate::functions::matches_polymorphic(expected, actual, self)
                    }
                    (Some(expected), Some(actual)) => self.has_implicit_cast(actual, expected),
                    (None, None) => true,
                    _ => false,
                };
                let right_ok = if right_unknown {
                    true
                } else if crate::functions::is_polymorphic(o.oprright) {
                    crate::functions::matches_polymorphic(o.oprright, right_oid, self)
                } else {
                    self.has_implicit_cast(right_oid, o.oprright)
                };
                left_ok && right_ok
            })
            .copied()
            .collect();

        if remaining.len() <= 1 {
            return remaining
                .into_iter()
                .next()
                .and_then(|o| concretize_operator(o, left_oid, right_oid, self));
        }

        // 3b. If one side is known, keep only candidates that accept exactly
        //     that type on the known side.
        if !left_unknown {
            let exact: Vec<&PgOperator> = remaining
                .iter()
                .filter(|o| op_left(o) == left_oid)
                .copied()
                .collect();
            if !exact.is_empty() {
                remaining = exact;
            }
        }
        if !right_unknown {
            let exact: Vec<&PgOperator> = remaining
                .iter()
                .filter(|o| o.oprright == right_oid)
                .copied()
                .collect();
            if !exact.is_empty() {
                remaining = exact;
            }
        }

        if remaining.len() <= 1 {
            return remaining
                .into_iter()
                .next()
                .and_then(|o| concretize_operator(o, left_oid, right_oid, self));
        }

        // 3c (PG §10.2 step 3e-f). For each UNKNOWN position, check if all
        //     remaining candidates agree on the type category. If so, prefer
        //     the candidate that uses the *preferred* type in that category.
        if left_unknown {
            let preferred = self.prefer_by_category(&remaining, |o| op_left(o));
            if !preferred.is_empty() {
                remaining = preferred;
            }
        }
        if remaining.len() > 1 && right_unknown {
            let preferred = self.prefer_by_category(&remaining, |o| Some(o.oprright));
            if !preferred.is_empty() {
                remaining = preferred;
            }
        }

        if remaining.len() == 1 {
            return concretize_operator(remaining[0], left_oid, right_oid, self);
        }

        // 3d. Final fallback: resolve UNKNOWN positions to `text`, since
        //     string constants default to text in PostgreSQL.
        let text_oid = oid::TEXT;
        let resolved_left = if left_unknown {
            Some(text_oid)
        } else {
            left_oid
        };
        let resolved_right = if right_unknown { text_oid } else { right_oid };

        let exact_matches: Vec<&PgOperator> = remaining
            .iter()
            .filter(|o| op_left(o) == resolved_left && o.oprright == resolved_right)
            .copied()
            .collect();
        if exact_matches.len() == 1 {
            return concretize_operator(exact_matches[0], resolved_left, resolved_right, self);
        }

        let text_matches: Vec<&PgOperator> = remaining
            .iter()
            .filter(|o| {
                let left_ok = match (op_left(o), resolved_left) {
                    (Some(expected), Some(actual)) => {
                        expected == actual || self.has_implicit_cast(actual, expected)
                    }
                    (None, None) => true,
                    _ => false,
                };
                let right_ok = o.oprright == resolved_right
                    || self.has_implicit_cast(resolved_right, o.oprright);
                left_ok && right_ok
            })
            .copied()
            .collect();
        if text_matches.len() == 1 {
            return concretize_operator(text_matches[0], resolved_left, resolved_right, self);
        }

        None
    }

    /// Among `candidates`, keep those whose type at the position extracted by
    /// `get_oid` is the preferred type in its category — but only when all
    /// candidates agree on the same category (PG §10.2 step 3f).
    fn prefer_by_category<'a>(
        &self,
        candidates: &[&'a PgOperator],
        get_oid: impl Fn(&PgOperator) -> Option<PgTypeOid>,
    ) -> Vec<&'a PgOperator> {
        let cats: Vec<Option<TypCategory>> = candidates
            .iter()
            .map(|o| {
                get_oid(o)
                    .and_then(|id| self.pg_type.get(&id))
                    .map(|t| t.typcategory)
            })
            .collect();
        let first = match cats.first() {
            Some(Some(c)) => *c,
            _ => return Vec::new(),
        };
        if !cats.iter().all(|c| *c == Some(first)) {
            return Vec::new();
        }
        candidates
            .iter()
            .filter(|o| {
                get_oid(o)
                    .and_then(|id| self.pg_type.get(&id))
                    .is_some_and(|t| t.typispreferred)
            })
            .copied()
            .collect()
    }

    // ── Relationship helpers ────────────────────────────────────────────

    /// All attributes of a relation (table/view/composite type), ordered by
    /// `attnum`. Returns an empty slice when the relation has none (or when
    /// `relid` is unknown — callers usually verify the relation first).
    pub fn attributes_of(&self, relid: PgClassOid) -> &[PgAttribute] {
        self.pg_attribute
            .get(&relid)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Find one attribute of a relation by name. Linear scan over the
    /// relation's attributes (typically a handful).
    pub fn attribute_by_name(&self, relid: PgClassOid, name: &str) -> Option<&PgAttribute> {
        self.attributes_of(relid).iter().find(|a| a.attname == name)
    }

    /// Enum labels of a type, ordered by `enumsortorder`.
    pub fn enum_labels_of(&self, typid: PgTypeOid) -> Vec<&str> {
        self.pg_enum
            .get(&typid)
            .map(|v| v.iter().map(|e| e.enumlabel.as_str()).collect())
            .unwrap_or_default()
    }

    /// Composite type fields (the `pg_attribute` rows of the type's
    /// `pg_class` row, found via `typrelid`).
    pub fn composite_fields_of(&self, typid: PgTypeOid) -> &[PgAttribute] {
        let Some(t) = self.pg_type.get(&typid) else {
            return &[];
        };
        let Some(relid) = t.typrelid else {
            return &[];
        };
        self.attributes_of(relid)
    }

    /// All `pg_class` rows a view depends on (deptype=Normal, classid=
    /// PG_CLASS_RELID). Yields `(refobjid, refobjsubid)` — `refobjsubid` is
    /// the column attnum, or 0 if the view depends on the whole relation.
    pub fn view_dependencies(
        &self,
        view_oid: PgClassOid,
    ) -> impl Iterator<Item = (PgClassOid, i16)> + '_ {
        self.pg_depend
            .iter()
            .filter(move |d| {
                d.classid == PG_CLASS_RELID
                    && d.objid.get() == view_oid.get()
                    && d.refclassid == PG_CLASS_RELID
                    && matches!(d.deptype, DepType::Normal)
            })
            .map(|d| {
                (
                    PgClassOid::from_nonzero(d.refobjid.into_nonzero()),
                    d.refobjsubid,
                )
            })
    }

    /// All catalog objects an extension created (deptype=Extension,
    /// refclassid=PG_EXTENSION_RELID). Yields `(classid, objid)`.
    pub fn extension_objects(
        &self,
        ext_oid: PgExtensionOid,
    ) -> impl Iterator<Item = (PgClassOid, crate::oid::PgGenericOid)> + '_ {
        self.pg_depend.iter().filter_map(move |d| {
            (d.refclassid == PG_EXTENSION_RELID
                && d.refobjid.get() == ext_oid.get()
                && matches!(d.deptype, DepType::Extension))
            .then_some((d.classid, d.objid))
        })
    }

    /// Name of the extension that owns this type, if any. Looks for a
    /// `pg_depend` row with `deptype=Extension`, `classid=PG_TYPE_RELID`,
    /// `objid=type_oid`, and resolves `refobjid` to the extension's `extname`.
    pub fn extension_of_type(&self, type_oid: PgTypeOid) -> Option<&str> {
        for d in &self.pg_depend {
            if matches!(d.deptype, DepType::Extension)
                && d.classid == PG_TYPE_RELID
                && d.objid.get() == type_oid.get()
                && d.refclassid == PG_EXTENSION_RELID
                && let Some(ext_oid) = crate::oid::PgExtensionOid::new(d.refobjid.get())
                && let Some(ext) = self.pg_extension.get(&ext_oid)
            {
                return Some(ext.extname.as_str());
            }
        }
        None
    }

    /// Iterate all `pg_depend` rows in the catalog. Reserved for the
    /// CASCADE walker in `ddl/drop.rs`.
    pub fn iter_pg_depend(&self) -> impl Iterator<Item = &PgDepend> + '_ {
        self.pg_depend.iter()
    }

    // ── Internal-feature accessors (tests + internal feature) ───────────

    #[cfg(any(test, feature = "internal"))]
    pub fn pg_namespace(
        &self,
    ) -> &std::collections::HashMap<PgNamespaceOid, crate::pg_catalog::PgNamespace> {
        &self.pg_namespace
    }

    #[cfg(any(test, feature = "internal"))]
    pub fn pg_type(&self) -> &std::collections::HashMap<PgTypeOid, PgType> {
        &self.pg_type
    }

    #[cfg(any(test, feature = "internal"))]
    pub fn pg_class(&self) -> &std::collections::HashMap<PgClassOid, PgClass> {
        &self.pg_class
    }

    #[cfg(any(test, feature = "internal"))]
    pub fn pg_class_mut(&mut self) -> &mut std::collections::HashMap<PgClassOid, PgClass> {
        &mut self.pg_class
    }

    #[cfg(any(test, feature = "internal"))]
    pub fn pg_attribute(&self) -> &std::collections::HashMap<PgClassOid, Vec<PgAttribute>> {
        &self.pg_attribute
    }

    #[cfg(any(test, feature = "internal"))]
    pub fn pg_inherits(&self) -> &[crate::pg_catalog::PgInherits] {
        &self.pg_inherits
    }

    /// Iterate over every `pg_index` row. Tests use this to assert the
    /// shape of indexes the DDL emitted; runtime callers don't need it.
    #[cfg(any(test, feature = "internal"))]
    pub fn pg_index_values(&self) -> impl Iterator<Item = &crate::pg_catalog::PgIndex> {
        self.pg_index.values()
    }

    /// Iterate over every `pg_constraint` row. Used by the `ON CONFLICT`
    /// validator to find PK/UNIQUE constraints for a relation.
    pub(crate) fn pg_constraint_values(
        &self,
    ) -> impl Iterator<Item = &crate::pg_catalog::PgConstraint> {
        self.pg_constraint.values()
    }

    /// Names of every `pg_constraint` row attached to a relation. Returns
    /// the empty `Vec` if the schema or table is unknown.
    #[cfg(any(test, feature = "internal"))]
    pub fn pg_constraint_names_for_table(&self, schema: &str, name: &str) -> Vec<String> {
        let Some(nsoid) = self.namespace_oid(schema) else {
            return Vec::new();
        };
        let Some(class_oid) = self.class_by_qname.get(&(nsoid, name.to_owned())).copied() else {
            return Vec::new();
        };
        self.pg_constraint
            .values()
            .filter(|c| c.conrelid == class_oid)
            .map(|c| c.conname.clone())
            .collect()
    }

    #[cfg(any(test, feature = "internal"))]
    pub fn pg_proc(&self) -> &std::collections::HashMap<crate::oid::PgProcOid, PgProc> {
        &self.pg_proc
    }

    #[cfg(any(test, feature = "internal"))]
    pub fn pg_aggregate(
        &self,
    ) -> &std::collections::HashMap<crate::oid::PgProcOid, crate::pg_catalog::PgAggregate> {
        &self.pg_aggregate
    }

    #[cfg(any(test, feature = "internal"))]
    pub fn pg_operator(&self) -> &std::collections::HashMap<crate::oid::PgOperatorOid, PgOperator> {
        &self.pg_operator
    }

    #[cfg(any(test, feature = "internal"))]
    pub fn pg_cast(&self) -> &std::collections::HashMap<crate::oid::PgCastOid, PgCast> {
        &self.pg_cast
    }

    #[cfg(any(test, feature = "internal"))]
    pub fn pg_extension(
        &self,
    ) -> &std::collections::HashMap<crate::oid::PgExtensionOid, crate::pg_catalog::PgExtension>
    {
        &self.pg_extension
    }

    #[cfg(any(test, feature = "internal"))]
    pub fn pg_depend(&self) -> &[PgDepend] {
        &self.pg_depend
    }

    /// Current `search_path` namespace OIDs, in order.
    #[cfg(any(test, feature = "internal"))]
    pub fn search_path(&self) -> &[PgNamespaceOid] {
        &self.search_path
    }
}

/// Turn a [`PgOperator`] — which may declare polymorphic pseudo-types on its
/// operands and result — into a [`ResolvedOperator`] whose OIDs are already
/// substituted with the concrete types derived from the caller's operands.
/// For non-polymorphic operators the result just mirrors the entry's declared
/// OIDs.
///
/// Returns `None` for shell operators (`oprresult = None`); those are
/// pre-filtered out of [`PgCatalog::find_operator`]'s candidate list, so in
/// practice this only short-circuits if a caller bypasses that filter.
fn concretize_operator(
    op: &PgOperator,
    left_actual: Option<PgTypeOid>,
    right_actual: PgTypeOid,
    db: &PgCatalog,
) -> Option<ResolvedOperator> {
    let op_left = op.oprleft;
    let mut bound_element: Option<PgTypeOid> = None;
    let mut bound_array: Option<PgTypeOid> = None;
    if let (Some(expected_l), Some(actual_l)) = (op_left, left_actual)
        && crate::functions::is_polymorphic(expected_l)
    {
        crate::functions::bind_polymorphic_from(
            expected_l,
            actual_l,
            db,
            &mut bound_element,
            &mut bound_array,
        );
    }
    if crate::functions::is_polymorphic(op.oprright) {
        crate::functions::bind_polymorphic_from(
            op.oprright,
            right_actual,
            db,
            &mut bound_element,
            &mut bound_array,
        );
    }
    Some(ResolvedOperator {
        left_type_oid: op_left
            .map(|o| crate::functions::substitute_polymorphic(o, bound_element, bound_array, db)),
        right_type_oid: crate::functions::substitute_polymorphic(
            op.oprright,
            bound_element,
            bound_array,
            db,
        ),
        result_type_oid: crate::functions::substitute_polymorphic(
            op.oprresult?,
            bound_element,
            bound_array,
            db,
        ),
    })
}
