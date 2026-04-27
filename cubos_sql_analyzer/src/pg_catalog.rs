//! The [`PgCatalog`] type: a mutable in-memory PostgreSQL schema.
//!
//! `PgCatalog` starts from the embedded PostgreSQL 18 seed catalog and evolves
//! by applying DDL statements via [`PgCatalog::apply_sql`]. It is the single
//! entry point for schema construction in the public API.

use std::collections::{HashMap, HashSet};

use crate::ddl::{DdlError, InstalledExtension, apply_sql_to};
use crate::error::AnalyzeError;
use crate::lexer::lex;
use crate::qualified_name::QualifiedName;
use crate::resolve::{AnalyzedQuery, analyze_static, build_spread_sample_sql, fuse};
use crate::schema::{
    CastContext, CastInfo, CastMethod, FunctionEntry, OperatorEntry, ResolvedOperator, TableEntry,
    TypeEntry, TypeKind, oid,
};
use crate::seed::load_seed;

/// A mutable in-memory schema. Applies DDL statements on top of a seed
/// catalog and keeps every catalog-level table, type, function, operator,
/// and cast updated as each statement is processed.
#[derive(Clone)]
pub struct PgCatalog {
    pub(crate) types: HashMap<u32, TypeEntry>,
    pub(crate) type_by_name: HashMap<QualifiedName, u32>,
    pub(crate) tables: HashMap<QualifiedName, TableEntry>,
    pub(crate) functions_by_name: HashMap<QualifiedName, Vec<FunctionEntry>>,
    pub(crate) operators_by_name: HashMap<QualifiedName, Vec<OperatorEntry>>,
    pub(crate) casts: HashMap<String, CastInfo>,
    pub(crate) search_path: Vec<String>,
    pub(crate) schemas: HashSet<String>,
    pub(crate) next_oid: u32,
    pub(crate) installed_extensions: HashMap<String, InstalledExtension>,
}

/// Starting OID for user-defined objects. Well above PG system OIDs (~16384).
pub(crate) const USER_OID_START: u32 = 100_000;

impl Default for PgCatalog {
    fn default() -> Self {
        Self::new()
    }
}

impl PgCatalog {
    /// Create a new catalog seeded with the PostgreSQL 18 built-in catalog.
    pub fn new() -> Self {
        let seed = load_seed();
        Self {
            types: seed.types,
            type_by_name: seed.type_by_name,
            tables: seed.tables,
            functions_by_name: seed.functions_by_name,
            operators_by_name: seed.operators_by_name,
            casts: seed.casts,
            search_path: seed.search_path,
            schemas: seed.schemas,
            next_oid: USER_OID_START,
            installed_extensions: HashMap::new(),
        }
    }

    /// Parse and apply all DDL statements in a SQL string, updating the schema
    /// in-place.
    pub fn apply_sql(&mut self, sql: &str) -> Result<(), DdlError> {
        apply_sql_to(self, sql)
    }

    /// Analyze a SQL query template against this catalog.
    ///
    /// Lexes `sql` to extract named parameters (`$name`), spreads (`$..name`),
    /// and nullability annotations (`$foo?`, `$foo!`); rewrites the SQL with
    /// positional placeholders; infers parameter and output column types; and
    /// returns everything combined in an [`AnalyzedQuery`].
    pub fn analyze(&self, sql: &str) -> Result<AnalyzedQuery, AnalyzeError> {
        let lex_output = lex(sql)?;

        // Collect explicit nullability annotations from the lexer, ordered by
        // positional parameter index (regular params first, then spread fields).
        let mut param_nullability: Vec<Option<bool>> =
            lex_output.params.iter().map(|p| p.nullable).collect();
        for spread in &lex_output.spreads {
            if let Some(fields) = &spread.fields {
                param_nullability.extend(
                    fields
                        .iter()
                        .map(|f| if f.nullable { Some(true) } else { None }),
                );
            }
        }

        // When the query has spreads, run analysis on a sample SQL where each
        // spread is materialized as a single row of placeholders, so the
        // analyzer can infer the field types from surrounding context.
        let analysis_sql = if lex_output.spreads.is_empty() {
            lex_output.sql.clone()
        } else {
            build_spread_sample_sql(&lex_output)
        };

        let (columns, mut info_params) = analyze_static(self, &analysis_sql, &param_nullability)?;

        // Invariant: the analyzer must produce exactly one param entry per
        // positional placeholder the lexer extracted (regular params +
        // materialized spread fields). A mismatch means the analyzer missed
        // a placeholder during walk — e.g. hit an unsupported node and
        // swallowed the error — which would silently drop params from
        // generated types. Surface as an `Internal` error rather than a
        // panic so the macro host process can report it cleanly.
        let expected_param_count = lex_output.params.len()
            + lex_output
                .spreads
                .iter()
                .map(|s| s.fields.as_ref().map(|f| f.len()).unwrap_or(0))
                .sum::<usize>();
        if info_params.len() != expected_param_count {
            return Err(AnalyzeError::Internal(format!(
                "analyzer param count ({}) does not match lexer placeholder count ({}) \
                 for SQL: {analysis_sql}",
                info_params.len(),
                expected_param_count,
            )));
        }

        // Merge explicit $foo? / $foo! annotations from the lexer on top of
        // the analyzer's inferred nullability (explicit always wins).
        for (pi, &lexer_nullable) in info_params.iter_mut().zip(param_nullability.iter()) {
            if let Some(explicit) = lexer_nullable {
                pi.nullable = explicit;
            }
        }

        Ok(fuse(lex_output, columns, info_params))
    }

    // ── Internal access for tests and the `internal` feature ────────────────
    //
    // Field accessors below are only compiled in test/internal builds. Their
    // return types reference `crate::schema` types, which are themselves only
    // re-exported under the same cfg gate, so external consumers without the
    // feature can't see them anyway.

    /// All types indexed by OID.
    #[cfg(any(test, feature = "internal"))]
    pub fn types(&self) -> &HashMap<u32, TypeEntry> {
        &self.types
    }

    /// Tables and views, keyed by their schema-qualified name.
    #[cfg(any(test, feature = "internal"))]
    pub fn tables(&self) -> &HashMap<QualifiedName, TableEntry> {
        &self.tables
    }

    /// Mutable handle to the tables/views map. Reserved for the legacy-view
    /// regression test in `tests/ddl/alter_table.rs`, which needs to clear a
    /// view's `resolved_ast` to simulate a snapshot loaded from older JSON.
    #[cfg(any(test, feature = "internal"))]
    pub fn tables_mut(&mut self) -> &mut HashMap<QualifiedName, TableEntry> {
        &mut self.tables
    }

    /// Functions indexed by their schema-qualified name; each entry is the
    /// list of overloads.
    #[cfg(any(test, feature = "internal"))]
    pub fn functions_by_name(&self) -> &HashMap<QualifiedName, Vec<FunctionEntry>> {
        &self.functions_by_name
    }

    /// Operators indexed by their schema-qualified name; each entry is the
    /// list of overloads.
    #[cfg(any(test, feature = "internal"))]
    pub fn operators_by_name(&self) -> &HashMap<QualifiedName, Vec<OperatorEntry>> {
        &self.operators_by_name
    }

    /// Cast rules keyed by `"source_oid:target_oid"`.
    #[cfg(any(test, feature = "internal"))]
    pub fn casts(&self) -> &HashMap<String, CastInfo> {
        &self.casts
    }

    /// Current `search_path` schemas, in order.
    #[cfg(any(test, feature = "internal"))]
    pub fn search_path(&self) -> &[String] {
        &self.search_path
    }

    /// Serialize the catalog's schema-level data into a JSON-friendly seed
    /// shape. Round-trips with [`PgCatalog::from_seed`]. Extension state and
    /// the OID allocator are not part of the seed.
    #[cfg(any(test, feature = "internal"))]
    pub fn to_seed(&self) -> crate::seed::SchemaSeed {
        crate::seed::SchemaSeed {
            types: self.types.clone(),
            type_by_name: self.type_by_name.clone(),
            tables: self.tables.clone(),
            functions_by_name: self.functions_by_name.clone(),
            operators_by_name: self.operators_by_name.clone(),
            casts: self.casts.clone(),
            search_path: self.search_path.clone(),
            schemas: self.schemas.clone(),
        }
    }

    /// Build a [`PgCatalog`] from a previously serialized seed. Extension
    /// state is reset and OID allocation restarts at [`USER_OID_START`].
    #[cfg(any(test, feature = "internal"))]
    pub fn from_seed(seed: crate::seed::SchemaSeed) -> Self {
        Self {
            types: seed.types,
            type_by_name: seed.type_by_name,
            tables: seed.tables,
            functions_by_name: seed.functions_by_name,
            operators_by_name: seed.operators_by_name,
            casts: seed.casts,
            search_path: seed.search_path,
            schemas: seed.schemas,
            next_oid: USER_OID_START,
            installed_extensions: HashMap::new(),
        }
    }

    // ── Internal helpers used by the DDL submodules ─────────────────────────

    /// Build an empty catalog (no seed). Reserved for the view-AST rewrite
    /// walker, which needs a placeholder handle when descending into
    /// subselects whose RangeVars are already fully qualified.
    pub(crate) fn empty() -> Self {
        Self {
            types: HashMap::new(),
            type_by_name: HashMap::new(),
            tables: HashMap::new(),
            functions_by_name: HashMap::new(),
            operators_by_name: HashMap::new(),
            casts: HashMap::new(),
            search_path: Vec::new(),
            schemas: HashSet::new(),
            next_oid: USER_OID_START,
            installed_extensions: HashMap::new(),
        }
    }

    pub(crate) fn alloc_oid(&mut self) -> u32 {
        let oid = self.next_oid;
        self.next_oid += 1;
        oid
    }

    // ── Lookup methods (formerly on `SchemaSnapshot`) ───────────────────────

    /// Look up a table or view by name, searching the `search_path`.
    pub fn resolve_table(&self, schema: Option<&str>, name: &str) -> Option<&TableEntry> {
        if let Some(s) = schema {
            return self.tables.get(&QualifiedName::new(s, name));
        }
        // PG §5.9.5: pg_catalog is implicitly searched before the search_path
        // unless it is already listed explicitly.
        if !self.search_path.iter().any(|s| s == "pg_catalog")
            && let Some(entry) = self.tables.get(&QualifiedName::new("pg_catalog", name))
        {
            return Some(entry);
        }
        for s in &self.search_path {
            if let Some(entry) = self.tables.get(&QualifiedName::new(s, name)) {
                return Some(entry);
            }
        }
        None
    }

    /// Look up a type by name, searching the `search_path`.
    pub fn resolve_type_by_name(&self, schema: Option<&str>, name: &str) -> Option<&TypeEntry> {
        if let Some(s) = schema {
            let key = QualifiedName::new(s, name);
            return self
                .type_by_name
                .get(&key)
                .and_then(|oid| self.types.get(oid));
        }
        // PG §5.9.5: pg_catalog is implicitly searched before the search_path
        // unless it is already listed explicitly.
        if !self.search_path.iter().any(|s| s == "pg_catalog") {
            let pg_key = QualifiedName::new("pg_catalog", name);
            if let Some(oid) = self.type_by_name.get(&pg_key)
                && let Some(entry) = self.types.get(oid)
            {
                return Some(entry);
            }
        }
        for s in &self.search_path {
            let key = QualifiedName::new(s, name);
            if let Some(oid) = self.type_by_name.get(&key)
                && let Some(entry) = self.types.get(oid)
            {
                return Some(entry);
            }
        }
        None
    }

    /// Look up a type by OID.
    pub fn get_type(&self, oid: u32) -> Option<&TypeEntry> {
        self.types.get(&oid)
    }

    /// Find the OID of the array type whose elements are `element_oid`, if
    /// one is registered. Mirrors PG's automatic `_<name>` array type that
    /// gets created for every base/composite/domain type.
    pub fn array_type_of(&self, element_oid: u32) -> Option<u32> {
        self.types.values().find_map(|t| match t.kind {
            TypeKind::Array { element_type_oid } if element_type_oid == element_oid => Some(t.oid),
            _ => None,
        })
    }

    /// Unwrap domains to find the base type OID.
    pub fn unwrap_domain(&self, oid: u32) -> u32 {
        let mut current = oid;
        for _ in 0..32 {
            match self.types.get(&current) {
                Some(TypeEntry {
                    kind: TypeKind::Domain { base_type_oid },
                    ..
                }) => current = *base_type_oid,
                _ => break,
            }
        }
        current
    }

    /// The preferred type of a given `pg_type.typcategory`. Used when the
    /// analyzer needs to pick a concrete type for an expression whose inputs
    /// are all UNKNOWN (string-category literals default to `text`, numeric
    /// literals to `numeric`, etc., because those are the preferred types in
    /// their categories).
    pub fn preferred_type_in_category(&self, category: char) -> Option<u32> {
        self.types
            .values()
            .find(|t| t.category == category && t.is_preferred)
            .map(|t| t.oid)
    }

    /// Check if an implicit cast exists from `source` to `target`.
    pub fn has_implicit_cast(&self, source: u32, target: u32) -> bool {
        if source == target {
            return true;
        }
        let key = format!("{source}:{target}");
        matches!(
            self.casts.get(&key),
            Some(CastInfo {
                context: CastContext::Implicit,
                ..
            })
        )
    }

    /// Check if `source` is binary-coercible to `target` — the PG rule that
    /// lets `ALTER COLUMN TYPE` skip a table rewrite and keep dependent views
    /// intact. See `src/backend/parser/parse_coerce.c:IsBinaryCoercible`.
    ///
    /// True when:
    /// - `source == target`
    /// - `source` is a domain whose base type is `target` (unwrap one level)
    /// - `pg_cast` has an implicit, binary-method entry from `source` to `target`
    pub fn is_binary_coercible(&self, source: u32, target: u32) -> bool {
        if source == target {
            return true;
        }
        if let Some(TypeEntry {
            kind: TypeKind::Domain { base_type_oid },
            ..
        }) = self.get_type(source)
            && *base_type_oid == target
        {
            return true;
        }
        let key = format!("{source}:{target}");
        matches!(
            self.casts.get(&key),
            Some(CastInfo {
                context: CastContext::Implicit,
                method: CastMethod::Binary,
            })
        )
    }

    /// Find all functions matching a name, searching the `search_path`.
    ///
    /// When `schema` is `Some`, only overloads in that schema are returned.
    /// When `schema` is `None`, overloads from every schema on the
    /// `search_path` (plus `pg_catalog` if not explicitly listed) are
    /// concatenated; downstream type resolution picks the best match.
    pub fn find_functions(&self, schema: Option<&str>, name: &str) -> Vec<&FunctionEntry> {
        if let Some(s) = schema {
            return self
                .functions_by_name
                .get(&QualifiedName::new(s, name))
                .map(|v| v.iter().collect())
                .unwrap_or_default();
        }
        let mut out = Vec::new();
        if !self.search_path.iter().any(|s| s == "pg_catalog")
            && let Some(entries) = self
                .functions_by_name
                .get(&QualifiedName::new("pg_catalog", name))
        {
            out.extend(entries.iter());
        }
        for s in &self.search_path {
            if let Some(entries) = self.functions_by_name.get(&QualifiedName::new(s, name)) {
                out.extend(entries.iter());
            }
        }
        out
    }

    /// Find operators matching name and operand types.
    ///
    /// Implements the PostgreSQL §10.2 operator type resolution algorithm:
    ///   1. Exact match
    ///   2. Match via implicit casts
    ///   3. UNKNOWN-aware resolution with preferred-type disambiguation
    ///
    /// Candidates are gathered from every schema on the `search_path` (plus
    /// `pg_catalog` if not listed explicitly).
    pub fn find_operator(
        &self,
        name: &str,
        left_oid: Option<u32>,
        right_oid: u32,
    ) -> Option<ResolvedOperator> {
        let mut candidate_buf: Vec<&OperatorEntry> = Vec::new();
        if !self.search_path.iter().any(|s| s == "pg_catalog")
            && let Some(entries) = self
                .operators_by_name
                .get(&QualifiedName::new("pg_catalog", name))
        {
            candidate_buf.extend(entries.iter());
        }
        for s in &self.search_path {
            if let Some(entries) = self.operators_by_name.get(&QualifiedName::new(s, name)) {
                candidate_buf.extend(entries.iter());
            }
        }
        if candidate_buf.is_empty() {
            return None;
        }
        let candidates = &candidate_buf;

        // PG §10.2 step 3b: unwrap domain types to their base types.
        let left_oid = left_oid.map(|oid| self.unwrap_domain(oid));
        let right_oid = self.unwrap_domain(right_oid);

        // Step 1: exact match.
        if let Some(op) = candidates
            .iter()
            .find(|o| o.left_type_oid == left_oid && o.right_type_oid == right_oid)
        {
            return Some(concretize_operator(op, left_oid, right_oid, self));
        }

        // Step 2: match via implicit casts (non-UNKNOWN operands only). More
        // than one candidate can match — PG §10.2 step 3c resolves the tie
        // by keeping those with the most exact matches on input types (so
        // `numeric + int4` picks `numeric + numeric` over `float4 + float4`,
        // both reachable via implicit cast from numeric/int4).
        let cast_matches: Vec<&OperatorEntry> = candidates
            .iter()
            .filter(|o| {
                let left_ok = match (o.left_type_oid, left_oid) {
                    (Some(expected), Some(actual)) => {
                        actual == expected || self.has_implicit_cast(actual, expected)
                    }
                    (None, None) => true,
                    _ => false,
                };
                let right_ok = o.right_type_oid == right_oid
                    || self.has_implicit_cast(right_oid, o.right_type_oid);
                left_ok && right_ok
            })
            .copied()
            .collect();
        if !cast_matches.is_empty() {
            let exact_score = |o: &&OperatorEntry| -> u8 {
                let left_exact = match (o.left_type_oid, left_oid) {
                    (Some(e), Some(a)) => (e == a) as u8,
                    (None, None) => 1,
                    _ => 0,
                };
                let right_exact = (o.right_type_oid == right_oid) as u8;
                left_exact + right_exact
            };
            let max_score = cast_matches.iter().map(exact_score).max().unwrap();
            let best = cast_matches
                .iter()
                .find(|o| exact_score(o) == max_score)
                .copied()
                .unwrap();
            return Some(concretize_operator(best, left_oid, right_oid, self));
        }

        // Step 2b: polymorphic match. Operators declared over pseudo-types
        // (`anyarray || anyarray`, `anycompatible || anycompatiblearray`, …)
        // never appear as exact matches — PG resolves them by checking the
        // shape of the concrete operands against the pseudo-type's
        // constraint, then substitutes the bound types into the result.
        //
        // We narrow candidates to exactly one polymorphic match; if the
        // catalog has more than one (e.g. `anycompatible || anycompatiblearray`
        // vs `anycompatiblearray || anycompatible`), we rely on the actual
        // array-vs-element shape of the operands to pick the single right one.
        let poly_matches: Vec<&OperatorEntry> = candidates
            .iter()
            .filter(|o| {
                let left_ok = match (o.left_type_oid, left_oid) {
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
                let right_ok = if crate::functions::is_polymorphic(o.right_type_oid) {
                    crate::functions::matches_polymorphic(o.right_type_oid, right_oid, self)
                } else {
                    o.right_type_oid == right_oid
                        || self.has_implicit_cast(right_oid, o.right_type_oid)
                };
                let has_any_poly = o
                    .left_type_oid
                    .is_some_and(crate::functions::is_polymorphic)
                    || crate::functions::is_polymorphic(o.right_type_oid);
                has_any_poly && left_ok && right_ok
            })
            .copied()
            .collect();
        // PG tie-break: among polymorphic candidates, pick the most specific
        // signature. Sum the per-position specificity and keep only
        // candidates that tie at the maximum.
        if !poly_matches.is_empty() {
            let score = |o: &&OperatorEntry| -> u16 {
                let l = o
                    .left_type_oid
                    .map(crate::functions::polymorphic_specificity)
                    .unwrap_or(10) as u16;
                let r = crate::functions::polymorphic_specificity(o.right_type_oid) as u16;
                l + r
            };
            let max_score = poly_matches.iter().map(&score).max().unwrap();
            let best: Vec<&OperatorEntry> = poly_matches
                .iter()
                .filter(|o| score(o) == max_score)
                .copied()
                .collect();
            if best.len() == 1 {
                return Some(concretize_operator(best[0], left_oid, right_oid, self));
            }
        }

        // Step 3 (PG §10.2 step 3): UNKNOWN-aware resolution.
        let left_unknown = left_oid == Some(oid::UNKNOWN);
        let right_unknown = right_oid == oid::UNKNOWN;
        if !left_unknown && !right_unknown {
            return None;
        }

        // 3a. Keep candidates where known sides match (exact or implicit cast)
        //     and UNKNOWN sides are treated as compatible with anything.
        let mut remaining: Vec<&OperatorEntry> = candidates
            .iter()
            .filter(|o| {
                let left_ok = match (o.left_type_oid, left_oid) {
                    (Some(_), Some(actual)) if actual == oid::UNKNOWN => true,
                    (Some(expected), Some(actual)) => self.has_implicit_cast(actual, expected),
                    (None, None) => true,
                    _ => false,
                };
                let right_ok = right_unknown || self.has_implicit_cast(right_oid, o.right_type_oid);
                left_ok && right_ok
            })
            .copied()
            .collect();

        if remaining.len() <= 1 {
            return remaining
                .into_iter()
                .next()
                .map(|o| concretize_operator(o, left_oid, right_oid, self));
        }

        // 3b. If one side is known, keep only candidates that accept exactly
        //     that type on the known side (narrows from implicit-cast matches).
        if !left_unknown {
            let exact: Vec<&OperatorEntry> = remaining
                .iter()
                .filter(|o| o.left_type_oid == left_oid)
                .copied()
                .collect();
            if !exact.is_empty() {
                remaining = exact;
            }
        }
        if !right_unknown {
            let exact: Vec<&OperatorEntry> = remaining
                .iter()
                .filter(|o| o.right_type_oid == right_oid)
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
                .map(|o| concretize_operator(o, left_oid, right_oid, self));
        }

        // 3c (PG §10.2 step 3e-f). For each UNKNOWN position, check if all
        //     remaining candidates agree on the type category.  If so, prefer
        //     the candidate that uses the *preferred* type in that category.
        //     This mirrors PostgreSQL's "resolve to preferred type" rule.
        if left_unknown {
            let preferred = self.prefer_by_category(&remaining, |o| o.left_type_oid);
            if !preferred.is_empty() {
                remaining = preferred;
            }
        }
        if remaining.len() > 1 && right_unknown {
            let preferred = self.prefer_by_category(&remaining, |o| Some(o.right_type_oid));
            if !preferred.is_empty() {
                remaining = preferred;
            }
        }

        if remaining.len() == 1 {
            return Some(concretize_operator(remaining[0], left_oid, right_oid, self));
        }

        // 3d. Final fallback: resolve UNKNOWN positions to `text`, since
        //     string constants default to text in PostgreSQL.  Prefer an
        //     exact match on the substituted types; fall back to candidates
        //     reachable via implicit cast only if no exact match exists.
        let text_oid = oid::TEXT;
        let resolved_left = if left_unknown {
            Some(text_oid)
        } else {
            left_oid
        };
        let resolved_right = if right_unknown { text_oid } else { right_oid };

        let exact_matches: Vec<&OperatorEntry> = remaining
            .iter()
            .filter(|o| o.left_type_oid == resolved_left && o.right_type_oid == resolved_right)
            .copied()
            .collect();
        if exact_matches.len() == 1 {
            return Some(concretize_operator(
                exact_matches[0],
                resolved_left,
                resolved_right,
                self,
            ));
        }

        let text_matches: Vec<&OperatorEntry> = remaining
            .iter()
            .filter(|o| {
                let left_ok = match (o.left_type_oid, resolved_left) {
                    (Some(expected), Some(actual)) => {
                        expected == actual || self.has_implicit_cast(actual, expected)
                    }
                    (None, None) => true,
                    _ => false,
                };
                let right_ok = o.right_type_oid == resolved_right
                    || self.has_implicit_cast(resolved_right, o.right_type_oid);
                left_ok && right_ok
            })
            .copied()
            .collect();
        if text_matches.len() == 1 {
            return Some(concretize_operator(
                text_matches[0],
                resolved_left,
                resolved_right,
                self,
            ));
        }

        // Truly ambiguous — return None so callers can use fallback logic.
        None
    }

    /// Among `candidates`, narrow to those whose type at the position extracted
    /// by `get_oid` is the *preferred* type in its category — but only when all
    /// candidates agree on the same category for that position (PG §10.2 step 3f).
    fn prefer_by_category<'a>(
        &self,
        candidates: &[&'a OperatorEntry],
        get_oid: impl Fn(&OperatorEntry) -> Option<u32>,
    ) -> Vec<&'a OperatorEntry> {
        // Collect categories for this position.
        let cats: Vec<Option<char>> = candidates
            .iter()
            .map(|o| {
                get_oid(o)
                    .and_then(|id| self.types.get(&id))
                    .map(|t| t.category)
            })
            .collect();

        // All must agree on one category.
        let first = match cats.first() {
            Some(Some(c)) => *c,
            _ => return Vec::new(),
        };
        if !cats.iter().all(|c| *c == Some(first)) {
            return Vec::new();
        }

        // Keep only candidates using the preferred type in that category.
        let preferred: Vec<&'a OperatorEntry> = candidates
            .iter()
            .filter(|o| {
                get_oid(o)
                    .and_then(|id| self.types.get(&id))
                    .is_some_and(|t| t.is_preferred)
            })
            .copied()
            .collect();
        preferred
    }
}

/// Turn an [`OperatorEntry`] — which may declare polymorphic pseudo-types on
/// its operands and result — into a [`ResolvedOperator`] whose OIDs are
/// already substituted with the concrete types derived from the caller's
/// operands. For non-polymorphic operators the result just mirrors the
/// entry's declared OIDs.
fn concretize_operator(
    op: &OperatorEntry,
    left_actual: Option<u32>,
    right_actual: u32,
    db: &PgCatalog,
) -> ResolvedOperator {
    let mut bound_element: Option<u32> = None;
    let mut bound_array: Option<u32> = None;
    if let (Some(expected_l), Some(actual_l)) = (op.left_type_oid, left_actual)
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
    if crate::functions::is_polymorphic(op.right_type_oid) {
        crate::functions::bind_polymorphic_from(
            op.right_type_oid,
            right_actual,
            db,
            &mut bound_element,
            &mut bound_array,
        );
    }
    ResolvedOperator {
        left_type_oid: op
            .left_type_oid
            .map(|o| crate::functions::substitute_polymorphic(o, bound_element, bound_array, db)),
        right_type_oid: crate::functions::substitute_polymorphic(
            op.right_type_oid,
            bound_element,
            bound_array,
            db,
        ),
        result_type_oid: crate::functions::substitute_polymorphic(
            op.result_type_oid,
            bound_element,
            bound_array,
            db,
        ),
    }
}
