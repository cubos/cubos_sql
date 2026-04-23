//! CREATE FUNCTION handler (signature registration only).

use pg_query::protobuf::{CreateFunctionStmt, FunctionParameterMode, node};

use crate::qualified_name::QualifiedName;
use crate::schema::FunctionEntry;

use super::DdlError;
use super::util::{extract_names, resolve_type_name};
use crate::database::Database;

pub fn create_function(interp: &mut Database, stmt: &CreateFunctionStmt) -> Result<(), DdlError> {
    let (schema, name) = extract_names(&stmt.funcname, &interp.snapshot);

    // Resolve parameter types (IN params only for the signature).
    let mut arg_types = Vec::new();
    let mut is_variadic = false;
    for param_node in &stmt.parameters {
        let Some(node::Node::FunctionParameter(fp)) = param_node.node.as_ref() else {
            continue;
        };
        let mode =
            FunctionParameterMode::try_from(fp.mode).unwrap_or(FunctionParameterMode::FuncParamIn);

        // Skip OUT and TABLE params. Undefined (0) and Default (6) are treated as IN.
        // pg_query uses FuncParamDefault for implicit IN parameters.
        match mode {
            FunctionParameterMode::FuncParamIn
            | FunctionParameterMode::FuncParamInout
            | FunctionParameterMode::FuncParamVariadic
            | FunctionParameterMode::FuncParamDefault
            | FunctionParameterMode::Undefined => {}
            _ => continue,
        }

        if mode == FunctionParameterMode::FuncParamVariadic {
            is_variadic = true;
        }

        if let Some(tn) = &fp.arg_type {
            let oid = resolve_type_name(tn, &interp.snapshot).unwrap_or(0);
            arg_types.push(oid);
        }
    }

    // Resolve return type.
    let return_type_oid = stmt
        .return_type
        .as_ref()
        .and_then(|tn| resolve_type_name(tn, &interp.snapshot))
        .unwrap_or(0);

    let is_set_returning = stmt.return_type.as_ref().is_some_and(|tn| tn.setof);

    // Check options for STRICT (CALLED ON NULL INPUT vs RETURNS NULL ON NULL INPUT).
    let is_strict = stmt.options.iter().any(|n| {
        if let Some(node::Node::DefElem(de)) = n.node.as_ref()
            && de.defname == "strict"
            && let Some(arg) = de.arg.as_deref()
        {
            if let Some(node::Node::Integer(i)) = arg.node.as_ref() {
                return i.ival == 1;
            }
            if let Some(node::Node::Boolean(b)) = arg.node.as_ref() {
                return b.boolval;
            }
        }
        false
    });

    let entry = FunctionEntry {
        name: name.clone(),
        schema,
        arg_types,
        return_type_oid,
        is_aggregate: false,
        is_window: false,
        is_variadic,
        is_set_returning,
        is_strict,
        is_procedure: stmt.is_procedure,
        agg_final_type_oid: None,
        out_args: Vec::new(),
    };

    // Check for existing entry with same (signature, kind). Functions and
    // procedures share the `functions_by_name` bucket but PG treats them as
    // separate object kinds, so a CREATE FUNCTION may coexist with a
    // CREATE PROCEDURE of the same name and signature.
    let key = QualifiedName::new(&entry.schema, &entry.name);
    if let Some(fns) = interp.snapshot.functions_by_name.get_mut(&key) {
        let exists = fns
            .iter()
            .any(|f| f.arg_types == entry.arg_types && f.is_procedure == entry.is_procedure);
        if exists {
            if stmt.replace {
                fns.retain(|f| {
                    f.arg_types != entry.arg_types || f.is_procedure != entry.is_procedure
                });
            } else {
                let kind = if entry.is_procedure {
                    "procedure"
                } else {
                    "function"
                };
                return Err(DdlError::DuplicateObject(format!(
                    "{kind} \"{name}\" already exists with same argument types"
                )));
            }
        }
    }

    interp
        .snapshot
        .functions_by_name
        .entry(key)
        .or_default()
        .push(entry);

    Ok(())
}
