//! DROP statement handler.

use pg_query::protobuf::{DropBehavior, DropStmt, ObjectType, node};

use super::DdlError;
use super::util::{extract_names, node_string, resolve_type_name};
use super::views;
use crate::database::Database;
use crate::qualified_name::QualifiedName;

pub fn drop_objects(interp: &mut Database, stmt: &DropStmt) -> Result<(), DdlError> {
    let obj_type = ObjectType::try_from(stmt.remove_type).unwrap_or(ObjectType::Undefined);
    let cascade = matches!(
        DropBehavior::try_from(stmt.behavior),
        Ok(DropBehavior::DropCascade)
    );

    for obj_node in &stmt.objects {
        match obj_type {
            ObjectType::ObjectTable
            | ObjectType::ObjectView
            | ObjectType::ObjectMatview
            | ObjectType::ObjectSequence => {
                drop_relation(interp, obj_node, stmt.missing_ok, cascade)?;
            }
            ObjectType::ObjectType | ObjectType::ObjectDomain => {
                drop_type(interp, obj_node, stmt.missing_ok, cascade)?;
            }
            ObjectType::ObjectFunction | ObjectType::ObjectProcedure => {
                drop_function(interp, obj_node, stmt.missing_ok, cascade, obj_type)?;
            }
            ObjectType::ObjectExtension => {
                drop_extension(interp, obj_node, stmt.missing_ok, cascade)?;
            }
            ObjectType::ObjectCast => {
                drop_cast(interp, obj_node, stmt.missing_ok)?;
            }
            ObjectType::ObjectOperator => {
                drop_operator(interp, obj_node, stmt.missing_ok)?;
            }
            ObjectType::ObjectAggregate => {
                drop_aggregate(interp, obj_node, stmt.missing_ok, cascade)?;
            }
            ObjectType::ObjectSchema => {
                drop_schema(interp, obj_node, stmt.missing_ok, cascade)?;
            }
            ObjectType::ObjectIndex
            | ObjectType::ObjectTrigger
            | ObjectType::ObjectRule
            | ObjectType::ObjectPolicy
            | ObjectType::ObjectForeignTable => {}
            _ => {}
        }
    }

    Ok(())
}

fn drop_relation(
    interp: &mut Database,
    obj_node: &pg_query::protobuf::Node,
    missing_ok: bool,
    cascade: bool,
) -> Result<(), DdlError> {
    let names = match obj_node.node.as_ref() {
        Some(node::Node::List(list)) => &list.items,
        _ => return Ok(()),
    };

    let (schema, name) = extract_names(names, &interp.snapshot);
    let key = QualifiedName::new(&schema, &name);

    if !interp.snapshot.tables.contains_key(&key) {
        if missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(key.to_string()));
    }

    // Check for dependent views.
    let dependent_views = views::find_dependent_views(&interp.snapshot, &key);
    if !dependent_views.is_empty() && !cascade {
        let view_names: Vec<String> = dependent_views.iter().map(|k| k.to_string()).collect();
        return Err(DdlError::DependencyError(format!(
            "cannot drop {key} because view(s) {} depend on it",
            view_names.join(", "),
        )));
    }

    // CASCADE: drop dependent views first.
    if !dependent_views.is_empty() {
        views::drop_views(&mut interp.snapshot, &dependent_views);
    }

    interp.snapshot.tables.remove(&key);

    // Also remove the composite type and its array type.
    if let Some(&composite_oid) = interp.snapshot.type_by_name.get(&key) {
        interp.snapshot.types.remove(&composite_oid);
        interp.snapshot.type_by_name.remove(&key);

        let array_key = QualifiedName::new(&schema, format!("_{name}"));
        if let Some(&array_oid) = interp.snapshot.type_by_name.get(&array_key) {
            interp.snapshot.types.remove(&array_oid);
            interp.snapshot.type_by_name.remove(&array_key);
        }
    }

    Ok(())
}

fn drop_type(
    interp: &mut Database,
    obj_node: &pg_query::protobuf::Node,
    missing_ok: bool,
    cascade: bool,
) -> Result<(), DdlError> {
    let names: &[pg_query::protobuf::Node] = match obj_node.node.as_ref() {
        Some(node::Node::TypeName(tn)) => &tn.names,
        Some(node::Node::List(list)) => &list.items,
        _ => return Ok(()),
    };

    let (schema, name) = extract_names(names, &interp.snapshot);
    let key = QualifiedName::new(&schema, &name);

    let Some(&type_oid) = interp.snapshot.type_by_name.get(&key) else {
        if missing_ok {
            return Ok(());
        }
        return Err(DdlError::TypeNotFound(key.to_string()));
    };

    // Check for tables with columns of this type.
    let dependents: Vec<QualifiedName> = interp
        .snapshot
        .tables
        .iter()
        .filter(|(_, t)| t.columns.iter().any(|c| c.type_oid == type_oid))
        .map(|(k, _)| k.clone())
        .collect();

    // Also check array type usage.
    let array_key = QualifiedName::new(&schema, format!("_{name}"));
    let array_oid = interp.snapshot.type_by_name.get(&array_key).copied();

    let array_dependents: Vec<QualifiedName> = if let Some(arr_oid) = array_oid {
        interp
            .snapshot
            .tables
            .iter()
            .filter(|(_, t)| t.columns.iter().any(|c| c.type_oid == arr_oid))
            .map(|(k, _)| k.clone())
            .collect()
    } else {
        Vec::new()
    };

    let all_dep_names: Vec<String> = dependents
        .iter()
        .chain(array_dependents.iter())
        .map(|k| k.to_string())
        .collect();

    if !all_dep_names.is_empty() && !cascade {
        return Err(DdlError::DependencyError(format!(
            "cannot drop type {key} because table(s) {} depend on it",
            all_dep_names.join(", "),
        )));
    }

    // CASCADE: drop columns of this type from dependent tables.
    if cascade {
        for table_key in &dependents {
            if let Some(table) = interp.snapshot.tables.get_mut(table_key) {
                table.columns.retain(|c| c.type_oid != type_oid);
            }
        }
        if let Some(arr_oid) = array_oid {
            for table_key in &array_dependents {
                if let Some(table) = interp.snapshot.tables.get_mut(table_key) {
                    table.columns.retain(|c| c.type_oid != arr_oid);
                }
            }
        }
    }

    interp.snapshot.types.remove(&type_oid);
    interp.snapshot.type_by_name.remove(&key);

    if let Some(arr_oid) = array_oid {
        interp.snapshot.types.remove(&arr_oid);
        interp.snapshot.type_by_name.remove(&array_key);
    }

    Ok(())
}

fn drop_extension(
    interp: &mut Database,
    obj_node: &pg_query::protobuf::Node,
    missing_ok: bool,
    _cascade: bool,
) -> Result<(), DdlError> {
    // Extension name is a String node.
    let name = match obj_node.node.as_ref() {
        Some(node::Node::String(s)) => s.sval.clone(),
        _ => return Ok(()),
    };

    let Some(installed) = interp.installed_extensions.remove(&name) else {
        if missing_ok {
            return Ok(());
        }
        return Err(DdlError::ExtensionError(format!(
            "extension \"{name}\" does not exist"
        )));
    };

    // Remove types created by the extension.
    for oid in &installed.type_oids {
        if let Some(te) = interp.snapshot.types.remove(oid) {
            let key = QualifiedName::new(&te.schema, &te.name);
            interp.snapshot.type_by_name.remove(&key);
        }
    }

    // Remove functions created by the extension.
    for fname in &installed.function_names {
        interp.snapshot.functions_by_name.remove(fname);
    }

    // Remove casts created by the extension.
    for cast_key in &installed.cast_keys {
        interp.snapshot.casts.remove(cast_key);
    }

    Ok(())
}

/// `DROP FUNCTION` / `DROP PROCEDURE`. The `remove_type` of the enclosing
/// `DropStmt` determines which bucket of overloads to consider, so callers
/// tell us via `expected_kind`.
///
/// `cascade` is accepted (and required syntactically for consistency with
/// PostgreSQL) but has no effect in the analyzer: functions are not allowed
/// to participate in query-level dependencies that affect static typing.
fn drop_function(
    interp: &mut Database,
    obj_node: &pg_query::protobuf::Node,
    missing_ok: bool,
    _cascade: bool,
    expected_kind: ObjectType,
) -> Result<(), DdlError> {
    let Some(node::Node::ObjectWithArgs(owa)) = obj_node.node.as_ref() else {
        return Ok(());
    };

    let parts: Vec<String> = owa
        .objname
        .iter()
        .filter_map(node_string)
        .map(|s| s.to_owned())
        .collect();

    let (schema_opt, name) = match parts.as_slice() {
        [name] => (None, name.clone()),
        [schema, name] => (Some(schema.clone()), name.clone()),
        _ => return Ok(()),
    };

    // Resolve argument types from objargs to match the specific overload.
    let arg_oids: Vec<u32> = owa
        .objargs
        .iter()
        .filter_map(|n| {
            if let Some(node::Node::TypeName(tn)) = n.node.as_ref() {
                super::util::resolve_type_name(tn, &interp.snapshot)
            } else {
                None
            }
        })
        .collect();

    // A DROP FUNCTION must match `is_procedure = false`, and DROP PROCEDURE
    // the opposite — PG enforces the same asymmetry to prevent accidentally
    // dropping an object of the wrong kind.
    let want_procedure = expected_kind == ObjectType::ObjectProcedure;

    let matches_overload = |f: &crate::schema::FunctionEntry| -> bool {
        let kind_ok = !f.is_aggregate && f.is_procedure == want_procedure;
        if !kind_ok {
            return false;
        }
        if owa.objargs.is_empty() && owa.args_unspecified {
            true
        } else {
            f.arg_types == arg_oids
        }
    };

    // Find the schema-qualified key: either the one given explicitly, or the
    // first one on `search_path` that actually has a matching overload.
    let target_key = resolve_function_key(
        &interp.snapshot,
        schema_opt.as_deref(),
        &name,
        &matches_overload,
    );

    let existed = target_key.as_ref().is_some();

    if !existed && !missing_ok {
        let kind = if want_procedure {
            "procedure"
        } else {
            "function"
        };
        return Err(DdlError::DependencyError(format!(
            "{kind} {name} does not exist"
        )));
    }

    if let Some(key) = target_key
        && let Some(fns) = interp.snapshot.functions_by_name.get_mut(&key)
    {
        fns.retain(|f| !matches_overload(f));
        if fns.is_empty() {
            interp.snapshot.functions_by_name.remove(&key);
        }
    }

    Ok(())
}

/// Resolve the schema-qualified key for a function-like object.
///
/// If `schema` is `Some`, returns the single qualified name when any overload
/// matches the predicate. If `schema` is `None`, scans the `search_path`
/// (`pg_catalog` first unless explicitly listed) for the first schema whose
/// bucket holds a matching overload.
fn resolve_function_key(
    snapshot: &crate::schema::SchemaSnapshot,
    schema: Option<&str>,
    name: &str,
    matches: &dyn Fn(&crate::schema::FunctionEntry) -> bool,
) -> Option<crate::qualified_name::QualifiedName> {
    if let Some(s) = schema {
        let key = crate::qualified_name::QualifiedName::new(s, name);
        return snapshot
            .functions_by_name
            .get(&key)
            .filter(|fns| fns.iter().any(matches))
            .map(|_| key);
    }
    if !snapshot.search_path.iter().any(|s| s == "pg_catalog") {
        let k = crate::qualified_name::QualifiedName::new("pg_catalog", name);
        if snapshot
            .functions_by_name
            .get(&k)
            .is_some_and(|fns| fns.iter().any(matches))
        {
            return Some(k);
        }
    }
    for s in &snapshot.search_path {
        let k = crate::qualified_name::QualifiedName::new(s.clone(), name);
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

/// DROP AGGREGATE name(argtypes) — shares storage with functions, but we
/// only remove entries where `is_aggregate = true` so a DROP AGGREGATE cannot
/// accidentally remove a scalar function with the same signature.
///
/// `cascade` is accepted for syntactic parity with PostgreSQL but has no
/// effect here for the same reason as `drop_function`.
fn drop_aggregate(
    interp: &mut Database,
    obj_node: &pg_query::protobuf::Node,
    missing_ok: bool,
    _cascade: bool,
) -> Result<(), DdlError> {
    let Some(node::Node::ObjectWithArgs(owa)) = obj_node.node.as_ref() else {
        return Ok(());
    };

    let parts: Vec<String> = owa
        .objname
        .iter()
        .filter_map(node_string)
        .map(|s| s.to_owned())
        .collect();
    let (schema_opt, name) = match parts.as_slice() {
        [name] => (None, name.clone()),
        [schema, name] => (Some(schema.clone()), name.clone()),
        _ => return Ok(()),
    };

    // Resolve argument types. A zero-arg aggregate (`DROP AGGREGATE name(*)`)
    // has an empty objargs list.
    let arg_oids: Vec<u32> = owa
        .objargs
        .iter()
        .filter_map(|n| {
            if let Some(node::Node::TypeName(tn)) = n.node.as_ref() {
                resolve_type_name(tn, &interp.snapshot)
            } else {
                None
            }
        })
        .collect();

    let matches = |f: &crate::schema::FunctionEntry| f.is_aggregate && f.arg_types == arg_oids;
    let target_key = resolve_function_key(&interp.snapshot, schema_opt.as_deref(), &name, &matches);

    if target_key.is_none() && !missing_ok {
        return Err(DdlError::DependencyError(format!(
            "aggregate {name}({}) does not exist",
            format_arg_oids(&arg_oids, &interp.snapshot),
        )));
    }

    if let Some(key) = target_key
        && let Some(fns) = interp.snapshot.functions_by_name.get_mut(&key)
    {
        fns.retain(|f| !matches(f));
        if fns.is_empty() {
            interp.snapshot.functions_by_name.remove(&key);
        }
    }

    Ok(())
}

/// DROP OPERATOR name(lefttype, righttype) — the pg_query AST always emits
/// two objargs, with a `TypeName` whose names list is empty for NONE (prefix
/// operators). We detect that by inspecting `names.is_empty()`.
fn drop_operator(
    interp: &mut Database,
    obj_node: &pg_query::protobuf::Node,
    missing_ok: bool,
) -> Result<(), DdlError> {
    let Some(node::Node::ObjectWithArgs(owa)) = obj_node.node.as_ref() else {
        return Ok(());
    };

    // Operator name: `objname` holds either `[name]` or `[schema, name]`.
    let parts: Vec<String> = owa
        .objname
        .iter()
        .filter_map(node_string)
        .map(|s| s.to_owned())
        .collect();
    let (schema_opt, op_name) = match parts.as_slice() {
        [name] => (None, name.clone()),
        [schema, name] => (Some(schema.clone()), name.clone()),
        _ => return Ok(()),
    };

    let (left_oid, right_oid) = parse_operator_arg_types(&owa.objargs, &interp.snapshot);
    let Some(right_oid) = right_oid else {
        // Right operand is required for both binary and prefix operators.
        return Ok(());
    };

    let matches = |o: &crate::schema::OperatorEntry| {
        o.left_type_oid == left_oid && o.right_type_oid == right_oid
    };

    let target_key =
        resolve_operator_key(&interp.snapshot, schema_opt.as_deref(), &op_name, &matches);

    if target_key.is_none() && !missing_ok {
        return Err(DdlError::DependencyError(format!(
            "operator {op_name} does not exist for the requested operand types"
        )));
    }

    if let Some(key) = target_key
        && let Some(ops) = interp.snapshot.operators_by_name.get_mut(&key)
    {
        ops.retain(|o| !matches(o));
        if ops.is_empty() {
            interp.snapshot.operators_by_name.remove(&key);
        }
    }

    Ok(())
}

fn resolve_operator_key(
    snapshot: &crate::schema::SchemaSnapshot,
    schema: Option<&str>,
    name: &str,
    matches: &dyn Fn(&crate::schema::OperatorEntry) -> bool,
) -> Option<crate::qualified_name::QualifiedName> {
    if let Some(s) = schema {
        let key = crate::qualified_name::QualifiedName::new(s, name);
        return snapshot
            .operators_by_name
            .get(&key)
            .filter(|ops| ops.iter().any(matches))
            .map(|_| key);
    }
    if !snapshot.search_path.iter().any(|s| s == "pg_catalog") {
        let k = crate::qualified_name::QualifiedName::new("pg_catalog", name);
        if snapshot
            .operators_by_name
            .get(&k)
            .is_some_and(|ops| ops.iter().any(matches))
        {
            return Some(k);
        }
    }
    for s in &snapshot.search_path {
        let k = crate::qualified_name::QualifiedName::new(s.clone(), name);
        if snapshot
            .operators_by_name
            .get(&k)
            .is_some_and(|ops| ops.iter().any(matches))
        {
            return Some(k);
        }
    }
    None
}

/// Parse `(left, right)` type OIDs from the two-element `objargs` of a
/// `DROP OPERATOR`. A `TypeName` with an empty `names` list stands for
/// `NONE`, indicating a prefix operator (no left operand).
fn parse_operator_arg_types(
    objargs: &[pg_query::protobuf::Node],
    snapshot: &crate::schema::SchemaSnapshot,
) -> (Option<u32>, Option<u32>) {
    let resolve = |n: &pg_query::protobuf::Node| -> Option<u32> {
        if let Some(node::Node::TypeName(tn)) = n.node.as_ref() {
            if tn.names.is_empty() {
                return None; // NONE — prefix operator
            }
            return resolve_type_name(tn, snapshot);
        }
        None
    };

    match objargs {
        [l, r] => (resolve(l), resolve(r)),
        [r] => (None, resolve(r)),
        _ => (None, None),
    }
}

/// DROP CAST (source AS target) — objects list contains a single `List`
/// with two `TypeName` elements.
fn drop_cast(
    interp: &mut Database,
    obj_node: &pg_query::protobuf::Node,
    missing_ok: bool,
) -> Result<(), DdlError> {
    let items = match obj_node.node.as_ref() {
        Some(node::Node::List(list)) => &list.items,
        _ => return Ok(()),
    };

    let (src_node, tgt_node) = match items.as_slice() {
        [s, t] => (s, t),
        _ => return Ok(()),
    };

    let src_oid = match src_node.node.as_ref() {
        Some(node::Node::TypeName(tn)) => resolve_type_name(tn, &interp.snapshot),
        _ => None,
    };
    let tgt_oid = match tgt_node.node.as_ref() {
        Some(node::Node::TypeName(tn)) => resolve_type_name(tn, &interp.snapshot),
        _ => None,
    };

    let (Some(src), Some(tgt)) = (src_oid, tgt_oid) else {
        if missing_ok {
            return Ok(());
        }
        return Err(DdlError::TypeNotFound("cast source or target type".into()));
    };

    let key = format!("{src}:{tgt}");
    if interp.snapshot.casts.remove(&key).is_none() && !missing_ok {
        return Err(DdlError::DependencyError(format!(
            "cast from OID {src} to OID {tgt} does not exist"
        )));
    }

    Ok(())
}

/// Format a list of argument OIDs as PG type names for error messages.
/// Falls back to `"oid:N"` when a type isn't in the snapshot.
fn format_arg_oids(oids: &[u32], snapshot: &crate::schema::SchemaSnapshot) -> String {
    oids.iter()
        .map(|oid| {
            snapshot
                .get_type(*oid)
                .map(|t| t.name.clone())
                .unwrap_or_else(|| format!("oid:{oid}"))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// DROP SCHEMA name [CASCADE | RESTRICT].
///
/// Without CASCADE: fail if the schema contains any objects we track.
/// With CASCADE: remove every table, type, and function living in that
/// schema (transitively dropping views that depend on them via the normal
/// `drop_views` machinery).
fn drop_schema(
    interp: &mut Database,
    obj_node: &pg_query::protobuf::Node,
    missing_ok: bool,
    cascade: bool,
) -> Result<(), DdlError> {
    let name = match obj_node.node.as_ref() {
        Some(node::Node::String(s)) => s.sval.clone(),
        _ => return Ok(()),
    };

    // PG's system schemas are never in our snapshot's search_path writable
    // surface, but users sometimes DROP SCHEMA IF EXISTS them. Treat absence
    // as missing.
    let has_objects = interp.snapshot.tables.keys().any(|k| k.schema == name)
        || interp
            .snapshot
            .type_by_name
            .keys()
            .any(|k| k.schema == name)
        || interp
            .snapshot
            .functions_by_name
            .values()
            .any(|fns| fns.iter().any(|f| f.schema == name));

    let exists = interp.snapshot.schemas.contains(&name)
        || interp.snapshot.search_path.contains(&name)
        || has_objects;

    if !exists {
        if missing_ok {
            return Ok(());
        }
        return Err(DdlError::DependencyError(format!(
            "schema \"{name}\" does not exist"
        )));
    }

    if has_objects && !cascade {
        return Err(DdlError::DependencyError(format!(
            "cannot drop schema \"{name}\" because other objects depend on it"
        )));
    }

    // CASCADE: gather everything in this schema.
    let tables_to_drop: Vec<QualifiedName> = interp
        .snapshot
        .tables
        .keys()
        .filter(|k| k.schema == name)
        .cloned()
        .collect();

    // Drop views/tables. `drop_views` transitively removes dependents; for
    // plain tables we also need to strip the composite type + array type,
    // mirroring the `drop_relation` cleanup logic.
    views::drop_views(&mut interp.snapshot, &tables_to_drop);
    for key in &tables_to_drop {
        if let Some(te) = interp.snapshot.tables.remove(key)
            && let Some(&ctype_oid) = interp.snapshot.type_by_name.get(key)
        {
            interp.snapshot.types.remove(&ctype_oid);
            interp.snapshot.type_by_name.remove(key);
            let arr_key = QualifiedName::new(&te.schema, format!("_{}", te.name));
            if let Some(arr_oid) = interp.snapshot.type_by_name.remove(&arr_key) {
                interp.snapshot.types.remove(&arr_oid);
            }
        }
    }

    // Remove all remaining types in this schema (enums, domains, ranges,
    // standalone composites, …).
    let type_keys: Vec<QualifiedName> = interp
        .snapshot
        .type_by_name
        .keys()
        .filter(|k| k.schema == name)
        .cloned()
        .collect();
    for key in type_keys {
        if let Some(oid) = interp.snapshot.type_by_name.remove(&key) {
            interp.snapshot.types.remove(&oid);
        }
    }

    // Remove all functions and operators in this schema.
    interp
        .snapshot
        .functions_by_name
        .retain(|k, _| k.schema != name);
    interp
        .snapshot
        .operators_by_name
        .retain(|k, _| k.schema != name);

    // Finally, drop the schema itself from the search_path and the known set.
    interp.snapshot.search_path.retain(|s| s != &name);
    interp.snapshot.schemas.remove(&name);

    Ok(())
}
