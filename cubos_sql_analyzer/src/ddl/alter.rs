//! `ALTER ... RENAME TO` and `ALTER ... SET SCHEMA` handlers.
//!
//! Cover the subset of `ALTER FUNCTION`, `ALTER AGGREGATE`, `ALTER PROCEDURE`,
//! `ALTER TABLE`, `ALTER TYPE`, and `ALTER DOMAIN` that changes the *identity*
//! of an object — every other attribute (STRICT, VOLATILE, owner, tablespace,
//! …) is irrelevant for static type analysis and remains a no-op.

use pg_query::protobuf::{AlterObjectSchemaStmt, ObjectType, RenameStmt, node};

use super::DdlError;
use super::util::{ensure_namespace, node_string, resolve_type_name};
use super::views;
use crate::oid::{PgNamespaceOid, PgProcOid, PgTypeOid};
use crate::pg_catalog::{PgCatalog, PgProc, ProKind};

// ─── ALTER ... RENAME TO ────────────────────────────────────────────────────

pub fn rename(interp: &mut PgCatalog, stmt: &RenameStmt) -> Result<(), DdlError> {
    let rename_type = ObjectType::try_from(stmt.rename_type).unwrap_or(ObjectType::Undefined);

    match rename_type {
        ObjectType::ObjectTable
        | ObjectType::ObjectView
        | ObjectType::ObjectMatview
        | ObjectType::ObjectForeignTable => rename_relation(interp, stmt),
        ObjectType::ObjectFunction | ObjectType::ObjectProcedure | ObjectType::ObjectAggregate => {
            rename_function_like(interp, stmt, rename_type)
        }
        ObjectType::ObjectType | ObjectType::ObjectDomain => rename_type_obj(interp, stmt),
        ObjectType::ObjectSchema => rename_schema(interp, stmt),
        ObjectType::ObjectColumn => rename_column(interp, stmt),
        ObjectType::ObjectTabconstraint | ObjectType::ObjectDomconstraint => {
            rename_constraint(interp, stmt)
        }
        _ => Ok(()),
    }
}

fn rename_constraint(interp: &mut PgCatalog, stmt: &RenameStmt) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    let (schema_name, relname) = crate::ddl::util::range_var_names(rv, interp);
    let Some(nsoid) = interp.namespace_oid(&schema_name) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(format!("{schema_name}.{relname}")));
    };
    let Some(class_oid) = interp
        .class_by_qname
        .get(&(nsoid, relname.clone()))
        .copied()
    else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(format!("{schema_name}.{relname}")));
    };

    // PG: `constraint "x" of relation "t" does not exist` when the name
    // doesn't match anything attached to the relation.
    let target_oid = interp
        .pg_constraint
        .values()
        .find(|c| c.conrelid == class_oid && c.conname == stmt.subname)
        .map(|c| c.oid);
    let Some(target_oid) = target_oid else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::DependencyError(format!(
            "constraint \"{}\" of relation \"{relname}\" does not exist",
            stmt.subname,
        )));
    };

    let old_name = stmt.subname.clone();
    let is_pkey_or_unique = matches!(
        interp.pg_constraint.get(&target_oid).map(|c| c.contype),
        Some(crate::pg_catalog::ConType::PrimaryKey | crate::pg_catalog::ConType::Unique)
    );
    if let Some(row) = interp.pg_constraint.get_mut(&target_oid) {
        row.conname = stmt.newname.clone();
    }
    // The backing index for PK/UNIQUE shares its name with the constraint
    // (PG conflates them). Rename the pg_class entry so subsequent DROP
    // INDEX / SQL references find the index under its new name too.
    if is_pkey_or_unique
        && let Some(idx_oid) = interp.class_by_qname.get(&(nsoid, old_name)).copied()
        && matches!(
            interp.pg_class.get(&idx_oid).map(|c| c.relkind),
            Some(crate::pg_catalog::RelKind::Index)
        )
    {
        interp.rename_pg_class(idx_oid, stmt.newname.clone(), nsoid);
    }
    Ok(())
}

fn rename_relation(interp: &mut PgCatalog, stmt: &RenameStmt) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    let schema_name = if rv.schemaname.is_empty() {
        interp
            .search_path
            .first()
            .and_then(|&oid| interp.namespace_name(oid).map(str::to_owned))
            .unwrap_or_else(|| "public".to_owned())
    } else {
        rv.schemaname.clone()
    };
    let Some(nsoid) = interp.namespace_oid(&schema_name) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(format!(
            "{schema_name}.{}",
            rv.relname
        )));
    };
    let Some(class_oid) = interp
        .class_by_qname
        .get(&(nsoid, rv.relname.clone()))
        .copied()
    else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(format!(
            "{schema_name}.{}",
            rv.relname
        )));
    };

    let old_name = rv.relname.clone();
    let new_name = stmt.newname.clone();

    interp.rename_pg_class(class_oid, new_name.clone(), nsoid);

    // Composite type mirroring the relation: rename it and the array.
    if let Some(&type_oid) = interp.type_by_qname.get(&(nsoid, old_name.clone())) {
        interp.rename_pg_type(type_oid, new_name.clone(), nsoid);
        let arr_old = format!("_{old_name}");
        if let Some(&arr_oid) = interp.type_by_qname.get(&(nsoid, arr_old)) {
            interp.rename_pg_type(arr_oid, format!("_{new_name}"), nsoid);
        }
    }

    views::rewrite_views_on_table_rename(interp, &schema_name, &old_name, &schema_name, &new_name);

    Ok(())
}

fn rename_column(interp: &mut PgCatalog, stmt: &RenameStmt) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    let schema_name = if rv.schemaname.is_empty() {
        interp
            .search_path
            .first()
            .and_then(|&oid| interp.namespace_name(oid).map(str::to_owned))
            .unwrap_or_else(|| "public".to_owned())
    } else {
        rv.schemaname.clone()
    };
    let Some(nsoid) = interp.namespace_oid(&schema_name) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(format!(
            "{schema_name}.{}",
            rv.relname
        )));
    };
    let Some(relid) = interp
        .class_by_qname
        .get(&(nsoid, rv.relname.clone()))
        .copied()
    else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(format!(
            "{schema_name}.{}",
            rv.relname
        )));
    };

    if let Some(attrs) = interp.pg_attribute.get_mut(&relid)
        && let Some(col) = attrs.iter_mut().find(|c| c.attname == stmt.subname)
    {
        col.attname = stmt.newname.clone();
    }
    views::rewrite_views_on_column_rename(interp, relid, &stmt.subname, &stmt.newname);
    Ok(())
}

fn rename_function_like(
    interp: &mut PgCatalog,
    stmt: &RenameStmt,
    expected: ObjectType,
) -> Result<(), DdlError> {
    let Some((schema_opt, old_name, arg_oids)) = extract_func_target(&stmt.object, interp) else {
        return Ok(());
    };

    let want_kind = match expected {
        ObjectType::ObjectAggregate => ProKind::Aggregate,
        ObjectType::ObjectProcedure => ProKind::Procedure,
        _ => ProKind::Function,
    };
    let matches_kind = move |k: ProKind| {
        matches!(
            (want_kind, k),
            (ProKind::Function, ProKind::Function)
                | (ProKind::Function, ProKind::Window)
                | (ProKind::Procedure, ProKind::Procedure)
                | (ProKind::Aggregate, ProKind::Aggregate)
        )
    };
    let matches = move |p: &PgProc| matches_kind(p.prokind) && p.proargtypes == arg_oids;

    let Some((nsoid, oid)) = find_proc(interp, schema_opt.as_deref(), &old_name, &matches) else {
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

    interp.rename_pg_proc(oid, stmt.newname.clone(), nsoid);
    Ok(())
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

    let (schema_name, old_name) = match parts.as_slice() {
        [s, n] => ((*s).to_owned(), (*n).to_owned()),
        [n] => {
            let s = interp
                .search_path
                .first()
                .and_then(|&oid| interp.namespace_name(oid).map(str::to_owned))
                .unwrap_or_else(|| "public".to_owned());
            (s, (*n).to_owned())
        }
        _ => return Ok(()),
    };

    let Some(nsoid) = interp.namespace_oid(&schema_name) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TypeNotFound(format!("{schema_name}.{old_name}")));
    };
    let Some(&type_oid) = interp.type_by_qname.get(&(nsoid, old_name.clone())) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TypeNotFound(format!("{schema_name}.{old_name}")));
    };

    let new_name = stmt.newname.clone();
    interp.rename_pg_type(type_oid, new_name.clone(), nsoid);

    let arr_old = format!("_{old_name}");
    if let Some(&arr_oid) = interp.type_by_qname.get(&(nsoid, arr_old)) {
        interp.rename_pg_type(arr_oid, format!("_{new_name}"), nsoid);
    }
    Ok(())
}

fn rename_schema(interp: &mut PgCatalog, stmt: &RenameStmt) -> Result<(), DdlError> {
    let old = &stmt.subname;
    let new = &stmt.newname;

    let Some(nsoid) = interp.namespace_oid(old) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::DependencyError(format!(
            "schema \"{old}\" does not exist"
        )));
    };

    interp.rename_pg_namespace(nsoid, new.clone());
    views::rewrite_views_on_schema_rename(interp, old, new);
    Ok(())
}

// ─── ALTER ... SET SCHEMA ───────────────────────────────────────────────────

pub fn set_schema(interp: &mut PgCatalog, stmt: &AlterObjectSchemaStmt) -> Result<(), DdlError> {
    let object_type = ObjectType::try_from(stmt.object_type).unwrap_or(ObjectType::Undefined);
    let new_schema = stmt.newschema.clone();
    let new_nsoid = ensure_namespace(interp, &new_schema);

    match object_type {
        ObjectType::ObjectTable
        | ObjectType::ObjectView
        | ObjectType::ObjectMatview
        | ObjectType::ObjectForeignTable
        | ObjectType::ObjectSequence => set_relation_schema(interp, stmt, new_nsoid, &new_schema),
        ObjectType::ObjectFunction | ObjectType::ObjectProcedure | ObjectType::ObjectAggregate => {
            set_function_like_schema(interp, stmt, new_nsoid, object_type)
        }
        ObjectType::ObjectType | ObjectType::ObjectDomain => {
            set_type_schema(interp, stmt, new_nsoid)
        }
        _ => Ok(()),
    }
}

fn set_relation_schema(
    interp: &mut PgCatalog,
    stmt: &AlterObjectSchemaStmt,
    new_nsoid: PgNamespaceOid,
    new_schema: &str,
) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    let old_schema = if rv.schemaname.is_empty() {
        interp
            .search_path
            .first()
            .and_then(|&oid| interp.namespace_name(oid).map(str::to_owned))
            .unwrap_or_else(|| "public".to_owned())
    } else {
        rv.schemaname.clone()
    };
    let Some(old_nsoid) = interp.namespace_oid(&old_schema) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(format!(
            "{old_schema}.{}",
            rv.relname
        )));
    };
    let Some(class_oid) = interp
        .class_by_qname
        .get(&(old_nsoid, rv.relname.clone()))
        .copied()
    else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(format!(
            "{old_schema}.{}",
            rv.relname
        )));
    };

    let name = rv.relname.clone();
    interp.rename_pg_class(class_oid, name.clone(), new_nsoid);

    if let Some(&type_oid) = interp.type_by_qname.get(&(old_nsoid, name.clone())) {
        interp.rename_pg_type(type_oid, name.clone(), new_nsoid);
        let arr_key = format!("_{name}");
        if let Some(&arr_oid) = interp.type_by_qname.get(&(old_nsoid, arr_key.clone())) {
            interp.rename_pg_type(arr_oid, arr_key, new_nsoid);
        }
    }

    views::rewrite_views_on_table_rename(interp, &old_schema, &name, new_schema, &name);
    Ok(())
}

fn set_function_like_schema(
    interp: &mut PgCatalog,
    stmt: &AlterObjectSchemaStmt,
    new_nsoid: PgNamespaceOid,
    expected: ObjectType,
) -> Result<(), DdlError> {
    let Some((schema_opt, name, arg_oids)) = extract_func_target(&stmt.object, interp) else {
        return Ok(());
    };
    let want_kind = match expected {
        ObjectType::ObjectAggregate => ProKind::Aggregate,
        ObjectType::ObjectProcedure => ProKind::Procedure,
        _ => ProKind::Function,
    };
    let matches_kind = move |k: ProKind| {
        matches!(
            (want_kind, k),
            (ProKind::Function, ProKind::Function)
                | (ProKind::Function, ProKind::Window)
                | (ProKind::Procedure, ProKind::Procedure)
                | (ProKind::Aggregate, ProKind::Aggregate)
        )
    };
    let matches = move |p: &PgProc| matches_kind(p.prokind) && p.proargtypes == arg_oids;

    let Some((_, oid)) = find_proc(interp, schema_opt.as_deref(), &name, &matches) else {
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

    interp.rename_pg_proc(oid, name, new_nsoid);
    Ok(())
}

fn set_type_schema(
    interp: &mut PgCatalog,
    stmt: &AlterObjectSchemaStmt,
    new_nsoid: PgNamespaceOid,
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
        [n] => {
            let s = interp
                .search_path
                .first()
                .and_then(|&oid| interp.namespace_name(oid).map(str::to_owned))
                .unwrap_or_else(|| "public".to_owned());
            (s, (*n).to_owned())
        }
        _ => return Ok(()),
    };

    let Some(old_nsoid) = interp.namespace_oid(&old_schema) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TypeNotFound(format!("{old_schema}.{name}")));
    };
    let Some(&type_oid) = interp.type_by_qname.get(&(old_nsoid, name.clone())) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TypeNotFound(format!("{old_schema}.{name}")));
    };

    interp.rename_pg_type(type_oid, name.clone(), new_nsoid);

    let arr_key = format!("_{name}");
    if let Some(&arr_oid) = interp.type_by_qname.get(&(old_nsoid, arr_key.clone())) {
        interp.rename_pg_type(arr_oid, arr_key, new_nsoid);
    }

    Ok(())
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Extract `(schema_opt, name, arg_oids)` from an `ObjectWithArgs` target.
fn extract_func_target(
    object: &Option<Box<pg_query::protobuf::Node>>,
    interp: &PgCatalog,
) -> Option<(Option<String>, String, Vec<PgTypeOid>)> {
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

    let arg_oids: Vec<PgTypeOid> = owa
        .objargs
        .iter()
        .filter_map(|n| {
            if let Some(node::Node::TypeName(tn)) = n.node.as_ref() {
                resolve_type_name(tn, interp)
            } else {
                None
            }
        })
        .collect();

    Some((schema, name, arg_oids))
}

/// Resolve `(nspoid, oid)` of a `pg_proc` row matching `predicate`, walking
/// the search path when `schema` is `None`.
fn find_proc(
    snapshot: &PgCatalog,
    schema: Option<&str>,
    name: &str,
    matches: &dyn Fn(&PgProc) -> bool,
) -> Option<(PgNamespaceOid, PgProcOid)> {
    let candidate_schemas: Vec<PgNamespaceOid> = if let Some(s) = schema {
        snapshot.namespace_oid(s).into_iter().collect()
    } else {
        let mut v = Vec::new();
        if let Some(pg_oid) = snapshot.namespace_oid("pg_catalog")
            && !snapshot.search_path.contains(&pg_oid)
        {
            v.push(pg_oid);
        }
        v.extend(snapshot.search_path.iter().copied());
        v
    };
    for nsoid in candidate_schemas {
        if let Some(oids) = snapshot.proc_by_qname.get(&(nsoid, name.to_owned())) {
            for &oid in oids {
                if let Some(p) = snapshot.pg_proc.get(&oid)
                    && matches(p)
                {
                    return Some((nsoid, oid));
                }
            }
        }
    }
    None
}
