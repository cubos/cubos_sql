//! CREATE SCHEMA handler.

use pg_query::protobuf::CreateSchemaStmt;

use super::{DdlError, DdlInterpreter};

pub fn create_schema(interp: &mut DdlInterpreter, stmt: &CreateSchemaStmt) -> Result<(), DdlError> {
    // Schemas are implicit in the snapshot (tracked via "schema.name" keys).
    // Process any inline schema elements (CREATE SCHEMA ... CREATE TABLE ...).
    for elt in &stmt.schema_elts {
        if let Some(node) = elt.node.as_ref() {
            interp.apply_statement(node)?;
        }
    }
    Ok(())
}
