//! CREATE / ALTER SEQUENCE handlers.
//!
//! A sequence is registered as a `pg_class` row with `relkind = Sequence`.
//! The analyzer doesn't model the sequence's numeric state (START, INCREMENT,
//! …) — those options don't affect query typing, so they're accepted as
//! no-ops. DROP SEQUENCE is handled by `ddl/drop.rs` (sequences share the
//! relation-drop path with tables and views). RENAME / SET SCHEMA flow
//! through `ddl/alter.rs`.

use pg_query::protobuf::{AlterSeqStmt, CreateSeqStmt};

use super::DdlError;
use super::util::{ensure_range_var, range_var_names};
use crate::oid::PgClassOid;
use crate::pg_catalog::{PgCatalog, PgClass, RelKind};

/// `CREATE SEQUENCE [IF NOT EXISTS] name [options]`.
pub fn create_sequence(interp: &mut PgCatalog, stmt: &CreateSeqStmt) -> Result<(), DdlError> {
    let rv = stmt
        .sequence
        .as_ref()
        .ok_or_else(|| DdlError::Parse("CREATE SEQUENCE without relation".into()))?;

    let (nsoid, name) = ensure_range_var(interp, rv)?;

    if interp.class_by_qname.contains_key(&(nsoid, name.clone())) {
        if stmt.if_not_exists {
            return Ok(());
        }
        return Err(DdlError::DuplicateObject(format!(
            "relation \"{name}\" already exists"
        )));
    }

    let class_oid = PgClassOid::from_nonzero(interp.alloc_oid()?);
    interp.insert_pg_class(PgClass {
        oid: class_oid,
        relname: name,
        relnamespace: nsoid,
        relkind: RelKind::Sequence,
        // Sequences have a backing row type in PG, but the analyzer never
        // resolves a column list off a sequence, so leave it unset.
        reltype: None,
    });
    Ok(())
}

/// `ALTER SEQUENCE [IF EXISTS] name [options]`.
///
/// Every option (`RESTART`, `INCREMENT BY`, `MINVALUE`, …) is a no-op for
/// static type analysis — this handler only validates that the sequence
/// exists. RENAME TO and SET SCHEMA arrive as `RenameStmt` /
/// `AlterObjectSchemaStmt` instead and are handled in `ddl/alter.rs`.
pub fn alter_sequence(interp: &mut PgCatalog, stmt: &AlterSeqStmt) -> Result<(), DdlError> {
    let Some(rv) = stmt.sequence.as_ref() else {
        return Ok(());
    };
    let (schema, name) = range_var_names(rv, interp);

    let resolved = interp
        .namespace_oid(&schema)
        .and_then(|nsoid| interp.class_by_qname.get(&(nsoid, name.clone())).copied());

    if resolved.is_none() && !stmt.missing_ok {
        return Err(DdlError::TableNotFound(format!(
            "relation \"{name}\" does not exist"
        )));
    }
    Ok(())
}
