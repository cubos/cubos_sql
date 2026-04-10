//! `ALTER ... RENAME TO` and `ALTER ... SET SCHEMA` handlers.
//!
//! These cover the subset of `ALTER FUNCTION`, `ALTER AGGREGATE`, `ALTER
//! PROCEDURE`, `ALTER TABLE`, `ALTER TYPE`, and `ALTER DOMAIN` that changes
//! the *identity* of an object — everything else (attributes like STRICT,
//! VOLATILE, owner, tablespace, …) is irrelevant for static type analysis and
//! remains a no-op.

use pg_query::protobuf::{AlterObjectSchemaStmt, ObjectType, RenameStmt, node};

use super::util::{node_string, resolve_type_name};
use super::{DdlError, DdlInterpreter};

// ─── ALTER ... RENAME TO ────────────────────────────────────────────────────

pub fn rename(interp: &mut DdlInterpreter, stmt: &RenameStmt) -> Result<(), DdlError> {
    let rename_type = ObjectType::try_from(stmt.rename_type).unwrap_or(ObjectType::Undefined);

    match rename_type {
        // ── Relations (handled via `relation` field) ────────────────────
        ObjectType::ObjectTable
        | ObjectType::ObjectView
        | ObjectType::ObjectMatview
        | ObjectType::ObjectForeignTable => rename_relation(interp, stmt),

        // ── Functions / procedures / aggregates (handled via `object`) ──
        ObjectType::ObjectFunction | ObjectType::ObjectProcedure | ObjectType::ObjectAggregate => {
            rename_function_like(interp, stmt, rename_type)
        }

        // ── Types / domains ─────────────────────────────────────────────
        ObjectType::ObjectType | ObjectType::ObjectDomain => rename_type_obj(interp, stmt),

        // ── Schemas ─────────────────────────────────────────────────────
        ObjectType::ObjectSchema => rename_schema(interp, stmt),

        // ── Columns — `ALTER TABLE t RENAME COLUMN a TO b` ──────────────
        ObjectType::ObjectColumn => rename_column(interp, stmt),

        // Everything else (index, trigger, policy, role, …) has no impact
        // on static type analysis.
        _ => Ok(()),
    }
}

fn rename_relation(interp: &mut DdlInterpreter, stmt: &RenameStmt) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
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
    let old_key = format!("{schema}.{}", rv.relname);
    let Some(&oid) = interp.snapshot.table_by_name.get(&old_key) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(old_key));
    };

    let new_key = format!("{schema}.{}", stmt.newname);
    if let Some(table) = interp.snapshot.tables.get_mut(&oid) {
        table.name = stmt.newname.clone();
    }
    interp.snapshot.table_by_name.remove(&old_key);
    interp.snapshot.table_by_name.insert(new_key, oid);

    // Composite type mirroring the table name: rename it too.
    if let Some(&ctype_oid) = interp.snapshot.type_by_name.get(&old_key) {
        if let Some(te) = interp.snapshot.types.get_mut(&ctype_oid) {
            te.name = stmt.newname.clone();
        }
        interp.snapshot.type_by_name.remove(&old_key);
        interp
            .snapshot
            .type_by_name
            .insert(format!("{schema}.{}", stmt.newname), ctype_oid);

        let old_array_key = format!("{schema}._{}", rv.relname);
        if let Some(arr_oid) = interp.snapshot.type_by_name.remove(&old_array_key) {
            if let Some(te) = interp.snapshot.types.get_mut(&arr_oid) {
                te.name = format!("_{}", stmt.newname);
            }
            interp
                .snapshot
                .type_by_name
                .insert(format!("{schema}._{}", stmt.newname), arr_oid);
        }
    }

    Ok(())
}

fn rename_column(interp: &mut DdlInterpreter, stmt: &RenameStmt) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
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
    let key = format!("{schema}.{}", rv.relname);
    let Some(&oid) = interp.snapshot.table_by_name.get(&key) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(key));
    };
    if let Some(table) = interp.snapshot.tables.get_mut(&oid)
        && let Some(col) = table.columns.iter_mut().find(|c| c.name == stmt.subname)
    {
        col.name = stmt.newname.clone();
    }
    Ok(())
}

fn rename_function_like(
    interp: &mut DdlInterpreter,
    stmt: &RenameStmt,
    expected: ObjectType,
) -> Result<(), DdlError> {
    let Some((schema, old_name, arg_oids)) = extract_func_target(&stmt.object, &interp.snapshot)
    else {
        return Ok(());
    };

    let is_aggregate = expected == ObjectType::ObjectAggregate;
    let is_procedure = expected == ObjectType::ObjectProcedure;

    // Remove the matching overload from the old name's bucket.
    let mut moved_entry = None;
    if let Some(fns) = interp.snapshot.functions_by_name.get_mut(&old_name) {
        if let Some(pos) = fns.iter().position(|f| {
            f.arg_types == arg_oids
                && f.is_aggregate == is_aggregate
                && f.is_procedure == is_procedure
                && schema.as_deref().is_none_or(|s| f.schema == s)
        }) {
            moved_entry = Some(fns.remove(pos));
        }
        if fns.is_empty() {
            interp.snapshot.functions_by_name.remove(&old_name);
        }
    }

    let Some(mut entry) = moved_entry else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::DependencyError(format!(
            "{} {old_name} does not exist for the requested argument types",
            match expected {
                ObjectType::ObjectAggregate => "aggregate",
                ObjectType::ObjectProcedure => "procedure",
                _ => "function",
            }
        )));
    };

    entry.name = stmt.newname.clone();
    interp
        .snapshot
        .functions_by_name
        .entry(stmt.newname.clone())
        .or_default()
        .push(entry);

    Ok(())
}

fn rename_type_obj(interp: &mut DdlInterpreter, stmt: &RenameStmt) -> Result<(), DdlError> {
    let Some(object) = stmt.object.as_deref() else {
        return Ok(());
    };
    let parts: Vec<&str> = match object.node.as_ref() {
        Some(node::Node::TypeName(tn)) => tn.names.iter().filter_map(node_string).collect(),
        Some(node::Node::List(list)) => list.items.iter().filter_map(node_string).collect(),
        _ => return Ok(()),
    };

    let (schema, old_name) = match parts.as_slice() {
        [s, n] => ((*s).to_owned(), (*n).to_owned()),
        [n] => (
            interp
                .snapshot
                .search_path
                .first()
                .cloned()
                .unwrap_or_else(|| "public".to_owned()),
            (*n).to_owned(),
        ),
        _ => return Ok(()),
    };

    let old_key = format!("{schema}.{old_name}");
    let Some(&oid) = interp.snapshot.type_by_name.get(&old_key) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TypeNotFound(old_key));
    };

    if let Some(te) = interp.snapshot.types.get_mut(&oid) {
        te.name = stmt.newname.clone();
    }
    interp.snapshot.type_by_name.remove(&old_key);
    let new_key = format!("{schema}.{}", stmt.newname);
    interp.snapshot.type_by_name.insert(new_key, oid);

    // Rename the matching `_<name>` array type as well.
    let old_array_key = format!("{schema}._{old_name}");
    if let Some(arr_oid) = interp.snapshot.type_by_name.remove(&old_array_key) {
        if let Some(te) = interp.snapshot.types.get_mut(&arr_oid) {
            te.name = format!("_{}", stmt.newname);
        }
        interp
            .snapshot
            .type_by_name
            .insert(format!("{schema}._{}", stmt.newname), arr_oid);
    }

    Ok(())
}

fn rename_schema(interp: &mut DdlInterpreter, stmt: &RenameStmt) -> Result<(), DdlError> {
    let old = &stmt.subname;
    let new = &stmt.newname;

    // Rewrite all tables, types, and functions that live in the old schema.
    let rekeyed_tables: Vec<(String, String, u32)> = interp
        .snapshot
        .table_by_name
        .iter()
        .filter_map(|(k, &oid)| {
            let (s, n) = k.split_once('.')?;
            if s == old {
                Some((k.clone(), format!("{new}.{n}"), oid))
            } else {
                None
            }
        })
        .collect();
    for (old_key, new_key, oid) in rekeyed_tables {
        interp.snapshot.table_by_name.remove(&old_key);
        interp.snapshot.table_by_name.insert(new_key, oid);
        if let Some(t) = interp.snapshot.tables.get_mut(&oid) {
            t.schema = new.clone();
        }
    }

    let rekeyed_types: Vec<(String, String, u32)> = interp
        .snapshot
        .type_by_name
        .iter()
        .filter_map(|(k, &oid)| {
            let (s, n) = k.split_once('.')?;
            if s == old {
                Some((k.clone(), format!("{new}.{n}"), oid))
            } else {
                None
            }
        })
        .collect();
    for (old_key, new_key, oid) in rekeyed_types {
        interp.snapshot.type_by_name.remove(&old_key);
        interp.snapshot.type_by_name.insert(new_key, oid);
        if let Some(t) = interp.snapshot.types.get_mut(&oid) {
            t.schema = new.clone();
        }
    }

    for fns in interp.snapshot.functions_by_name.values_mut() {
        for f in fns.iter_mut() {
            if f.schema == *old {
                f.schema = new.clone();
            }
        }
    }

    // Update `search_path` if it referenced the old name.
    for s in interp.snapshot.search_path.iter_mut() {
        if *s == *old {
            *s = new.clone();
        }
    }

    // And the set of known schemas.
    if interp.snapshot.schemas.remove(old) {
        interp.snapshot.schemas.insert(new.clone());
    }

    Ok(())
}

// ─── ALTER ... SET SCHEMA ───────────────────────────────────────────────────

pub fn set_schema(
    interp: &mut DdlInterpreter,
    stmt: &AlterObjectSchemaStmt,
) -> Result<(), DdlError> {
    let object_type = ObjectType::try_from(stmt.object_type).unwrap_or(ObjectType::Undefined);
    let new_schema = stmt.newschema.clone();

    match object_type {
        // ── Relations ────────────────────────────────────────────────────
        ObjectType::ObjectTable
        | ObjectType::ObjectView
        | ObjectType::ObjectMatview
        | ObjectType::ObjectForeignTable
        | ObjectType::ObjectSequence => set_relation_schema(interp, stmt, new_schema),

        // ── Functions / procedures / aggregates ─────────────────────────
        ObjectType::ObjectFunction | ObjectType::ObjectProcedure | ObjectType::ObjectAggregate => {
            set_function_like_schema(interp, stmt, new_schema, object_type)
        }

        // ── Types / domains ─────────────────────────────────────────────
        ObjectType::ObjectType | ObjectType::ObjectDomain => {
            set_type_schema(interp, stmt, new_schema)
        }

        _ => Ok(()),
    }
}

fn set_relation_schema(
    interp: &mut DdlInterpreter,
    stmt: &AlterObjectSchemaStmt,
    new_schema: String,
) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    let old_schema = if rv.schemaname.is_empty() {
        interp
            .snapshot
            .search_path
            .first()
            .cloned()
            .unwrap_or_else(|| "public".to_owned())
    } else {
        rv.schemaname.clone()
    };
    let old_key = format!("{old_schema}.{}", rv.relname);
    let Some(&oid) = interp.snapshot.table_by_name.get(&old_key) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(old_key));
    };
    let new_key = format!("{new_schema}.{}", rv.relname);
    interp.snapshot.table_by_name.remove(&old_key);
    interp.snapshot.table_by_name.insert(new_key, oid);
    if let Some(t) = interp.snapshot.tables.get_mut(&oid) {
        t.schema = new_schema.clone();
    }

    // Move the composite type + array type to the new schema as well.
    if let Some(&ctype_oid) = interp.snapshot.type_by_name.get(&old_key) {
        interp.snapshot.type_by_name.remove(&old_key);
        interp
            .snapshot
            .type_by_name
            .insert(format!("{new_schema}.{}", rv.relname), ctype_oid);
        if let Some(te) = interp.snapshot.types.get_mut(&ctype_oid) {
            te.schema = new_schema.clone();
        }
        let old_arr_key = format!("{old_schema}._{}", rv.relname);
        if let Some(arr_oid) = interp.snapshot.type_by_name.remove(&old_arr_key) {
            interp
                .snapshot
                .type_by_name
                .insert(format!("{new_schema}._{}", rv.relname), arr_oid);
            if let Some(te) = interp.snapshot.types.get_mut(&arr_oid) {
                te.schema = new_schema;
            }
        }
    }

    Ok(())
}

fn set_function_like_schema(
    interp: &mut DdlInterpreter,
    stmt: &AlterObjectSchemaStmt,
    new_schema: String,
    expected: ObjectType,
) -> Result<(), DdlError> {
    let Some((schema, name, arg_oids)) = extract_func_target(&stmt.object, &interp.snapshot) else {
        return Ok(());
    };
    let is_aggregate = expected == ObjectType::ObjectAggregate;
    let is_procedure = expected == ObjectType::ObjectProcedure;

    let Some(fns) = interp.snapshot.functions_by_name.get_mut(&name) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::DependencyError(format!(
            "function {name} does not exist"
        )));
    };
    let mut matched = false;
    for f in fns.iter_mut() {
        if f.arg_types == arg_oids
            && f.is_aggregate == is_aggregate
            && f.is_procedure == is_procedure
            && schema.as_deref().is_none_or(|s| f.schema == s)
        {
            f.schema = new_schema.clone();
            matched = true;
        }
    }
    if !matched && !stmt.missing_ok {
        return Err(DdlError::DependencyError(format!(
            "{} {name} does not exist for the requested argument types",
            match expected {
                ObjectType::ObjectAggregate => "aggregate",
                ObjectType::ObjectProcedure => "procedure",
                _ => "function",
            }
        )));
    }
    Ok(())
}

fn set_type_schema(
    interp: &mut DdlInterpreter,
    stmt: &AlterObjectSchemaStmt,
    new_schema: String,
) -> Result<(), DdlError> {
    let Some(object) = stmt.object.as_deref() else {
        return Ok(());
    };
    let parts: Vec<&str> = match object.node.as_ref() {
        Some(node::Node::TypeName(tn)) => tn.names.iter().filter_map(node_string).collect(),
        Some(node::Node::List(list)) => list.items.iter().filter_map(node_string).collect(),
        _ => return Ok(()),
    };
    let (old_schema, name) = match parts.as_slice() {
        [s, n] => ((*s).to_owned(), (*n).to_owned()),
        [n] => (
            interp
                .snapshot
                .search_path
                .first()
                .cloned()
                .unwrap_or_else(|| "public".to_owned()),
            (*n).to_owned(),
        ),
        _ => return Ok(()),
    };

    let old_key = format!("{old_schema}.{name}");
    let Some(&oid) = interp.snapshot.type_by_name.get(&old_key) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TypeNotFound(old_key));
    };
    interp.snapshot.type_by_name.remove(&old_key);
    interp
        .snapshot
        .type_by_name
        .insert(format!("{new_schema}.{name}"), oid);
    if let Some(te) = interp.snapshot.types.get_mut(&oid) {
        te.schema = new_schema.clone();
    }

    let old_arr_key = format!("{old_schema}._{name}");
    if let Some(arr_oid) = interp.snapshot.type_by_name.remove(&old_arr_key) {
        interp
            .snapshot
            .type_by_name
            .insert(format!("{new_schema}._{name}"), arr_oid);
        if let Some(te) = interp.snapshot.types.get_mut(&arr_oid) {
            te.schema = new_schema;
        }
    }

    Ok(())
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Extract `(schema, name, arg_oids)` from an `ObjectWithArgs` target. The
/// schema is optional — when `None`, callers should match across schemas.
fn extract_func_target(
    object: &Option<Box<pg_query::protobuf::Node>>,
    snapshot: &crate::schema::SchemaSnapshot,
) -> Option<(Option<String>, String, Vec<u32>)> {
    let node = object.as_deref()?;
    let owa = match node.node.as_ref()? {
        node::Node::ObjectWithArgs(owa) => owa,
        _ => return None,
    };

    let parts: Vec<&str> = owa.objname.iter().filter_map(node_string).collect();
    let (schema, name) = match parts.as_slice() {
        [s, n] => (Some((*s).to_owned()), (*n).to_owned()),
        [n] => (None, (*n).to_owned()),
        _ => return None,
    };

    let arg_oids: Vec<u32> = owa
        .objargs
        .iter()
        .filter_map(|n| {
            if let Some(node::Node::TypeName(tn)) = n.node.as_ref() {
                resolve_type_name(tn, snapshot)
            } else {
                None
            }
        })
        .collect();

    Some((schema, name, arg_oids))
}
