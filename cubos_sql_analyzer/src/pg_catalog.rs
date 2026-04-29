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
    PgCastOid, PgClassOid, PgEnumOid, PgExtensionOid, PgGenericOid, PgNamespaceOid, PgOperatorOid,
    PgProcOid, PgTypeOid,
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

/// `pg_class.relkind`. PG chars used here: `r` table, `v` view, `m` matview,
/// `p` partitioned table, `c` composite type. Sequences/indexes/foreign tables
/// are not tracked by the analyzer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelKind {
    #[serde(rename = "r")]
    Table,
    #[serde(rename = "v")]
    View,
    #[serde(rename = "m")]
    MaterializedView,
    #[serde(rename = "p")]
    Partitioned,
    #[serde(rename = "c")]
    CompositeType,
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

/// `pg_class`: a relation (table, view, matview, partitioned table, composite
/// type's row type).
///
/// `relviewdef` is non-PG: it stores the protobuf-encoded `pg_query::Node` of
/// the view's SELECT (resolved against the catalog at creation time) so we
/// can re-analyze the view after `ALTER COLUMN TYPE` without the original
/// SQL. View-to-table dependencies live in `pg_depend`.
///
/// `viewbindings` is the side-table that resolves every name slot in the AST
/// to a catalog OID at view creation time. RENAME / SET SCHEMA become no-ops
/// on the AST: the OIDs in the bindings remain valid and the deparser uses
/// them to look up *current* names. See [`ViewBinding`] for details.
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
    /// Non-PG: serialized AST of the SELECT for views/matviews. Empty for
    /// other relkinds. (Tuple-positional now, so always emitted; empty
    /// rows still serialize as the empty base64 string.)
    #[serde(with = "serde_base64")]
    pub relviewdef: Vec<u8>,
    /// Non-PG: per-view side-table mapping each name slot in `relviewdef` to
    /// the catalog OID it resolved to at creation time. Walked in the same
    /// pre-order as the binding emitter; consumed in lockstep by the rewrite
    /// pass before deparse / reanalysis. Empty for non-view relations.
    pub viewbindings: Vec<ViewBinding>,
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
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ViewBinding {
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
}

/// `pg_aggregate`: extra metadata for aggregate `pg_proc` rows.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgAggregate {
    /// FK `pg_proc.oid`.
    pub aggfnoid: PgProcOid,
    /// FK `pg_type.oid` of the aggregate's effective return type when a
    /// finalfn is present. `None` when there is no finalfn (return type
    /// comes from `pg_proc.prorettype`).
    #[serde(with = "crate::oid::oid_or_zero")]
    pub aggfinaltype: Option<PgTypeOid>,
}

/// `pg_operator`: a registered operator.
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
    /// FK `pg_type.oid`.
    pub oprresult: PgTypeOid,
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
            search_path: self.search_path.clone(),
        }
    }

    /// Parse and apply all DDL statements in `sql`, mutating the catalog.
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
