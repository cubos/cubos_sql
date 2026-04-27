//! CREATE AGGREGATE handler.
//!
//! Aggregates are stored in the same `functions_by_name` map as ordinary
//! functions, with `is_aggregate = true`. The reported return type is the
//! `FINALFUNC` return type when one is specified, otherwise the `STYPE`
//! (state type), mirroring how PostgreSQL resolves aggregate result types.

use pg_query::protobuf::{DefineStmt, FunctionParameterMode, node};

use crate::schema::FunctionEntry;

use super::DdlError;
use super::util::{extract_names, resolve_type_name};
use crate::pg_catalog::PgCatalog;

pub fn define_aggregate(interp: &mut PgCatalog, stmt: &DefineStmt) -> Result<(), DdlError> {
    let (schema, name) = extract_names(&stmt.defnames, interp);

    // Argument types come from `args`. The shape varies between
    //   CREATE AGGREGATE name (type1, type2)        — bare type list
    //   CREATE AGGREGATE name (a int, b int)        — FunctionParameter list
    //   CREATE AGGREGATE name (* )                  — zero-arg aggregate
    // The bare-list form may be wrapped in a nested `args[0] = List`. We
    // accept both shapes for robustness.
    let mut arg_types: Vec<u32> = Vec::new();
    let mut is_variadic = false;
    let arg_nodes: Vec<&pg_query::protobuf::Node> = if stmt.args.len() == 2
        && let Some(node::Node::List(list)) = stmt.args[0].node.as_ref()
    {
        // Wrapped form: args = [List(types), Integer(num_direct_args)]
        list.items.iter().collect()
    } else {
        stmt.args.iter().collect()
    };

    for arg_node in arg_nodes {
        match arg_node.node.as_ref() {
            Some(node::Node::FunctionParameter(fp)) => {
                let mode = FunctionParameterMode::try_from(fp.mode)
                    .unwrap_or(FunctionParameterMode::FuncParamIn);
                if mode == FunctionParameterMode::FuncParamVariadic {
                    is_variadic = true;
                }
                if let Some(tn) = &fp.arg_type {
                    let oid = resolve_type_name(tn, interp).unwrap_or(0);
                    arg_types.push(oid);
                }
            }
            Some(node::Node::TypeName(tn)) => {
                let oid = resolve_type_name(tn, interp).unwrap_or(0);
                arg_types.push(oid);
            }
            _ => {}
        }
    }

    // Walk the option list (`SFUNC`, `STYPE`, `FINALFUNC`, …).
    let mut state_type: Option<u32> = None;
    let mut finalfunc: Option<(Option<String>, String)> = None;

    for opt in &stmt.definition {
        let Some(node::Node::DefElem(de)) = opt.node.as_ref() else {
            continue;
        };
        let Some(arg) = de.arg.as_deref() else {
            continue;
        };
        match de.defname.to_ascii_lowercase().as_str() {
            "stype" => {
                if let Some(node::Node::TypeName(tn)) = arg.node.as_ref() {
                    state_type = resolve_type_name(tn, interp);
                }
            }
            "finalfunc" => {
                finalfunc = parse_func_name(arg);
            }
            _ => {}
        }
    }

    // Determine the aggregate's effective return type.
    //   1. FINALFUNC return type, if a final function is declared.
    //   2. Otherwise the state type.
    let final_return = finalfunc.as_ref().and_then(|(schema, name)| {
        let candidates = interp.find_functions(schema.as_deref(), name);
        candidates.into_iter().next().map(|f| f.return_type_oid)
    });

    let return_type_oid = final_return.or(state_type).unwrap_or(0);

    let entry = FunctionEntry {
        name: name.clone(),
        schema,
        arg_types,
        return_type_oid,
        is_aggregate: true,
        is_window: false,
        is_variadic,
        is_set_returning: false,
        is_strict: false,
        is_procedure: false,
        agg_final_type_oid: final_return,
        out_args: Vec::new(),
        num_default_args: 0,
    };

    let key = crate::qualified_name::QualifiedName::new(&entry.schema, &entry.name);
    interp.functions_by_name.entry(key).or_default().push(entry);

    Ok(())
}

/// Parse a function name from a DefElem argument. Same shapes as in
/// `operators::parse_func_name` (TypeName / String / List).
fn parse_func_name(arg: &pg_query::protobuf::Node) -> Option<(Option<String>, String)> {
    let parts: Vec<&str> = match arg.node.as_ref()? {
        node::Node::TypeName(tn) => tn
            .names
            .iter()
            .filter_map(|n| match n.node.as_ref()? {
                node::Node::String(s) => Some(s.sval.as_str()),
                _ => None,
            })
            .collect(),
        node::Node::String(s) => vec![s.sval.as_str()],
        node::Node::List(list) => list
            .items
            .iter()
            .filter_map(|n| match n.node.as_ref()? {
                node::Node::String(s) => Some(s.sval.as_str()),
                _ => None,
            })
            .collect(),
        _ => return None,
    };

    match parts.as_slice() {
        [schema, name] => Some((Some((*schema).to_owned()), (*name).to_owned())),
        [name] => Some((None, (*name).to_owned())),
        _ => None,
    }
}
