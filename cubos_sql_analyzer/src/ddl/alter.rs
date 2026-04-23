//! `ALTER ... RENAME TO` and `ALTER ... SET SCHEMA` handlers.
//!
//! These cover the subset of `ALTER FUNCTION`, `ALTER AGGREGATE`, `ALTER
//! PROCEDURE`, `ALTER TABLE`, `ALTER TYPE`, and `ALTER DOMAIN` that changes
//! the *identity* of an object — everything else (attributes like STRICT,
//! VOLATILE, owner, tablespace, …) is irrelevant for static type analysis and
//! remains a no-op.

use pg_query::protobuf::{AlterObjectSchemaStmt, ObjectType, RenameStmt, node};

use super::DdlError;
use super::util::{node_string, resolve_type_name};
use super::views;
use crate::pg_catalog::PgCatalog;
use crate::qualified_name::QualifiedName;

// ─── ALTER ... RENAME TO ────────────────────────────────────────────────────

pub fn rename(interp: &mut PgCatalog, stmt: &RenameStmt) -> Result<(), DdlError> {
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

fn rename_relation(interp: &mut PgCatalog, stmt: &RenameStmt) -> Result<(), DdlError> {
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
    let old_key = QualifiedName::new(&schema, &rv.relname);
    let Some(mut entry) = interp.snapshot.tables.remove(&old_key) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(old_key.to_string()));
    };

    let new_key = QualifiedName::new(&schema, &stmt.newname);
    entry.name = stmt.newname.clone();
    interp.snapshot.tables.insert(new_key.clone(), entry);

    // Composite type mirroring the table name: rename it too.
    if let Some(&ctype_oid) = interp.snapshot.type_by_name.get(&old_key) {
        if let Some(te) = interp.snapshot.types.get_mut(&ctype_oid) {
            te.name = stmt.newname.clone();
        }
        interp.snapshot.type_by_name.remove(&old_key);
        interp
            .snapshot
            .type_by_name
            .insert(QualifiedName::new(&schema, &stmt.newname), ctype_oid);

        let old_array_key = QualifiedName::new(&schema, format!("_{}", rv.relname));
        if let Some(arr_oid) = interp.snapshot.type_by_name.remove(&old_array_key) {
            if let Some(te) = interp.snapshot.types.get_mut(&arr_oid) {
                te.name = format!("_{}", stmt.newname);
            }
            interp.snapshot.type_by_name.insert(
                QualifiedName::new(&schema, format!("_{}", stmt.newname)),
                arr_oid,
            );
        }
    }

    // Point every view's deps at the new key so downstream CASCADE/DROP
    // queries still resolve correctly.
    views::rewrite_deps_on_table_rename(&mut interp.snapshot, &old_key, &new_key);

    Ok(())
}

fn rename_column(interp: &mut PgCatalog, stmt: &RenameStmt) -> Result<(), DdlError> {
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
    let key = QualifiedName::new(&schema, &rv.relname);
    let Some(table) = interp.snapshot.tables.get_mut(&key) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(key.to_string()));
    };
    if let Some(col) = table.columns.iter_mut().find(|c| c.name == stmt.subname) {
        col.name = stmt.newname.clone();
    }
    views::rewrite_deps_on_column_rename(&mut interp.snapshot, &key, &stmt.subname, &stmt.newname);
    Ok(())
}

fn rename_function_like(
    interp: &mut PgCatalog,
    stmt: &RenameStmt,
    expected: ObjectType,
) -> Result<(), DdlError> {
    let Some((schema, old_name, arg_oids)) = extract_func_target(&stmt.object, &interp.snapshot)
    else {
        return Ok(());
    };

    let is_aggregate = expected == ObjectType::ObjectAggregate;
    let is_procedure = expected == ObjectType::ObjectProcedure;

    // Resolve the (schema, name) key — either explicit, or the first hit on
    // the search path that has a matching overload.
    let matches = |f: &crate::schema::FunctionEntry| {
        f.arg_types == arg_oids && f.is_aggregate == is_aggregate && f.is_procedure == is_procedure
    };
    let Some(old_key) = find_function_key(&interp.snapshot, schema.as_deref(), &old_name, &matches)
    else {
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

    // Remove the matching overload from the old key's bucket.
    let fns = interp.snapshot.functions_by_name.get_mut(&old_key).unwrap();
    let pos = fns.iter().position(matches).unwrap();
    let mut entry = fns.remove(pos);
    if fns.is_empty() {
        interp.snapshot.functions_by_name.remove(&old_key);
    }

    entry.name = stmt.newname.clone();
    let new_key = QualifiedName::new(&old_key.schema, &stmt.newname);
    interp
        .snapshot
        .functions_by_name
        .entry(new_key)
        .or_default()
        .push(entry);

    Ok(())
}

/// Find the schema-qualified key of a function-like object, scanning the
/// search_path when `schema` is `None`.
fn find_function_key(
    snapshot: &crate::schema::SchemaSnapshot,
    schema: Option<&str>,
    name: &str,
    matches: &dyn Fn(&crate::schema::FunctionEntry) -> bool,
) -> Option<QualifiedName> {
    if let Some(s) = schema {
        let k = QualifiedName::new(s, name);
        return snapshot
            .functions_by_name
            .get(&k)
            .filter(|fns| fns.iter().any(matches))
            .map(|_| k);
    }
    if !snapshot.search_path.iter().any(|s| s == "pg_catalog") {
        let k = QualifiedName::new("pg_catalog", name);
        if snapshot
            .functions_by_name
            .get(&k)
            .is_some_and(|fns| fns.iter().any(matches))
        {
            return Some(k);
        }
    }
    for s in &snapshot.search_path {
        let k = QualifiedName::new(s.clone(), name);
        if snapshot
            .functions_by_name
            .get(&k)
            .is_some_and(|fns| fns.iter().any(matches))
        {
            return Some(k);
        }
    }
    None
}

fn rename_type_obj(interp: &mut PgCatalog, stmt: &RenameStmt) -> Result<(), DdlError> {
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

    let old_key = QualifiedName::new(&schema, &old_name);
    let Some(&oid) = interp.snapshot.type_by_name.get(&old_key) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TypeNotFound(old_key.to_string()));
    };

    if let Some(te) = interp.snapshot.types.get_mut(&oid) {
        te.name = stmt.newname.clone();
    }
    interp.snapshot.type_by_name.remove(&old_key);
    let new_key = QualifiedName::new(&schema, &stmt.newname);
    interp.snapshot.type_by_name.insert(new_key, oid);

    // Rename the matching `_<name>` array type as well.
    let old_array_key = QualifiedName::new(&schema, format!("_{old_name}"));
    if let Some(arr_oid) = interp.snapshot.type_by_name.remove(&old_array_key) {
        if let Some(te) = interp.snapshot.types.get_mut(&arr_oid) {
            te.name = format!("_{}", stmt.newname);
        }
        interp.snapshot.type_by_name.insert(
            QualifiedName::new(&schema, format!("_{}", stmt.newname)),
            arr_oid,
        );
    }

    Ok(())
}

fn rename_schema(interp: &mut PgCatalog, stmt: &RenameStmt) -> Result<(), DdlError> {
    let old = &stmt.subname;
    let new = &stmt.newname;

    // Rewrite all tables, types, and functions that live in the old schema.
    let rekeyed_tables: Vec<(QualifiedName, QualifiedName)> = interp
        .snapshot
        .tables
        .keys()
        .filter(|k| k.schema == *old)
        .map(|k| (k.clone(), QualifiedName::new(new.clone(), &k.name)))
        .collect();
    for (old_key, new_key) in rekeyed_tables {
        if let Some(mut entry) = interp.snapshot.tables.remove(&old_key) {
            entry.schema = new.clone();
            interp.snapshot.tables.insert(new_key, entry);
        }
    }

    let rekeyed_types: Vec<(QualifiedName, QualifiedName, u32)> = interp
        .snapshot
        .type_by_name
        .iter()
        .filter_map(|(k, &oid)| {
            if k.schema == *old {
                Some((k.clone(), QualifiedName::new(new.clone(), &k.name), oid))
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

    let rekeyed_functions: Vec<(QualifiedName, QualifiedName)> = interp
        .snapshot
        .functions_by_name
        .keys()
        .filter(|k| k.schema == *old)
        .map(|k| (k.clone(), QualifiedName::new(new.clone(), &k.name)))
        .collect();
    for (old_key, new_key) in rekeyed_functions {
        if let Some(mut fns) = interp.snapshot.functions_by_name.remove(&old_key) {
            for f in fns.iter_mut() {
                f.schema = new.clone();
            }
            interp
                .snapshot
                .functions_by_name
                .entry(new_key)
                .or_default()
                .extend(fns);
        }
    }

    let rekeyed_operators: Vec<(QualifiedName, QualifiedName)> = interp
        .snapshot
        .operators_by_name
        .keys()
        .filter(|k| k.schema == *old)
        .map(|k| (k.clone(), QualifiedName::new(new.clone(), &k.name)))
        .collect();
    for (old_key, new_key) in rekeyed_operators {
        if let Some(ops) = interp.snapshot.operators_by_name.remove(&old_key) {
            interp
                .snapshot
                .operators_by_name
                .entry(new_key)
                .or_default()
                .extend(ops);
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

    // Rewrite view dependencies so anything that pointed at `old.*` now
    // points at `new.*`.
    views::rewrite_deps_on_schema_rename(&mut interp.snapshot, old, new);

    Ok(())
}

// ─── ALTER ... SET SCHEMA ───────────────────────────────────────────────────

pub fn set_schema(interp: &mut PgCatalog, stmt: &AlterObjectSchemaStmt) -> Result<(), DdlError> {
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
    interp: &mut PgCatalog,
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
    let old_key = QualifiedName::new(&old_schema, &rv.relname);
    let Some(mut entry) = interp.snapshot.tables.remove(&old_key) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(old_key.to_string()));
    };
    let new_key = QualifiedName::new(&new_schema, &rv.relname);
    entry.schema = new_schema.clone();
    interp.snapshot.tables.insert(new_key.clone(), entry);

    // Move the composite type + array type to the new schema as well.
    if let Some(&ctype_oid) = interp.snapshot.type_by_name.get(&old_key) {
        interp.snapshot.type_by_name.remove(&old_key);
        interp
            .snapshot
            .type_by_name
            .insert(QualifiedName::new(&new_schema, &rv.relname), ctype_oid);
        if let Some(te) = interp.snapshot.types.get_mut(&ctype_oid) {
            te.schema = new_schema.clone();
        }
        let old_arr_key = QualifiedName::new(&old_schema, format!("_{}", rv.relname));
        if let Some(arr_oid) = interp.snapshot.type_by_name.remove(&old_arr_key) {
            interp.snapshot.type_by_name.insert(
                QualifiedName::new(&new_schema, format!("_{}", rv.relname)),
                arr_oid,
            );
            if let Some(te) = interp.snapshot.types.get_mut(&arr_oid) {
                te.schema = new_schema;
            }
        }
    }

    // Point every view's deps at the new key.
    views::rewrite_deps_on_table_rename(&mut interp.snapshot, &old_key, &new_key);

    Ok(())
}

fn set_function_like_schema(
    interp: &mut PgCatalog,
    stmt: &AlterObjectSchemaStmt,
    new_schema: String,
    expected: ObjectType,
) -> Result<(), DdlError> {
    let Some((schema, name, arg_oids)) = extract_func_target(&stmt.object, &interp.snapshot) else {
        return Ok(());
    };
    let is_aggregate = expected == ObjectType::ObjectAggregate;
    let is_procedure = expected == ObjectType::ObjectProcedure;

    let matches = |f: &crate::schema::FunctionEntry| {
        f.arg_types == arg_oids && f.is_aggregate == is_aggregate && f.is_procedure == is_procedure
    };
    let Some(old_key) = find_function_key(&interp.snapshot, schema.as_deref(), &name, &matches)
    else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::DependencyError(format!(
            "{} {name} does not exist for the requested argument types",
            match expected {
                ObjectType::ObjectAggregate => "aggregate",
                ObjectType::ObjectProcedure => "procedure",
                _ => "function",
            }
        )));
    };

    // Pull the matching overload from the old bucket, retarget its schema,
    // and re-insert under the new key.
    let fns = interp.snapshot.functions_by_name.get_mut(&old_key).unwrap();
    let pos = fns.iter().position(matches).unwrap();
    let mut entry = fns.remove(pos);
    if fns.is_empty() {
        interp.snapshot.functions_by_name.remove(&old_key);
    }
    entry.schema = new_schema.clone();
    interp
        .snapshot
        .functions_by_name
        .entry(QualifiedName::new(&new_schema, &name))
        .or_default()
        .push(entry);
    Ok(())
}

fn set_type_schema(
    interp: &mut PgCatalog,
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

    let old_key = QualifiedName::new(&old_schema, &name);
    let Some(&oid) = interp.snapshot.type_by_name.get(&old_key) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TypeNotFound(old_key.to_string()));
    };
    interp.snapshot.type_by_name.remove(&old_key);
    interp
        .snapshot
        .type_by_name
        .insert(QualifiedName::new(&new_schema, &name), oid);
    if let Some(te) = interp.snapshot.types.get_mut(&oid) {
        te.schema = new_schema.clone();
    }

    let old_arr_key = QualifiedName::new(&old_schema, format!("_{name}"));
    if let Some(arr_oid) = interp.snapshot.type_by_name.remove(&old_arr_key) {
        interp
            .snapshot
            .type_by_name
            .insert(QualifiedName::new(&new_schema, format!("_{name}")), arr_oid);
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
