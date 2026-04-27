//! CREATE OPERATOR handler.
//!
//! Registers binary (and prefix) operators defined via `CREATE OPERATOR`.
//! The result type is inferred from the procedure's return type by looking up
//! the function signature already registered in the snapshot.

use pg_query::protobuf::{DefineStmt, node};

use crate::qualified_name::QualifiedName;
use crate::schema::OperatorEntry;

use super::DdlError;
use super::util::resolve_type_name;
use crate::pg_catalog::PgCatalog;

pub fn define_operator(interp: &mut PgCatalog, stmt: &DefineStmt) -> Result<(), DdlError> {
    // Operator name: `defnames` holds either `[name]` or `[schema, name]`.
    let parts: Vec<String> = stmt
        .defnames
        .iter()
        .filter_map(|n| match n.node.as_ref()? {
            node::Node::String(s) => Some(s.sval.clone()),
            _ => None,
        })
        .collect();
    let (schema, op_name) = match parts.as_slice() {
        [name] => (
            interp
                .search_path
                .first()
                .cloned()
                .unwrap_or_else(|| "public".to_owned()),
            name.clone(),
        ),
        [schema, name] => (schema.clone(), name.clone()),
        _ => return Ok(()),
    };

    let mut left_type: Option<u32> = None;
    let mut right_type: Option<u32> = None;
    let mut procedure: Option<(Option<String>, String)> = None;

    for opt in &stmt.definition {
        let Some(node::Node::DefElem(de)) = opt.node.as_ref() else {
            continue;
        };
        let Some(arg) = de.arg.as_deref() else {
            continue;
        };
        match de.defname.to_ascii_lowercase().as_str() {
            "leftarg" => {
                if let Some(node::Node::TypeName(tn)) = arg.node.as_ref() {
                    left_type = resolve_type_name(tn, interp);
                }
            }
            "rightarg" => {
                if let Some(node::Node::TypeName(tn)) = arg.node.as_ref() {
                    right_type = resolve_type_name(tn, interp);
                }
            }
            "procedure" | "function" => {
                procedure = parse_func_name(arg);
            }
            _ => {}
        }
    }

    // Right operand is required for both prefix and binary operators.
    let Some(right_oid) = right_type else {
        return Ok(());
    };

    let result_oid = procedure
        .as_ref()
        .and_then(|(schema, name)| {
            resolve_procedure_return(interp, schema.as_deref(), name, left_type, right_oid)
        })
        .unwrap_or(0);

    interp
        .operators_by_name
        .entry(QualifiedName::new(schema, &op_name))
        .or_default()
        .push(OperatorEntry {
            name: op_name,
            left_type_oid: left_type,
            right_type_oid: right_oid,
            result_type_oid: result_oid,
        });

    Ok(())
}

/// Parse a function name from a DefElem argument. `pg_query` represents the
/// value of `PROCEDURE = foo` as a [`TypeName`] whose `names` field holds the
/// (schema-qualified) name parts — the same shape used for other identifier
/// references in DefElem options.
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

/// Look up a procedure in the snapshot and return its result type, matching
/// on the operator's operand types.
fn resolve_procedure_return(
    interp: &PgCatalog,
    schema: Option<&str>,
    name: &str,
    left: Option<u32>,
    right: u32,
) -> Option<u32> {
    let candidates = interp.find_functions(schema, name);
    candidates
        .into_iter()
        .find(|f| match (left, f.arg_types.as_slice()) {
            (Some(l), [a, b]) => *a == l && *b == right,
            (None, [a]) => *a == right,
            _ => false,
        })
        .map(|f| f.return_type_oid)
}
