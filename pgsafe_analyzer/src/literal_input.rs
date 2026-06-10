//! Parse-time validation of untyped string literals against a target type.
//!
//! When PostgreSQL coerces an `unknown` *constant* to a concrete type (an
//! explicit `'x'::T` cast, an operator/function argument, a CASE/COALESCE
//! branch, an INSERT/UPDATE assignment, a WHERE/LIMIT clause, …) it runs the
//! type's input function immediately, at parse-analysis time — so `'x'::int`
//! fails `prepare` with `invalid input syntax for type integer: "x"`. The
//! static analyzer mirrors that here for the types whose input grammar is
//! small and stable enough to model exactly.
//!
//! The contract is **conservative**: [`validate`] must never reject a string
//! PostgreSQL's input function would accept. Types whose grammar we don't
//! model (datetime values, geometric coordinates, inet, …) are accepted
//! wholesale — except for the empty string, which a fixed list of them is
//! known to reject (see [`EMPTY_INVALID`]). When in doubt, accept.
//!
//! Each rejection carries PG's message verbatim (the analyzer's error-message
//! contract): `invalid input syntax for type %s: "%s"`, `value "%s" is out of
//! range for type %s`, `malformed array literal: "%s"`, `malformed range
//! literal: "%s"`, `invalid input value for enum %s: "%s"`, or `invalid name
//! syntax` — all verified against PostgreSQL 18.

use crate::oid::PgTypeOid;
use crate::pg_catalog::{PgCatalog, TypCategory, TypType, oid};

/// Outcome of validating literal `content` against `target`: `Ok(())` when PG
/// would accept it (or we can't tell), `Err(message)` with PG's verbatim
/// parse-time error when it provably wouldn't.
pub(crate) fn validate(
    content: &str,
    target: PgTypeOid,
    snapshot: &PgCatalog,
) -> Result<(), String> {
    // Domain values are validated by the *base* type's input function, and
    // PG's message names the base type (`'x'::posint` → `… for type integer`).
    let target = snapshot.unwrap_domain(target);

    match target {
        oid::BOOL => return validate_bool(content),
        oid::INT2 => return validate_int(content, i16::MIN as i128, i16::MAX as i128, "smallint"),
        oid::INT4 => return validate_int(content, i32::MIN as i128, i32::MAX as i128, "integer"),
        oid::INT8 => return validate_int(content, i64::MIN as i128, i64::MAX as i128, "bigint"),
        oid::OID => return validate_oid(content),
        oid::FLOAT4 => return validate_float(content, "real"),
        oid::FLOAT8 => return validate_float(content, "double precision"),
        oid::NUMERIC => return validate_numeric(content),
        _ => {}
    }

    let Some(t) = snapshot.get_type(target) else {
        return Ok(());
    };

    // Enums: exact label match (no whitespace trimming, case-sensitive).
    if t.typtype == TypType::Enum {
        if snapshot.enum_labels_of(target).contains(&content) {
            return Ok(());
        }
        // PG renders the enum's name search-path aware (`st`, `s2.en2`).
        let name = crate::ddl::util::format_type_for_message(snapshot, target);
        return Err(format!(
            "invalid input value for enum {name}: \"{content}\""
        ));
    }

    // Ranges: must be `empty` (case-insensitive, surrounding whitespace
    // allowed) or start with `(` / `[`. Bound contents are not validated.
    if t.typtype == TypType::Range {
        let trimmed = content.trim_matches(|c: char| c.is_ascii_whitespace());
        if trimmed.eq_ignore_ascii_case("empty") || trimmed.starts_with(['(', '[']) {
            return Ok(());
        }
        return Err(format!("malformed range literal: \"{content}\""));
    }

    // Multiranges: the value must open with `{` after optional whitespace
    // (`'{}'` is the valid empty multirange). Member ranges aren't validated.
    if t.typtype == TypType::Multirange {
        let trimmed = content.trim_start_matches(|c: char| c.is_ascii_whitespace());
        if trimmed.starts_with('{') {
            return Ok(());
        }
        return Err(format!("malformed multirange literal: \"{content}\""));
    }

    // True arrays: after optional leading whitespace the value must open with
    // `{` (or `[` for the explicit-dimensions form `[1:2]={…}`). Element
    // contents are not validated. `oidvector`/`int2vector` share the Array
    // category but use their own space-separated input format — skip them
    // (they're exactly the types whose element doesn't point back via
    // `typarray`).
    if t.typcategory == TypCategory::Array
        && t.typelem
            .is_some_and(|e| snapshot.array_type_of(e) == Some(target))
    {
        let trimmed = content.trim_start_matches(|c: char| c.is_ascii_whitespace());
        if trimmed.starts_with('{') {
            return Ok(());
        }
        // The explicit-dimensions form is `[lo:hi]…={…}` — a `[` opener
        // without the `={` separator (e.g. `'[1,]'`) is malformed.
        if trimmed.starts_with('[') && trimmed.contains("={") {
            return Ok(());
        }
        return Err(format!("malformed array literal: \"{content}\""));
    }

    // Name-resolving and fixed-syntax pg_catalog builtins, keyed by name.
    if snapshot.namespace_name(t.typnamespace) != Some("pg_catalog") {
        return Ok(());
    }
    match t.typname.as_str() {
        "uuid" => validate_uuid(content),
        "json" | "jsonb" => validate_json(content),
        // The object-resolving reg* family parses the value as a (possibly
        // qualified) SQL identifier and resolves it at parse time. An
        // empty/whitespace-only string is never a valid name; for the two
        // members whose target catalog we fully model (relations and
        // functions) a *simple* unquoted name is also resolved here. The
        // rest (regtype's full type-name grammar, roles/collations/text
        // search objects we don't track) are accepted unchecked.
        name @ ("regproc" | "regprocedure" | "regoper" | "regoperator" | "regclass" | "regtype"
        | "regcollation" | "regconfig" | "regdictionary" | "regnamespace" | "regrole") => {
            let trimmed = content.trim_matches(|c: char| c.is_ascii_whitespace());
            if trimmed.is_empty() {
                return Err("invalid name syntax".to_string());
            }
            // All-digits is an OID literal — subject to oid's range check
            // (`'99…9'::regproc` → `value "…" is out of range for type oid`).
            // The check is on the *raw* content: reg* input functions only
            // take the OID path when the whole string is digits.
            if content.chars().all(|c| c.is_ascii_digit()) {
                if content.parse::<u64>().is_ok_and(|v| v <= u32::MAX as u64) {
                    return Ok(());
                }
                return Err(format!("value \"{content}\" is out of range for type oid"));
            }
            // Surrounding whitespace interacts with each reg* type's own
            // trimming rules (`' 42 '::regproc` is rejected, `' users '` is
            // not necessarily) — accept rather than model them.
            if trimmed != content {
                return Ok(());
            }
            if !matches!(name, "regclass" | "regproc" | "regtype") {
                return Ok(());
            }
            // Quoted / qualified forms need real identifier parsing — skip.
            if trimmed.contains(['"', '.']) {
                return Ok(());
            }
            // `regtype` parses its value with the full type-name grammar
            // (`'character varying'`, `'int[]'`, `'numeric(10,2)'`, …); only
            // a single bare identifier is simple enough to resolve here.
            if name == "regtype"
                && !trimmed
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                return Ok(());
            }
            // An unquoted name containing whitespace fails PG's identifier
            // splitting up front (`'1 day'::regclass` → invalid name syntax).
            if name != "regtype" && trimmed.chars().any(|c| c.is_ascii_whitespace()) {
                return Err("invalid name syntax".to_string());
            }
            // Unquoted identifiers fold to lowercase before lookup. System
            // objects (`pg_*`, information_schema) may be absent from the
            // snapshot — accept those rather than risk a false rejection.
            let folded = trimmed.to_ascii_lowercase();
            if folded.starts_with("pg_") || folded.starts_with("information_schema") {
                return Ok(());
            }
            match name {
                "regclass" if snapshot.resolve_table(None, &folded).is_none() => {
                    Err(format!("relation \"{folded}\" does not exist"))
                }
                // `regproc` (unlike `regprocedure`) requires the bare name to
                // resolve to exactly one function.
                "regproc" => match snapshot.find_functions(None, &folded).len() {
                    0 => Err(format!("function \"{folded}\" does not exist")),
                    1 => Ok(()),
                    _ => Err(format!("more than one function named \"{folded}\"")),
                },
                // A bare identifier for `regtype`: try the SQL-standard
                // aliases (`integer` → `int4`) then the catalog.
                "regtype" => {
                    let normalized = crate::ddl::util::normalize_type_name(&folded);
                    if snapshot.resolve_type_by_name(None, normalized).is_some()
                        || snapshot.resolve_type_by_name(None, &folded).is_some()
                    {
                        Ok(())
                    } else {
                        Err(format!("type \"{folded}\" does not exist"))
                    }
                }
                _ => Ok(()),
            }
        }
        // Datetime family: the full input grammar is out of scope, but two
        // slices are exactly checkable (verified against PG 18): the empty
        // string, and purely alphabetic tokens — PG's datetime lexer only
        // accepts those when they're one of the special keywords (`now`,
        // `today`, `epoch`, `infinity`, …); any other bare word is
        // `invalid input syntax`. Anything with digits or punctuation
        // (which can route to *other* messages, e.g. `time zone "a.m." not
        // recognized`, or be a valid value like `'now()'`) is accepted
        // unchecked. The message uses the input function's own type-name
        // string, which differs from `format_type` for the timestamp family.
        name @ ("date" | "time" | "timetz" | "timestamp" | "timestamptz" | "interval") => {
            let msg_name = match name {
                "timetz" => "time with time zone",
                "timestamptz" => "timestamp with time zone",
                other => other,
            };
            let keywords: &[&str] = match name {
                "time" | "timetz" => &["now", "allballs"],
                "interval" => &["infinity"],
                _ => &[
                    "now",
                    "today",
                    "tomorrow",
                    "yesterday",
                    "epoch",
                    "infinity",
                ],
            };
            let trimmed = content
                .trim_matches(|c: char| c.is_ascii_whitespace())
                .to_ascii_lowercase();
            // `'+infinity'` / `'-infinity'` are valid wherever `infinity` is.
            let unsigned = trimmed.strip_prefix(['+', '-']).unwrap_or(&trimmed);
            if keywords.contains(&unsigned) {
                return Ok(());
            }
            let purely_alphabetic = !trimmed.is_empty()
                && trimmed
                    .chars()
                    .all(|c| c.is_ascii_alphabetic() || c.is_ascii_whitespace());
            if trimmed.is_empty() || purely_alphabetic {
                return Err(format!(
                    "invalid input syntax for type {msg_name}: \"{content}\""
                ));
            }
            Ok(())
        }
        // Internal statistics / parse-tree types whose input functions
        // unconditionally refuse input. The message string is the input
        // function's own (note `pg_brin_minmax_multi_summary`'s drops the
        // prefix) — verified against PG 18.
        name @ ("pg_node_tree" | "pg_ndistinct" | "pg_dependencies" | "pg_mcv_list"
        | "pg_brin_bloom_summary" | "pg_brin_minmax_multi_summary" | "pg_ddl_command") => {
            let msg_name = match name {
                "pg_brin_minmax_multi_summary" => "brin_minmax_multi_summary",
                other => other,
            };
            Err(format!("cannot accept a value of type {msg_name}"))
        }
        // Network/geometric types — and the system identifier types — whose
        // input functions are too complex to model but are known to reject
        // the empty string. (No alphabetic shortcut here:
        // `'aabbccddeeff'::macaddr` is a valid MAC.)
        name @ ("macaddr" | "macaddr8" | "inet" | "cidr" | "point" | "lseg" | "box" | "path"
        | "polygon" | "circle" | "line" | "tid" | "xid" | "xid8" | "cid") => {
            if content
                .trim_matches(|c: char| c.is_ascii_whitespace())
                .is_empty()
            {
                return Err(format!(
                    "invalid input syntax for type {name}: \"{content}\""
                ));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

// ─── boolean ────────────────────────────────────────────────────────────────

/// Mirrors `parse_bool_with_len` (bool.c): case-insensitive prefixes of
/// `true`/`false`/`yes`/`no`, the exact-prefix family of `on`/`off` (where a
/// bare `o` is ambiguous and rejected), and single-character `1`/`0`.
/// Surrounding ASCII whitespace is trimmed.
fn validate_bool(content: &str) -> Result<(), String> {
    let v = content
        .trim_matches(|c: char| c.is_ascii_whitespace())
        .to_ascii_lowercase();
    let ok = match v.as_str() {
        "1" | "0" | "on" => true,
        _ if !v.is_empty() && ("true".starts_with(&v) || "false".starts_with(&v)) => true,
        _ if !v.is_empty() && ("yes".starts_with(&v) || "no".starts_with(&v)) => true,
        // `off` prefixes need length ≥ 2 (`o` alone is ambiguous with `on`).
        _ if v.len() >= 2 && "off".starts_with(&v) => true,
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(format!(
            "invalid input syntax for type boolean: \"{content}\""
        ))
    }
}

// ─── integers ───────────────────────────────────────────────────────────────

/// Digit-run parser shared by the integer/numeric validators: a non-empty
/// sequence of digits from `set`, with single underscores allowed *between*
/// digits (PG 16+). Returns the digits (underscores stripped) or `None` on a
/// malformed run; `rest` is left positioned after the run.
fn take_digits(s: &mut &str, radix: u32) -> Option<String> {
    let mut out = String::new();
    let mut chars = s.char_indices().peekable();
    let mut last_was_digit = false;
    let mut end = 0;
    while let Some(&(i, c)) = chars.peek() {
        if c.is_digit(radix) {
            out.push(c);
            last_was_digit = true;
            chars.next();
            end = i + c.len_utf8();
        } else if c == '_' && last_was_digit {
            // A `_` must be followed by another digit (no trailing or double
            // underscores).
            chars.next();
            match chars.peek() {
                Some(&(_, d)) if d.is_digit(radix) => {
                    last_was_digit = false; // consumed on next loop turn
                }
                _ => return None,
            }
        } else {
            break;
        }
    }
    if out.is_empty() {
        return None;
    }
    *s = &s[end..];
    Some(out)
}

/// Mirrors `pg_strtointNN` (numutils.c): optional surrounding whitespace, an
/// optional sign, then decimal digits or a `0x`/`0o`/`0b` radix prefix —
/// underscores allowed between digits — followed by a range check.
fn validate_int(content: &str, min: i128, max: i128, type_name: &str) -> Result<(), String> {
    let syntax_err = || format!("invalid input syntax for type {type_name}: \"{content}\"");
    let mut s = content.trim_matches(|c: char| c.is_ascii_whitespace());

    let negative = match s.as_bytes().first() {
        Some(b'-') => {
            s = &s[1..];
            true
        }
        Some(b'+') => {
            s = &s[1..];
            false
        }
        _ => false,
    };

    let (radix, digits) = parse_radix_digits(&mut s).ok_or_else(syntax_err)?;
    if !s.is_empty() {
        return Err(syntax_err());
    }

    match i128::from_str_radix(&digits, radix) {
        Ok(v) => {
            let v = if negative { -v } else { v };
            if v < min || v > max {
                return Err(format!(
                    "value \"{content}\" is out of range for type {type_name}"
                ));
            }
            Ok(())
        }
        // > 38 digits — definitely out of range for any integer type.
        Err(_) => Err(format!(
            "value \"{content}\" is out of range for type {type_name}"
        )),
    }
}

/// `0x`/`0o`/`0b`-prefixed or decimal digit run (with underscore rules).
/// Returns `(radix, digits)`; leaves `s` positioned after the run.
fn parse_radix_digits(s: &mut &str) -> Option<(u32, String)> {
    let lower = s.as_bytes();
    let radix = if lower.len() >= 2 && lower[0] == b'0' {
        match lower[1] {
            b'x' | b'X' => Some(16),
            b'o' | b'O' => Some(8),
            b'b' | b'B' => Some(2),
            _ => None,
        }
    } else {
        None
    };
    if let Some(r) = radix {
        *s = &s[2..];
        // After the prefix the first char must be a digit (no `0x_1`).
        let digits = take_digits(s, r)?;
        Some((r, digits))
    } else {
        let digits = take_digits(s, 10)?;
        Some((10, digits))
    }
}

/// Mirrors `uint32in_subr` (numutils.c): `strtoul` semantics — optional
/// whitespace and sign, decimal or `0x`-prefixed digits, **no** underscores
/// or `0o`/`0b`. The range check follows strtoul's wrap-around acceptance:
/// a value is in range when it fits `uint32`, or when it's negative and its
/// magnitude fits `int32` (so `'-1'::oid` is 4294967295 but
/// `'-4294967295'::oid` is out of range) — verified against PG 18.
fn validate_oid(content: &str) -> Result<(), String> {
    let syntax_err = || format!("invalid input syntax for type oid: \"{content}\"");
    let mut s = content.trim_matches(|c: char| c.is_ascii_whitespace());
    let negative = match s.strip_prefix(['+', '-']) {
        Some(rest) => {
            let neg = s.starts_with('-');
            s = rest;
            neg
        }
        None => false,
    };
    let (radix, digits) = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(rest) => (16, rest),
        None => (10, s),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return Err(syntax_err());
    }
    let in_range = match u64::from_str_radix(digits, radix) {
        Ok(v) if negative => v <= i32::MAX as u64 + 1,
        Ok(v) => v <= u32::MAX as u64,
        Err(_) => false, // > u64 digits — far out of range
    };
    if !in_range {
        return Err(format!("value \"{content}\" is out of range for type oid"));
    }
    Ok(())
}

// ─── floats ─────────────────────────────────────────────────────────────────

/// Mirrors `float8in` / `float4in`, which delegate to `strtod`: optional
/// whitespace and sign, then `inf`/`infinity`/`nan` (case-insensitive), a
/// decimal float (`1`, `.5`, `5.`, `1e3`, `5.e-3`), or a C99 hex float
/// (`0x1F`, `0x1.8p3`). No underscores, no range check (kept conservative).
fn validate_float(content: &str, type_name: &str) -> Result<(), String> {
    let err = || format!("invalid input syntax for type {type_name}: \"{content}\"");
    let mut s = content.trim_matches(|c: char| c.is_ascii_whitespace());
    if let Some(rest) = s.strip_prefix(['+', '-']) {
        s = rest;
    }
    let lower = s.to_ascii_lowercase();
    if lower == "inf" || lower == "infinity" || lower == "nan" {
        return Ok(());
    }
    // C99 hex float: 0x H* [. H*] [p [sign] D+] — at least one hex digit
    // overall.
    if let Some(hex) = lower.strip_prefix("0x") {
        let (mantissa, exp) = match hex.split_once('p') {
            Some((m, e)) => (m, Some(e)),
            None => (hex, None),
        };
        let (int_part, frac_part) = match mantissa.split_once('.') {
            Some((i, f)) => (i, f),
            None => (mantissa, ""),
        };
        if int_part.is_empty() && frac_part.is_empty() {
            return Err(err());
        }
        if !int_part.chars().all(|c| c.is_ascii_hexdigit())
            || !frac_part.chars().all(|c| c.is_ascii_hexdigit())
        {
            return Err(err());
        }
        if let Some(e) = exp {
            let e = e.strip_prefix(['+', '-']).unwrap_or(e);
            if e.is_empty() || !e.chars().all(|c| c.is_ascii_digit()) {
                return Err(err());
            }
        }
        return Ok(());
    }
    // Decimal float: D* [. D*] [e [sign] D+] — at least one digit in the
    // mantissa.
    let (mantissa, exp) = match lower.split_once('e') {
        Some((m, e)) => (m, Some(e)),
        None => (lower.as_str(), None),
    };
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((i, f)) => (i, f),
        None => (mantissa, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return Err(err());
    }
    if !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac_part.chars().all(|c| c.is_ascii_digit())
    {
        return Err(err());
    }
    if let Some(e) = exp {
        let e = e.strip_prefix(['+', '-']).unwrap_or(e);
        if e.is_empty() || !e.chars().all(|c| c.is_ascii_digit()) {
            return Err(err());
        }
    }
    Ok(())
}

// ─── numeric ────────────────────────────────────────────────────────────────

/// Mirrors `numeric_in`: optional whitespace and sign, then `NaN` /
/// `inf[inity]` (case-insensitive), a `0x`/`0o`/`0b` integer, or a decimal
/// value with optional fraction and `e`-exponent — underscores allowed
/// between digits everywhere. No precision limit check.
fn validate_numeric(content: &str) -> Result<(), String> {
    let err = || format!("invalid input syntax for type numeric: \"{content}\"");
    let mut s = content.trim_matches(|c: char| c.is_ascii_whitespace());
    if let Some(rest) = s.strip_prefix(['+', '-']) {
        s = rest;
    }
    let lower = s.to_ascii_lowercase();
    if lower == "nan" || lower == "inf" || lower == "infinity" {
        return Ok(());
    }

    // Radix-prefixed integer form.
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'0' && matches!(bytes[1] | 0x20, b'x' | b'o' | b'b') {
        let mut rest = s;
        match parse_radix_digits(&mut rest) {
            Some(_) if rest.is_empty() => return Ok(()),
            _ => return Err(err()),
        }
    }

    // Decimal: D+ [. D*] | . D+, then optional exponent.
    let mut rest = s;
    let int_digits = take_digits(&mut rest, 10);
    let mut any_digit = int_digits.is_some();
    if let Some(r) = rest.strip_prefix('.') {
        rest = r;
        if rest.starts_with(|c: char| c.is_ascii_digit()) {
            if take_digits(&mut rest, 10).is_none() {
                return Err(err());
            }
            any_digit = true;
        }
    }
    if !any_digit {
        return Err(err());
    }
    if let Some(r) = rest.strip_prefix(['e', 'E']) {
        rest = r;
        if let Some(r2) = rest.strip_prefix(['+', '-']) {
            rest = r2;
        }
        if take_digits(&mut rest, 10).is_none() {
            return Err(err());
        }
    }
    if !rest.is_empty() {
        return Err(err());
    }
    Ok(())
}

// ─── uuid ───────────────────────────────────────────────────────────────────

/// Mirrors `uuid_in`: exactly 32 hex digits, optionally wrapped in one pair
/// of braces, with hyphens allowed only on the standard group boundaries
/// (after hex digits 8, 12, 16, 20). No surrounding whitespace.
fn validate_uuid(content: &str) -> Result<(), String> {
    let err = || format!("invalid input syntax for type uuid: \"{content}\"");
    let s = match content.strip_prefix('{') {
        Some(rest) => rest.strip_suffix('}').ok_or_else(err)?,
        None => content,
    };
    let mut ndigits = 0u32;
    for c in s.chars() {
        if c == '-' {
            if !matches!(ndigits, 8 | 12 | 16 | 20) {
                return Err(err());
            }
        } else if c.is_ascii_hexdigit() {
            ndigits += 1;
            if ndigits > 32 {
                return Err(err());
            }
        } else {
            return Err(err());
        }
    }
    if ndigits != 32 {
        return Err(err());
    }
    Ok(())
}

// ─── json ───────────────────────────────────────────────────────────────────

/// Structural RFC 8259 validation, mirroring PG's `json_lex`/`parse_json`.
/// `\u` escapes only check for 4 hex digits — the jsonb-only surrogate-pair
/// and ` ` restrictions produce *different* PG messages and are
/// deliberately not modeled (accepted). The message carries no content:
/// PG emits a bare `invalid input syntax for type json` (the specifics go in
/// the DETAIL field, which the prefix contract doesn't cover).
fn validate_json(content: &str) -> Result<(), String> {
    let mut p = JsonParser {
        bytes: content.as_bytes(),
        pos: 0,
        depth: 0,
    };
    let ok = (|| {
        p.skip_ws();
        p.value()?;
        p.skip_ws();
        if p.pos != p.bytes.len() {
            return None;
        }
        Some(())
    })();
    match ok {
        Some(()) => Ok(()),
        None => Err("invalid input syntax for type json".to_string()),
    }
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
    depth: u32,
}

impl JsonParser<'_> {
    /// JSON whitespace: space, tab, LF, CR.
    fn skip_ws(&mut self) {
        while matches!(self.bytes.get(self.pos), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn eat(&mut self, b: u8) -> Option<()> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Some(())
        } else {
            None
        }
    }

    fn value(&mut self) -> Option<()> {
        // Beyond any plausible real document the cost/benefit flips; PG's
        // own limit is the stack guard. Accept by consuming the rest.
        if self.depth > 256 {
            self.pos = self.bytes.len();
            return Some(());
        }
        match self.peek()? {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => self.string(),
            b't' => self.keyword(b"true"),
            b'f' => self.keyword(b"false"),
            b'n' => self.keyword(b"null"),
            _ => self.number(),
        }
    }

    fn keyword(&mut self, kw: &[u8]) -> Option<()> {
        if self.bytes[self.pos..].starts_with(kw) {
            self.pos += kw.len();
            Some(())
        } else {
            None
        }
    }

    fn object(&mut self) -> Option<()> {
        self.eat(b'{')?;
        self.depth += 1;
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            self.depth -= 1;
            return Some(());
        }
        loop {
            self.skip_ws();
            self.string()?;
            self.skip_ws();
            self.eat(b':')?;
            self.skip_ws();
            self.value()?;
            self.skip_ws();
            match self.peek()? {
                b',' => {
                    self.pos += 1;
                }
                b'}' => {
                    self.pos += 1;
                    self.depth -= 1;
                    return Some(());
                }
                _ => return None,
            }
        }
    }

    fn array(&mut self) -> Option<()> {
        self.eat(b'[')?;
        self.depth += 1;
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            self.depth -= 1;
            return Some(());
        }
        loop {
            self.skip_ws();
            self.value()?;
            self.skip_ws();
            match self.peek()? {
                b',' => {
                    self.pos += 1;
                }
                b']' => {
                    self.pos += 1;
                    self.depth -= 1;
                    return Some(());
                }
                _ => return None,
            }
        }
    }

    fn string(&mut self) -> Option<()> {
        self.eat(b'"')?;
        loop {
            match self.peek()? {
                b'"' => {
                    self.pos += 1;
                    return Some(());
                }
                b'\\' => {
                    self.pos += 1;
                    match self.peek()? {
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => self.pos += 1,
                        b'u' => {
                            self.pos += 1;
                            for _ in 0..4 {
                                if !self.peek()?.is_ascii_hexdigit() {
                                    return None;
                                }
                                self.pos += 1;
                            }
                        }
                        _ => return None,
                    }
                }
                // Unescaped control characters are rejected by PG's lexer.
                c if c < 0x20 => return None,
                _ => self.pos += 1,
            }
        }
    }

    fn number(&mut self) -> Option<()> {
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        // Integer part: `0` or [1-9][0-9]* — no leading zeros.
        match self.peek()? {
            b'0' => self.pos += 1,
            b'1'..=b'9' => {
                while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    self.pos += 1;
                }
            }
            _ => return None,
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !self.peek()?.is_ascii_digit() {
                return None;
            }
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !self.peek()?.is_ascii_digit() {
                return None;
            }
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    // Pure-string validators are testable without a catalog; the
    // catalog-dependent paths (enum, array, range, reg*) are covered by the
    // analyzer's query tests.
    use super::*;

    #[test]
    fn bool_inputs() {
        for ok in ["t", "tr", "TRUE", " yes ", "of", "off", "1", "0", "on", "n"] {
            assert!(validate_bool(ok).is_ok(), "{ok:?} should be valid");
        }
        for bad in ["", "o", "10", "x", "tru e", "onn"] {
            assert!(validate_bool(bad).is_err(), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn int_inputs() {
        let v = |s| validate_int(s, i32::MIN as i128, i32::MAX as i128, "integer");
        for ok in [
            " 42 ",
            "+42",
            "-2147483648",
            "0x1F",
            "0o17",
            "0b101",
            "1_000",
            "0x1_F",
        ] {
            assert!(v(ok).is_ok(), "{ok:?} should be valid");
        }
        for bad in ["", "- 42", "42abc", "0x", "1__0", "_1", "1_", "0x_1"] {
            assert_eq!(
                v(bad).unwrap_err(),
                format!("invalid input syntax for type integer: \"{bad}\""),
            );
        }
        assert_eq!(
            v("2147483648").unwrap_err(),
            "value \"2147483648\" is out of range for type integer"
        );
        assert_eq!(
            v("0xFFFFFFFF").unwrap_err(),
            "value \"0xFFFFFFFF\" is out of range for type integer"
        );
    }

    #[test]
    fn float_inputs() {
        let v = |s| validate_float(s, "double precision");
        for ok in [
            "1",
            ".5",
            "5.",
            "1e3",
            "5.e-3",
            "inf",
            "-Infinity",
            "NaN",
            "0x1F",
            "0x1.8p3",
        ] {
            assert!(v(ok).is_ok(), "{ok:?} should be valid");
        }
        for bad in ["", "1e", "1_000", "x", "1.2.3", "0x", "1e+"] {
            assert!(v(bad).is_err(), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn numeric_inputs() {
        for ok in [
            "1_000.5_0",
            "0x1F",
            "NaN",
            " inf ",
            "-Infinity",
            "1e10",
            ".5",
            "5.",
            "1.5e+3",
        ] {
            assert!(validate_numeric(ok).is_ok(), "{ok:?} should be valid");
        }
        for bad in ["", "1e", "1.2.3", "hello", "0x", "5..", "._5"] {
            assert!(validate_numeric(bad).is_err(), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn uuid_inputs() {
        for ok in [
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
            "A0EEBC999C0B4EF8BB6D6BB9BD380A11",
            "{a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11}",
        ] {
            assert!(validate_uuid(ok).is_ok(), "{ok:?} should be valid");
        }
        for bad in [
            "",
            "xyz",
            "a0-eebc999c0b4ef8bb6d6bb9bd380a11",
            " a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11 ",
            "a0eebc999c0b4ef8bb6d6bb9bd380a111",
        ] {
            assert!(validate_uuid(bad).is_err(), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn json_inputs() {
        for ok in [
            "{}",
            " {\"a\": [1, -0.5e+3, true, null, \"\\u00ff\"]} ",
            "1.5e3",
            "\"x\"",
            "[1 , 2]",
            "-0",
        ] {
            assert!(validate_json(ok).is_ok(), "{ok:?} should be valid");
        }
        for bad in [
            "",
            "  ",
            "01",
            "nullx",
            "[1,]",
            "{\"a\"}",
            "\"\\u00zz\"",
            "{1:2}",
            "'x'",
        ] {
            assert!(validate_json(bad).is_err(), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn oid_inputs() {
        for ok in [" 42 ", "-1", "0x10", "+7"] {
            assert!(validate_oid(ok).is_ok(), "{ok:?} should be valid");
        }
        for bad in ["", "1_0", "42x", "0x", "hello"] {
            assert!(validate_oid(bad).is_err(), "{bad:?} should be invalid");
        }
    }
}
