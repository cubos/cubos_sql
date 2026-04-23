//! CREATE TABLE and ALTER TABLE DDL handlers.

use pg_query::protobuf::{
    AlterTableCmd, AlterTableStmt, AlterTableType, ConstrType, CreateStmt, DropBehavior, node,
};

use crate::schema::{CompositeField, RelationKind, TableColumn, TableEntry, TypeEntry, TypeKind};

use super::DdlError;
use super::util::{range_var_key, range_var_names, resolve_type_name};
use super::views;
use crate::database::Database;
use crate::qualified_name::QualifiedName;

// ─── CREATE TABLE ───────────────────────────────────────────────────────────

pub fn create_table(interp: &mut Database, stmt: &CreateStmt) -> Result<(), DdlError> {
    let rv = stmt
        .relation
        .as_ref()
        .ok_or_else(|| DdlError::Parse("CREATE TABLE without relation".into()))?;

    let key = range_var_key(rv, &interp.snapshot);
    let (schema, name) = range_var_names(rv, &interp.snapshot);

    // Check for existing table.
    if interp.snapshot.tables.contains_key(&key) {
        if stmt.if_not_exists {
            return Ok(());
        }
        return Err(DdlError::DuplicateObject(format!(
            "relation \"{name}\" already exists"
        )));
    }

    // Collect columns and constraints.
    let mut columns = Vec::new();
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
    let mut attnum: i16 = 0;
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

        attnum += 1;
        let col = parse_column_def(interp, cd, attnum, &pk_columns)?;
        columns.push(col);
    }

    // Allocate OIDs for the composite and array types (tables don't need
    // their own OID — the map key is the qualified name).
    let composite_oid = interp.alloc_oid();
    let array_oid = interp.alloc_oid();

    // Register composite type (PG creates one for each table).
    let composite_fields: Vec<CompositeField> = columns
        .iter()
        .map(|c| CompositeField {
            name: c.name.clone(),
            type_oid: c.type_oid,
            not_null: c.not_null,
        })
        .collect();

    let composite_key = QualifiedName::new(&schema, &name);
    interp.snapshot.types.insert(
        composite_oid,
        TypeEntry {
            oid: composite_oid,
            name: name.clone(),
            schema: schema.clone(),
            kind: TypeKind::Composite {
                fields: composite_fields,
            },
            category: 'C',
            is_preferred: false,
            extension: None,
        },
    );
    interp
        .snapshot
        .type_by_name
        .insert(composite_key, composite_oid);

    // Register array type for the composite.
    let array_name = format!("_{name}");
    let array_key = QualifiedName::new(&schema, &array_name);
    interp.snapshot.types.insert(
        array_oid,
        TypeEntry {
            oid: array_oid,
            name: array_name,
            schema: schema.clone(),
            kind: TypeKind::Array {
                element_type_oid: composite_oid,
            },
            category: 'A',
            is_preferred: false,
            extension: None,
        },
    );
    interp.snapshot.type_by_name.insert(array_key, array_oid);

    // Register the table.
    interp.snapshot.tables.insert(
        key,
        TableEntry {
            name: name.clone(),
            schema: schema.clone(),
            kind: RelationKind::Table,
            columns,
            view_def: None,
        },
    );

    Ok(())
}

/// Parse a `ColumnDef` AST node into a `TableColumn`.
fn parse_column_def(
    interp: &Database,
    cd: &pg_query::protobuf::ColumnDef,
    attnum: i16,
    pk_columns: &[String],
) -> Result<TableColumn, DdlError> {
    // Detect SERIAL/BIGSERIAL/SMALLSERIAL from type name — pg_query keeps the
    // original name and does NOT rewrite to int4 + nextval(...) like older versions.
    let is_serial = cd.type_name.as_ref().is_some_and(|tn| {
        tn.names.iter().any(|n| {
            matches!(n.node.as_ref(), Some(node::Node::String(s))
                if matches!(s.sval.as_str(), "serial" | "bigserial" | "smallserial"))
        })
    });

    let type_oid = cd
        .type_name
        .as_ref()
        .and_then(|tn| resolve_type_name(tn, &interp.snapshot))
        .unwrap_or(0); // 0 if unresolved — will produce a warning downstream.

    let mut not_null = cd.is_not_null;
    let mut has_default = cd.raw_default.is_some() || cd.cooked_default.is_some();

    // SERIAL/BIGSERIAL/SMALLSERIAL imply has_default (auto-sequence).
    if is_serial {
        has_default = true;
    }

    // Check IDENTITY.
    if !cd.identity.is_empty() {
        has_default = true;
        not_null = true;
    }

    // Check GENERATED.
    if !cd.generated.is_empty() {
        has_default = true;
    }

    // Check column-level constraints.
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
                }
                _ => {}
            }
        }
    }

    // Table-level PRIMARY KEY includes this column → NOT NULL.
    if pk_columns.iter().any(|pk| pk == &cd.colname) {
        not_null = true;
    }

    Ok(TableColumn {
        name: cd.colname.clone(),
        type_oid,
        not_null,
        has_default,
        attnum,
    })
}

// ─── ALTER TABLE ────────────────────────────────────────────────────────────

pub fn alter_table(interp: &mut Database, stmt: &AlterTableStmt) -> Result<(), DdlError> {
    let rv = stmt
        .relation
        .as_ref()
        .ok_or_else(|| DdlError::Parse("ALTER TABLE without relation".into()))?;

    let key = range_var_key(rv, &interp.snapshot);

    // Verify the table exists. Handle missing_ok (IF EXISTS).
    if !interp.snapshot.tables.contains_key(&key) {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(key.to_string()));
    }

    for cmd_node in &stmt.cmds {
        let Some(node::Node::AlterTableCmd(cmd)) = cmd_node.node.as_ref() else {
            continue;
        };
        apply_alter_cmd(interp, &key, cmd)?;
    }

    Ok(())
}

fn apply_alter_cmd(
    interp: &mut Database,
    table_key: &QualifiedName,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let subtype = AlterTableType::try_from(cmd.subtype).unwrap_or(AlterTableType::Undefined);

    match subtype {
        AlterTableType::AtAddColumn | AlterTableType::AtAddColumnToView => {
            add_column(interp, table_key, cmd)
        }
        AlterTableType::AtDropColumn => drop_column(interp, table_key, cmd),
        AlterTableType::AtSetNotNull => set_not_null(interp, table_key, &cmd.name, true),
        AlterTableType::AtDropNotNull => set_not_null(interp, table_key, &cmd.name, false),
        AlterTableType::AtColumnDefault => set_default(interp, table_key, cmd),
        AlterTableType::AtAlterColumnType => alter_column_type(interp, table_key, cmd),
        AlterTableType::AtAddConstraint => add_constraint(interp, table_key, cmd),
        // Other subtypes are no-ops for schema analysis.
        _ => Ok(()),
    }
}

fn add_column(
    interp: &mut Database,
    table_key: &QualifiedName,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let Some(def) = cmd.def.as_deref() else {
        return Ok(());
    };
    let Some(node::Node::ColumnDef(cd)) = def.node.as_ref() else {
        return Ok(());
    };

    let table = interp
        .snapshot
        .tables
        .get(table_key)
        .ok_or_else(|| DdlError::TableNotFound(table_key.to_string()))?;
    let next_attnum = table.columns.iter().map(|c| c.attnum).max().unwrap_or(0) + 1;

    // Check for duplicate column.
    if table.columns.iter().any(|c| c.name == cd.colname) {
        if cmd.missing_ok {
            return Ok(());
        }
        return Err(DdlError::DuplicateObject(format!(
            "column \"{}\" of relation already exists",
            cd.colname
        )));
    }

    let col = parse_column_def(interp, cd, next_attnum, &[])?;

    // Mutate table and composite type.
    let table = interp.snapshot.tables.get_mut(table_key).unwrap();
    table.columns.push(col.clone());

    // Update composite type.
    update_composite_for_table(interp, table_key);

    Ok(())
}

fn drop_column(
    interp: &mut Database,
    table_key: &QualifiedName,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let table = interp
        .snapshot
        .tables
        .get(table_key)
        .ok_or_else(|| DdlError::TableNotFound(table_key.to_string()))?;

    // Check column exists.
    if !table.columns.iter().any(|c| c.name == cmd.name) {
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

    // Check for dependent views.
    let dependent_views =
        views::find_views_depending_on_column(&interp.snapshot, table_key, &cmd.name);
    if !dependent_views.is_empty() && !cascade {
        let view_names: Vec<String> = dependent_views.iter().map(|k| k.to_string()).collect();
        return Err(DdlError::DependencyError(format!(
            "cannot drop column {}.{} because view(s) {} depend on it",
            table_key,
            cmd.name,
            view_names.join(", "),
        )));
    }

    // CASCADE: drop dependent views.
    if !dependent_views.is_empty() {
        views::drop_views(&mut interp.snapshot, &dependent_views);
    }

    let table = interp.snapshot.tables.get_mut(table_key).unwrap();
    table.columns.retain(|c| c.name != cmd.name);
    update_composite_for_table(interp, table_key);
    Ok(())
}

fn set_not_null(
    interp: &mut Database,
    table_key: &QualifiedName,
    col_name: &str,
    not_null: bool,
) -> Result<(), DdlError> {
    let table = interp
        .snapshot
        .tables
        .get_mut(table_key)
        .ok_or_else(|| DdlError::TableNotFound(table_key.to_string()))?;

    let col = table
        .columns
        .iter_mut()
        .find(|c| c.name == col_name)
        .ok_or_else(|| {
            DdlError::Parse(format!("column \"{col_name}\" of relation does not exist"))
        })?;
    col.not_null = not_null;
    update_composite_for_table(interp, table_key);
    Ok(())
}

fn set_default(
    interp: &mut Database,
    table_key: &QualifiedName,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let table = interp
        .snapshot
        .tables
        .get_mut(table_key)
        .ok_or_else(|| DdlError::TableNotFound(table_key.to_string()))?;

    let col = table
        .columns
        .iter_mut()
        .find(|c| c.name == cmd.name)
        .ok_or_else(|| {
            DdlError::Parse(format!(
                "column \"{}\" of relation does not exist",
                cmd.name
            ))
        })?;
    col.has_default = cmd.def.is_some();
    Ok(())
}

fn alter_column_type(
    interp: &mut Database,
    table_key: &QualifiedName,
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
        .and_then(|tn| resolve_type_name(tn, &interp.snapshot))
        .unwrap_or(0);

    if !interp.snapshot.tables.contains_key(table_key) {
        return Err(DdlError::TableNotFound(table_key.to_string()));
    }

    // In PostgreSQL, ALTER COLUMN TYPE always fails if views depend on the column.
    // The user must DROP VIEW first, ALTER TYPE, then recreate the view.
    let dependent_views =
        views::find_views_depending_on_column(&interp.snapshot, table_key, &cmd.name);
    if !dependent_views.is_empty() {
        let view_names: Vec<String> = dependent_views.iter().map(|k| k.to_string()).collect();
        return Err(DdlError::DependencyError(format!(
            "cannot alter type of column {}.{} because view(s) {} depend on it \
             (hint: drop the view(s) first, alter the column, then recreate)",
            table_key,
            cmd.name,
            view_names.join(", "),
        )));
    }

    let table = interp.snapshot.tables.get_mut(table_key).unwrap();
    let col = table
        .columns
        .iter_mut()
        .find(|c| c.name == cmd.name)
        .ok_or_else(|| {
            DdlError::Parse(format!(
                "column \"{}\" of relation does not exist",
                cmd.name
            ))
        })?;
    col.type_oid = new_type_oid;
    update_composite_for_table(interp, table_key);
    Ok(())
}

fn add_constraint(
    interp: &mut Database,
    table_key: &QualifiedName,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let Some(def) = cmd.def.as_deref() else {
        return Ok(());
    };
    let Some(node::Node::Constraint(c)) = def.node.as_ref() else {
        return Ok(());
    };

    // PRIMARY KEY: mark referenced columns as NOT NULL.
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

        let table = interp
            .snapshot
            .tables
            .get_mut(table_key)
            .ok_or_else(|| DdlError::TableNotFound(table_key.to_string()))?;

        for col in &mut table.columns {
            if pk_cols.contains(&col.name) {
                col.not_null = true;
            }
        }
        update_composite_for_table(interp, table_key);
    }

    // NOT NULL constraint.
    if c.contype == ConstrType::ConstrNotnull as i32 {
        let table = interp
            .snapshot
            .tables
            .get_mut(table_key)
            .ok_or_else(|| DdlError::TableNotFound(table_key.to_string()))?;

        // The column name may be in keys[0] or cmd.name.
        let col_name = c
            .keys
            .first()
            .and_then(|k| {
                if let Some(node::Node::String(s)) = k.node.as_ref() {
                    Some(s.sval.as_str())
                } else {
                    None
                }
            })
            .unwrap_or(&cmd.name);

        if let Some(col) = table.columns.iter_mut().find(|col| col.name == col_name) {
            col.not_null = true;
        }
        update_composite_for_table(interp, table_key);
    }

    Ok(())
}

/// Sync the composite type fields with the table's columns.
fn update_composite_for_table(interp: &mut Database, table_key: &QualifiedName) {
    let Some(table) = interp.snapshot.tables.get(table_key) else {
        return;
    };
    let Some(&composite_oid) = interp.snapshot.type_by_name.get(table_key) else {
        return;
    };
    let fields: Vec<CompositeField> = table
        .columns
        .iter()
        .map(|c| CompositeField {
            name: c.name.clone(),
            type_oid: c.type_oid,
            not_null: c.not_null,
        })
        .collect();

    if let Some(te) = interp.snapshot.types.get_mut(&composite_oid) {
        te.kind = TypeKind::Composite { fields };
    }
}
