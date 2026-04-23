//! Schema snapshot data model.
//!
//! A [`SchemaSnapshot`] captures the complete type system of a PostgreSQL
//! database at a point in time (post-migration). It is exported once from
//! a live database and then used for offline static analysis.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::qualified_name::QualifiedName;

/// Well-known OIDs for builtin PostgreSQL types. Used by the analyzer to
/// drive coercion, operator resolution, and pseudo-type detection.
pub(crate) mod oid {
    pub const BOOL: u32 = 16;
    pub const BYTEA: u32 = 17;
    pub const NAME: u32 = 19;
    pub const INT8: u32 = 20;
    pub const INT2: u32 = 21;
    pub const INT4: u32 = 23;
    pub const TEXT: u32 = 25;
    pub const FLOAT4: u32 = 700;
    pub const FLOAT8: u32 = 701;
    pub const UNKNOWN: u32 = 705;
    pub const BPCHAR: u32 = 1042;
    pub const VARCHAR: u32 = 1043;
    pub const NUMERIC: u32 = 1700;
    pub const RECORD: u32 = 2249;
}

// ──────────────────────────────────────────────────────────────────────────────
// Top-level snapshot
// ──────────────────────────────────────────────────────────────────────────────

/// Complete snapshot of a PostgreSQL schema sufficient for static type inference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaSnapshot {
    /// All types indexed by OID.
    pub types: HashMap<u32, TypeEntry>,
    /// Type name lookup keyed by the schema-qualified type name.
    pub type_by_name: HashMap<QualifiedName, u32>,
    /// All tables and views, keyed by their schema-qualified name.
    pub tables: HashMap<QualifiedName, TableEntry>,
    /// Functions indexed by their schema-qualified name. Each key maps to
    /// the list of overloads (by argument types) defined under that name.
    pub functions_by_name: HashMap<QualifiedName, Vec<FunctionEntry>>,
    /// Operators indexed by their schema-qualified name. Each key maps to
    /// the list of overloads (by operand types) defined under that name.
    pub operators_by_name: HashMap<QualifiedName, Vec<OperatorEntry>>,
    /// Cast rules: `"source_oid:target_oid"` → cast info for O(1) lookup.
    pub casts: HashMap<String, CastInfo>,
    /// Current `search_path` schemas, in order.
    pub search_path: Vec<String>,
    /// Set of all schema names known to the snapshot. Populated by
    /// `CREATE SCHEMA` and seeded from `search_path`; used by `DROP SCHEMA`
    /// to detect the "empty but exists" case.
    #[serde(default)]
    pub schemas: std::collections::HashSet<String>,
}

// ──────────────────────────────────────────────────────────────────────────────
// Types
// ──────────────────────────────────────────────────────────────────────────────

/// A PostgreSQL type from `pg_type`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeEntry {
    pub oid: u32,
    pub name: String,
    pub schema: String,
    pub kind: TypeKind,
    /// Type category from `pg_type.typcategory` (e.g. 'S' for string, 'N' for
    /// numeric).  Used during operator/function resolution when operands are
    /// `UNKNOWN` — see PostgreSQL §10.2 step 3.e.
    #[serde(default = "default_category")]
    pub category: char,
    /// Whether this type is *preferred* in its category (`pg_type.typispreferred`).
    #[serde(default)]
    pub is_preferred: bool,
    /// Name of the extension that created this type, if any. Set during
    /// `CREATE EXTENSION` so the Rust type mapper can route extension types
    /// (e.g. pgvector's `vector`) to crate-specific Rust types like
    /// `pgvector::Vector`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension: Option<String>,
}

fn default_category() -> char {
    'U'
}

/// The category of a PostgreSQL type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TypeKind {
    /// A built-in or extension base type (int4, text, uuid, etc.).
    Base,
    /// A domain type wrapping another type.
    Domain { base_type_oid: u32 },
    /// An array type.
    Array { element_type_oid: u32 },
    /// An enum type with its labels in order.
    Enum { labels: Vec<String> },
    /// A composite (row) type with named fields.
    Composite { fields: Vec<CompositeField> },
    /// A range type over a subtype.
    Range { subtype_oid: u32 },
    /// A pseudo-type (void, trigger, record, any, etc.).
    Pseudo,
}

/// A field in a composite type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositeField {
    pub name: String,
    pub type_oid: u32,
    pub not_null: bool,
}

// ──────────────────────────────────────────────────────────────────────────────
// Tables and views
// ──────────────────────────────────────────────────────────────────────────────

/// A table or view from `pg_class`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableEntry {
    pub name: String,
    pub schema: String,
    pub kind: RelationKind,
    pub columns: Vec<TableColumn>,
    /// For views: the definition needed to recreate the view after CASCADE.
    /// Contains the deparsed SQL query and column dependencies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_def: Option<ViewDef>,
}

/// View dependency tracking for CASCADE behavior.
///
/// Views never get automatically recreated — if a dependent column is altered
/// or dropped with CASCADE, the view is simply dropped (matching PostgreSQL).
/// The user must DROP + CREATE VIEW manually in their migration.
///
/// References are stored by [`QualifiedName`], so any `ALTER TABLE ... RENAME`,
/// `ALTER TABLE ... SET SCHEMA`, or `ALTER SCHEMA ... RENAME` must update
/// these fields to keep dependencies pointing at the right target (see
/// `crate::ddl::views::rewrite_deps_*` helpers).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewDef {
    /// Tables/views this view reads from.
    pub depends_on_tables: Vec<QualifiedName>,
    /// Specific columns this view depends on: `(table, column_name)`.
    pub depends_on_columns: Vec<(QualifiedName, String)>,
    /// Serialized `pg_query::protobuf::Node` of the view's SELECT. All
    /// `RangeVar`/`ColumnRef`/`TypeName`/`FuncCall`/operator references are
    /// already resolved to their current schema-qualified form at view
    /// creation time. This is the analog of PG's `pg_rewrite` Query tree —
    /// lets RENAME handlers rewrite references in place and lets
    /// `ALTER COLUMN TYPE` re-analyze the view without the original SQL.
    ///
    /// Stored as protobuf bytes; `serde_base64` keeps the JSON snapshot
    /// compact instead of emitting a giant `[int, int, …]` array.
    #[serde(default, with = "serde_base64", skip_serializing_if = "Vec::is_empty")]
    pub resolved_ast: Vec<u8>,
}

/// Serde adapter that encodes `Vec<u8>` as a base64 string in JSON while
/// staying a plain byte buffer in memory.
pub(crate) mod serde_base64 {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(de)?;
        STANDARD.decode(&s).map_err(serde::de::Error::custom)
    }
}

/// The kind of relation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelationKind {
    Table,
    View,
    MaterializedView,
    Partitioned,
}

/// A column in a table or view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableColumn {
    pub name: String,
    pub type_oid: u32,
    pub not_null: bool,
    pub has_default: bool,
}

// ──────────────────────────────────────────────────────────────────────────────
// Functions
// ──────────────────────────────────────────────────────────────────────────────

/// A function or aggregate from `pg_proc`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionEntry {
    pub name: String,
    pub schema: String,
    pub arg_types: Vec<u32>,
    pub return_type_oid: u32,
    pub is_aggregate: bool,
    pub is_window: bool,
    pub is_variadic: bool,
    pub is_set_returning: bool,
    /// For strict functions: returns NULL if any input is NULL.
    pub is_strict: bool,
    /// `true` for `CREATE PROCEDURE`. Procedures share storage with functions
    /// but cannot appear in query expressions (only `CALL`), so the analyzer
    /// uses this flag to filter them out of expression-level function lookups.
    #[serde(default)]
    pub is_procedure: bool,
    /// For aggregates: the actual return type (may differ from prorettype
    /// when a finalfn transforms the transition state).
    pub agg_final_type_oid: Option<u32>,
    /// Output columns for SRFs with TABLE args (e.g. `pg_options_to_table`
    /// returns `TABLE(option_name text, option_value text)`) or for functions
    /// declared with OUT/INOUT args. Empty for functions that just return a
    /// scalar or a registered composite. Populated from `pg_proc.proargnames`
    /// + `pg_proc.proargmodes` (modes 'o', 'b', 't').
    ///
    /// This is what lets `(pg_options_to_table(x)).option_name` resolve: the
    /// outer `record`/`setof record` return type is opaque, but `out_args`
    /// carries the real named columns PG would expand into at runtime.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub out_args: Vec<CompositeField>,
}

// ──────────────────────────────────────────────────────────────────────────────
// Operators
// ──────────────────────────────────────────────────────────────────────────────

/// An operator from `pg_operator`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorEntry {
    pub name: String,
    /// Left operand type OID. `None` for prefix operators.
    pub left_type_oid: Option<u32>,
    /// Right operand type OID.
    pub right_type_oid: u32,
    /// Result type OID.
    pub result_type_oid: u32,
}

/// Result of operator resolution: the operand and result OIDs with any
/// polymorphic pseudo-types (`anyelement`, `anycompatiblearray`, …)
/// already substituted by the concrete types of the arguments.
#[derive(Debug, Clone)]
pub struct ResolvedOperator {
    pub left_type_oid: Option<u32>,
    pub right_type_oid: u32,
    pub result_type_oid: u32,
}

// ──────────────────────────────────────────────────────────────────────────────
// Casts
// ──────────────────────────────────────────────────────────────────────────────

/// When a cast is allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CastContext {
    /// Cast is applied implicitly (e.g., int4 → int8 in expressions).
    Implicit,
    /// Cast is applied in assignment context (e.g., INSERT target column).
    Assignment,
    /// Cast requires explicit `::type` or `CAST(... AS type)`.
    Explicit,
}

/// How a cast is physically performed. Mirrors `pg_cast.castmethod`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CastMethod {
    /// `'f'` — cast uses a conversion function from `pg_proc`.
    Function,
    /// `'b'` — binary coercible: source and target share the same
    /// in-memory representation, so no conversion is needed. This is what
    /// lets `ALTER COLUMN TYPE` skip a table rewrite (and by extension
    /// keeps dependent views valid without re-analysis).
    Binary,
    /// `'i'` — cast goes through the I/O (text) representation.
    InOut,
}

fn default_cast_method() -> CastMethod {
    CastMethod::Function
}

/// Combined cast information from `pg_cast`. Keeps `context` (when the cast
/// fires) and `method` (how it's executed) in one place.
///
/// Serialization accepts two shapes to keep older snapshot JSONs loadable:
/// - New: `{ "context": "Implicit", "method": "Binary" }`
/// - Legacy: `"Implicit"` (just the context; method defaults to `Function`)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CastInfo {
    pub context: CastContext,
    #[serde(default = "default_cast_method")]
    pub method: CastMethod,
}

impl<'de> Deserialize<'de> for CastInfo {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Form {
            Full {
                context: CastContext,
                #[serde(default = "default_cast_method")]
                method: CastMethod,
            },
            Legacy(CastContext),
        }
        Ok(match Form::deserialize(deserializer)? {
            Form::Full { context, method } => CastInfo { context, method },
            Form::Legacy(context) => CastInfo {
                context,
                method: CastMethod::Function,
            },
        })
    }
}

impl CastInfo {
    pub fn new(context: CastContext, method: CastMethod) -> Self {
        Self { context, method }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Lookup methods
// ──────────────────────────────────────────────────────────────────────────────

impl SchemaSnapshot {
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
        use self::oid;

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
    snapshot: &SchemaSnapshot,
) -> ResolvedOperator {
    let mut bound_element: Option<u32> = None;
    let mut bound_array: Option<u32> = None;
    if let (Some(expected_l), Some(actual_l)) = (op.left_type_oid, left_actual)
        && crate::functions::is_polymorphic(expected_l)
    {
        crate::functions::bind_polymorphic_from(
            expected_l,
            actual_l,
            snapshot,
            &mut bound_element,
            &mut bound_array,
        );
    }
    if crate::functions::is_polymorphic(op.right_type_oid) {
        crate::functions::bind_polymorphic_from(
            op.right_type_oid,
            right_actual,
            snapshot,
            &mut bound_element,
            &mut bound_array,
        );
    }
    ResolvedOperator {
        left_type_oid: op.left_type_oid.map(|o| {
            crate::functions::substitute_polymorphic(o, bound_element, bound_array, snapshot)
        }),
        right_type_oid: crate::functions::substitute_polymorphic(
            op.right_type_oid,
            bound_element,
            bound_array,
            snapshot,
        ),
        result_type_oid: crate::functions::substitute_polymorphic(
            op.result_type_oid,
            bound_element,
            bound_array,
            snapshot,
        ),
    }
}
