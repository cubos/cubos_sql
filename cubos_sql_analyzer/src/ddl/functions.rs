//! CREATE FUNCTION handler (signature registration only).

use pg_query::protobuf::{CreateFunctionStmt, FunctionParameterMode, node};

use crate::qualified_name::QualifiedName;
use crate::schema::{CompositeField, FunctionEntry, oid as builtin_oid};

use super::DdlError;
use super::util::{extract_names, resolve_type_name};
use crate::pg_catalog::PgCatalog;

pub fn create_function(interp: &mut PgCatalog, stmt: &CreateFunctionStmt) -> Result<(), DdlError> {
    let (schema, name) = extract_names(&stmt.funcname, interp);

    // Walk parameters once, splitting IN/INOUT/VARIADIC into the call
    // signature and OUT/TABLE/INOUT into the named output columns.
    // pg_query uses FuncParamDefault for implicit IN parameters; Undefined
    // (0) is the parser's catch-all that we also treat as IN.
    let mut arg_types = Vec::new();
    let mut out_args: Vec<CompositeField> = Vec::new();
    let mut is_variadic = false;
    for param_node in &stmt.parameters {
        let Some(node::Node::FunctionParameter(fp)) = param_node.node.as_ref() else {
            continue;
        };
        let mode =
            FunctionParameterMode::try_from(fp.mode).unwrap_or(FunctionParameterMode::FuncParamIn);
        let resolved_oid = fp
            .arg_type
            .as_ref()
            .and_then(|tn| resolve_type_name(tn, interp))
            .unwrap_or(0);

        match mode {
            FunctionParameterMode::FuncParamIn
            | FunctionParameterMode::FuncParamDefault
            | FunctionParameterMode::Undefined => {
                arg_types.push(resolved_oid);
            }
            FunctionParameterMode::FuncParamVariadic => {
                arg_types.push(resolved_oid);
                is_variadic = true;
            }
            FunctionParameterMode::FuncParamInout => {
                arg_types.push(resolved_oid);
                out_args.push(CompositeField {
                    name: fp.name.clone(),
                    type_oid: resolved_oid,
                    not_null: false,
                });
            }
            FunctionParameterMode::FuncParamOut | FunctionParameterMode::FuncParamTable => {
                out_args.push(CompositeField {
                    name: fp.name.clone(),
                    type_oid: resolved_oid,
                    not_null: false,
                });
            }
        }
    }

    // Resolve return type. PG synthesizes one when there's no explicit
    // RETURNS but OUT/INOUT params are present:
    //   - exactly one OUT/INOUT slot → that slot's type bubbles up.
    //   - multiple slots             → pseudo `record` (out_args carries
    //                                  the named columns for SRF callers).
    let explicit_return_oid = stmt
        .return_type
        .as_ref()
        .and_then(|tn| resolve_type_name(tn, interp));
    let return_type_oid = match explicit_return_oid {
        Some(oid) => oid,
        None => match out_args.len() {
            0 => 0,
            1 => out_args[0].type_oid,
            _ => builtin_oid::RECORD,
        },
    };

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
        out_args,
        num_default_args: 0,
    };

    // Check for existing entry with same (signature, kind). Functions and
    // procedures share the `functions_by_name` bucket but PG treats them as
    // separate object kinds, so a CREATE FUNCTION may coexist with a
    // CREATE PROCEDURE of the same name and signature.
    let key = QualifiedName::new(&entry.schema, &entry.name);
    if let Some(fns) = interp.functions_by_name.get_mut(&key) {
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

    interp.functions_by_name.entry(key).or_default().push(entry);

    Ok(())
}
