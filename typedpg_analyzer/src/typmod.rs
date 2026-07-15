//! Codec for `pg_attribute.atttypmod` / `pg_type.typtypmod`.
//!
//! PostgreSQL stores parametric type modifiers (the `n` in `varchar(n)`, the
//! `(p,s)` in `numeric(p,s)`, the dimension count in `vector(N)`, …) as a
//! single packed `int32`. The encoding depends on the type:
//!
//! | typname                                       | encoding                                  |
//! |-----------------------------------------------|-------------------------------------------|
//! | `varchar`, `bpchar`, `char`                   | `n + 4` (VARHDRSZ)                        |
//! | `numeric`                                     | `((p << 16) \| (s & 0xFFFF)) + 4`         |
//! | `time`, `timetz`, `timestamp`, `timestamptz`  | `p` (precision, 0–6)                      |
//! | `interval`                                    | `p` (we ignore the field-mask bits)       |
//! | `bit`, `varbit`                               | `n`                                       |
//! | `vector` (pgvector)                           | `n` (dimension)                           |
//!
//! `None` represents PG's `-1` sentinel ("no typmod").
//!
//! Functions in this module never panic on invalid AST input — they return a
//! `DdlError::UnsupportedDdl` so the caller can surface a clear migration
//! error.
//!
//! Decoding is mostly used for diagnostics (overflow messages) and to surface
//! structured info (precision, scale, dimension) to consumers.
//!
//! Behaviour when the type is not in the table above: encoding silently
//! returns `Ok(None)` so users of custom types with their own typmodin
//! function don't break the migration; we just don't track typmod for those.

use pg_query::protobuf::{Node, node};

use crate::ddl::DdlError;
use crate::error::AnalyzeError;
use crate::oid::PgTypeOid;
use crate::pg_catalog::{PgCatalog, oid as builtin_oid};

const VARHDRSZ: i32 = 4;
const MAX_NUMERIC_PRECISION: i32 = 1000;
const MAX_TIMESTAMP_PRECISION: i32 = 6;
const MAX_VECTOR_DIM: i32 = 16000;

/// Decoded view of a typmod, type-aware. `None` and the catch-all
/// `Other(i32)` keep the structure honest for types we don't model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodedTypmod {
    /// No typmod (PG's `-1`).
    None,
    /// Length-bounded string / bit type: `varchar(n)`, `bpchar(n)`, `bit(n)`.
    Length(i32),
    /// Decimal precision/scale: `numeric(p, s)`.
    Numeric { precision: i32, scale: i32 },
    /// Date/time precision: `timestamp(p)`, `time(p)`, `interval(p)`.
    Precision(i32),
    /// pgvector dimension count.
    VectorDim(i32),
    /// Recognised type whose typmod we don't decode further (or one whose
    /// shape isn't in the table above).
    Other(i32),
}

/// Encode an AST `typmods: Vec<Node>` into the packed `i32` PG would store.
///
/// `Ok(None)` means either (a) no typmods supplied (`varchar` plain) or (b)
/// the type is not in our supported table. `Ok(Some(v))` is the encoded
/// value. `Err` is for invalid inputs (wrong arity, out of range).
pub fn encode(
    snapshot: &PgCatalog,
    type_oid: PgTypeOid,
    typmods: &[Node],
) -> Result<Option<i32>, DdlError> {
    if typmods.is_empty() {
        return Ok(None);
    }

    let typname = snapshot.get_type(type_oid).map(|t| t.typname.as_str());
    let pgvector = is_pgvector_type(snapshot, type_oid);

    // Pull integer values out of each typmod node. PG's parser only emits
    // `AConst::Integer` for numeric typmods; anything else is invalid.
    let raw: Result<Vec<i32>, DdlError> = typmods.iter().map(extract_int).collect();
    let raw = raw?;

    if pgvector {
        return encode_vector(&raw).map(Some);
    }

    match (type_oid, typname) {
        // PG distinguishes `varchar` and `character` (bpchar) in the
        // length-validation error: `length for type varchar must be at
        // least 1` vs `length for type character …`. Mirror that so the
        // `pglite_sanity` mirror's prefix check matches.
        (builtin_oid::VARCHAR, _) => encode_length(&raw, "varchar").map(Some),
        (builtin_oid::BPCHAR, _) => encode_length(&raw, "character").map(Some),
        (_, Some("char")) => encode_length(&raw, "character").map(Some),
        (builtin_oid::NUMERIC, _) => encode_numeric(&raw).map(Some),
        (_, Some("time" | "timetz" | "timestamp" | "timestamptz" | "interval")) => {
            encode_precision(&raw).map(Some)
        }
        (_, Some("bit" | "varbit")) => encode_length(&raw, "bit").map(Some),
        // Unknown / user-defined parametric type — silently drop the typmod
        // so we don't fail migrations using bespoke typmodin functions.
        _ => Ok(None),
    }
}

/// Decode a packed typmod into a structured form. Returns `DecodedTypmod::None`
/// for `None` input.
pub fn decode(snapshot: &PgCatalog, type_oid: PgTypeOid, typmod: Option<i32>) -> DecodedTypmod {
    let Some(t) = typmod else {
        return DecodedTypmod::None;
    };

    if is_pgvector_type(snapshot, type_oid) {
        return DecodedTypmod::VectorDim(t);
    }

    let typname = snapshot.get_type(type_oid).map(|x| x.typname.as_str());
    match (type_oid, typname) {
        (builtin_oid::VARCHAR | builtin_oid::BPCHAR, _) => DecodedTypmod::Length(t - VARHDRSZ),
        (_, Some("char")) => DecodedTypmod::Length(t - VARHDRSZ),
        (builtin_oid::NUMERIC, _) => {
            let inner = t - VARHDRSZ;
            let precision = (inner >> 16) & 0xFFFF;
            let scale_raw = inner & 0xFFFF;
            // Scale is signed (PG allows negative scale). Sign-extend the
            // 16-bit field.
            let scale = if scale_raw & 0x8000 != 0 {
                scale_raw | !0xFFFF
            } else {
                scale_raw
            };
            DecodedTypmod::Numeric { precision, scale }
        }
        (_, Some("time" | "timetz" | "timestamp" | "timestamptz" | "interval")) => {
            DecodedTypmod::Precision(t)
        }
        (_, Some("bit" | "varbit")) => DecodedTypmod::Length(t),
        _ => DecodedTypmod::Other(t),
    }
}

// ─── Per-type encoders ─────────────────────────────────────────────────────

fn encode_length(raw: &[i32], kind: &str) -> Result<i32, DdlError> {
    if raw.len() != 1 {
        return Err(DdlError::UnsupportedDdl(format!(
            "{kind} type takes exactly one length argument, got {}",
            raw.len()
        )));
    }
    let n = raw[0];
    if n < 1 {
        return Err(DdlError::UnsupportedDdl(format!(
            "length for type {kind} must be at least 1 (got {n})"
        )));
    }
    Ok(n + VARHDRSZ)
}

fn encode_numeric(raw: &[i32]) -> Result<i32, DdlError> {
    let (precision, scale) = match raw.len() {
        1 => (raw[0], 0),
        2 => (raw[0], raw[1]),
        n => {
            return Err(DdlError::UnsupportedDdl(format!(
                "numeric type takes 1 or 2 arguments, got {n}"
            )));
        }
    };
    if !(1..=MAX_NUMERIC_PRECISION).contains(&precision) {
        return Err(DdlError::UnsupportedDdl(format!(
            "NUMERIC precision {precision} must be between 1 and {MAX_NUMERIC_PRECISION}"
        )));
    }
    if scale < -MAX_NUMERIC_PRECISION || scale > precision {
        return Err(DdlError::UnsupportedDdl(format!(
            "NUMERIC scale {scale} must be between {} and {precision}",
            -MAX_NUMERIC_PRECISION
        )));
    }
    Ok(((precision << 16) | (scale & 0xFFFF)) + VARHDRSZ)
}

fn encode_precision(raw: &[i32]) -> Result<i32, DdlError> {
    if raw.len() != 1 {
        return Err(DdlError::UnsupportedDdl(format!(
            "type takes exactly one precision argument, got {}",
            raw.len()
        )));
    }
    let p = raw[0];
    if !(0..=MAX_TIMESTAMP_PRECISION).contains(&p) {
        return Err(DdlError::UnsupportedDdl(format!(
            "precision {p} must be between 0 and {MAX_TIMESTAMP_PRECISION}"
        )));
    }
    Ok(p)
}

fn encode_vector(raw: &[i32]) -> Result<i32, DdlError> {
    if raw.len() != 1 {
        return Err(DdlError::UnsupportedDdl(format!(
            "vector type takes exactly one dimension argument, got {}",
            raw.len()
        )));
    }
    let n = raw[0];
    if !(1..=MAX_VECTOR_DIM).contains(&n) {
        return Err(DdlError::UnsupportedDdl(format!(
            "vector dimension {n} must be between 1 and {MAX_VECTOR_DIM}"
        )));
    }
    Ok(n)
}

// ─── Helpers ──────────────────────────────────────────────────────────────

fn extract_int(node: &Node) -> Result<i32, DdlError> {
    match node.node.as_ref() {
        Some(node::Node::AConst(c)) => match c.val.as_ref() {
            Some(pg_query::protobuf::a_const::Val::Ival(i)) => Ok(i.ival),
            _ => Err(DdlError::UnsupportedDdl(
                "non-integer typmod argument".into(),
            )),
        },
        _ => Err(DdlError::UnsupportedDdl(
            "non-literal typmod argument".into(),
        )),
    }
}

/// True when `oid` resolves to pgvector's `vector` type (any namespace,
/// since pgvector typically lives in `public` but users can install it
/// elsewhere).
fn is_pgvector_type(snapshot: &PgCatalog, oid: PgTypeOid) -> bool {
    snapshot
        .get_type(oid)
        .map(|t| t.typname == "vector")
        .unwrap_or(false)
}

// ─── Validation: literal vs column typmod ─────────────────────────────────

/// Check whether assigning `value` (a SQL literal node) to a column with
/// `(type_oid, typmod)` would violate the typmod's bound. Returns
/// `Some(error)` when we can prove a violation at compile time; `None`
/// otherwise (param refs, expressions, or types we don't validate).
pub fn check_literal_assignment(
    snapshot: &PgCatalog,
    type_oid: PgTypeOid,
    typmod: Option<i32>,
    value: &Node,
) -> Option<AnalyzeError> {
    let decoded = decode(snapshot, type_oid, typmod);
    match decoded {
        DecodedTypmod::Length(n)
            if matches!(
                (
                    type_oid,
                    snapshot.get_type(type_oid).map(|t| t.typname.as_str())
                ),
                (builtin_oid::VARCHAR | builtin_oid::BPCHAR, _) | (_, Some("char"))
            ) =>
        {
            let s = string_literal(value)?;
            if s.chars().count() as i32 > n {
                // Match PG's wording: SQL-standard names instead of the
                // catalog `typname` so error messages line up across
                // tooling (e.g. `\d+` output / sqlstate-driven UIs).
                let typ_label = match type_oid {
                    builtin_oid::VARCHAR => "character varying".to_string(),
                    builtin_oid::BPCHAR => "character".to_string(),
                    _ => snapshot
                        .get_type(type_oid)
                        .map(|t| t.typname.clone())
                        .unwrap_or_else(|| "character varying".into()),
                };
                Some(AnalyzeError::Invalid(format!(
                    "value too long for type {typ_label}({n})"
                )))
            } else {
                None
            }
        }
        DecodedTypmod::Numeric { precision, scale } => {
            let raw = numeric_literal_string(value)?;
            // Content first, magnitude second — PG runs numeric_in before
            // applying the typmod, so a malformed literal is 22P02
            // (`invalid input syntax`), never `numeric field overflow`.
            if let Err(msg) = crate::literal_input::validate_numeric(&raw) {
                return Some(AnalyzeError::InvalidLiteral(msg));
            }
            check_numeric_overflow(&raw, precision, scale)
        }
        DecodedTypmod::VectorDim(n) => {
            let count = vector_literal_dim_count(value)?;
            if count != n as usize {
                Some(AnalyzeError::Invalid(format!(
                    "expected {n} dimensions, not {count}"
                )))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn string_literal(node: &Node) -> Option<&str> {
    match node.node.as_ref()? {
        node::Node::AConst(c) => match c.val.as_ref()? {
            pg_query::protobuf::a_const::Val::Sval(s) => Some(s.sval.as_str()),
            _ => None,
        },
        // Allow `'abc'::varchar(N)` — drill through the cast so the literal
        // length still gets checked against the assignment target.
        node::Node::TypeCast(tc) => tc.arg.as_deref().and_then(string_literal),
        _ => None,
    }
}

fn numeric_literal_string(node: &Node) -> Option<String> {
    match node.node.as_ref()? {
        node::Node::AConst(c) => match c.val.as_ref()? {
            pg_query::protobuf::a_const::Val::Ival(i) => Some(i.ival.to_string()),
            pg_query::protobuf::a_const::Val::Fval(f) => Some(f.fval.clone()),
            pg_query::protobuf::a_const::Val::Sval(s) => Some(s.sval.clone()),
            _ => None,
        },
        node::Node::TypeCast(tc) => tc.arg.as_deref().and_then(numeric_literal_string),
        _ => None,
    }
}

fn check_numeric_overflow(raw: &str, precision: i32, scale: i32) -> Option<AnalyzeError> {
    let s = raw.trim();
    let s = s.strip_prefix(['+', '-']).unwrap_or(s);
    let int_part = match s.find('.') {
        Some(idx) => &s[..idx],
        None => s,
    };
    let mut int_digits = int_part.trim_start_matches('0').len() as i32;
    if int_digits == 0 {
        int_digits = 0;
    }
    // PG: int_part_digits must fit in (precision - scale).
    let max_int_digits = precision - scale;
    if int_digits > max_int_digits {
        return Some(AnalyzeError::Invalid(format!(
            "numeric field overflow: a field with precision {precision}, scale {scale} \
             must round to an absolute value less than 10^{max_int_digits}"
        )));
    }
    None
}

fn vector_literal_dim_count(node: &Node) -> Option<usize> {
    match node.node.as_ref()? {
        // ARRAY[1, 2, 3]::vector
        node::Node::AArrayExpr(arr) => Some(arr.elements.len()),
        // '[1,2,3]'::vector
        node::Node::TypeCast(tc) => {
            // First try drilling into the cast argument as an array.
            if let Some(arg) = tc.arg.as_deref()
                && let count = vector_literal_dim_count(arg)
                && count.is_some()
            {
                return count;
            }
            // Or a string literal in pgvector's `[1,2,3]` syntax.
            let s = string_literal(tc.arg.as_deref()?)?;
            parse_vector_string(s)
        }
        node::Node::AConst(_) => {
            let s = string_literal(node)?;
            parse_vector_string(s)
        }
        _ => None,
    }
}

fn parse_vector_string(s: &str) -> Option<usize> {
    let trimmed = s.trim();
    let inner = trimmed.strip_prefix('[')?.strip_suffix(']')?;
    if inner.trim().is_empty() {
        return Some(0);
    }
    Some(inner.split(',').count())
}
