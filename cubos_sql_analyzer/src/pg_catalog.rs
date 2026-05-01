//! In-memory mirror of PostgreSQL's `pg_catalog` schema.
//!
//! Each table here corresponds 1:1 to a real `pg_catalog` table — same name,
//! same column names (literal PG identifiers like `typname`, `relkind`,
//! `prokind`, …), same FK semantics. Schemas are referenced by OID through
//! `pg_namespace`; views' SELECT ASTs and extension membership are tracked
//! through `pg_class.relviewdef` and `pg_depend` rather than ad-hoc fields on
//! the schema entries.
//!
//! On top of the tables, [`PgCatalog`] keeps a handful of name-keyed
//! HashMap indexes that are rebuilt from the rows in [`PgCatalog::from_seed`]
//! and kept in sync by the DDL handlers via the `insert_pg_*` /
//! `remove_pg_*` helpers.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_tuple::{Deserialize_tuple, Serialize_tuple};

use crate::ddl::{DdlError, apply_sql_to};
use crate::error::AnalyzeError;
use crate::lexer::lex;
use crate::oid::{
    PgCastOid, PgClassOid, PgConstraintOid, PgEnumOid, PgExtensionOid, PgGenericOid,
    PgNamespaceOid, PgOperatorOid, PgProcOid, PgRewriteOid, PgTypeOid,
};
use crate::resolve::{AnalyzedQuery, analyze_static, build_spread_sample_sql, fuse};
use crate::seed::load_seed;

// ─── Built-in type OIDs ────────────────────────────────────────────────────
//
// Stable OIDs PG hard-codes for builtin types. Used by the analyzer to drive
// coercion, operator resolution, and pseudo-type detection.

pub(crate) mod oid {
    use crate::oid::PgTypeOid;

    pub const BOOL: PgTypeOid = PgTypeOid::from_raw(16);
    pub const BYTEA: PgTypeOid = PgTypeOid::from_raw(17);
    pub const NAME: PgTypeOid = PgTypeOid::from_raw(19);
    pub const INT8: PgTypeOid = PgTypeOid::from_raw(20);
    pub const INT2: PgTypeOid = PgTypeOid::from_raw(21);
    pub const INT4: PgTypeOid = PgTypeOid::from_raw(23);
    pub const TEXT: PgTypeOid = PgTypeOid::from_raw(25);
    pub const FLOAT4: PgTypeOid = PgTypeOid::from_raw(700);
    pub const FLOAT8: PgTypeOid = PgTypeOid::from_raw(701);
    pub const UNKNOWN: PgTypeOid = PgTypeOid::from_raw(705);
    pub const BPCHAR: PgTypeOid = PgTypeOid::from_raw(1042);
    pub const VARCHAR: PgTypeOid = PgTypeOid::from_raw(1043);
    pub const NUMERIC: PgTypeOid = PgTypeOid::from_raw(1700);
    pub const RECORD: PgTypeOid = PgTypeOid::from_raw(2249);
}

// ─── Catalog table OIDs ────────────────────────────────────────────────────
//
// Stable OIDs PG hard-codes for the catalog tables themselves. Used as
// `classid` / `refclassid` in `pg_depend` rows so that, e.g., a view's
// dependency on a table reads as `classid = PG_CLASS_RELID, objid = view_oid,
// refclassid = PG_CLASS_RELID, refobjid = table_oid`.

pub const PG_NAMESPACE_RELID: PgClassOid = PgClassOid::from_raw(2615);
pub const PG_TYPE_RELID: PgClassOid = PgClassOid::from_raw(1247);
pub const PG_CLASS_RELID: PgClassOid = PgClassOid::from_raw(1259);
pub const PG_PROC_RELID: PgClassOid = PgClassOid::from_raw(1255);
pub const PG_OPERATOR_RELID: PgClassOid = PgClassOid::from_raw(2617);
pub const PG_CAST_RELID: PgClassOid = PgClassOid::from_raw(2605);
pub const PG_EXTENSION_RELID: PgClassOid = PgClassOid::from_raw(3079);

// ─── Enums ─────────────────────────────────────────────────────────────────

/// `pg_type.typtype`. PG chars: `b` base, `c` composite, `d` domain, `e` enum,
/// `p` pseudo, `r` range, `m` multirange. Serialized as the bare PG char.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypType {
    #[serde(rename = "b")]
    Base,
    #[serde(rename = "c")]
    Composite,
    #[serde(rename = "d")]
    Domain,
    #[serde(rename = "e")]
    Enum,
    #[serde(rename = "p")]
    Pseudo,
    #[serde(rename = "r")]
    Range,
    #[serde(rename = "m")]
    Multirange,
}

/// `pg_type.typcategory`. PG chars: A array, B boolean, C composite, D
/// date/time, E enum, G geometric, I network, N numeric, P pseudo, R range, S
/// string, T timespan, U user-defined, V bit-string, X unknown, Z internal.
/// Serialized as the bare PG char.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypCategory {
    #[serde(rename = "A")]
    Array,
    #[serde(rename = "B")]
    Boolean,
    #[serde(rename = "C")]
    Composite,
    #[serde(rename = "D")]
    DateTime,
    #[serde(rename = "E")]
    Enum,
    #[serde(rename = "G")]
    Geometric,
    #[serde(rename = "I")]
    Network,
    #[serde(rename = "N")]
    Numeric,
    #[serde(rename = "P")]
    Pseudo,
    #[serde(rename = "R")]
    Range,
    #[serde(rename = "S")]
    String,
    #[serde(rename = "T")]
    Timespan,
    #[serde(rename = "U")]
    UserDefined,
    #[serde(rename = "V")]
    BitString,
    #[serde(rename = "X")]
    Unknown,
    #[serde(rename = "Z")]
    Internal,
}

/// `pg_class.relkind`. Carries every variant PG emits, even the ones the
/// analyzer doesn't consult — keeping them lets the seed mirror `pg_class`
/// 1:1 without dropping rows. PG chars: `r` table, `i` index, `S` sequence,
/// `t` TOAST table, `v` view, `m` matview, `c` composite type, `f` foreign
/// table, `p` partitioned table, `I` partitioned index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelKind {
    #[serde(rename = "r")]
    Table,
    #[serde(rename = "i")]
    Index,
    #[serde(rename = "S")]
    Sequence,
    #[serde(rename = "t")]
    ToastTable,
    #[serde(rename = "v")]
    View,
    #[serde(rename = "m")]
    MaterializedView,
    #[serde(rename = "c")]
    CompositeType,
    #[serde(rename = "f")]
    ForeignTable,
    #[serde(rename = "p")]
    Partitioned,
    #[serde(rename = "I")]
    PartitionedIndex,
}

/// `pg_proc.prokind`. PG chars: `f` normal function, `a` aggregate, `w`
/// window, `p` procedure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProKind {
    #[serde(rename = "f")]
    Function,
    #[serde(rename = "a")]
    Aggregate,
    #[serde(rename = "w")]
    Window,
    #[serde(rename = "p")]
    Procedure,
}

/// `pg_proc.provolatile`. PG chars: `i` immutable (deterministic, no
/// side-effects, same input → same output forever), `s` stable (same
/// result within a single statement; depends on session/snapshot state
/// like `now()`), `v` volatile (may produce different results within one
/// statement; default for user-defined functions). The analyzer consults
/// this in DDL contexts that require IMMUTABLE callees: CHECK
/// constraints, `GENERATED ... STORED` expressions, and index
/// expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProVolatile {
    #[serde(rename = "i")]
    Immutable,
    #[serde(rename = "s")]
    Stable,
    #[serde(rename = "v")]
    Volatile,
}

/// `pg_proc.proargmodes` element. PG chars: `i` IN, `o` OUT, `b` INOUT, `v`
/// VARIADIC, `t` TABLE.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArgMode {
    #[serde(rename = "i")]
    In,
    #[serde(rename = "o")]
    Out,
    #[serde(rename = "b")]
    InOut,
    #[serde(rename = "v")]
    Variadic,
    #[serde(rename = "t")]
    Table,
}

/// `pg_attribute.attgenerated`. PG stores `\0` for "not generated", `s` for
/// STORED, `v` for VIRTUAL. The "not generated" case is modeled as
/// `Option::None` on the field, so this enum only carries the two real
/// variants. JSON form: `null` / `"s"` / `"v"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttGenerated {
    #[serde(rename = "s")]
    Stored,
    #[serde(rename = "v")]
    Virtual,
}

/// `pg_attribute.attidentity`. PG stores `\0` for "not an identity column",
/// `a` for `GENERATED ALWAYS AS IDENTITY`, `d` for `GENERATED BY DEFAULT AS
/// IDENTITY`. The "not an identity column" case is modeled as `Option::None`
/// on the field, so this enum only carries the two real variants. JSON form:
/// `null` / `"a"` / `"d"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttIdentity {
    #[serde(rename = "a")]
    Always,
    #[serde(rename = "d")]
    ByDefault,
}

/// `pg_cast.castcontext`. PG chars: `i` implicit, `a` assignment, `e`
/// explicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CastContext {
    #[serde(rename = "i")]
    Implicit,
    #[serde(rename = "a")]
    Assignment,
    #[serde(rename = "e")]
    Explicit,
}

/// `pg_cast.castmethod`. PG chars: `f` function, `b` binary, `i` I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CastMethod {
    #[serde(rename = "f")]
    Function,
    #[serde(rename = "b")]
    Binary,
    #[serde(rename = "i")]
    InOut,
}

/// `pg_rewrite.ev_type`. PG stores this as a single char: `'1'` SELECT
/// (used for views' implicit `_RETURN` rule), `'2'` UPDATE, `'3'` INSERT,
/// `'4'` DELETE.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvType {
    #[serde(rename = "1")]
    Select,
    #[serde(rename = "2")]
    Update,
    #[serde(rename = "3")]
    Insert,
    #[serde(rename = "4")]
    Delete,
}

/// `pg_rewrite.ev_enabled`. PG chars: `O` origin (default), `R` replica,
/// `A` always, `D` disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvEnabled {
    #[serde(rename = "O")]
    Origin,
    #[serde(rename = "R")]
    Replica,
    #[serde(rename = "A")]
    Always,
    #[serde(rename = "D")]
    Disabled,
}

/// `pg_depend.deptype`. PG chars used here: `n` normal, `a` auto, `i`
/// internal, `e` extension, `x` auto-extension, `p` pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DepType {
    #[serde(rename = "n")]
    Normal,
    #[serde(rename = "a")]
    Auto,
    #[serde(rename = "i")]
    Internal,
    #[serde(rename = "e")]
    Extension,
    #[serde(rename = "x")]
    AutoExtension,
    #[serde(rename = "p")]
    Pin,
}

// ─── Catalog rows ──────────────────────────────────────────────────────────

/// `pg_namespace`: a schema.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgNamespace {
    pub oid: PgNamespaceOid,
    pub nspname: String,
}

/// `pg_type`: a registered type.
///
/// Enum labels live in `pg_enum`, range subtype in `pg_range`, composite
/// fields in `pg_attribute` (keyed by `typrelid`), so this row is just
/// scalar metadata.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgType {
    pub oid: PgTypeOid,
    pub typname: String,
    /// FK `pg_namespace.oid`.
    pub typnamespace: PgNamespaceOid,
    pub typtype: TypType,
    pub typcategory: TypCategory,
    pub typispreferred: bool,
    /// FK `pg_class.oid` for composite types (the row type's class). `None`
    /// (serialized as `0`) otherwise.
    #[serde(with = "crate::oid::oid_or_zero")]
    pub typrelid: Option<PgClassOid>,
    /// FK `pg_type.oid` of the array's element type. `None` if not an array.
    #[serde(with = "crate::oid::oid_or_zero")]
    pub typelem: Option<PgTypeOid>,
    /// FK `pg_type.oid` of the canonical array type whose elements are this
    /// type. PG sets this on every base/composite/domain when it auto-creates
    /// the `_<name>` array; the legacy `oidvector` and `int2vector` have
    /// `typelem = oid/int2` but nobody points `typarray` at them, which is
    /// what disambiguates them from the real `_oid` / `_int2` arrays.
    #[serde(with = "crate::oid::oid_or_zero")]
    pub typarray: Option<PgTypeOid>,
    /// FK `pg_type.oid` of the domain's base type. `None` if not a domain.
    #[serde(with = "crate::oid::oid_or_zero")]
    pub typbasetype: Option<PgTypeOid>,
    /// Domain-level `NOT NULL`. `false` for every non-domain type. Mirrors
    /// `pg_type.typnotnull` so a column declared `colname mydomain` inherits
    /// non-nullness without an explicit column constraint.
    pub typnotnull: bool,
    /// Domain-level type modifier, mirroring `pg_type.typtypmod`. `Some`
    /// only for domains over a parametric base (`CREATE DOMAIN d AS
    /// varchar(20)`); `None` for everything else (PG's `-1`). Use
    /// [`crate::typmod`] to encode/decode.
    #[serde(with = "crate::oid::option_i32_neg_one")]
    pub typtypmod: Option<i32>,
}

/// `pg_enum`: one row per enum label.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgEnum {
    pub oid: PgEnumOid,
    /// FK `pg_type.oid`.
    pub enumtypid: PgTypeOid,
    pub enumsortorder: f32,
    pub enumlabel: String,
}

/// `pg_range`: subtype info for range types.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgRange {
    /// FK `pg_type.oid`.
    pub rngtypid: PgTypeOid,
    /// FK `pg_type.oid` of the subtype.
    pub rngsubtype: PgTypeOid,
}

/// `pg_constraint.contype`. PG chars: `c` check, `f` foreign key, `n` not
/// null (PG18+), `p` primary key, `u` unique, `t` constraint trigger, `x`
/// exclusion. We carry the four kinds the analyzer actually consults
/// (CHECK, FOREIGN KEY, PRIMARY KEY, UNIQUE) and map the rest to
/// `Other` so the row still round-trips through the seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConType {
    #[serde(rename = "c")]
    Check,
    #[serde(rename = "f")]
    ForeignKey,
    #[serde(rename = "p")]
    PrimaryKey,
    #[serde(rename = "u")]
    Unique,
    #[serde(rename = "x")]
    Exclusion,
    /// Catch-all for `n` (not-null), `t` (constraint trigger), and any
    /// future variants.
    #[serde(other)]
    Other,
}

/// `pg_constraint`: one row per integrity constraint.
///
/// We track the dimensions the analyzer consults: the constrained relation
/// (`conrelid`), the constraint kind, the columns it covers (`conkey`,
/// 1-based attnums), the human-visible name (`conname`, used by
/// `ON CONFLICT ON CONSTRAINT name`), and the FK target (`confrelid`,
/// `confkey`) so the dependency graph is reachable in either direction.
/// FK action codes (`confdeltype`, `confupdtype`) and `consrc` are not
/// modeled.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgConstraint {
    pub oid: PgConstraintOid,
    pub conname: String,
    /// FK `pg_class.oid` of the constrained relation.
    pub conrelid: PgClassOid,
    pub contype: ConType,
    /// Attnums (1-based) of the columns covered. Empty for table-level
    /// CHECK constraints with no direct column reference.
    pub conkey: Vec<i16>,
    /// FK `pg_class.oid` of the target relation. `None` (serialized as
    /// `0`) for non-FK constraints.
    #[serde(with = "crate::oid::oid_or_zero")]
    pub confrelid: Option<PgClassOid>,
    /// Attnums (1-based) of the columns on the target relation that the
    /// FK references. Empty for non-FK constraints.
    pub confkey: Vec<i16>,
}

/// `pg_index`: one row per index. Keyed by `indexrelid` — PG models the index
/// itself as a row in `pg_class` (`relkind = 'i'`) and the metadata about
/// what it's indexing here.
///
/// `indkey` mirrors PG: a list of attnums, one per indexed element. A `0`
/// means "this slot is an expression"; the expressions are walked from
/// `indexprs` in order. `indpred` carries the partial-index predicate.
/// Both expression and predicate share the [`SerializedAst`] shape so
/// dependency rewrites (RENAME / SET SCHEMA on an indexed column) flow
/// through the same applier the views use.
///
/// We don't model `indisvalid`, `indisready`, `indisclustered`, etc. — the
/// analyzer doesn't consult them and they would only inflate the seed.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgIndex {
    /// FK `pg_class.oid` of the index relation itself.
    pub indexrelid: PgClassOid,
    /// FK `pg_class.oid` of the indexed table.
    pub indrelid: PgClassOid,
    /// Total number of index columns + included columns.
    pub indnatts: i16,
    /// Number of *key* columns (excludes `INCLUDE (cols)` columns).
    pub indnkeyatts: i16,
    pub indisunique: bool,
    pub indisprimary: bool,
    /// Attnums of the indexed columns. `0` means the slot is an expression
    /// (consumed in order from `indexprs`).
    pub indkey: Vec<i16>,
    /// Per-expression ASTs for slots where `indkey[i] == 0`, in the same
    /// order. `Vec::new()` when no slot is an expression.
    ///
    /// Diverges from PG's literal shape (PG packs the whole list into one
    /// `pg_node_tree`); splitting per expression keeps each entry's
    /// bindings stream local to its own AST and makes per-expression
    /// reanalysis trivial.
    pub indexprs: Vec<SerializedAst>,
    /// Partial-index predicate. `None` for non-partial indexes.
    pub indpred: Option<SerializedAst>,
}

/// `pg_inherits`: one row per (child, parent) edge in the inheritance graph.
///
/// `CREATE TABLE child () INHERITS (p1, p2, …)` emits one row per parent.
/// `inhseqno` orders parents within a child so column merging happens
/// deterministically. The actual columns inherited from the parent live on
/// the child's own `pg_attribute` rows; this table just records the link
/// so cascade operations (`DROP COLUMN`, `RENAME`, etc.) can walk it.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgInherits {
    /// FK `pg_class.oid` — the child relation.
    pub inhrelid: PgClassOid,
    /// FK `pg_class.oid` — the parent relation.
    pub inhparent: PgClassOid,
    /// 1-based ordinal among the child's parents.
    pub inhseqno: i32,
}

/// `pg_class`: a relation (table, view, matview, partitioned table, composite
/// type's row type, sequence, index, …).
///
/// Views/matviews keep their SELECT body in `pg_rewrite` under `rulename =
/// '_RETURN'`, exactly the way PG does. Look up via [`PgCatalog::view_body`]
/// — there's no `relviewdef` shortcut on this row.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgClass {
    pub oid: PgClassOid,
    pub relname: String,
    /// FK `pg_namespace.oid`.
    pub relnamespace: PgNamespaceOid,
    pub relkind: RelKind,
    /// FK `pg_type.oid` of the row's composite type. `None` for relations
    /// that don't get one (rare in our scope).
    #[serde(with = "crate::oid::oid_or_zero")]
    pub reltype: Option<PgTypeOid>,
}

/// `pg_rewrite`: rules attached to a relation. Views are modeled as a
/// table with relkind = 'v' plus a single rule of type SELECT named
/// `_RETURN` that carries the body of the view in `ev_action`. PG also
/// stores user-defined `CREATE RULE INSTEAD INSERT/UPDATE/DELETE` rules
/// here; the analyzer doesn't currently emit those but the row shape is
/// the same.
///
/// `ev_action` is the SELECT (or DML) AST. PG stores it as `pg_node_tree`;
/// we keep it as [`SerializedAst`] (protobuf bytes + AstBindings) so
/// dependency renames flow through the same applier the views always
/// used. `ev_qual` is the rule's WHERE — `None` for views' `_RETURN`
/// rule.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgRewrite {
    pub oid: PgRewriteOid,
    pub rulename: String,
    /// FK `pg_class.oid` — the relation the rule is attached to.
    pub ev_class: PgClassOid,
    pub ev_type: EvType,
    pub ev_enabled: EvEnabled,
    pub is_instead: bool,
    /// Optional WHERE clause attached to the rule. Always `None` for
    /// views' `_RETURN`.
    pub ev_qual: Option<SerializedAst>,
    /// The rule body. For views/matviews this is the SELECT AST.
    pub ev_action: SerializedAst,
}

/// Bundle of a protobuf-encoded `pg_query::Node` plus the per-name-slot
/// binding side-table that resolves every `RangeVar` / `ColumnRef` /
/// `FuncCall` / `TypeName` in the AST to a catalog OID.
///
/// Used by [`PgClass::relviewdef`] (the SELECT AST of a view) and — once
/// `pg_index` lands — by index expressions / predicates, where we also need
/// the AST + bindings to reanalyze after dependency renames.
///
/// The emitter and the rewrite pass walk the AST in identical pre-order:
/// each name-bearing node consumes exactly one [`AstBinding`] entry. RENAME
/// / SET SCHEMA become no-ops on the AST itself — the OIDs in the bindings
/// remain valid and the deparser looks up *current* names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize_tuple, Deserialize_tuple)]
pub struct SerializedAst {
    /// Protobuf-encoded `pg_query::Node`. Base64 in JSON.
    #[serde(with = "serde_base64")]
    pub ast: Vec<u8>,
    /// One entry per name slot, walked in lockstep with the AST.
    pub bindings: Vec<AstBinding>,
}

/// One resolved name slot in a stored view AST.
///
/// Walked in deterministic pre-order alongside the AST: the emitter visits
/// every name-bearing node (`RangeVar` / `ColumnRef` / `FuncCall` /
/// `TypeName`) and pushes one variant; the rewrite pass walks the AST
/// identically and consumes one entry per slot. `Unresolved` is the
/// sentinel for slots that referred to a CTE / subquery alias /
/// unrecognized function — those keep the literal AST text through
/// round-trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AstBinding {
    /// A `RangeVar` or the relation-qualifier of a `ColumnRef`. JSON: `{"r":1234}`.
    #[serde(rename = "r")]
    Relation(PgClassOid),
    /// A `ColumnRef` terminal that names a real-relation column. The first
    /// payload is the relation, the second the column's `attnum`. JSON:
    /// `{"c":[1234,5]}`.
    #[serde(rename = "c")]
    Column(PgClassOid, i16),
    /// A `FuncCall.funcname`. JSON: `{"f":1234}`.
    #[serde(rename = "f")]
    Function(PgProcOid),
    /// A `TypeName` reference (CAST target, etc.). JSON: `{"t":1234}`.
    #[serde(rename = "t")]
    Type(PgTypeOid),
    /// Slot whose name doesn't resolve to a catalog object — CTE alias,
    /// subquery alias, function-FROM-item alias, unrecognized function.
    /// The rewrite pass leaves the literal AST text alone. JSON: `"_"`.
    #[serde(rename = "_")]
    Unresolved,
}

/// `pg_attribute`: one row per column of a relation (or per field of a
/// composite type — the typrelid points at a `pg_class` row in both cases).
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgAttribute {
    /// FK `pg_class.oid`.
    pub attrelid: PgClassOid,
    pub attname: String,
    /// FK `pg_type.oid`.
    pub atttypid: PgTypeOid,
    pub attnum: i16,
    pub attnotnull: bool,
    pub atthasdef: bool,
    pub attgenerated: Option<AttGenerated>,
    /// Type modifier: the `n` in `varchar(n)`, the packed `(p,s)` of
    /// `numeric(p,s)`, the dimension count of pgvector's `vector(N)`, etc.
    /// `None` matches PG's `-1` sentinel ("no typmod"); `Some` carries the
    /// packed `int32` exactly as PG would store it. Use [`crate::typmod`]
    /// to encode/decode.
    #[serde(with = "crate::oid::option_i32_neg_one")]
    pub atttypmod: Option<i32>,
    /// `pg_attribute.attidentity`. `None` for non-identity columns
    /// (PG's `\0`), `Some(Always)` for `GENERATED ALWAYS AS IDENTITY`,
    /// `Some(ByDefault)` for `GENERATED BY DEFAULT AS IDENTITY`.
    pub attidentity: Option<AttIdentity>,
}

/// `pg_proc`: a function, aggregate, window function, or procedure.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgProc {
    pub oid: PgProcOid,
    pub proname: String,
    /// FK `pg_namespace.oid`.
    pub pronamespace: PgNamespaceOid,
    pub prokind: ProKind,
    /// Argument types in call-signature order. Excludes pure-OUT and TABLE
    /// args (those only appear in `proallargtypes`).
    pub proargtypes: Vec<PgTypeOid>,
    /// FK `pg_type.oid`.
    pub prorettype: PgTypeOid,
    pub proretset: bool,
    /// FK `pg_type.oid` of the variadic element type, or `None` if not
    /// variadic.
    #[serde(with = "crate::oid::oid_or_zero")]
    pub provariadic: Option<PgTypeOid>,
    pub proisstrict: bool,
    pub pronargdefaults: i16,
    /// All formal arg types (IN/OUT/INOUT/VARIADIC/TABLE), parallel to
    /// `proargmodes`/`proargnames`. Empty when no OUT/INOUT/TABLE args.
    pub proallargtypes: Vec<PgTypeOid>,
    pub proargmodes: Vec<ArgMode>,
    pub proargnames: Vec<String>,
    /// Volatility category. Drives DDL validation that requires IMMUTABLE
    /// callees (CHECK / GENERATED / index expressions).
    pub provolatile: ProVolatile,
}

/// `pg_aggregate`: extra metadata for aggregate `pg_proc` rows.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgAggregate {
    /// FK `pg_proc.oid` — the aggregate's own pg_proc entry.
    pub aggfnoid: PgProcOid,
    /// FK `pg_proc.oid` of the final function. `None` (PG: `0`) when the
    /// aggregate has no finalfn, in which case its effective return type
    /// is `aggfnoid.prorettype`. Callers walk the FK to derive the
    /// effective return type instead of caching it on the row.
    #[serde(with = "crate::oid::oid_or_zero")]
    pub aggfinalfn: Option<PgProcOid>,
}

/// `pg_operator`: a registered operator.
///
/// `oprresult` is `Option<PgTypeOid>` — PG `0` is a "shell operator" placed
/// by `CREATE OPERATOR` before the implementation function exists. Such
/// operators can't be used in queries; the operator-resolution path skips
/// them when their result type is `None`. We still keep the row so the
/// catalog mirror is faithful and a follow-up CREATE OPERATOR can finish
/// it.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgOperator {
    pub oid: PgOperatorOid,
    pub oprname: String,
    /// FK `pg_namespace.oid`.
    pub oprnamespace: PgNamespaceOid,
    /// FK `pg_type.oid`. `None` for prefix operators.
    #[serde(with = "crate::oid::oid_or_zero")]
    pub oprleft: Option<PgTypeOid>,
    /// FK `pg_type.oid`.
    pub oprright: PgTypeOid,
    /// FK `pg_type.oid`. `None` for shell operators (PG: `0`).
    #[serde(with = "crate::oid::oid_or_zero")]
    pub oprresult: Option<PgTypeOid>,
}

/// `pg_cast`: a cast rule between two types.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgCast {
    pub oid: PgCastOid,
    /// FK `pg_type.oid`.
    pub castsource: PgTypeOid,
    /// FK `pg_type.oid`.
    pub casttarget: PgTypeOid,
    pub castcontext: CastContext,
    pub castmethod: CastMethod,
}

/// `pg_extension`: an installed extension.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgExtension {
    pub oid: PgExtensionOid,
    pub extname: String,
    /// FK `pg_namespace.oid`.
    pub extnamespace: PgNamespaceOid,
    pub extversion: String,
}

/// `pg_depend`: a dependency between two catalog objects.
///
/// `(classid, objid, objsubid)` identifies the dependent; `(refclassid,
/// refobjid, refobjsubid)` identifies the referenced object. `deptype` tells
/// the rule:
/// - `Normal` — view depends on a table/column; DROP CASCADE drops the view.
/// - `Extension` — `objid` was created by `refobjid` (extension); DROP
///   EXTENSION drops the object.
/// - Other variants mirror PG semantics but are not produced by the DDL
///   handlers today (kept for round-trip with PG snapshots).
///
/// `objid`/`refobjid` use [`PgGenericOid`] (a non-zero `u32`) because their
/// concrete catalog table varies with the sibling `classid`/`refclassid` —
/// callers convert to/from the per-table newtype at the boundary via
/// `PgXxxOid::new(g.get())`.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgDepend {
    pub classid: PgClassOid,
    pub objid: PgGenericOid,
    pub objsubid: i16,
    pub refclassid: PgClassOid,
    pub refobjid: PgGenericOid,
    pub refobjsubid: i16,
    pub deptype: DepType,
}

// ─── Base64 adapter for relviewdef ─────────────────────────────────────────

/// Serde adapter that encodes `Vec<u8>` as a base64 string in JSON while
/// keeping it as a plain byte buffer in memory. Used by
/// [`PgClass::relviewdef`] so view ASTs don't blow up the seed JSON.
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

// ─── Seed (on-disk shape) ──────────────────────────────────────────────────

/// On-disk shape of `seed.json`. Each catalog table is a flat `Vec<Row>`
/// ordered by OID (or by `(attrelid, attnum)` for `pg_attribute`, by
/// `(enumtypid, enumsortorder)` for `pg_enum`, by `(classid, objid)` for
/// `pg_depend`). `serde_json` round-trips this directly — no `BTreeMap`
/// re-ordering needed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PgCatalogSeed {
    #[serde(default)]
    pub pg_namespace: Vec<PgNamespace>,
    #[serde(default)]
    pub pg_type: Vec<PgType>,
    #[serde(default)]
    pub pg_enum: Vec<PgEnum>,
    #[serde(default)]
    pub pg_range: Vec<PgRange>,
    #[serde(default)]
    pub pg_class: Vec<PgClass>,
    #[serde(default)]
    pub pg_attribute: Vec<PgAttribute>,
    #[serde(default)]
    pub pg_proc: Vec<PgProc>,
    #[serde(default)]
    pub pg_aggregate: Vec<PgAggregate>,
    #[serde(default)]
    pub pg_operator: Vec<PgOperator>,
    #[serde(default)]
    pub pg_cast: Vec<PgCast>,
    #[serde(default)]
    pub pg_extension: Vec<PgExtension>,
    #[serde(default)]
    pub pg_depend: Vec<PgDepend>,
    #[serde(default)]
    pub pg_inherits: Vec<PgInherits>,
    #[serde(default)]
    pub pg_constraint: Vec<PgConstraint>,
    #[serde(default)]
    pub pg_index: Vec<PgIndex>,
    #[serde(default)]
    pub pg_rewrite: Vec<PgRewrite>,
    /// Namespace OIDs in search order. Non-PG (PG keeps this in a GUC).
    #[serde(default, with = "crate::oid::vec_oid")]
    pub search_path: Vec<PgNamespaceOid>,
}

// ─── In-memory catalog ─────────────────────────────────────────────────────

/// Mutable in-memory PostgreSQL catalog.
///
/// Starts from the embedded PG18 seed and evolves by applying DDL via
/// [`PgCatalog::apply_sql`]. Each catalog table is stored as a HashMap keyed
/// by primary OID (or by FK in the case of `pg_attribute`/`pg_enum`/
/// `pg_range`); name-keyed indexes are rebuilt at construction time and kept
/// in sync by the DDL handlers via the `insert_pg_*` / `remove_pg_*` helpers
/// in the construction impl block below.
#[derive(Clone)]
pub struct PgCatalog {
    // ── Catalog tables ──
    pub(crate) pg_namespace: HashMap<PgNamespaceOid, PgNamespace>,
    pub(crate) pg_type: HashMap<PgTypeOid, PgType>,
    pub(crate) pg_class: HashMap<PgClassOid, PgClass>,
    pub(crate) pg_proc: HashMap<PgProcOid, PgProc>,
    pub(crate) pg_aggregate: HashMap<PgProcOid, PgAggregate>,
    pub(crate) pg_operator: HashMap<PgOperatorOid, PgOperator>,
    pub(crate) pg_cast: HashMap<PgCastOid, PgCast>,
    pub(crate) pg_extension: HashMap<PgExtensionOid, PgExtension>,
    /// Keyed by `attrelid`; vec ordered by `attnum`.
    pub(crate) pg_attribute: HashMap<PgClassOid, Vec<PgAttribute>>,
    /// Keyed by `enumtypid`; vec ordered by `enumsortorder`.
    pub(crate) pg_enum: HashMap<PgTypeOid, Vec<PgEnum>>,
    /// Keyed by `rngtypid`.
    pub(crate) pg_range: HashMap<PgTypeOid, PgRange>,
    pub(crate) pg_depend: Vec<PgDepend>,
    pub(crate) pg_inherits: Vec<PgInherits>,
    pub(crate) pg_constraint: HashMap<PgConstraintOid, PgConstraint>,
    /// Keyed by `indexrelid` (the index's `pg_class.oid`).
    pub(crate) pg_index: HashMap<PgClassOid, PgIndex>,
    pub(crate) pg_rewrite: HashMap<PgRewriteOid, PgRewrite>,

    // ── Name-keyed indexes (built by `from_seed`, maintained by DDL) ──
    pub(crate) namespace_by_name: HashMap<String, PgNamespaceOid>,
    /// `(typnamespace, typname) -> typoid`.
    pub(crate) type_by_qname: HashMap<(PgNamespaceOid, String), PgTypeOid>,
    /// `(relnamespace, relname) -> classoid`.
    pub(crate) class_by_qname: HashMap<(PgNamespaceOid, String), PgClassOid>,
    /// `(pronamespace, proname) -> [procoid]` (overloads).
    pub(crate) proc_by_qname: HashMap<(PgNamespaceOid, String), Vec<PgProcOid>>,
    /// `(oprnamespace, oprname) -> [opoid]` (overloads).
    pub(crate) operator_by_qname: HashMap<(PgNamespaceOid, String), Vec<PgOperatorOid>>,
    /// `(castsource, casttarget) -> castoid`.
    pub(crate) cast_by_pair: HashMap<(PgTypeOid, PgTypeOid), PgCastOid>,
    pub(crate) extension_by_name: HashMap<String, PgExtensionOid>,

    // ── Session state (non-PG) ──
    /// Namespace OIDs in search order (analog of PG's `search_path` GUC).
    pub(crate) search_path: Vec<PgNamespaceOid>,
    pub(crate) next_oid: u32,
}

/// Starting OID for user-defined objects. Well above PG system OIDs (~16384).
pub(crate) const USER_OID_START: u32 = 100_000;

impl Default for PgCatalog {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Construction ──────────────────────────────────────────────────────────

impl PgCatalog {
    /// Create a catalog seeded with the embedded PG18 snapshot.
    pub fn new() -> Self {
        Self::from_seed(load_seed())
    }

    /// Build a catalog from a serialized seed. `next_oid` is set to one past
    /// the max OID seen in the seed (or [`USER_OID_START`] if higher), so
    /// freshly allocated user objects can never collide with PG18's
    /// information_schema tables (which sit above 100k).
    pub fn from_seed(seed: PgCatalogSeed) -> Self {
        let mut cat = Self::empty();

        let mut max_oid: u32 = USER_OID_START;
        let bump = |oid: u32, m: &mut u32| {
            if oid > *m {
                *m = oid;
            }
        };
        for n in &seed.pg_namespace {
            bump(n.oid.get(), &mut max_oid);
        }
        for t in &seed.pg_type {
            bump(t.oid.get(), &mut max_oid);
        }
        for c in &seed.pg_class {
            bump(c.oid.get(), &mut max_oid);
        }
        for p in &seed.pg_proc {
            bump(p.oid.get(), &mut max_oid);
        }
        for o in &seed.pg_operator {
            bump(o.oid.get(), &mut max_oid);
        }
        for c in &seed.pg_cast {
            bump(c.oid.get(), &mut max_oid);
        }
        for e in &seed.pg_extension {
            bump(e.oid.get(), &mut max_oid);
        }
        for e in &seed.pg_enum {
            bump(e.oid.get(), &mut max_oid);
        }
        cat.next_oid = max_oid.saturating_add(1).max(USER_OID_START);

        for ns in seed.pg_namespace {
            cat.namespace_by_name.insert(ns.nspname.clone(), ns.oid);
            cat.pg_namespace.insert(ns.oid, ns);
        }
        for t in seed.pg_type {
            cat.type_by_qname
                .insert((t.typnamespace, t.typname.clone()), t.oid);
            cat.pg_type.insert(t.oid, t);
        }
        for c in seed.pg_class {
            cat.class_by_qname
                .insert((c.relnamespace, c.relname.clone()), c.oid);
            cat.pg_class.insert(c.oid, c);
        }
        for a in seed.pg_attribute {
            cat.pg_attribute.entry(a.attrelid).or_default().push(a);
        }
        for v in cat.pg_attribute.values_mut() {
            v.sort_by_key(|a| a.attnum);
        }
        for e in seed.pg_enum {
            cat.pg_enum.entry(e.enumtypid).or_default().push(e);
        }
        for v in cat.pg_enum.values_mut() {
            v.sort_by(|a, b| {
                a.enumsortorder
                    .partial_cmp(&b.enumsortorder)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        for r in seed.pg_range {
            cat.pg_range.insert(r.rngtypid, r);
        }
        for p in seed.pg_proc {
            cat.proc_by_qname
                .entry((p.pronamespace, p.proname.clone()))
                .or_default()
                .push(p.oid);
            cat.pg_proc.insert(p.oid, p);
        }
        for a in seed.pg_aggregate {
            cat.pg_aggregate.insert(a.aggfnoid, a);
        }
        for o in seed.pg_operator {
            cat.operator_by_qname
                .entry((o.oprnamespace, o.oprname.clone()))
                .or_default()
                .push(o.oid);
            cat.pg_operator.insert(o.oid, o);
        }
        for c in seed.pg_cast {
            cat.cast_by_pair.insert((c.castsource, c.casttarget), c.oid);
            cat.pg_cast.insert(c.oid, c);
        }
        for e in seed.pg_extension {
            cat.extension_by_name.insert(e.extname.clone(), e.oid);
            cat.pg_extension.insert(e.oid, e);
        }
        cat.pg_depend = seed.pg_depend;
        cat.pg_inherits = seed.pg_inherits;
        for c in seed.pg_constraint {
            cat.pg_constraint.insert(c.oid, c);
        }
        for i in seed.pg_index {
            cat.pg_index.insert(i.indexrelid, i);
        }
        for r in seed.pg_rewrite {
            cat.pg_rewrite.insert(r.oid, r);
        }
        cat.search_path = seed.search_path;
        cat
    }

    /// Build an empty catalog (no seed). Reserved for the view-AST rewrite
    /// walker, which needs a placeholder handle when descending into
    /// subselects whose RangeVars are already fully qualified.
    pub(crate) fn empty() -> Self {
        Self {
            pg_namespace: HashMap::new(),
            pg_type: HashMap::new(),
            pg_class: HashMap::new(),
            pg_proc: HashMap::new(),
            pg_aggregate: HashMap::new(),
            pg_operator: HashMap::new(),
            pg_cast: HashMap::new(),
            pg_extension: HashMap::new(),
            pg_attribute: HashMap::new(),
            pg_enum: HashMap::new(),
            pg_range: HashMap::new(),
            pg_depend: Vec::new(),
            pg_inherits: Vec::new(),
            pg_constraint: HashMap::new(),
            pg_index: HashMap::new(),
            pg_rewrite: HashMap::new(),
            namespace_by_name: HashMap::new(),
            type_by_qname: HashMap::new(),
            class_by_qname: HashMap::new(),
            proc_by_qname: HashMap::new(),
            operator_by_qname: HashMap::new(),
            cast_by_pair: HashMap::new(),
            extension_by_name: HashMap::new(),
            search_path: Vec::new(),
            next_oid: USER_OID_START,
        }
    }

    /// Snapshot the catalog into a JSON-friendly seed. Round-trips with
    /// [`PgCatalog::from_seed`]. The OID allocator and any non-table state
    /// (namespace_by_name and friends) are not part of the seed.
    pub fn to_seed(&self) -> PgCatalogSeed {
        let mut pg_namespace: Vec<_> = self.pg_namespace.values().cloned().collect();
        pg_namespace.sort_by_key(|n| n.oid);

        let mut pg_type: Vec<_> = self.pg_type.values().cloned().collect();
        pg_type.sort_by_key(|t| t.oid);

        let mut pg_enum: Vec<_> = self
            .pg_enum
            .values()
            .flat_map(|v| v.iter().cloned())
            .collect();
        pg_enum.sort_by(|a, b| {
            a.enumtypid.cmp(&b.enumtypid).then(
                a.enumsortorder
                    .partial_cmp(&b.enumsortorder)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
        });

        let mut pg_range: Vec<_> = self.pg_range.values().cloned().collect();
        pg_range.sort_by_key(|r| r.rngtypid);

        let mut pg_class: Vec<_> = self.pg_class.values().cloned().collect();
        pg_class.sort_by_key(|c| c.oid);

        let mut pg_attribute: Vec<_> = self
            .pg_attribute
            .values()
            .flat_map(|v| v.iter().cloned())
            .collect();
        pg_attribute.sort_by_key(|a| (a.attrelid, a.attnum));

        let mut pg_proc: Vec<_> = self.pg_proc.values().cloned().collect();
        pg_proc.sort_by_key(|p| p.oid);

        let mut pg_aggregate: Vec<_> = self.pg_aggregate.values().cloned().collect();
        pg_aggregate.sort_by_key(|a| a.aggfnoid);

        let mut pg_operator: Vec<_> = self.pg_operator.values().cloned().collect();
        pg_operator.sort_by_key(|o| o.oid);

        let mut pg_cast: Vec<_> = self.pg_cast.values().cloned().collect();
        pg_cast.sort_by_key(|c| c.oid);

        let mut pg_extension: Vec<_> = self.pg_extension.values().cloned().collect();
        pg_extension.sort_by_key(|e| e.oid);

        let mut pg_depend = self.pg_depend.clone();
        pg_depend.sort_by_key(|d| (d.classid, d.objid, d.objsubid, d.refclassid, d.refobjid));

        let mut pg_inherits = self.pg_inherits.clone();
        pg_inherits.sort_by_key(|i| (i.inhrelid, i.inhseqno));

        let mut pg_constraint: Vec<_> = self.pg_constraint.values().cloned().collect();
        pg_constraint.sort_by_key(|c| c.oid);

        let mut pg_index: Vec<_> = self.pg_index.values().cloned().collect();
        pg_index.sort_by_key(|i| i.indexrelid);

        let mut pg_rewrite: Vec<_> = self.pg_rewrite.values().cloned().collect();
        pg_rewrite.sort_by_key(|r| r.oid);

        PgCatalogSeed {
            pg_namespace,
            pg_type,
            pg_enum,
            pg_range,
            pg_class,
            pg_attribute,
            pg_proc,
            pg_aggregate,
            pg_operator,
            pg_cast,
            pg_extension,
            pg_depend,
            pg_inherits,
            pg_constraint,
            pg_index,
            pg_rewrite,
            search_path: self.search_path.clone(),
        }
    }

    /// Parse and apply all DDL statements in `sql`, mutating the catalog.
    pub fn apply_sql(&mut self, sql: &str) -> Result<(), DdlError> {
        apply_sql_to(self, sql)
    }

    /// Parse `expr_sql` as a SELECT-list expression (`SELECT <expr>`),
    /// resolve every name slot to a catalog OID against `self`, and return
    /// the serialized AST + bindings. Used by the seed exporter to capture
    /// `pg_index.indexprs` from `pg_get_indexdef` output without requiring
    /// the seed crate to depend on the binding walker directly.
    #[cfg(any(test, feature = "internal"))]
    pub fn serialize_expression(&self, expr_sql: &str) -> Result<SerializedAst, DdlError> {
        let select = format!("SELECT {expr_sql}");
        crate::ddl::serialize_subnode(self, &select, crate::ddl::views::extract_first_target)
    }

    /// Like [`Self::serialize_expression`] but for partial-index predicates
    /// — parses `SELECT 1 WHERE <pred>` and serializes the WHERE node.
    #[cfg(any(test, feature = "internal"))]
    pub fn serialize_predicate(&self, pred_sql: &str) -> Result<SerializedAst, DdlError> {
        let select = format!("SELECT 1 WHERE {pred_sql}");
        crate::ddl::serialize_subnode(self, &select, crate::ddl::views::extract_where)
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
        // positional placeholder the lexer extracted. Surface a mismatch as
        // an Internal error so the macro host process can report it cleanly.
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

    pub(crate) fn alloc_oid(&mut self) -> u32 {
        let oid = self.next_oid;
        self.next_oid += 1;
        oid
    }
}

// ─── DDL mutation helpers ──────────────────────────────────────────────────
//
// These keep the rows + name indexes in sync. DDL handlers should use them
// rather than writing the HashMaps directly.

impl PgCatalog {
    pub(crate) fn insert_pg_namespace(&mut self, row: PgNamespace) {
        self.namespace_by_name.insert(row.nspname.clone(), row.oid);
        self.pg_namespace.insert(row.oid, row);
    }

    pub(crate) fn remove_pg_namespace(&mut self, oid: PgNamespaceOid) -> Option<PgNamespace> {
        let row = self.pg_namespace.remove(&oid)?;
        self.namespace_by_name.remove(&row.nspname);
        Some(row)
    }

    pub(crate) fn rename_pg_namespace(&mut self, oid: PgNamespaceOid, new_name: String) {
        if let Some(row) = self.pg_namespace.get_mut(&oid) {
            self.namespace_by_name.remove(&row.nspname);
            row.nspname = new_name.clone();
            self.namespace_by_name.insert(new_name, oid);
        }
    }

    pub(crate) fn insert_pg_type(&mut self, row: PgType) {
        self.type_by_qname
            .insert((row.typnamespace, row.typname.clone()), row.oid);
        self.pg_type.insert(row.oid, row);
    }

    pub(crate) fn remove_pg_type(&mut self, oid: PgTypeOid) -> Option<PgType> {
        let row = self.pg_type.remove(&oid)?;
        self.type_by_qname
            .remove(&(row.typnamespace, row.typname.clone()));
        // Tear down dependent enum labels / range subtype / cast pairs and
        // pg_depend rows that named this type.
        self.pg_enum.remove(&oid);
        self.pg_range.remove(&oid);
        Some(row)
    }

    pub(crate) fn rename_pg_type(
        &mut self,
        oid: PgTypeOid,
        new_name: String,
        new_nspoid: PgNamespaceOid,
    ) {
        if let Some(row) = self.pg_type.get_mut(&oid) {
            let old_key = (row.typnamespace, row.typname.clone());
            row.typname = new_name.clone();
            row.typnamespace = new_nspoid;
            self.type_by_qname.remove(&old_key);
            self.type_by_qname.insert((new_nspoid, new_name), oid);
        }
    }

    pub(crate) fn insert_pg_class(&mut self, row: PgClass) {
        self.class_by_qname
            .insert((row.relnamespace, row.relname.clone()), row.oid);
        self.pg_class.insert(row.oid, row);
    }

    pub(crate) fn remove_pg_class(&mut self, oid: PgClassOid) -> Option<PgClass> {
        let row = self.pg_class.remove(&oid)?;
        self.class_by_qname
            .remove(&(row.relnamespace, row.relname.clone()));
        self.pg_attribute.remove(&oid);
        Some(row)
    }

    pub(crate) fn rename_pg_class(
        &mut self,
        oid: PgClassOid,
        new_name: String,
        new_nspoid: PgNamespaceOid,
    ) {
        if let Some(row) = self.pg_class.get_mut(&oid) {
            let old_key = (row.relnamespace, row.relname.clone());
            row.relname = new_name.clone();
            row.relnamespace = new_nspoid;
            self.class_by_qname.remove(&old_key);
            self.class_by_qname.insert((new_nspoid, new_name), oid);
        }
    }

    pub(crate) fn insert_pg_attribute(&mut self, attr: PgAttribute) {
        let attrs = self.pg_attribute.entry(attr.attrelid).or_default();
        attrs.push(attr);
        attrs.sort_by_key(|a| a.attnum);
    }

    pub(crate) fn insert_pg_enum(&mut self, row: PgEnum) {
        let labels = self.pg_enum.entry(row.enumtypid).or_default();
        labels.push(row);
        labels.sort_by(|a, b| {
            a.enumsortorder
                .partial_cmp(&b.enumsortorder)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    pub(crate) fn insert_pg_range(&mut self, row: PgRange) {
        self.pg_range.insert(row.rngtypid, row);
    }

    pub(crate) fn insert_pg_proc(&mut self, row: PgProc) {
        self.proc_by_qname
            .entry((row.pronamespace, row.proname.clone()))
            .or_default()
            .push(row.oid);
        self.pg_proc.insert(row.oid, row);
    }

    pub(crate) fn remove_pg_proc(&mut self, oid: PgProcOid) -> Option<PgProc> {
        let row = self.pg_proc.remove(&oid)?;
        if let Some(v) = self
            .proc_by_qname
            .get_mut(&(row.pronamespace, row.proname.clone()))
        {
            v.retain(|&o| o != oid);
            if v.is_empty() {
                self.proc_by_qname
                    .remove(&(row.pronamespace, row.proname.clone()));
            }
        }
        self.pg_aggregate.remove(&oid);
        Some(row)
    }

    pub(crate) fn rename_pg_proc(
        &mut self,
        oid: PgProcOid,
        new_name: String,
        new_nspoid: PgNamespaceOid,
    ) {
        if let Some(row) = self.pg_proc.get_mut(&oid) {
            let old_key = (row.pronamespace, row.proname.clone());
            row.proname = new_name.clone();
            row.pronamespace = new_nspoid;
            if let Some(v) = self.proc_by_qname.get_mut(&old_key) {
                v.retain(|&o| o != oid);
                if v.is_empty() {
                    self.proc_by_qname.remove(&old_key);
                }
            }
            self.proc_by_qname
                .entry((new_nspoid, new_name))
                .or_default()
                .push(oid);
        }
    }

    pub(crate) fn insert_pg_aggregate(&mut self, row: PgAggregate) {
        self.pg_aggregate.insert(row.aggfnoid, row);
    }

    pub(crate) fn insert_pg_operator(&mut self, row: PgOperator) {
        self.operator_by_qname
            .entry((row.oprnamespace, row.oprname.clone()))
            .or_default()
            .push(row.oid);
        self.pg_operator.insert(row.oid, row);
    }

    pub(crate) fn remove_pg_operator(&mut self, oid: PgOperatorOid) -> Option<PgOperator> {
        let row = self.pg_operator.remove(&oid)?;
        if let Some(v) = self
            .operator_by_qname
            .get_mut(&(row.oprnamespace, row.oprname.clone()))
        {
            v.retain(|&o| o != oid);
            if v.is_empty() {
                self.operator_by_qname
                    .remove(&(row.oprnamespace, row.oprname.clone()));
            }
        }
        Some(row)
    }

    pub(crate) fn insert_pg_cast(&mut self, row: PgCast) {
        self.cast_by_pair
            .insert((row.castsource, row.casttarget), row.oid);
        self.pg_cast.insert(row.oid, row);
    }

    pub(crate) fn remove_pg_cast(&mut self, oid: PgCastOid) -> Option<PgCast> {
        let row = self.pg_cast.remove(&oid)?;
        self.cast_by_pair.remove(&(row.castsource, row.casttarget));
        Some(row)
    }

    pub(crate) fn insert_pg_extension(&mut self, row: PgExtension) {
        self.extension_by_name.insert(row.extname.clone(), row.oid);
        self.pg_extension.insert(row.oid, row);
    }

    pub(crate) fn remove_pg_extension(&mut self, oid: PgExtensionOid) -> Option<PgExtension> {
        let row = self.pg_extension.remove(&oid)?;
        self.extension_by_name.remove(&row.extname);
        Some(row)
    }

    pub(crate) fn insert_pg_constraint(&mut self, row: PgConstraint) {
        self.pg_constraint.insert(row.oid, row);
    }

    pub(crate) fn remove_pg_constraints_of(&mut self, relid: PgClassOid) {
        self.pg_constraint.retain(|_, c| c.conrelid != relid);
    }

    pub(crate) fn insert_pg_index(&mut self, row: PgIndex) {
        self.pg_index.insert(row.indexrelid, row);
    }

    pub(crate) fn remove_pg_index(&mut self, indexrelid: PgClassOid) -> Option<PgIndex> {
        self.pg_index.remove(&indexrelid)
    }

    pub(crate) fn insert_pg_rewrite(&mut self, row: PgRewrite) {
        self.pg_rewrite.insert(row.oid, row);
    }

    /// Drop every `pg_rewrite` row attached to `relid`. Used by
    /// DROP TABLE / DROP VIEW so the rule body doesn't leak past the
    /// relation.
    pub(crate) fn remove_pg_rewrites_of(&mut self, relid: PgClassOid) {
        self.pg_rewrite.retain(|_, r| r.ev_class != relid);
    }

    /// Look up the SELECT body for a view (the `_RETURN` rule). Returns
    /// `None` for non-views or if the rule was never installed.
    pub fn view_body(&self, view_oid: PgClassOid) -> Option<&SerializedAst> {
        self.pg_rewrite.values().find_map(|r| {
            (r.ev_class == view_oid && r.rulename == "_RETURN").then_some(&r.ev_action)
        })
    }

    /// Replace the SELECT body for a view (the `_RETURN` rule). Tests use
    /// this to simulate a legacy snapshot whose `_RETURN` rule was never
    /// populated.
    #[cfg(any(test, feature = "internal"))]
    pub fn clear_view_body(&mut self, view_oid: PgClassOid) {
        self.pg_rewrite
            .retain(|_, r| !(r.ev_class == view_oid && r.rulename == "_RETURN"));
    }

    /// Drop every `pg_index` row whose `indrelid` is `relid` and return the
    /// indexrelids — callers tear down the matching `pg_class` rows
    /// afterwards.
    pub(crate) fn remove_pg_indexes_of(&mut self, relid: PgClassOid) -> Vec<PgClassOid> {
        let to_drop: Vec<PgClassOid> = self
            .pg_index
            .values()
            .filter(|i| i.indrelid == relid)
            .map(|i| i.indexrelid)
            .collect();
        for oid in &to_drop {
            self.pg_index.remove(oid);
        }
        to_drop
    }

    /// Add a `pg_depend` row by directly inserting the `PgDepend` value the
    /// caller built. Lets call sites use struct-update syntax for the common
    /// case (only `objid`/`refobjid` change between rows, the rest is fixed
    /// for a given DDL operation).
    pub(crate) fn add_dependency(&mut self, row: PgDepend) {
        self.pg_depend.push(row);
    }

    /// Drop every `pg_depend` row whose dependent is `(classid, objid)`.
    pub(crate) fn remove_dependencies_of(&mut self, classid: PgClassOid, objid: PgGenericOid) {
        self.pg_depend
            .retain(|d| !(d.classid == classid && d.objid == objid));
    }

    /// Drop every `pg_depend` row whose referenced object is `(refclassid,
    /// refobjid)`.
    pub(crate) fn remove_dependencies_on(
        &mut self,
        refclassid: PgClassOid,
        refobjid: PgGenericOid,
    ) {
        self.pg_depend
            .retain(|d| !(d.refclassid == refclassid && d.refobjid == refobjid));
    }
}
