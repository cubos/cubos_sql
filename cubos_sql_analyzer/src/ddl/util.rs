//! Shared utilities for DDL interpretation.

use pg_query::protobuf::{Node, RangeVar, TypeName, node};

use crate::qualified_name::QualifiedName;
use crate::schema::SchemaSnapshot;

/// Extract the (schema, name) pair from a `RangeVar`.
/// If no schema is specified, defaults to the first entry in `search_path`.
pub fn range_var_names(rv: &RangeVar, snapshot: &SchemaSnapshot) -> (String, String) {
    let schema = if rv.schemaname.is_empty() {
        snapshot
            .search_path
            .first()
            .cloned()
            .unwrap_or_else(|| "public".to_owned())
    } else {
        rv.schemaname.clone()
    };
    (schema, rv.relname.clone())
}

/// Extract schema-qualified key from a `RangeVar`.
pub fn range_var_key(rv: &RangeVar, snapshot: &SchemaSnapshot) -> QualifiedName {
    let (schema, name) = range_var_names(rv, snapshot);
    QualifiedName::new(schema, name)
}

/// Extract (schema, name) from a list of name nodes (e.g., `domainname`, `type_name` in DDL).
/// Handles both `["name"]` and `["schema", "name"]` forms.
pub fn extract_names(names: &[Node], snapshot: &SchemaSnapshot) -> (String, String) {
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
                .cloned()
                .unwrap_or_else(|| "public".to_owned());
            (schema, (*name).to_owned())
        }
        _ => ("public".to_owned(), String::new()),
    }
}

/// Extract a schema-qualified key from name nodes.
pub fn names_key(names: &[Node], snapshot: &SchemaSnapshot) -> QualifiedName {
    let (schema, name) = extract_names(names, snapshot);
    QualifiedName::new(schema, name)
}

/// Resolve a `TypeName` AST node to a type OID in the snapshot.
///
/// Handles:
/// - Qualified names: `pg_catalog.int4`
/// - Unqualified names: `int4`, `text`, `uuid`
/// - Array bounds: `int4[]` → array element type OID
/// - Shorthand aliases: `integer` → `int4`, `bigint` → `int8`, etc.
pub fn resolve_type_name(tn: &TypeName, snapshot: &SchemaSnapshot) -> Option<u32> {
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

    // Normalize shorthand aliases.
    let name = normalize_type_name(raw_name);

    // Try to find the type by name.
    let oid = if let Some(schema) = schema {
        let key = QualifiedName::new(schema, name);
        snapshot.type_by_name.get(&key).copied()
    } else {
        // Search path then pg_catalog.
        let mut found = None;
        for s in &snapshot.search_path {
            let key = QualifiedName::new(s.clone(), name);
            if let Some(oid) = snapshot.type_by_name.get(&key) {
                found = Some(*oid);
                break;
            }
        }
        if found.is_none() {
            let key = QualifiedName::new("pg_catalog", name);
            found = snapshot.type_by_name.get(&key).copied();
        }
        found
    };

    // Handle array bounds: if the type name has array_bounds, look up the array type.
    if !tn.array_bounds.is_empty()
        && let Some(base_oid) = oid
    {
        return snapshot.types.values().find_map(|t| {
            if let crate::schema::TypeKind::Array {
                element_type_oid: elem,
            } = &t.kind
                && *elem == base_oid
            {
                return Some(t.oid);
            }
            None
        });
    }

    oid
}

/// Normalize PostgreSQL type name aliases to their canonical form.
fn normalize_type_name(name: &str) -> &str {
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

/// Extract a string value from a Node.
pub fn node_string(n: &Node) -> Option<&str> {
    match n.node.as_ref()? {
        node::Node::String(s) => Some(s.sval.as_str()),
        _ => None,
    }
}
