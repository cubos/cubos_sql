//! CREATE SCHEMA handler.

use pg_query::protobuf::CreateSchemaStmt;

use super::DdlError;
use super::util::ensure_namespace;
use crate::pg_catalog::PgCatalog;

pub fn create_schema(interp: &mut PgCatalog, stmt: &CreateSchemaStmt) -> Result<(), DdlError> {
    if !stmt.schemaname.is_empty() {
        ensure_namespace(interp, &stmt.schemaname)?;
    }
    // Process any inline schema elements (CREATE SCHEMA ... CREATE TABLE ...).
    for elt in &stmt.schema_elts {
        if let Some(node) = elt.node.as_ref() {
            super::apply_statement(interp, node)?;
        }
    }
    Ok(())
}
