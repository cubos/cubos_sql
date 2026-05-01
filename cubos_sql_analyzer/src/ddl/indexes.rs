//! `CREATE INDEX` handler.
//!
//! Indexes don't change query result types — they're invisible to the
//! analyzer's type/nullability inference. We still parse the statement
//! for two reasons:
//!
//! 1. **Volatility check** — PG forbids VOLATILE callees in expression
//!    indexes, since the index would otherwise never agree with itself.
//! 2. **`pg_constraint` for `ON CONFLICT`** — PG treats a non-partial
//!    `UNIQUE INDEX` on column names exactly like a `UNIQUE` constraint
//!    for the purposes of `ON CONFLICT (cols)` matching, so we mirror
//!    that here. Partial unique indexes (with a `WHERE`) are *not*
//!    valid `ON CONFLICT` targets and so we deliberately skip them.

use pg_query::protobuf::{IndexStmt, node};

use super::DdlError;
use super::util::range_var_names;
use super::volatile::{ExprLocation, check_no_volatile};
use crate::oid::PgConstraintOid;
use crate::pg_catalog::{ConType, PgCatalog, PgConstraint};

pub fn create_index(db: &mut PgCatalog, stmt: &IndexStmt) -> Result<(), DdlError> {
    // ── Volatility check on expression indexes ──
    for param in &stmt.index_params {
        let Some(node::Node::IndexElem(elem)) = param.node.as_ref() else {
            continue;
        };
        if let Some(expr) = elem.expr.as_deref() {
            check_no_volatile(expr, ExprLocation::Index, db)?;
        }
    }

    // ── pg_constraint emission for non-partial UNIQUE INDEX ──
    //
    // PG only treats a unique index as a valid `ON CONFLICT` target when
    // it has no predicate (or a predicate that covers every row). A
    // partial unique index `WHERE deleted_at IS NULL` doesn't qualify
    // for the generic insert. By skipping rows with a `where_clause` we
    // make `ON CONFLICT (slug)` correctly fail to find a match for a
    // partial-unique-only schema.
    if stmt.unique && stmt.where_clause.is_none() {
        let Some(rv) = stmt.relation.as_ref() else {
            return Ok(());
        };
        let (schema, table_name) = range_var_names(rv, db);
        let Some(nsoid) = db.namespace_oid(&schema) else {
            return Ok(());
        };
        let Some(class_oid) = db.class_by_qname.get(&(nsoid, table_name.clone())).copied() else {
            return Ok(());
        };

        // `index_params` lists either column names (`name` set) or
        // expressions. For ON CONFLICT (cols) matching we need the
        // attnum list — expression-based UNIQUE INDEXes never line up
        // with a column-list ON CONFLICT, so we skip them.
        let mut conkey: Vec<i16> = Vec::new();
        let mut all_columns = true;
        for param in &stmt.index_params {
            let Some(node::Node::IndexElem(elem)) = param.node.as_ref() else {
                continue;
            };
            if elem.name.is_empty() {
                all_columns = false;
                break;
            }
            let Some(an) = db
                .attributes_of(class_oid)
                .iter()
                .find(|a| a.attname == elem.name)
                .map(|a| a.attnum)
            else {
                all_columns = false;
                break;
            };
            conkey.push(an);
        }
        if !all_columns || conkey.is_empty() {
            return Ok(());
        }

        let conname = if stmt.idxname.is_empty() {
            format!(
                "{table_name}_{}_key",
                stmt.index_params
                    .iter()
                    .filter_map(|p| match p.node.as_ref()? {
                        node::Node::IndexElem(e) if !e.name.is_empty() => Some(e.name.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("_")
            )
        } else {
            stmt.idxname.clone()
        };
        let oid = PgConstraintOid::new(db.alloc_oid()).expect("alloc_oid is non-zero");
        db.insert_pg_constraint(PgConstraint {
            oid,
            conname,
            conrelid: class_oid,
            contype: ConType::Unique,
            conkey,
            confrelid: None,
            confkey: Vec::new(),
        });
    }

    Ok(())
}
