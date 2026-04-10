//! DROP statement handler.

use pg_query::protobuf::{DropBehavior, DropStmt, ObjectType, node};

use super::util::{extract_names, node_string, resolve_type_name};
use super::views;
use super::{DdlError, DdlInterpreter};

pub fn drop_objects(interp: &mut DdlInterpreter, stmt: &DropStmt) -> Result<(), DdlError> {
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
                drop_function(interp, obj_node, stmt.missing_ok)?;
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
                drop_aggregate(interp, obj_node, stmt.missing_ok)?;
            }
            ObjectType::ObjectSchema
            | ObjectType::ObjectIndex
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
    interp: &mut DdlInterpreter,
    obj_node: &pg_query::protobuf::Node,
    missing_ok: bool,
    cascade: bool,
) -> Result<(), DdlError> {
    let names = match obj_node.node.as_ref() {
        Some(node::Node::List(list)) => &list.items,
        _ => return Ok(()),
    };

    let (schema, name) = extract_names(names, &interp.snapshot);
    let key = format!("{schema}.{name}");

    let Some(&table_oid) = interp.snapshot.table_by_name.get(&key) else {
        if missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(key));
    };

    // Check for dependent views.
    let dependent_views = views::find_dependent_views(&interp.snapshot, table_oid);
    if !dependent_views.is_empty() && !cascade {
        let view_names: Vec<String> = dependent_views
            .iter()
            .filter_map(|oid| interp.snapshot.tables.get(oid))
            .map(|t| format!("{}.{}", t.schema, t.name))
            .collect();
        return Err(DdlError::DependencyError(format!(
            "cannot drop {schema}.{name} because view(s) {} depend on it",
            view_names.join(", "),
        )));
    }

    // CASCADE: drop dependent views first.
    if !dependent_views.is_empty() {
        views::drop_views(&mut interp.snapshot, &dependent_views);
    }

    interp.snapshot.table_by_name.remove(&key);
    interp.snapshot.tables.remove(&table_oid);

    // Also remove the composite type and its array type.
    if let Some(&composite_oid) = interp.snapshot.type_by_name.get(&key) {
        interp.snapshot.types.remove(&composite_oid);
        interp.snapshot.type_by_name.remove(&key);

        let array_key = format!("{schema}._{name}");
        if let Some(&array_oid) = interp.snapshot.type_by_name.get(&array_key) {
            interp.snapshot.types.remove(&array_oid);
            interp.snapshot.type_by_name.remove(&array_key);
        }
    }

    Ok(())
}

fn drop_type(
    interp: &mut DdlInterpreter,
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
    let key = format!("{schema}.{name}");

    let Some(&type_oid) = interp.snapshot.type_by_name.get(&key) else {
        if missing_ok {
            return Ok(());
        }
        return Err(DdlError::TypeNotFound(key));
    };

    // Check for tables with columns of this type.
    let dependents: Vec<(u32, String)> = interp
        .snapshot
        .tables
        .iter()
        .filter(|(_, t)| t.columns.iter().any(|c| c.type_oid == type_oid))
        .map(|(&oid, t)| (oid, format!("{}.{}", t.schema, t.name)))
        .collect();

    // Also check array type usage.
    let array_key = format!("{schema}._{name}");
    let array_oid = interp.snapshot.type_by_name.get(&array_key).copied();

    let array_dependents: Vec<(u32, String)> = if let Some(arr_oid) = array_oid {
        interp
            .snapshot
            .tables
            .iter()
            .filter(|(_, t)| t.columns.iter().any(|c| c.type_oid == arr_oid))
            .map(|(&oid, t)| (oid, format!("{}.{}", t.schema, t.name)))
            .collect()
    } else {
        Vec::new()
    };

    let all_dep_names: Vec<&str> = dependents
        .iter()
        .chain(array_dependents.iter())
        .map(|(_, n)| n.as_str())
        .collect();

    if !all_dep_names.is_empty() && !cascade {
        return Err(DdlError::DependencyError(format!(
            "cannot drop type {key} because table(s) {} depend on it",
            all_dep_names.join(", "),
        )));
    }

    // CASCADE: drop columns of this type from dependent tables.
    if cascade {
        for (table_oid, _) in &dependents {
            if let Some(table) = interp.snapshot.tables.get_mut(table_oid) {
                table.columns.retain(|c| c.type_oid != type_oid);
            }
        }
        if let Some(arr_oid) = array_oid {
            for (table_oid, _) in &array_dependents {
                if let Some(table) = interp.snapshot.tables.get_mut(table_oid) {
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
    interp: &mut DdlInterpreter,
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
            let key = format!("{}.{}", te.schema, te.name);
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

fn drop_function(
    interp: &mut DdlInterpreter,
    obj_node: &pg_query::protobuf::Node,
    _missing_ok: bool,
) -> Result<(), DdlError> {
    let Some(node::Node::ObjectWithArgs(owa)) = obj_node.node.as_ref() else {
        return Ok(());
    };

    let parts: Vec<&str> = owa.objname.iter().filter_map(node_string).collect();

    let name = match parts.last() {
        Some(n) => (*n).to_owned(),
        None => return Ok(()),
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

    if let Some(fns) = interp.snapshot.functions_by_name.get_mut(&name) {
        // Remove only the overload matching the argument types.
        // If objargs was empty (no signature specified), remove all.
        if owa.objargs.is_empty() && owa.args_unspecified {
            fns.clear();
        } else {
            fns.retain(|f| f.arg_types != arg_oids);
        }
        // Clean up empty entries.
        if fns.is_empty() {
            interp.snapshot.functions_by_name.remove(&name);
        }
    }

    Ok(())
}

/// DROP AGGREGATE name(argtypes) — shares storage with functions, but we
/// only remove entries where `is_aggregate = true` so a DROP AGGREGATE cannot
/// accidentally remove a scalar function with the same signature.
fn drop_aggregate(
    interp: &mut DdlInterpreter,
    obj_node: &pg_query::protobuf::Node,
    missing_ok: bool,
) -> Result<(), DdlError> {
    let Some(node::Node::ObjectWithArgs(owa)) = obj_node.node.as_ref() else {
        return Ok(());
    };

    let parts: Vec<&str> = owa.objname.iter().filter_map(node_string).collect();
    let name = match parts.last() {
        Some(n) => (*n).to_owned(),
        None => return Ok(()),
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

    let existed = interp
        .snapshot
        .functions_by_name
        .get(&name)
        .is_some_and(|fns| {
            fns.iter()
                .any(|f| f.is_aggregate && f.arg_types == arg_oids)
        });

    if !existed && !missing_ok {
        return Err(DdlError::DependencyError(format!(
            "aggregate {name}({}) does not exist",
            format_arg_oids(&arg_oids, &interp.snapshot),
        )));
    }

    if let Some(fns) = interp.snapshot.functions_by_name.get_mut(&name) {
        fns.retain(|f| !(f.is_aggregate && f.arg_types == arg_oids));
        if fns.is_empty() {
            interp.snapshot.functions_by_name.remove(&name);
        }
    }

    Ok(())
}

/// DROP OPERATOR name(lefttype, righttype) — the pg_query AST always emits
/// two objargs, with a `TypeName` whose names list is empty for NONE (prefix
/// operators). We detect that by inspecting `names.is_empty()`.
fn drop_operator(
    interp: &mut DdlInterpreter,
    obj_node: &pg_query::protobuf::Node,
    missing_ok: bool,
) -> Result<(), DdlError> {
    let Some(node::Node::ObjectWithArgs(owa)) = obj_node.node.as_ref() else {
        return Ok(());
    };

    // Operator name is the last element of objname (may be schema-qualified).
    let op_name = match owa.objname.iter().filter_map(node_string).last() {
        Some(n) => n.to_owned(),
        None => return Ok(()),
    };

    let (left_oid, right_oid) = parse_operator_arg_types(&owa.objargs, &interp.snapshot);
    let Some(right_oid) = right_oid else {
        // Right operand is required for both binary and prefix operators.
        return Ok(());
    };

    let existed = interp
        .snapshot
        .operators_by_name
        .get(&op_name)
        .is_some_and(|ops| {
            ops.iter()
                .any(|o| o.left_type_oid == left_oid && o.right_type_oid == right_oid)
        });

    if !existed && !missing_ok {
        return Err(DdlError::DependencyError(format!(
            "operator {op_name} does not exist for the requested operand types"
        )));
    }

    if let Some(ops) = interp.snapshot.operators_by_name.get_mut(&op_name) {
        ops.retain(|o| !(o.left_type_oid == left_oid && o.right_type_oid == right_oid));
        if ops.is_empty() {
            interp.snapshot.operators_by_name.remove(&op_name);
        }
    }

    Ok(())
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
    interp: &mut DdlInterpreter,
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
        return Err(DdlError::TypeNotFound(
            "cast source or target type".into(),
        ));
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
