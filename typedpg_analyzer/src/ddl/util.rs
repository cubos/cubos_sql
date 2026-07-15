//! Shared utilities for DDL interpretation.

use pg_query::protobuf::{Node, RangeVar, TypeName, node};

use crate::ddl::DdlError;
use crate::oid::{PgCastOid, PgNamespaceOid, PgTypeOid};
use crate::pg_catalog::{
    CastContext, CastMethod, PgCast, PgCatalog, PgNamespace, oid as builtin_oid,
};
use crate::qualified_name::QualifiedName;

/// Extract the (schema, name) pair from a `RangeVar`.
/// If no schema is specified, defaults to the first entry in `search_path`.
pub fn range_var_names(rv: &RangeVar, snapshot: &PgCatalog) -> (String, String) {
    let schema = if rv.schemaname.is_empty() {
        snapshot
            .search_path
            .first()
            .and_then(|&oid| snapshot.namespace_name(oid).map(str::to_owned))
            .unwrap_or_else(|| "public".to_owned())
    } else {
        rv.schemaname.clone()
    };
    (schema, rv.relname.clone())
}

/// Extract (schema, name) from a list of name nodes (e.g., `domainname`,
/// `type_name` in DDL). Handles both `["name"]` and `["schema", "name"]`
/// forms.
pub fn extract_names(names: &[Node], snapshot: &PgCatalog) -> (String, String) {
    let parts: Vec<&str> = names
        .iter()
        .filter_map(|n| match n.node.as_ref()? {
            node::Node::String(s) => Some(s.sval.as_str()),
            _ => None,
        })
        .collect();

    match parts.as_slice() {
        [schema, name] => ((*schema).to_owned(), (*name).to_owned()),
        [name] => {
            let schema = snapshot
                .search_path
                .first()
                .and_then(|&oid| snapshot.namespace_name(oid).map(str::to_owned))
                .unwrap_or_else(|| "public".to_owned());
            (schema, (*name).to_owned())
        }
        _ => ("public".to_owned(), String::new()),
    }
}

/// Extract a schema-qualified key from name nodes.
pub fn names_key(names: &[Node], snapshot: &PgCatalog) -> QualifiedName {
    let (schema, name) = extract_names(names, snapshot);
    QualifiedName::new(schema, name)
}

/// Look up (or implicitly create) the OID of the named namespace.
///
/// PG would error on a missing schema, but the analyzer historically tolerates
/// `CREATE TABLE my_schema.foo` without a prior `CREATE SCHEMA my_schema`. We
/// keep that leniency by registering the schema on demand, allocating a fresh
/// OID. Callers that *need* strict checks (e.g. `ALTER … RENAME TO`) should
/// look at [`PgCatalog::namespace_oid`] directly and surface their own errors.
pub fn ensure_namespace(interp: &mut PgCatalog, name: &str) -> Result<PgNamespaceOid, DdlError> {
    if let Some(oid) = interp.namespace_oid(name) {
        return Ok(oid);
    }
    let oid = PgNamespaceOid::from_nonzero(interp.alloc_oid()?);
    interp.insert_pg_namespace(PgNamespace {
        oid,
        nspname: name.to_owned(),
    });
    Ok(oid)
}

/// Extract a `(nspoid, name)` pair, creating the namespace if it doesn't
/// exist yet. Convenience wrapper around `extract_names` + `ensure_namespace`
/// for DDL handlers that are about to insert a row.
pub fn ensure_qualified_name(
    interp: &mut PgCatalog,
    names: &[Node],
) -> Result<(PgNamespaceOid, String), DdlError> {
    let (schema, name) = extract_names(names, interp);
    Ok((ensure_namespace(interp, &schema)?, name))
}

/// Same as `ensure_qualified_name` but for `RangeVar` inputs.
pub fn ensure_range_var(
    interp: &mut PgCatalog,
    rv: &RangeVar,
) -> Result<(PgNamespaceOid, String), DdlError> {
    let (schema, name) = range_var_names(rv, interp);
    Ok((ensure_namespace(interp, &schema)?, name))
}

/// Resolve a `TypeName` AST node to a type OID in the snapshot.
///
/// Handles:
/// - Qualified names: `pg_catalog.int4`
/// - Unqualified names: `int4`, `text`, `uuid`
/// - Array bounds: `int4[]` → array element type OID
/// - Shorthand aliases: `integer` → `int4`, `bigint` → `int8`, etc.
pub fn resolve_type_name(tn: &TypeName, snapshot: &PgCatalog) -> Option<PgTypeOid> {
    let parts: Vec<&str> = tn
        .names
        .iter()
        .filter_map(|n| match n.node.as_ref()? {
            node::Node::String(s) => Some(s.sval.as_str()),
            _ => None,
        })
        .collect();

    let (schema, raw_name) = match parts.as_slice() {
        [schema, name] => (Some(*schema), *name),
        [name] => (None, *name),
        _ => return None,
    };

    let name = normalize_type_name(raw_name);
    let base_oid = snapshot.resolve_type_by_name(schema, name).map(|t| t.oid)?;

    if !tn.array_bounds.is_empty() {
        return snapshot.array_type_of(base_oid);
    }
    Some(base_oid)
}

/// Normalize PostgreSQL type name aliases to their canonical form.
pub(crate) fn normalize_type_name(name: &str) -> &str {
    match name {
        "integer" | "int" => "int4",
        "smallint" => "int2",
        "bigint" => "int8",
        "real" => "float4",
        "double precision" | "double" => "float8",
        "boolean" => "bool",
        "character varying" | "varchar" => "varchar",
        "character" | "char" => "bpchar",
        "decimal" | "numeric" => "numeric",
        "serial" => "int4",
        "bigserial" => "int8",
        "smallserial" => "int2",
        other => other,
    }
}

/// Render a type OID into PG's user-facing name for diagnostic messages.
///
/// Mirrors `format_type_extended` in `src/backend/utils/adt/format_type.c`
/// (PG 18) with `flags = 0` (no typmod, no force-qualify):
///
/// 1. Arrays: when the type's `typelem` points at a real array element
///    type, render as `<element>[]` (skipping pseudo-arrays / plain-storage
///    arrays like `oidvector`, same check PG does).
/// 2. Special-case the SQL-standard built-ins (`BOOL` → `boolean`,
///    `INT4` → `integer`, `TIMESTAMP` → `timestamp without time zone`, etc.).
/// 3. Otherwise: render the catalog name, qualified with the schema if the
///    type isn't visible on the search path (`pg_catalog` builtins stay
///    unqualified). `QualifiedName::Display` handles the PG identifier
///    quoting rules.
pub fn format_type_for_message(snapshot: &PgCatalog, oid: PgTypeOid) -> String {
    let Some(t) = snapshot.pg_type.get(&oid) else {
        return format!("oid={oid}");
    };

    // Array deconstruction. PG checks `IsTrueArrayType(typeform) &&
    // typeform->typstorage != TYPSTORAGE_PLAIN` — we approximate by
    // requiring the element's canonical `typarray` to point back at this
    // type, which excludes `oidvector`/`int2vector` (plain-storage
    // pseudo-arrays that PG renders by their own name, e.g.
    // `function to_char(oidvector) does not exist`).
    if t.typcategory == crate::pg_catalog::TypCategory::Array
        && let Some(elem_oid) = t.typelem
        && snapshot.array_type_of(elem_oid) == Some(oid)
    {
        return format!("{}[]", format_type_for_message(snapshot, elem_oid));
    }

    // Special-case the SQL-standard built-ins. Mirrors the big `switch
    // (type_oid)` block in `format_type_extended`.
    let aliased = match t.typname.as_str() {
        "bool" => Some("boolean"),
        // The internal single-byte type (OID 18) — PG always renders it
        // double-quoted as `"char"` to distinguish it from SQL `char`/`bpchar`.
        "char" => Some("\"char\""),
        "int2" => Some("smallint"),
        "int4" => Some("integer"),
        "int8" => Some("bigint"),
        "float4" => Some("real"),
        "float8" => Some("double precision"),
        "bpchar" => Some("character"),
        "varchar" => Some("character varying"),
        "varbit" => Some("bit varying"),
        "bit" => Some("bit"),
        "timestamp" => Some("timestamp without time zone"),
        "timestamptz" => Some("timestamp with time zone"),
        "time" => Some("time without time zone"),
        "timetz" => Some("time with time zone"),
        "interval" => Some("interval"),
        "numeric" => Some("numeric"),
        "json" => Some("json"),
        _ => None,
    };
    if let Some(name) = aliased {
        return name.to_string();
    }

    // Default handling: catalog name, qualified iff the type isn't visible
    // on the search path. `pg_catalog` types are always visible without
    // qualification; for everything else we ask the lookup.
    let visible = snapshot
        .resolve_type_by_name(None, &t.typname)
        .map(|found| found.oid == oid)
        .unwrap_or(false);
    if !visible && let Some(ns) = snapshot.namespace_name(t.typnamespace) {
        return QualifiedName::new(ns, &t.typname).to_string();
    }
    t.typname.clone()
}

/// Extract a string value from a Node.
pub fn node_string(n: &Node) -> Option<&str> {
    match n.node.as_ref()? {
        node::Node::String(s) => Some(s.sval.as_str()),
        _ => None,
    }
}

/// Register the implicit `composite_oid → record` cast PG creates for every
/// composite type. Used by the operator resolver so that `composite =
/// composite` reaches the polymorphic `record = record` (record_eq) operator
/// via cast lookup.
pub fn register_composite_to_record_cast(
    interp: &mut PgCatalog,
    composite_oid: PgTypeOid,
) -> Result<(), DdlError> {
    let cast_oid = PgCastOid::from_nonzero(interp.alloc_oid()?);
    interp.insert_pg_cast(PgCast {
        oid: cast_oid,
        castsource: composite_oid,
        casttarget: builtin_oid::RECORD,
        castcontext: CastContext::Implicit,
        castmethod: CastMethod::Binary,
    });
    Ok(())
}
