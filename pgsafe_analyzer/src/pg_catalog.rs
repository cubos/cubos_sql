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
    PgCastOid, PgClassOid, PgCollationOid, PgConstraintOid, PgEnumOid, PgExtensionOid,
    PgGenericOid, PgNamespaceOid, PgOperatorOid, PgProcOid, PgRewriteOid, PgTypeOid,
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
    pub const OID: PgTypeOid = PgTypeOid::from_raw(26);
    pub const TID: PgTypeOid = PgTypeOid::from_raw(27);
    pub const XID: PgTypeOid = PgTypeOid::from_raw(28);
    pub const CID: PgTypeOid = PgTypeOid::from_raw(29);
    pub const FLOAT4: PgTypeOid = PgTypeOid::from_raw(700);
    pub const FLOAT8: PgTypeOid = PgTypeOid::from_raw(701);
    pub const UNKNOWN: PgTypeOid = PgTypeOid::from_raw(705);
    pub const BPCHAR: PgTypeOid = PgTypeOid::from_raw(1042);
    pub const VARCHAR: PgTypeOid = PgTypeOid::from_raw(1043);
    pub const NUMERIC: PgTypeOid = PgTypeOid::from_raw(1700);
    pub const RECORD: PgTypeOid = PgTypeOid::from_raw(2249);
}

/// PG's hidden system columns, present on every table/view. Negative attnums
/// match PG's `pg_attribute` convention. Excluded from `SELECT *` expansion
/// and from "did you mean" suggestions, but resolvable when named explicitly.
pub const SYSTEM_COLUMNS: &[(&str, PgTypeOid, i16)] = &[
    ("tableoid", oid::OID, -6),
    ("cmax", oid::CID, -5),
    ("xmax", oid::XID, -4),
    ("cmin", oid::CID, -3),
    ("xmin", oid::XID, -2),
    ("ctid", oid::TID, -1),
];

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
    /// FK `pg_collation.oid` — the type's default collation. `None`
    /// (PG's `0`) for non-collatable types and base text types that
    /// inherit the database default. Set explicitly for `citext` and
    /// for domains created with an explicit `COLLATE` decoration. PG
    /// uses this to derive `attcollation` when a column doesn't pin its
    /// own collation.
    #[serde(with = "crate::oid::oid_or_zero")]
    pub typcollation: Option<PgCollationOid>,
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

/// `pg_collation`: a registered collation. PG keys this by oid; the analyzer
/// also keeps a name index so `COLLATE "x"` validation (and column-level
/// `attcollation`) can resolve names quickly.
///
/// We carry only the fields the analyzer consults: name, namespace,
/// encoding (`-1` = collation-independent / "any"). `collprovider`,
/// `collisdeterministic`, `collcollate`, `collctype`, etc. are not modeled
/// because nothing in the analyzer reads them.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgCollation {
    pub oid: PgCollationOid,
    pub collname: String,
    /// FK `pg_namespace.oid`.
    pub collnamespace: PgNamespaceOid,
    /// PG's `pg_database.encoding` integer for the collation's target
    /// encoding (`-1` = "any encoding"; common for `"C"` and `"POSIX"`).
    pub collencoding: i32,
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
    /// FK `pg_collation.oid` — the column's default collation. PG sets
    /// this for collatable types (text, varchar, citext, …) and 0 for
    /// non-collatable types. We model both as `None`/`Some` and let the
    /// DDL parser assign explicit `COLLATE "x"` decorations.
    #[serde(with = "crate::oid::oid_or_zero")]
    pub attcollation: Option<PgCollationOid>,
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
    #[serde(default)]
    pub pg_collation: Vec<PgCollation>,
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
    pub(crate) pg_collation: HashMap<PgCollationOid, PgCollation>,

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
    /// `(collnamespace, collname) -> collation_oid`. Walked by analyzer
    /// when validating `COLLATE "x"` decorations.
    pub(crate) collation_by_qname: HashMap<(PgNamespaceOid, String), PgCollationOid>,

    // ── Session state (non-PG) ──
    /// Namespace OIDs in search order (analog of PG's `search_path` GUC).
    pub(crate) search_path: Vec<PgNamespaceOid>,
    pub(crate) next_oid: std::num::NonZeroU32,

    /// Lazy-initialized PG sanity mirror used by the `pg_sanity` feature to
    /// cross-check `apply_sql` / `analyze` against a real PG protocol server.
    /// `None` means the catalog was built from a non-default seed (via
    /// [`PgCatalog::from_seed`]) where no synced live server is available.
    /// The outer `Arc` lets `Clone` share one server across catalog clones —
    /// only the macro caches a clone, and it never enables `pg_sanity`.
    #[cfg(feature = "pg_sanity")]
    pub(crate) pg_sanity: Option<
        std::sync::Arc<std::sync::OnceLock<std::sync::Mutex<crate::pg_sanity::PgSanityServer>>>,
    >,

    /// Set once any `CREATE/ALTER/DROP EXTENSION` statement has touched the
    /// catalog. Our embedded extension SQL diverges from what PG sanity ships
    /// natively, so once an extension is in play the sanity check skips
    /// every later mirror call to avoid spurious panics.
    #[cfg(feature = "pg_sanity")]
    pub(crate) pg_sanity_tainted: bool,

    /// Per-catalog opt-out: tests that exercise behavior PG sanity can't
    /// validate (e.g. ON CONFLICT planner-level checks that the wire-level
    /// `prepare` doesn't reach) flip this on via [`PgCatalog::skip_pg_sanity`]
    /// to silence the sanity check without giving up the compile-time
    /// analyzer assertion.
    #[cfg(feature = "pg_sanity")]
    pub(crate) pg_sanity_skip: bool,
}

/// Starting OID for user-defined objects. Well above PG system OIDs (~16384).
pub(crate) const USER_OID_START: u32 = 100_000;

/// `USER_OID_START` as a [`NonZeroU32`]. Validated at compile time so the
/// `next_oid` counter can be a `NonZeroU32` from construction onward without
/// a runtime check at each `alloc_oid` call.
pub(crate) const USER_OID_START_NZ: std::num::NonZeroU32 =
    match std::num::NonZeroU32::new(USER_OID_START) {
        Some(v) => v,
        None => panic!("USER_OID_START must be non-zero"),
    };

// ─── Construction ──────────────────────────────────────────────────────────

impl PgCatalog {
    /// Create a catalog seeded with the embedded PG18 snapshot.
    ///
    /// Returns an [`AnalyzeError`] only if the embedded `seed.json` fails to
    /// deserialize — a build-time invariant of the analyzer crate that would
    /// only fire on a corrupted seed regeneration. Surfacing it as an error
    /// (rather than a panic) lets host processes such as the `sql!` macro
    /// report the failure cleanly.
    pub fn new() -> Result<Self, AnalyzeError> {
        #[cfg(not(feature = "pg_sanity"))]
        {
            Ok(Self::from_seed(load_seed()?))
        }
        #[cfg(feature = "pg_sanity")]
        {
            // Only `new()` (default seed) gets a PG sanity mirror — `from_seed`
            // with arbitrary seeds can't be reproduced on a fresh PG sanity
            // instance.
            let mut cat = Self::from_seed(load_seed()?);
            cat.pg_sanity = Some(std::sync::Arc::new(std::sync::OnceLock::new()));
            Ok(cat)
        }
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
        // `max_oid` always carries a value >= `USER_OID_START` (the seed
        // bumper started there), so the `+1` cannot wrap and the result is
        // always >= `USER_OID_START + 1` — non-zero by construction.
        cat.next_oid = std::num::NonZeroU32::new(max_oid.saturating_add(1).max(USER_OID_START))
            .unwrap_or(USER_OID_START_NZ);

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
        for c in seed.pg_collation {
            cat.collation_by_qname
                .insert((c.collnamespace, c.collname.clone()), c.oid);
            cat.pg_collation.insert(c.oid, c);
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
            pg_collation: HashMap::new(),
            namespace_by_name: HashMap::new(),
            type_by_qname: HashMap::new(),
            class_by_qname: HashMap::new(),
            proc_by_qname: HashMap::new(),
            operator_by_qname: HashMap::new(),
            cast_by_pair: HashMap::new(),
            extension_by_name: HashMap::new(),
            collation_by_qname: HashMap::new(),
            search_path: Vec::new(),
            next_oid: USER_OID_START_NZ,
            #[cfg(feature = "pg_sanity")]
            pg_sanity: None,
            #[cfg(feature = "pg_sanity")]
            pg_sanity_tainted: false,
            #[cfg(feature = "pg_sanity")]
            pg_sanity_skip: false,
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

        let mut pg_collation: Vec<_> = self.pg_collation.values().cloned().collect();
        pg_collation.sort_by_key(|c| c.oid);

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
            pg_collation,
            search_path: self.search_path.clone(),
        }
    }

    /// Parse and apply all DDL statements in `sql`, mutating the catalog.
    pub fn apply_sql(&mut self, sql: &str) -> Result<(), DdlError> {
        let result = apply_sql_to(self, sql);
        #[cfg(feature = "pg_sanity")]
        {
            if sql_touches_extension(sql) {
                // PG sanity doesn't ship the same extension catalog as real PG
                // (no uuid-ossp, etc.) — and even ones it does ship would
                // need explicit `--extensions=...` wiring. Once an extension
                // is in play, downstream tables/queries reference types
                // PG sanity doesn't know about, so we taint the catalog and
                // skip every later mirror call.
                self.pg_sanity_tainted = true;
            } else if !self.pg_sanity_tainted && !self.pg_sanity_skip {
                self.run_pg_sanity_apply_check(sql, &result);
            }
        }
        result
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
        let (_analysis_sql, result) = self.analyze_with_sql(sql);

        #[cfg(feature = "pg_sanity")]
        if !self.pg_sanity_tainted && !self.pg_sanity_skip {
            self.run_pg_sanity_analyze_check(&_analysis_sql, &result);
        }

        result
    }

    /// Inner analyze that also returns the rewritten SQL handed to the
    /// static pass — used by [`Self::analyze`] to mirror the same string on
    /// PG sanity under `pg_sanity`.
    fn analyze_with_sql(&self, sql: &str) -> (String, Result<AnalyzedQuery, AnalyzeError>) {
        // PG's wording for each lex-level failure — kept verbatim so the
        // `pg_sanity` prefix check matches. `at or near "..."` echoes a
        // snippet of the offending token; we approximate that by taking
        // ~24 bytes starting at the LexError's reported position.
        fn format_lex_error_pg(e: &crate::lexer::LexError, sql: &str) -> String {
            use crate::lexer::LexError as L;
            let near = |pos: usize| -> String {
                let bytes = sql.as_bytes();
                let mut end = (pos + 24).min(bytes.len());
                // Don't slice mid-char.
                while end < bytes.len() && (bytes[end] & 0xC0) == 0x80 {
                    end += 1;
                }
                let slice = &sql[pos..end];
                // Stop at the first newline if any — PG's `at or near`
                // typically shows a short, single-line snippet.
                let slice = slice.split('\n').next().unwrap_or(slice);
                slice.to_string()
            };
            match e {
                L::UnclosedString { position } => format!(
                    "unterminated quoted string at or near \"{}\"",
                    near(*position)
                ),
                L::UnclosedBlockComment { position } => {
                    format!("unterminated /* comment at or near \"{}\"", near(*position))
                }
                L::UnclosedDollarQuote { tag: _, position } => format!(
                    "unterminated dollar-quoted string at or near \"{}\"",
                    near(*position)
                ),
                L::UnclosedQuotedIdentifier { position } => format!(
                    "unterminated quoted identifier at or near \"{}\"",
                    near(*position)
                ),
            }
        }
        let lex_output = match lex(sql) {
            Ok(l) => l,
            Err(e) => {
                // The lexer's own errors carry a `position` in the original
                // SQL — render them with a snippet so the user sees the
                // offending location. Install a guard with an empty
                // `LexOutput` so offset translation is the identity.
                let empty_lex = crate::param::LexOutput {
                    sql: sql.to_string(),
                    params: Vec::new(),
                    spreads: Vec::new(),
                    rewrites: Vec::new(),
                };
                let _guard = crate::error::DiagContextGuard::install(sql, &empty_lex);
                let position = e.position();
                let span = crate::error::SourceSpan::one_char_at(position);
                let message = format_lex_error_pg(&e, sql);
                let raw = crate::error::RawError::lex(message, Some(span));
                return (sql.to_string(), Err(raw.finalize_implicit()));
            }
        };

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
            match build_spread_sample_sql(&lex_output) {
                Ok(s) => s,
                Err(e) => return (lex_output.sql.clone(), Err(e)),
            }
        };

        // Install the diagnostic context so that error sites deep in the
        // analyzer can render snippet + caret + hint against the original
        // SQL. When the query has spreads, `analysis_sql` differs from
        // `lex_output.sql` (the lexer-rewritten form the post-lex offsets
        // refer to) — falling back to no context yields flat error messages
        // for those queries, which is acceptable until we extend the offset
        // map across spread expansion.
        let (columns, mut info_params, can_run_as_subquery) = if lex_output.spreads.is_empty() {
            let _guard = crate::error::DiagContextGuard::install(sql, &lex_output);
            match analyze_static(self, &analysis_sql, &param_nullability) {
                Ok(p) => p,
                Err(e) => return (analysis_sql, Err(e)),
            }
        } else {
            match analyze_static(self, &analysis_sql, &param_nullability) {
                Ok(p) => p,
                Err(e) => return (analysis_sql, Err(e)),
            }
        };

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
            return (
                analysis_sql.clone(),
                Err(AnalyzeError::Internal(format!(
                    "analyzer param count ({}) does not match lexer placeholder count ({}) \
                     for SQL: {analysis_sql}",
                    info_params.len(),
                    expected_param_count,
                ))),
            );
        }

        // Merge explicit $foo? / $foo! annotations from the lexer on top of
        // the analyzer's inferred nullability (explicit always wins).
        for (pi, &lexer_nullable) in info_params.iter_mut().zip(param_nullability.iter()) {
            if let Some(explicit) = lexer_nullable {
                pi.nullable = explicit;
            }
        }

        let fused = fuse(lex_output, columns, info_params, can_run_as_subquery);
        (analysis_sql, fused)
    }

    /// Allocate a fresh user-space OID. Returns the [`NonZeroU32`] directly so
    /// callers can re-tag it as the appropriate typed OID kind via
    /// `XxxOid::from_nonzero(...)` (or the equivalent `From<NonZeroU32>`
    /// impl) without the fallible `XxxOid::new(...).expect(...)` round-trip.
    ///
    /// Errors with [`DdlError::Internal`] only when the OID space (`u32`) is
    /// exhausted — practically unreachable in a single analyzer run since
    /// allocation starts at [`USER_OID_START`] (100_000) and steps by one.
    pub(crate) fn alloc_oid(&mut self) -> Result<std::num::NonZeroU32, DdlError> {
        let oid = self.next_oid;
        self.next_oid = self
            .next_oid
            .checked_add(1)
            .ok_or_else(|| DdlError::Internal("OID space exhausted".into()))?;
        Ok(oid)
    }

    /// Disable the `pg_sanity` cross-check on this catalog instance for
    /// the rest of its lifetime. Useful when the analyzer enforces a stricter
    /// rule than what PG sanity's wire-level `prepare` validates — e.g. ON
    /// CONFLICT target matching, which real PG rejects at planning time but
    /// PG sanity's `prepare` skips. Without the feature this is a no-op so
    /// callers can always invoke it without `#[cfg]`.
    pub fn skip_pg_sanity(&mut self) {
        #[cfg(feature = "pg_sanity")]
        {
            self.pg_sanity_skip = true;
        }
    }
}

// ─── PG sanity sanity check (feature-gated) ───────────────────────────────────

/// Quick check: does `sql` contain any `CREATE/ALTER/DROP EXTENSION`
/// statement? Used to skip the PG sanity mirror when the analyzer's embedded
/// extension support diverges from what PG sanity ships natively. We parse via
/// `pg_query` (cheap — `apply_sql_to` parses anyway) and inspect the AST so
/// a column or identifier named `extension` doesn't cause a false skip.
#[cfg(feature = "pg_sanity")]
fn sql_touches_extension(sql: &str) -> bool {
    use pg_query::protobuf::node;
    let Ok(parsed) = pg_query::parse(sql) else {
        return false;
    };
    parsed.protobuf.stmts.iter().any(|raw| {
        let Some(stmt) = raw.stmt.as_ref().and_then(|n| n.node.as_ref()) else {
            return false;
        };
        match stmt {
            node::Node::CreateExtensionStmt(_) | node::Node::AlterExtensionStmt(_) => true,
            node::Node::DropStmt(d) => {
                d.remove_type == pg_query::protobuf::ObjectType::ObjectExtension as i32
            }
            _ => false,
        }
    })
}

#[cfg(feature = "pg_sanity")]
impl PgCatalog {
    /// Lazily spawn the PG sanity mirror and run `f` against it. No-op if the
    /// catalog was built via [`PgCatalog::from_seed`] (where there's no
    /// reproducible live state).
    fn with_pg_sanity<R>(
        &self,
        f: impl FnOnce(&mut crate::pg_sanity::PgSanityServer) -> R,
    ) -> Option<R> {
        let cell = self.pg_sanity.as_ref()?;
        let mutex = cell.get_or_init(|| {
            let server = crate::pg_sanity::PgSanityServer::spawn()
                .unwrap_or_else(|e| panic!("pg_sanity: {e}"));
            std::sync::Mutex::new(server)
        });
        let mut guard = mutex.lock().expect("pg_sanity: pg sanity mutex poisoned");
        Some(f(&mut guard))
    }

    fn run_pg_sanity_apply_check(&self, sql: &str, result: &Result<(), DdlError>) {
        self.with_pg_sanity::<()>(|server| server.assert_apply_matches(sql, result));
    }

    fn run_pg_sanity_analyze_check(
        &self,
        analysis_sql: &str,
        result: &Result<AnalyzedQuery, AnalyzeError>,
    ) {
        self.with_pg_sanity::<()>(|server| server.assert_analyze_matches(analysis_sql, result));
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

    #[allow(dead_code)] // Used only by user-DDL `CREATE COLLATION` once implemented;
    // exists today so the test/internal feature surface is symmetric with
    // `insert_pg_*` for the other catalog tables.
    pub(crate) fn insert_pg_collation(&mut self, row: PgCollation) {
        self.collation_by_qname
            .insert((row.collnamespace, row.collname.clone()), row.oid);
        self.pg_collation.insert(row.oid, row);
    }

    /// Resolve a collation name (with optional schema) against the catalog.
    /// Walks the search path when `schema` is `None`. Returns `None` for
    /// names that don't match any registered collation — callers surface
    /// this as PG's `collation "x" does not exist` error.
    pub fn resolve_collation(&self, schema: Option<&str>, name: &str) -> Option<&PgCollation> {
        let candidate_schemas: Vec<PgNamespaceOid> = if let Some(s) = schema {
            self.namespace_oid(s).into_iter().collect()
        } else {
            let mut v = Vec::new();
            if let Some(pg_oid) = self.namespace_oid("pg_catalog")
                && !self.search_path.contains(&pg_oid)
            {
                v.push(pg_oid);
            }
            v.extend(self.search_path.iter().copied());
            v
        };
        for nsoid in candidate_schemas {
            if let Some(&oid) = self.collation_by_qname.get(&(nsoid, name.to_owned())) {
                return self.pg_collation.get(&oid);
            }
        }
        None
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
