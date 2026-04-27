//! Schema data model.
//!
//! These types describe the contents of a PostgreSQL catalog (types, tables,
//! functions, operators, casts) sufficient for static type inference. They
//! live as fields directly on [`crate::pg_catalog::PgCatalog`] and as the
//! seed DTO in [`crate::seed`].

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

/// A field of a registered composite type or an OUT/TABLE arg of a function.
/// Pure schema-level data: just a name, OID and NOT NULL bit, identical to
/// what `pg_attribute` stores for the composite's row type.
///
/// Anonymous records flowing through expressions use [`crate::expr::RecordField`]
/// instead — that one carries an [`crate::expr::ExprType`] per element so it
/// can describe nested rows recursively. Use [`CompositeField::lift`] to bridge
/// from this schema form to the expression form when consuming a composite
/// type inside an expression.
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
    /// `GENERATED ALWAYS AS (...) STORED` columns reject any value other
    /// than `DEFAULT` in INSERT/UPDATE. Mirrors PG `pg_attribute.attgenerated`.
    #[serde(default)]
    pub is_generated: bool,
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

    /// Number of trailing parameters that have a `DEFAULT` in PG (mirrors
    /// `pg_proc.pronargdefaults`). When > 0, the function can be called
    /// with fewer arguments than `arg_types.len()` — the missing
    /// trailing args are filled by PG's default expressions. The
    /// analyzer doesn't evaluate the defaults but it must accept the
    /// shorter call form so overload resolution doesn't reject e.g.
    /// `jsonb_set(jsonb, text[], jsonb)` (3 args), which PG dispatches
    /// to the 4-arg signature with `create_if_missing := true`.
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub num_default_args: u8,
}

fn is_zero_u8(n: &u8) -> bool {
    *n == 0
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

// Lookup methods for these types live on [`crate::pg_catalog::PgCatalog`].
