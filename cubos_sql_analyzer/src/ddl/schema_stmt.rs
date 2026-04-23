//! CREATE SCHEMA handler.

use pg_query::protobuf::CreateSchemaStmt;

use super::DdlError;
use crate::database::Database;

pub fn create_schema(interp: &mut Database, stmt: &CreateSchemaStmt) -> Result<(), DdlError> {
    // Register the schema name so `DROP SCHEMA` can distinguish between
    // "schema is empty" and "schema doesn't exist".
    if !stmt.schemaname.is_empty() {
        interp.snapshot.schemas.insert(stmt.schemaname.clone());
    }
    // Process any inline schema elements (CREATE SCHEMA ... CREATE TABLE ...).
    for elt in &stmt.schema_elts {
        if let Some(node) = elt.node.as_ref() {
            super::apply_statement(interp, node)?;
        }
    }
    Ok(())
}
