//! CREATE TABLE and ALTER TABLE DDL handlers.

use pg_query::protobuf::{
    AlterTableCmd, AlterTableStmt, AlterTableType, ConstrType, CreateStmt, DropBehavior, node,
};

use crate::oid::{PgClassOid, PgTypeOid};
use crate::pg_catalog::{
    AttGenerated, PgAttribute, PgClass, PgType, RelKind, TypCategory, TypType,
};

use super::DdlError;
use super::util::{
    ensure_range_var, range_var_names, register_composite_to_record_cast, resolve_type_name,
};
use super::views;
use crate::pg_catalog::PgCatalog;

// ─── CREATE TABLE ───────────────────────────────────────────────────────────

pub fn create_table(interp: &mut PgCatalog, stmt: &CreateStmt) -> Result<(), DdlError> {
    let rv = stmt
        .relation
        .as_ref()
        .ok_or_else(|| DdlError::Parse("CREATE TABLE without relation".into()))?;

    let (nsoid, name) = ensure_range_var(interp, rv);

    if interp.class_by_qname.contains_key(&(nsoid, name.clone())) {
        if stmt.if_not_exists {
            return Ok(());
        }
        return Err(DdlError::DuplicateObject(format!(
            "relation \"{name}\" already exists"
        )));
    }

    let mut columns: Vec<ParsedColumn> = Vec::new();
    let mut pk_columns: Vec<String> = Vec::new();

    // First pass: extract table-level PRIMARY KEY constraint keys.
    for elt in &stmt.constraints {
        if let Some(node::Node::Constraint(c)) = elt.node.as_ref()
            && c.contype == ConstrType::ConstrPrimary as i32
        {
            for key_node in &c.keys {
                if let Some(node::Node::String(s)) = key_node.node.as_ref() {
                    pk_columns.push(s.sval.clone());
                }
            }
        }
    }

    // Also check table_elts for table-level constraints.
    for elt in &stmt.table_elts {
        if let Some(node::Node::Constraint(c)) = elt.node.as_ref()
            && c.contype == ConstrType::ConstrPrimary as i32
        {
            for key_node in &c.keys {
                if let Some(node::Node::String(s)) = key_node.node.as_ref() {
                    pk_columns.push(s.sval.clone());
                }
            }
        }
    }

    // Second pass: process columns.
    let mut seen_names = std::collections::HashSet::new();
    for elt in &stmt.table_elts {
        let Some(node::Node::ColumnDef(cd)) = elt.node.as_ref() else {
            continue;
        };

        if !seen_names.insert(cd.colname.clone()) {
            return Err(DdlError::DuplicateObject(format!(
                "column \"{}\" specified more than once",
                cd.colname
            )));
        }

        let col = parse_column_def(interp, cd, &pk_columns)?;
        columns.push(col);
    }

    // Allocate OIDs for the relation row, its composite type, and the array
    // type wrapping the composite.
    let class_oid = PgClassOid::new(interp.alloc_oid()).expect("alloc_oid is non-zero");
    let composite_oid = PgTypeOid::new(interp.alloc_oid()).expect("alloc_oid is non-zero");
    let array_oid = PgTypeOid::new(interp.alloc_oid()).expect("alloc_oid is non-zero");

    interp.insert_pg_class(PgClass {
        oid: class_oid,
        relname: name.clone(),
        relnamespace: nsoid,
        relkind: RelKind::Table,
        reltype: Some(composite_oid),
        relviewdef: Vec::new(),
        viewbindings: Vec::new(),
    });
    for (i, col) in columns.iter().enumerate() {
        interp.insert_pg_attribute(PgAttribute {
            attrelid: class_oid,
            attname: col.name.clone(),
            atttypid: col.type_oid,
            attnum: (i + 1) as i16,
            attnotnull: col.not_null,
            atthasdef: col.has_default,
            attgenerated: col.is_generated.then_some(AttGenerated::Stored),
        });
    }
    interp.insert_pg_type(PgType {
        oid: composite_oid,
        typname: name.clone(),
        typnamespace: nsoid,
        typtype: TypType::Composite,
        typcategory: TypCategory::Composite,
        typispreferred: false,
        typrelid: Some(class_oid),
        typelem: None,
        typarray: Some(array_oid),
        typbasetype: None,
    });
    register_composite_to_record_cast(interp, composite_oid);

    // Array type for the composite (`_<name>` in the same schema).
    interp.insert_pg_type(PgType {
        oid: array_oid,
        typname: format!("_{name}"),
        typnamespace: nsoid,
        typtype: TypType::Base,
        typcategory: TypCategory::Array,
        typispreferred: false,
        typrelid: None,
        typelem: Some(composite_oid),
        typarray: None,
        typbasetype: None,
    });

    Ok(())
}

/// Parsed column definition shared between `CREATE TABLE` and `ALTER TABLE`.
#[derive(Clone)]
struct ParsedColumn {
    name: String,
    type_oid: PgTypeOid,
    not_null: bool,
    has_default: bool,
    is_generated: bool,
}

/// Parse a `ColumnDef` AST node into a `ParsedColumn` (shared between
/// CREATE TABLE and ALTER TABLE ADD COLUMN paths).
fn parse_column_def(
    interp: &PgCatalog,
    cd: &pg_query::protobuf::ColumnDef,
    pk_columns: &[String],
) -> Result<ParsedColumn, DdlError> {
    // Detect SERIAL/BIGSERIAL/SMALLSERIAL from type name — pg_query keeps the
    // original name and does NOT rewrite to int4 + nextval(...).
    let is_serial = cd.type_name.as_ref().is_some_and(|tn| {
        tn.names.iter().any(|n| {
            matches!(n.node.as_ref(), Some(node::Node::String(s))
                if matches!(s.sval.as_str(), "serial" | "bigserial" | "smallserial"))
        })
    });

    let type_oid = cd
        .type_name
        .as_ref()
        .and_then(|tn| resolve_type_name(tn, interp))
        .unwrap_or(crate::pg_catalog::oid::UNKNOWN);

    let mut not_null = cd.is_not_null;
    let mut has_default = cd.raw_default.is_some() || cd.cooked_default.is_some();
    let mut is_generated = false;

    if is_serial {
        has_default = true;
    }

    if !cd.identity.is_empty() {
        has_default = true;
        not_null = true;
    }

    if !cd.generated.is_empty() {
        has_default = true;
        is_generated = true;
    }

    for c_node in &cd.constraints {
        if let Some(node::Node::Constraint(c)) = c_node.node.as_ref() {
            match ConstrType::try_from(c.contype) {
                Ok(ConstrType::ConstrNotnull) => not_null = true,
                Ok(ConstrType::ConstrPrimary) => {
                    not_null = true;
                }
                Ok(ConstrType::ConstrDefault) => {
                    has_default = true;
                }
                Ok(ConstrType::ConstrIdentity) => {
                    has_default = true;
                    not_null = true;
                }
                Ok(ConstrType::ConstrGenerated) => {
                    has_default = true;
                    is_generated = true;
                }
                _ => {}
            }
        }
    }

    if pk_columns.iter().any(|pk| pk == &cd.colname) {
        not_null = true;
    }

    Ok(ParsedColumn {
        name: cd.colname.clone(),
        type_oid,
        not_null,
        has_default,
        is_generated,
    })
}

// ─── ALTER TABLE ────────────────────────────────────────────────────────────

pub fn alter_table(interp: &mut PgCatalog, stmt: &AlterTableStmt) -> Result<(), DdlError> {
    let rv = stmt
        .relation
        .as_ref()
        .ok_or_else(|| DdlError::Parse("ALTER TABLE without relation".into()))?;

    let (schema, name) = range_var_names(rv, interp);
    let Some(nsoid) = interp.namespace_oid(&schema) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(format!("{schema}.{name}")));
    };
    let class_oid = match interp.class_by_qname.get(&(nsoid, name.clone())).copied() {
        Some(oid) => oid,
        None => {
            if stmt.missing_ok {
                return Ok(());
            }
            return Err(DdlError::TableNotFound(format!("{schema}.{name}")));
        }
    };

    for cmd_node in &stmt.cmds {
        let Some(node::Node::AlterTableCmd(cmd)) = cmd_node.node.as_ref() else {
            continue;
        };
        apply_alter_cmd(interp, class_oid, cmd)?;
    }

    Ok(())
}

fn apply_alter_cmd(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let subtype = AlterTableType::try_from(cmd.subtype).unwrap_or(AlterTableType::Undefined);

    match subtype {
        AlterTableType::AtAddColumn | AlterTableType::AtAddColumnToView => {
            add_column(interp, relid, cmd)
        }
        AlterTableType::AtDropColumn => drop_column(interp, relid, cmd),
        AlterTableType::AtSetNotNull => set_not_null(interp, relid, &cmd.name, true),
        AlterTableType::AtDropNotNull => set_not_null(interp, relid, &cmd.name, false),
        AlterTableType::AtColumnDefault => set_default(interp, relid, cmd),
        AlterTableType::AtAlterColumnType => alter_column_type(interp, relid, cmd),
        AlterTableType::AtAddConstraint => add_constraint(interp, relid, cmd),
        // Other subtypes are no-ops for schema analysis.
        _ => Ok(()),
    }
}

fn add_column(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let Some(def) = cmd.def.as_deref() else {
        return Ok(());
    };
    let Some(node::Node::ColumnDef(cd)) = def.node.as_ref() else {
        return Ok(());
    };

    if interp
        .attributes_of(relid)
        .iter()
        .any(|a| a.attname == cd.colname)
    {
        if cmd.missing_ok {
            return Ok(());
        }
        return Err(DdlError::DuplicateObject(format!(
            "column \"{}\" of relation already exists",
            cd.colname
        )));
    }

    let col = parse_column_def(interp, cd, &[])?;
    let next_attnum = interp
        .attributes_of(relid)
        .iter()
        .map(|a| a.attnum)
        .max()
        .unwrap_or(0)
        + 1;
    interp.insert_pg_attribute(PgAttribute {
        attrelid: relid,
        attname: col.name.clone(),
        atttypid: col.type_oid,
        attnum: next_attnum,
        attnotnull: col.not_null,
        atthasdef: col.has_default,
        attgenerated: col.is_generated.then_some(AttGenerated::Stored),
    });
    Ok(())
}

fn drop_column(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    if !interp
        .attributes_of(relid)
        .iter()
        .any(|a| a.attname == cmd.name)
    {
        if cmd.missing_ok {
            return Ok(());
        }
        return Err(DdlError::Parse(format!(
            "column \"{}\" of relation does not exist",
            cmd.name
        )));
    }

    let cascade = matches!(
        DropBehavior::try_from(cmd.behavior),
        Ok(DropBehavior::DropCascade)
    );

    // Find dependent views from pg_depend.
    let dependent_views = views::find_views_depending_on_column(interp, relid, &cmd.name);
    if !dependent_views.is_empty() && !cascade {
        let view_names: Vec<String> = dependent_views
            .iter()
            .filter_map(|&v| {
                let c = interp.pg_class.get(&v)?;
                let nsname = interp.namespace_name(c.relnamespace)?;
                Some(format!("{nsname}.{}", c.relname))
            })
            .collect();
        let relname = interp
            .pg_class
            .get(&relid)
            .map(|c| c.relname.clone())
            .unwrap_or_default();
        return Err(DdlError::DependencyError(format!(
            "cannot drop column {relname}.{} because view(s) {} depend on it",
            cmd.name,
            view_names.join(", "),
        )));
    }

    if !dependent_views.is_empty() {
        views::drop_views(interp, &dependent_views);
    }

    if let Some(attrs) = interp.pg_attribute.get_mut(&relid) {
        attrs.retain(|a| a.attname != cmd.name);
    }
    Ok(())
}

fn set_not_null(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    col_name: &str,
    not_null: bool,
) -> Result<(), DdlError> {
    let Some(attrs) = interp.pg_attribute.get_mut(&relid) else {
        return Err(DdlError::TableNotFound(format!("relation oid {relid}")));
    };
    let Some(col) = attrs.iter_mut().find(|c| c.attname == col_name) else {
        return Err(DdlError::Parse(format!(
            "column \"{col_name}\" of relation does not exist"
        )));
    };
    col.attnotnull = not_null;
    Ok(())
}

fn set_default(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let Some(attrs) = interp.pg_attribute.get_mut(&relid) else {
        return Err(DdlError::TableNotFound(format!("relation oid {relid}")));
    };
    let Some(col) = attrs.iter_mut().find(|c| c.attname == cmd.name) else {
        return Err(DdlError::Parse(format!(
            "column \"{}\" of relation does not exist",
            cmd.name
        )));
    };
    col.atthasdef = cmd.def.is_some();
    Ok(())
}

fn alter_column_type(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let Some(def) = cmd.def.as_deref() else {
        return Ok(());
    };
    let Some(node::Node::ColumnDef(cd)) = def.node.as_ref() else {
        return Ok(());
    };

    let new_type_oid = cd
        .type_name
        .as_ref()
        .and_then(|tn| resolve_type_name(tn, interp))
        .unwrap_or(crate::pg_catalog::oid::UNKNOWN);

    let old_type_oid = interp
        .attributes_of(relid)
        .iter()
        .find(|a| a.attname == cmd.name)
        .map(|a| a.atttypid)
        .ok_or_else(|| {
            DdlError::Parse(format!(
                "column \"{}\" of relation does not exist",
                cmd.name
            ))
        })?;

    let dependent_views = views::find_views_depending_on_column(interp, relid, &cmd.name);
    if !dependent_views.is_empty() && !interp.is_binary_coercible(old_type_oid, new_type_oid) {
        let view_names: Vec<String> = dependent_views
            .iter()
            .filter_map(|&v| {
                let c = interp.pg_class.get(&v)?;
                let nsname = interp.namespace_name(c.relnamespace)?;
                Some(format!("{nsname}.{}", c.relname))
            })
            .collect();
        let relname = interp
            .pg_class
            .get(&relid)
            .map(|c| c.relname.clone())
            .unwrap_or_default();
        return Err(DdlError::DependencyError(format!(
            "cannot alter type of column {relname}.{} because view(s) {} depend on it \
             and the new type is not binary coercible with the old one \
             (hint: drop the view(s) first, alter the column, then recreate)",
            cmd.name,
            view_names.join(", "),
        )));
    }

    if let Some(attrs) = interp.pg_attribute.get_mut(&relid)
        && let Some(col) = attrs.iter_mut().find(|c| c.attname == cmd.name)
    {
        col.atttypid = new_type_oid;
    }

    let _ = old_type_oid;
    for view_oid in &dependent_views {
        views::reanalyze_view(interp, *view_oid)?;
    }

    Ok(())
}

fn add_constraint(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let Some(def) = cmd.def.as_deref() else {
        return Ok(());
    };
    let Some(node::Node::Constraint(c)) = def.node.as_ref() else {
        return Ok(());
    };

    if c.contype == ConstrType::ConstrPrimary as i32 {
        let pk_cols: Vec<String> = c
            .keys
            .iter()
            .filter_map(|k| {
                if let Some(node::Node::String(s)) = k.node.as_ref() {
                    Some(s.sval.clone())
                } else {
                    None
                }
            })
            .collect();
        if let Some(attrs) = interp.pg_attribute.get_mut(&relid) {
            for col in attrs.iter_mut() {
                if pk_cols.contains(&col.attname) {
                    col.attnotnull = true;
                }
            }
        }
    }

    if c.contype == ConstrType::ConstrNotnull as i32 {
        let col_name = c
            .keys
            .first()
            .and_then(|k| {
                if let Some(node::Node::String(s)) = k.node.as_ref() {
                    Some(s.sval.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| cmd.name.clone());
        if let Some(attrs) = interp.pg_attribute.get_mut(&relid)
            && let Some(col) = attrs.iter_mut().find(|col| col.attname == col_name)
        {
            col.attnotnull = true;
        }
    }

    Ok(())
}
