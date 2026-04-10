//! CREATE TYPE / CREATE DOMAIN / ALTER TYPE DDL handlers.

use pg_query::protobuf::{
    AlterEnumStmt, CoercionContext, CompositeTypeStmt, CreateCastStmt, CreateDomainStmt,
    CreateEnumStmt, CreateRangeStmt, DefineStmt, ObjectType, node,
};

use crate::schema::{CompositeField, TypeEntry, TypeKind};

use super::util::{extract_names, names_key, node_string, resolve_type_name};
use super::{DdlError, DdlInterpreter};

// ─── CREATE DOMAIN ──────────────────────────────────────────────────────────

pub fn create_domain(interp: &mut DdlInterpreter, stmt: &CreateDomainStmt) -> Result<(), DdlError> {
    let (schema, name) = extract_names(&stmt.domainname, &interp.snapshot);
    let key = format!("{schema}.{name}");

    if interp.snapshot.type_by_name.contains_key(&key) {
        return Err(DdlError::DuplicateObject(format!(
            "type \"{name}\" already exists"
        )));
    }

    let base_type_oid = stmt
        .type_name
        .as_ref()
        .and_then(|tn| resolve_type_name(tn, &interp.snapshot))
        .ok_or_else(|| DdlError::TypeNotFound("domain base type".into()))?;

    let oid = interp.alloc_oid();
    let array_oid = interp.alloc_oid();

    // Domains inherit category/preferred from their base type.
    let (category, is_preferred) = interp
        .snapshot
        .types
        .get(&base_type_oid)
        .map(|t| (t.category, t.is_preferred))
        .unwrap_or(('U', false));
    interp.snapshot.types.insert(
        oid,
        TypeEntry {
            oid,
            name: name.clone(),
            schema: schema.clone(),
            kind: TypeKind::Domain { base_type_oid },
            category,
            is_preferred,
            extension: None,
        },
    );
    interp.snapshot.type_by_name.insert(key, oid);

    // Array type for the domain.
    register_array_type(interp, array_oid, &schema, &name, oid);

    Ok(())
}

// ─── CREATE TYPE AS ENUM ────────────────────────────────────────────────────

pub fn create_enum(interp: &mut DdlInterpreter, stmt: &CreateEnumStmt) -> Result<(), DdlError> {
    let (schema, name) = extract_names(&stmt.type_name, &interp.snapshot);
    let key = format!("{schema}.{name}");

    if interp.snapshot.type_by_name.contains_key(&key) {
        return Err(DdlError::DuplicateObject(format!(
            "type \"{name}\" already exists"
        )));
    }

    let labels: Vec<String> = stmt
        .vals
        .iter()
        .filter_map(|n| node_string(n).map(|s| s.to_owned()))
        .collect();

    let oid = interp.alloc_oid();
    let array_oid = interp.alloc_oid();

    interp.snapshot.types.insert(
        oid,
        TypeEntry {
            oid,
            name: name.clone(),
            schema: schema.clone(),
            kind: TypeKind::Enum { labels },
            category: 'E',
            is_preferred: false,
            extension: None,
        },
    );
    interp.snapshot.type_by_name.insert(key, oid);

    register_array_type(interp, array_oid, &schema, &name, oid);

    Ok(())
}

// ─── CREATE TYPE AS (composite) ─────────────────────────────────────────────

pub fn create_composite(
    interp: &mut DdlInterpreter,
    stmt: &CompositeTypeStmt,
) -> Result<(), DdlError> {
    let rv = stmt
        .typevar
        .as_ref()
        .ok_or_else(|| DdlError::Parse("CREATE TYPE without name".into()))?;

    let schema = if rv.schemaname.is_empty() {
        interp
            .snapshot
            .search_path
            .first()
            .cloned()
            .unwrap_or_else(|| "public".to_owned())
    } else {
        rv.schemaname.clone()
    };
    let name = rv.relname.clone();
    let key = format!("{schema}.{name}");

    if interp.snapshot.type_by_name.contains_key(&key) {
        return Err(DdlError::DuplicateObject(format!(
            "type \"{name}\" already exists"
        )));
    }

    let mut fields = Vec::new();
    for col_node in &stmt.coldeflist {
        if let Some(node::Node::ColumnDef(cd)) = col_node.node.as_ref() {
            let type_oid = cd
                .type_name
                .as_ref()
                .and_then(|tn| resolve_type_name(tn, &interp.snapshot))
                .unwrap_or(0);
            fields.push(CompositeField {
                name: cd.colname.clone(),
                type_oid,
                not_null: cd.is_not_null,
            });
        }
    }

    let oid = interp.alloc_oid();
    let array_oid = interp.alloc_oid();

    interp.snapshot.types.insert(
        oid,
        TypeEntry {
            oid,
            name: name.clone(),
            schema: schema.clone(),
            kind: TypeKind::Composite { fields },
            category: 'C',
            is_preferred: false,
            extension: None,
        },
    );
    interp.snapshot.type_by_name.insert(key, oid);

    register_array_type(interp, array_oid, &schema, &name, oid);

    Ok(())
}

// ─── CREATE TYPE AS RANGE ───────────────────────────────────────────────────

pub fn create_range(interp: &mut DdlInterpreter, stmt: &CreateRangeStmt) -> Result<(), DdlError> {
    let (schema, name) = extract_names(&stmt.type_name, &interp.snapshot);
    let key = format!("{schema}.{name}");

    if interp.snapshot.type_by_name.contains_key(&key) {
        return Err(DdlError::DuplicateObject(format!(
            "type \"{name}\" already exists"
        )));
    }

    // Extract subtype from params (look for DefElem with defname="subtype").
    let mut subtype_oid = 0u32;
    for param_node in &stmt.params {
        if let Some(node::Node::DefElem(de)) = param_node.node.as_ref()
            && de.defname == "subtype"
            && let Some(arg) = de.arg.as_deref()
            && let Some(node::Node::TypeName(tn)) = arg.node.as_ref()
        {
            subtype_oid = resolve_type_name(tn, &interp.snapshot).unwrap_or(0);
        }
    }

    let oid = interp.alloc_oid();
    let array_oid = interp.alloc_oid();

    interp.snapshot.types.insert(
        oid,
        TypeEntry {
            oid,
            name: name.clone(),
            schema: schema.clone(),
            kind: TypeKind::Range { subtype_oid },
            category: 'R',
            is_preferred: false,
            extension: None,
        },
    );
    interp.snapshot.type_by_name.insert(key, oid);

    register_array_type(interp, array_oid, &schema, &name, oid);

    Ok(())
}

// ─── ALTER TYPE ... ADD VALUE (enum) ────────────────────────────────────────

pub fn alter_enum(interp: &mut DdlInterpreter, stmt: &AlterEnumStmt) -> Result<(), DdlError> {
    let key = names_key(&stmt.type_name, &interp.snapshot);

    let Some(&oid) = interp.snapshot.type_by_name.get(&key) else {
        // IF NOT EXISTS applies to the VALUE, not the TYPE.
        // Type must always exist.
        return Err(DdlError::TypeNotFound(key));
    };

    let Some(te) = interp.snapshot.types.get_mut(&oid) else {
        return Err(DdlError::TypeNotFound(key));
    };

    if let TypeKind::Enum { labels } = &mut te.kind {
        if labels.contains(&stmt.new_val) {
            if stmt.skip_if_new_val_exists {
                return Ok(());
            }
            return Err(DdlError::DuplicateObject(format!(
                "enum label \"{}\" already exists",
                stmt.new_val
            )));
        }

        if stmt.new_val_neighbor.is_empty() {
            // No neighbor specified — append.
            labels.push(stmt.new_val.clone());
        } else if let Some(pos) = labels.iter().position(|l| l == &stmt.new_val_neighbor) {
            if stmt.new_val_is_after {
                labels.insert(pos + 1, stmt.new_val.clone());
            } else {
                labels.insert(pos, stmt.new_val.clone());
            }
        } else {
            labels.push(stmt.new_val.clone());
        }
    }

    Ok(())
}

// ─── DefineStmt: CREATE TYPE name / CREATE TYPE name (...) ──────────────────

/// Handle `DefineStmt` which covers shell types (`CREATE TYPE citext;`) and
/// full type definitions (`CREATE TYPE citext (INPUT = ..., OUTPUT = ...)`).
pub fn define_type(interp: &mut DdlInterpreter, stmt: &DefineStmt) -> Result<(), DdlError> {
    let obj_type = ObjectType::try_from(stmt.kind).unwrap_or(ObjectType::Undefined);
    if obj_type != ObjectType::ObjectType {
        // Not a type definition (could be an operator, aggregate, etc. via DefineStmt).
        return Ok(());
    }

    let (schema, name) = extract_names(&stmt.defnames, &interp.snapshot);
    let key = format!("{schema}.{name}");

    // If the type already exists (e.g., shell type followed by full definition), reuse OID.
    if interp.snapshot.type_by_name.contains_key(&key) {
        // Full definition after shell type — just confirm it exists.
        return Ok(());
    }

    // Register as a Base type. We don't distinguish between shell and full definitions
    // for static analysis — both result in a Base type entry.
    let oid = interp.alloc_oid();
    let array_oid = interp.alloc_oid();

    interp.snapshot.types.insert(
        oid,
        TypeEntry {
            oid,
            name: name.clone(),
            schema: schema.clone(),
            kind: TypeKind::Base,
            category: 'U',
            is_preferred: false,
            extension: None,
        },
    );
    interp.snapshot.type_by_name.insert(key, oid);

    register_array_type(interp, array_oid, &schema, &name, oid);

    Ok(())
}

// ─── CREATE CAST ────────────────────────────────────────────────────────────

pub fn create_cast(interp: &mut DdlInterpreter, stmt: &CreateCastStmt) -> Result<(), DdlError> {
    let source_oid = stmt
        .sourcetype
        .as_ref()
        .and_then(|tn| resolve_type_name(tn, &interp.snapshot));
    let target_oid = stmt
        .targettype
        .as_ref()
        .and_then(|tn| resolve_type_name(tn, &interp.snapshot));

    let (Some(src), Some(tgt)) = (source_oid, target_oid) else {
        // Can't resolve types — skip silently.
        return Ok(());
    };

    let context = match CoercionContext::try_from(stmt.context) {
        Ok(CoercionContext::CoercionImplicit) => crate::schema::CastContext::Implicit,
        Ok(CoercionContext::CoercionAssignment) => crate::schema::CastContext::Assignment,
        _ => crate::schema::CastContext::Explicit,
    };

    let key = format!("{src}:{tgt}");
    interp.snapshot.casts.insert(key, context);

    Ok(())
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Register an array type (`_name`) for a base type.
fn register_array_type(
    interp: &mut DdlInterpreter,
    array_oid: u32,
    schema: &str,
    base_name: &str,
    element_oid: u32,
) {
    let array_name = format!("_{base_name}");
    let array_key = format!("{schema}.{array_name}");
    interp.snapshot.types.insert(
        array_oid,
        TypeEntry {
            oid: array_oid,
            name: array_name,
            schema: schema.to_owned(),
            kind: TypeKind::Array {
                element_type_oid: element_oid,
            },
            category: 'A',
            is_preferred: false,
            extension: None,
        },
    );
    interp.snapshot.type_by_name.insert(array_key, array_oid);
}
