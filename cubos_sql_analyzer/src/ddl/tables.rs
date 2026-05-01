//! CREATE TABLE and ALTER TABLE DDL handlers.

use pg_query::protobuf::{
    AlterTableCmd, AlterTableStmt, AlterTableType, ConstrType, CreateStmt, DropBehavior, node,
};

use crate::oid::{PgClassOid, PgConstraintOid, PgTypeOid};
use crate::pg_catalog::{
    AttGenerated, AttIdentity, ConType, PgAttribute, PgClass, PgConstraint, PgIndex, PgInherits,
    PgType, RelKind, TypCategory, TypType,
};

use super::DdlError;
use super::util::{
    ensure_range_var, range_var_names, register_composite_to_record_cast, resolve_type_name,
};
use super::views;
use crate::pg_catalog::PgCatalog;

/// Pending `pg_constraint` row built up while walking a `CreateStmt`:
/// `(conname, contype, conkey, confrelid, confkey)`. Materialized into
/// real catalog rows after all FK targets have been validated.
type PendingConstraint = (String, ConType, Vec<i16>, Option<PgClassOid>, Vec<i16>);

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

    // First pass: extract table-level PRIMARY KEY constraint keys, and
    // validate any table-level CHECK constraint expressions for volatility.
    for elt in stmt.constraints.iter().chain(stmt.table_elts.iter()) {
        if let Some(node::Node::Constraint(c)) = elt.node.as_ref() {
            if c.contype == ConstrType::ConstrPrimary as i32 {
                for key_node in &c.keys {
                    if let Some(node::Node::String(s)) = key_node.node.as_ref() {
                        pk_columns.push(s.sval.clone());
                    }
                }
            }
            if c.contype == ConstrType::ConstrCheck as i32
                && let Some(expr) = c.raw_expr.as_deref()
            {
                super::volatile::check_no_volatile(
                    expr,
                    super::volatile::ExprLocation::Check,
                    interp,
                )?;
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
        relviewdef: None,
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
            atttypmod: col.typmod,
            attidentity: col.identity,
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
        typnotnull: false,
        typtypmod: None,
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
        typnotnull: false,
        typtypmod: None,
    });

    // `CREATE TABLE child () INHERITS (p1, p2, …)` (NOT `PARTITION OF`).
    // PG also reuses `inh_relations` for partitioned children, but those
    // additionally set `partbound`; we skip them here — the analyzer
    // doesn't model row-level partition routing.
    if !stmt.inh_relations.is_empty() && stmt.partbound.is_none() {
        apply_inherits(interp, class_oid, &stmt.inh_relations)?;
    }

    // Type-check CHECK and `GENERATED ... STORED` expressions against the
    // freshly-built table. CHECK must produce `bool`; the generated
    // expression must be assignable to the column's declared type.
    validate_constraint_expressions(interp, class_oid, &name, stmt)?;

    // Emit pg_constraint rows so ON CONFLICT, DROP CASCADE, and FK
    // dependency checks can consult them later. FK validation runs here.
    emit_constraints(interp, class_oid, &name, stmt)?;

    Ok(())
}

/// Emit `pg_constraint` rows for every PRIMARY KEY / UNIQUE / CHECK /
/// FOREIGN KEY constraint declared on a freshly-built table. FK targets
/// are validated (existence, column existence, type compatibility, and
/// uniqueness coverage on the referenced columns) and recorded with
/// `confrelid`/`confkey` so the dependency graph is traversable.
fn emit_constraints(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    relname: &str,
    stmt: &CreateStmt,
) -> Result<(), DdlError> {
    let attinfo_by_name: std::collections::HashMap<String, (i16, PgTypeOid)> = interp
        .attributes_of(relid)
        .iter()
        .map(|a| (a.attname.clone(), (a.attnum, a.atttypid)))
        .collect();
    let attnum_of = |name: &str| attinfo_by_name.get(name).map(|(an, _)| *an);
    let atttype_of = |name: &str| attinfo_by_name.get(name).map(|(_, t)| *t);

    let mut to_emit: Vec<PendingConstraint> = Vec::new();

    // Column-level constraints.
    for elt in &stmt.table_elts {
        let Some(node::Node::ColumnDef(cd)) = elt.node.as_ref() else {
            continue;
        };
        let Some(an) = attnum_of(&cd.colname) else {
            continue;
        };
        let Some(my_type) = atttype_of(&cd.colname) else {
            continue;
        };
        for c_node in &cd.constraints {
            let Some(node::Node::Constraint(c)) = c_node.node.as_ref() else {
                continue;
            };
            match ConstrType::try_from(c.contype) {
                Ok(ConstrType::ConstrPrimary) => {
                    to_emit.push((
                        constraint_name(&c.conname, || format!("{relname}_pkey")),
                        ConType::PrimaryKey,
                        vec![an],
                        None,
                        Vec::new(),
                    ));
                }
                Ok(ConstrType::ConstrUnique) => {
                    to_emit.push((
                        constraint_name(&c.conname, || format!("{relname}_{}_key", cd.colname)),
                        ConType::Unique,
                        vec![an],
                        None,
                        Vec::new(),
                    ));
                }
                Ok(ConstrType::ConstrCheck) => {
                    to_emit.push((
                        constraint_name(&c.conname, || format!("{relname}_{}_check", cd.colname)),
                        ConType::Check,
                        vec![an],
                        None,
                        Vec::new(),
                    ));
                }
                Ok(ConstrType::ConstrForeign) => {
                    let (target_oid, target_attnums) =
                        resolve_fk_target(interp, c, relname, &[my_type])?;
                    to_emit.push((
                        constraint_name(&c.conname, || format!("{relname}_{}_fkey", cd.colname)),
                        ConType::ForeignKey,
                        vec![an],
                        Some(target_oid),
                        target_attnums,
                    ));
                }
                _ => {}
            }
        }
    }

    // Table-level constraints.
    for elt in stmt.constraints.iter().chain(stmt.table_elts.iter()) {
        let Some(node::Node::Constraint(c)) = elt.node.as_ref() else {
            continue;
        };
        let column_names: Vec<String> = c
            .keys
            .iter()
            .filter_map(|k| match k.node.as_ref()? {
                node::Node::String(s) => Some(s.sval.clone()),
                _ => None,
            })
            .collect();
        let columns: Vec<i16> = column_names.iter().filter_map(|n| attnum_of(n)).collect();
        match ConstrType::try_from(c.contype) {
            Ok(ConstrType::ConstrPrimary) if !columns.is_empty() => {
                to_emit.push((
                    constraint_name(&c.conname, || format!("{relname}_pkey")),
                    ConType::PrimaryKey,
                    columns,
                    None,
                    Vec::new(),
                ));
            }
            Ok(ConstrType::ConstrUnique) if !columns.is_empty() => {
                to_emit.push((
                    constraint_name(&c.conname, || {
                        format!("{relname}_{}_key", column_names.join("_"))
                    }),
                    ConType::Unique,
                    columns,
                    None,
                    Vec::new(),
                ));
            }
            Ok(ConstrType::ConstrCheck) => {
                to_emit.push((
                    constraint_name(&c.conname, || format!("{relname}_check")),
                    ConType::Check,
                    columns,
                    None,
                    Vec::new(),
                ));
            }
            Ok(ConstrType::ConstrForeign) => {
                // Table-level FK uses `fk_attrs` for the local columns;
                // `keys` only carries column lists on PK / UNIQUE.
                let fk_names: Vec<String> = c
                    .fk_attrs
                    .iter()
                    .filter_map(|k| match k.node.as_ref()? {
                        node::Node::String(s) => Some(s.sval.clone()),
                        _ => None,
                    })
                    .collect();
                let local_types: Vec<PgTypeOid> =
                    fk_names.iter().filter_map(|n| atttype_of(n)).collect();
                if local_types.len() != fk_names.len() {
                    return Err(DdlError::Parse(format!(
                        "foreign key on {relname} references unknown local column"
                    )));
                }
                let fk_columns: Vec<i16> = fk_names.iter().filter_map(|n| attnum_of(n)).collect();
                let (target_oid, target_attnums) =
                    resolve_fk_target(interp, c, relname, &local_types)?;
                to_emit.push((
                    constraint_name(&c.conname, || {
                        format!("{relname}_{}_fkey", fk_names.join("_"))
                    }),
                    ConType::ForeignKey,
                    fk_columns,
                    Some(target_oid),
                    target_attnums,
                ));
            }
            _ => {}
        }
    }

    for (conname, contype, conkey, confrelid, confkey) in to_emit {
        emit_constraint_with_backing_index(
            interp, relid, conname, contype, conkey, confrelid, confkey,
        );
    }
    Ok(())
}

/// Insert a `pg_constraint` row and, for PK/UNIQUE, the backing
/// `pg_class` (relkind = 'i') + `pg_index` rows that PG auto-creates.
///
/// PG conflates the constraint and its backing index — `<table>_pkey` is
/// both a constraint and an index, sharing one name. Mirror that so DROP
/// COLUMN / DROP TABLE cascade through `pg_index` and `ON CONFLICT ON
/// CONSTRAINT name` finds the index by its conname.
fn emit_constraint_with_backing_index(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    conname: String,
    contype: ConType,
    conkey: Vec<i16>,
    confrelid: Option<PgClassOid>,
    confkey: Vec<i16>,
) {
    let oid = PgConstraintOid::new(interp.alloc_oid()).expect("alloc_oid is non-zero");
    interp.insert_pg_constraint(PgConstraint {
        oid,
        conname: conname.clone(),
        conrelid: relid,
        contype,
        conkey: conkey.clone(),
        confrelid,
        confkey,
    });
    if matches!(contype, ConType::PrimaryKey | ConType::Unique) {
        let table_ns = interp
            .pg_class
            .get(&relid)
            .map(|c| c.relnamespace)
            .expect("relid is registered");
        let indexrelid = PgClassOid::new(interp.alloc_oid()).expect("alloc_oid is non-zero");
        interp.insert_pg_class(PgClass {
            oid: indexrelid,
            relname: conname,
            relnamespace: table_ns,
            relkind: RelKind::Index,
            reltype: None,
            relviewdef: None,
        });
        let indnatts = conkey.len() as i16;
        interp.insert_pg_index(PgIndex {
            indexrelid,
            indrelid: relid,
            indnatts,
            indnkeyatts: indnatts,
            indisunique: true,
            indisprimary: matches!(contype, ConType::PrimaryKey),
            indkey: conkey,
            indexprs: Vec::new(),
            indpred: None,
        });
    }
}

/// Resolve a `FOREIGN KEY` target: returns `(target_class_oid, target_attnums)`.
///
/// Validates the same things PG does at CREATE/ALTER time:
/// - Target relation exists.
/// - Target columns exist.
/// - When the column list is omitted, defaults to the target's PRIMARY KEY.
/// - Target columns are covered exactly by a `PRIMARY KEY` or `UNIQUE`
///   constraint on the target relation.
/// - Local and target column types match (after domain unwrapping).
fn resolve_fk_target(
    interp: &PgCatalog,
    c: &pg_query::protobuf::Constraint,
    relname: &str,
    local_types: &[PgTypeOid],
) -> Result<(PgClassOid, Vec<i16>), DdlError> {
    let pkrv = c
        .pktable
        .as_ref()
        .ok_or_else(|| DdlError::Parse(format!("FOREIGN KEY on {relname} without REFERENCES")))?;
    let (target_schema, target_name) = range_var_names(pkrv, interp);
    let target_nsoid = interp.namespace_oid(&target_schema).ok_or_else(|| {
        DdlError::TableNotFound(format!(
            "referenced table \"{target_schema}.{target_name}\" does not exist"
        ))
    })?;
    let target_oid = interp
        .class_by_qname
        .get(&(target_nsoid, target_name.clone()))
        .copied()
        .ok_or_else(|| {
            DdlError::TableNotFound(format!("referenced table \"{target_name}\" does not exist"))
        })?;

    // No explicit column list → default to the target's PRIMARY KEY.
    let target_attnums: Vec<i16> = if c.pk_attrs.is_empty() {
        let pk = interp
            .pg_constraint
            .values()
            .find(|x| x.conrelid == target_oid && matches!(x.contype, ConType::PrimaryKey))
            .ok_or_else(|| {
                DdlError::DependencyError(format!(
                    "there is no primary key for referenced table \"{target_name}\""
                ))
            })?;
        pk.conkey.clone()
    } else {
        let target_attrs = interp.attributes_of(target_oid);
        let mut nums = Vec::new();
        for k in &c.pk_attrs {
            if let Some(node::Node::String(s)) = k.node.as_ref() {
                let Some(an) = target_attrs
                    .iter()
                    .find(|a| a.attname == s.sval)
                    .map(|a| a.attnum)
                else {
                    return Err(DdlError::Parse(format!(
                        "column \"{}\" referenced in foreign key constraint does not exist \
                         on \"{target_name}\"",
                        s.sval
                    )));
                };
                nums.push(an);
            }
        }
        nums
    };

    // PG: referenced columns must be covered by a UNIQUE/PK constraint
    // (set-equality).
    let target_set: std::collections::BTreeSet<i16> = target_attnums.iter().copied().collect();
    let covered = interp.pg_constraint.values().any(|x| {
        x.conrelid == target_oid
            && matches!(x.contype, ConType::PrimaryKey | ConType::Unique)
            && x.conkey
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                == target_set
    });
    if !covered {
        return Err(DdlError::DependencyError(format!(
            "there is no unique constraint matching given keys for referenced table \
             \"{target_name}\""
        )));
    }

    // Type compatibility — local vs target after domain unwrapping.
    let target_attrs = interp.attributes_of(target_oid);
    let target_types: Vec<PgTypeOid> = target_attnums
        .iter()
        .filter_map(|&an| {
            target_attrs
                .iter()
                .find(|a| a.attnum == an)
                .map(|a| a.atttypid)
        })
        .collect();
    if target_types.len() != target_attnums.len() {
        return Err(DdlError::Parse(format!(
            "foreign key on {relname} references unknown column on \"{target_name}\""
        )));
    }
    if target_types.len() != local_types.len() {
        return Err(DdlError::Parse(format!(
            "foreign key on {relname} has {} local column(s) but references {} on \"{target_name}\"",
            local_types.len(),
            target_types.len()
        )));
    }
    for (lt, tt) in local_types.iter().zip(target_types.iter()) {
        if interp.unwrap_domain(*lt) != interp.unwrap_domain(*tt) {
            let lt_name = interp
                .get_type(*lt)
                .map(|t| t.typname.clone())
                .unwrap_or_default();
            let tt_name = interp
                .get_type(*tt)
                .map(|t| t.typname.clone())
                .unwrap_or_default();
            return Err(DdlError::DependencyError(format!(
                "foreign key constraint on {relname} has incompatible types: \
                 local {lt_name} vs referenced {tt_name} on \"{target_name}\""
            )));
        }
    }

    Ok((target_oid, target_attnums))
}

/// Pick a constraint name: explicit one when supplied, otherwise the
/// PG-style auto-generated form provided by the caller's closure.
fn constraint_name(explicit: &str, fallback: impl FnOnce() -> String) -> String {
    if explicit.is_empty() {
        fallback()
    } else {
        explicit.to_owned()
    }
}

/// Walk every CHECK / `GENERATED STORED` expression in a `CREATE TABLE` and
/// verify that the inferred type is compatible with the role of that
/// expression (boolean for CHECK, the column's type for generated).
///
/// Volatility is checked separately by [`super::volatile`] before the
/// table is even built; this pass only runs once the table exists in the
/// catalog so the expression scope can resolve the column references.
fn validate_constraint_expressions(
    interp: &PgCatalog,
    class_oid: PgClassOid,
    relname: &str,
    stmt: &CreateStmt,
) -> Result<(), DdlError> {
    use crate::expr::{TypeGoal, infer_expr};
    use crate::nullability::NullabilityContext;
    use crate::param_collector::ParamCollector;
    use crate::pg_catalog::oid;
    use crate::qualified_name::QualifiedName;
    use crate::scope::Scope;

    let table_attrs = interp.attributes_of(class_oid).to_vec();
    let nspname = interp
        .namespace_name(
            interp
                .pg_class
                .get(&class_oid)
                .map(|c| c.relnamespace)
                .expect("class just inserted"),
        )
        .map(str::to_owned)
        .unwrap_or_else(|| "public".to_owned());

    let mut scope = Scope::default();
    scope.add_dml_target(
        interp,
        relname,
        QualifiedName::new(nspname, relname.to_owned()),
        &table_attrs,
    );
    let null_ctx = NullabilityContext::default();
    let mut params = ParamCollector::default();

    // Column-level constraints — CHECK and GENERATED expressions live on
    // the ColumnDef's `constraints` list.
    for elt in &stmt.table_elts {
        let Some(node::Node::ColumnDef(cd)) = elt.node.as_ref() else {
            continue;
        };
        for c_node in &cd.constraints {
            let Some(node::Node::Constraint(c)) = c_node.node.as_ref() else {
                continue;
            };
            match ConstrType::try_from(c.contype) {
                Ok(ConstrType::ConstrCheck) => {
                    if let Some(expr) = c.raw_expr.as_deref() {
                        // Infer with no type goal so a non-bool result
                        // doesn't surface as a TypeMismatch — we want PG's
                        // exact wording (`argument of CHECK must be type
                        // boolean, not type X`).
                        let result = infer_expr(
                            expr,
                            &scope,
                            &null_ctx,
                            interp,
                            &mut params,
                            TypeGoal::NONE,
                        )
                        .map_err(|e| {
                            DdlError::UnsupportedDdl(format!(
                                "CHECK constraint on \"{relname}\".\"{}\": {e}",
                                cd.colname
                            ))
                        })?;
                        if result.type_oid != oid::BOOL && result.type_oid != oid::UNKNOWN {
                            let typname = interp
                                .get_type(result.type_oid)
                                .map(|t| t.typname.clone())
                                .unwrap_or_else(|| format!("oid {}", result.type_oid));
                            return Err(DdlError::UnsupportedDdl(format!(
                                "argument of CHECK constraint on \"{relname}\".\"{}\" \
                                 must be type boolean, not type {typname}",
                                cd.colname
                            )));
                        }
                    }
                }
                Ok(ConstrType::ConstrGenerated) => {
                    if let Some(expr) = c.raw_expr.as_deref() {
                        let col_type = table_attrs
                            .iter()
                            .find(|a| a.attname == cd.colname)
                            .map(|a| a.atttypid)
                            .ok_or_else(|| {
                                DdlError::Parse(format!(
                                    "generated column \"{}\" of \"{relname}\" not found \
                                     in pg_attribute",
                                    cd.colname
                                ))
                            })?;
                        infer_expr(
                            expr,
                            &scope,
                            &null_ctx,
                            interp,
                            &mut params,
                            TypeGoal::assignment(col_type),
                        )
                        .map_err(|e| {
                            DdlError::UnsupportedDdl(format!(
                                "GENERATED expression on \"{relname}\".\"{}\": {e}",
                                cd.colname
                            ))
                        })?;
                    }
                }
                _ => {}
            }
        }
    }

    // Table-level CHECK constraints — both `stmt.constraints` and
    // ColumnDef-shaped `Constraint` nodes inside `stmt.table_elts`.
    for elt in stmt.constraints.iter().chain(stmt.table_elts.iter()) {
        if let Some(node::Node::Constraint(c)) = elt.node.as_ref()
            && c.contype == ConstrType::ConstrCheck as i32
            && let Some(expr) = c.raw_expr.as_deref()
        {
            let result = infer_expr(expr, &scope, &null_ctx, interp, &mut params, TypeGoal::NONE)
                .map_err(|e| {
                DdlError::UnsupportedDdl(format!(
                    "table-level CHECK constraint on \"{relname}\": {e}"
                ))
            })?;
            if result.type_oid != oid::BOOL && result.type_oid != oid::UNKNOWN {
                let typname = interp
                    .get_type(result.type_oid)
                    .map(|t| t.typname.clone())
                    .unwrap_or_else(|| format!("oid {}", result.type_oid));
                return Err(DdlError::UnsupportedDdl(format!(
                    "argument of CHECK constraint on \"{relname}\" \
                     must be type boolean, not type {typname}"
                )));
            }
        }
    }

    Ok(())
}

/// Resolve every parent in an `INHERITS (…)` clause, copy each parent's
/// columns into the child's `pg_attribute` (skipping names already present
/// on the child), and emit one `pg_inherits` row per parent.
fn apply_inherits(
    interp: &mut PgCatalog,
    child_oid: PgClassOid,
    parents: &[pg_query::protobuf::Node],
) -> Result<(), DdlError> {
    let mut seq: i32 = 1;
    for parent_node in parents {
        let Some(node::Node::RangeVar(rv)) = parent_node.node.as_ref() else {
            continue;
        };
        let (parent_schema, parent_name) = range_var_names(rv, interp);
        let Some(parent_nsoid) = interp.namespace_oid(&parent_schema) else {
            return Err(DdlError::TableNotFound(format!(
                "{parent_schema}.{parent_name}"
            )));
        };
        let parent_oid = interp
            .class_by_qname
            .get(&(parent_nsoid, parent_name.clone()))
            .copied()
            .ok_or_else(|| {
                DdlError::TableNotFound(format!("relation \"{parent_name}\" does not exist"))
            })?;

        // Snapshot existing child column names; PG merges any same-named
        // parent column into the child's existing column rather than adding
        // a duplicate.
        let existing: std::collections::HashSet<String> = interp
            .attributes_of(child_oid)
            .iter()
            .map(|a| a.attname.clone())
            .collect();
        let next_attnum = interp
            .attributes_of(child_oid)
            .iter()
            .map(|a| a.attnum)
            .max()
            .unwrap_or(0);
        let parent_attrs: Vec<PgAttribute> = interp.attributes_of(parent_oid).to_vec();

        let mut attnum = next_attnum;
        for pa in parent_attrs {
            if existing.contains(&pa.attname) {
                continue;
            }
            attnum += 1;
            interp.insert_pg_attribute(PgAttribute {
                attrelid: child_oid,
                attname: pa.attname,
                atttypid: pa.atttypid,
                attnum,
                attnotnull: pa.attnotnull,
                atthasdef: pa.atthasdef,
                attgenerated: pa.attgenerated,
                atttypmod: pa.atttypmod,
                attidentity: pa.attidentity,
            });
        }

        interp.pg_inherits.push(PgInherits {
            inhrelid: child_oid,
            inhparent: parent_oid,
            inhseqno: seq,
        });
        seq += 1;
    }
    Ok(())
}

/// Parsed column definition shared between `CREATE TABLE` and `ALTER TABLE`.
#[derive(Clone)]
struct ParsedColumn {
    name: String,
    type_oid: PgTypeOid,
    typmod: Option<i32>,
    not_null: bool,
    has_default: bool,
    is_generated: bool,
    identity: Option<AttIdentity>,
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

    // Encode any `(n)` / `(p,s)` modifier sitting next to the type name.
    // Empty `typmods` (`varchar` plain) yields `None`.
    let typmod = match cd.type_name.as_ref() {
        Some(tn) => crate::typmod::encode(interp, type_oid, &tn.typmods)?,
        None => None,
    };

    let mut not_null = cd.is_not_null;
    let mut has_default = cd.raw_default.is_some() || cd.cooked_default.is_some();
    let mut is_generated = false;

    if is_serial {
        has_default = true;
    }

    let mut identity: Option<AttIdentity> = None;
    if !cd.identity.is_empty() {
        has_default = true;
        not_null = true;
        identity = match cd.identity.as_str() {
            "a" => Some(AttIdentity::Always),
            "d" => Some(AttIdentity::ByDefault),
            _ => None,
        };
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
                    if identity.is_none() {
                        identity = match c.generated_when.as_str() {
                            // PG `ATTRIBUTE_IDENTITY_ALWAYS` is `'a'`,
                            // `ATTRIBUTE_IDENTITY_BY_DEFAULT` is `'d'`.
                            "a" => Some(AttIdentity::Always),
                            "d" => Some(AttIdentity::ByDefault),
                            // Default to BY DEFAULT when unspecified, matching
                            // `GENERATED AS IDENTITY` shorthand semantics in
                            // some grammars; PG itself always emits one of the
                            // two so this is just a defensive fallback.
                            _ => Some(AttIdentity::ByDefault),
                        };
                    }
                }
                Ok(ConstrType::ConstrGenerated) => {
                    has_default = true;
                    is_generated = true;
                    if let Some(expr) = c.raw_expr.as_deref() {
                        super::volatile::check_no_volatile(
                            expr,
                            super::volatile::ExprLocation::Generated,
                            interp,
                        )?;
                    }
                }
                Ok(ConstrType::ConstrCheck) => {
                    if let Some(expr) = c.raw_expr.as_deref() {
                        super::volatile::check_no_volatile(
                            expr,
                            super::volatile::ExprLocation::Check,
                            interp,
                        )?;
                    }
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
        typmod,
        not_null,
        has_default,
        is_generated,
        identity,
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
        AlterTableType::AtDropConstraint => drop_constraint(interp, relid, cmd),
        AlterTableType::AtAddIdentity => set_identity(interp, relid, cmd),
        AlterTableType::AtSetIdentity => set_identity(interp, relid, cmd),
        AlterTableType::AtDropIdentity => drop_identity(interp, relid, cmd),
        // Other subtypes are no-ops for schema analysis.
        _ => Ok(()),
    }
}

fn drop_constraint(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let conname = &cmd.name;
    let found_oid = interp
        .pg_constraint
        .values()
        .find(|c| c.conrelid == relid && &c.conname == conname)
        .map(|c| c.oid);
    let Some(oid) = found_oid else {
        if cmd.missing_ok {
            return Ok(());
        }
        let relname = interp
            .pg_class
            .get(&relid)
            .map(|c| c.relname.as_str())
            .unwrap_or("?");
        return Err(DdlError::DependencyError(format!(
            "constraint \"{conname}\" of relation \"{relname}\" does not exist"
        )));
    };

    // Refuse to drop a UNIQUE/PK constraint that an FK depends on.
    let is_pkey_or_unique = interp
        .pg_constraint
        .get(&oid)
        .is_some_and(|c| matches!(c.contype, ConType::PrimaryKey | ConType::Unique));
    let cascade = matches!(
        DropBehavior::try_from(cmd.behavior),
        Ok(DropBehavior::DropCascade)
    );
    if is_pkey_or_unique && !cascade {
        let target_set: std::collections::BTreeSet<i16> = interp
            .pg_constraint
            .get(&oid)
            .map(|c| c.conkey.iter().copied().collect())
            .unwrap_or_default();
        let dependent: Option<String> = interp.pg_constraint.values().find_map(|c| {
            if matches!(c.contype, ConType::ForeignKey)
                && c.confrelid == Some(relid)
                && c.confkey
                    .iter()
                    .copied()
                    .collect::<std::collections::BTreeSet<_>>()
                    == target_set
            {
                Some(c.conname.clone())
            } else {
                None
            }
        });
        if let Some(dep) = dependent {
            let relname = interp
                .pg_class
                .get(&relid)
                .map(|c| c.relname.as_str())
                .unwrap_or("?");
            return Err(DdlError::DependencyError(format!(
                "cannot drop constraint \"{conname}\" on \"{relname}\" because foreign key \
                 constraint \"{dep}\" depends on it"
            )));
        }
    }

    // Drop the backing index (`<conname>` shares the relname with the
    // constraint for PK/UNIQUE) before dropping the constraint itself.
    if is_pkey_or_unique {
        let nsoid = interp.pg_class.get(&relid).map(|c| c.relnamespace);
        if let Some(nsoid) = nsoid
            && let Some(idx_oid) = interp
                .class_by_qname
                .get(&(nsoid, conname.clone()))
                .copied()
            && matches!(
                interp.pg_class.get(&idx_oid).map(|c| c.relkind),
                Some(RelKind::Index)
            )
        {
            interp.remove_pg_index(idx_oid);
            interp.remove_pg_class(idx_oid);
            let obj = crate::oid::PgGenericOid::new(idx_oid.get()).unwrap();
            interp.remove_dependencies_of(crate::pg_catalog::PG_CLASS_RELID, obj);
            interp.remove_dependencies_on(crate::pg_catalog::PG_CLASS_RELID, obj);
        }
    }

    interp.pg_constraint.remove(&oid);
    Ok(())
}

fn set_identity(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let Some(def) = cmd.def.as_deref() else {
        return Ok(());
    };

    // `ALTER COLUMN x ADD GENERATED <kind> AS IDENTITY` parses with
    // `def = Constraint{contype=Identity, generated_when=…}`. The
    // `SET GENERATED <kind>` form parses with `def = List<DefElem>` where
    // one DefElem has `defname = "generated"` and `arg = Integer('a' | 'd')`.
    let new_identity = match def.node.as_ref() {
        Some(node::Node::Constraint(c)) if c.contype == ConstrType::ConstrIdentity as i32 => {
            match c.generated_when.as_str() {
                "a" => Some(AttIdentity::Always),
                "d" => Some(AttIdentity::ByDefault),
                _ => Some(AttIdentity::ByDefault),
            }
        }
        Some(node::Node::List(list)) => {
            let mut found = None;
            for item in &list.items {
                if let Some(node::Node::DefElem(de)) = item.node.as_ref()
                    && de.defname == "generated"
                    && let Some(arg) = de.arg.as_deref()
                    && let Some(node::Node::Integer(i)) = arg.node.as_ref()
                {
                    found = Some(match i.ival as u8 as char {
                        'a' => AttIdentity::Always,
                        _ => AttIdentity::ByDefault,
                    });
                    break;
                }
            }
            found
        }
        _ => return Ok(()),
    };

    let Some(attrs) = interp.pg_attribute.get_mut(&relid) else {
        return Err(DdlError::TableNotFound(format!("relation oid {relid}")));
    };
    let Some(col) = attrs.iter_mut().find(|c| c.attname == cmd.name) else {
        return Err(DdlError::Parse(format!(
            "column \"{}\" of relation does not exist",
            cmd.name
        )));
    };
    if let Some(new_identity) = new_identity {
        col.attidentity = Some(new_identity);
        col.attnotnull = true;
        col.atthasdef = true;
    }
    Ok(())
}

fn drop_identity(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let Some(attrs) = interp.pg_attribute.get_mut(&relid) else {
        return Err(DdlError::TableNotFound(format!("relation oid {relid}")));
    };
    let Some(col) = attrs.iter_mut().find(|c| c.attname == cmd.name) else {
        if cmd.missing_ok {
            return Ok(());
        }
        return Err(DdlError::Parse(format!(
            "column \"{}\" of relation does not exist",
            cmd.name
        )));
    };
    col.attidentity = None;
    Ok(())
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
        atttypmod: col.typmod,
        attidentity: col.identity,
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

    // Resolve the column's attnum *before* we touch anything — both the
    // FK protection and the children-cascade need it.
    let target_attnum = interp
        .attributes_of(relid)
        .iter()
        .find(|a| a.attname == cmd.name)
        .map(|a| a.attnum);

    // PG also blocks DROP COLUMN when a `pg_constraint` row references
    // it (PK/UNIQUE/CHECK on this relation, or an FK on another relation
    // whose target column matches), or when a `pg_index` row's `indkey`
    // contains the attnum. Without CASCADE we surface the same error PG
    // does.
    if let Some(an) = target_attnum {
        let mut blockers: Vec<String> = Vec::new();
        for c in interp.pg_constraint.values() {
            if c.conrelid == relid && c.conkey.contains(&an) {
                blockers.push(c.conname.clone());
            }
            if matches!(c.contype, ConType::ForeignKey)
                && c.confrelid == Some(relid)
                && c.confkey.contains(&an)
            {
                blockers.push(c.conname.clone());
            }
        }
        // Indexes whose indkey references this attnum block too. Skip
        // indexes that are *already* covered by a constraint above (their
        // pg_class name matches a pg_constraint we'll drop anyway) so the
        // error message doesn't double-up.
        let constraint_names: std::collections::HashSet<String> =
            blockers.iter().cloned().collect();
        let dependent_indexes: Vec<PgClassOid> = interp
            .pg_index
            .values()
            .filter(|i| i.indrelid == relid && i.indkey.contains(&an))
            .map(|i| i.indexrelid)
            .collect();
        for idx_oid in &dependent_indexes {
            if let Some(c) = interp.pg_class.get(idx_oid)
                && !constraint_names.contains(&c.relname)
            {
                blockers.push(c.relname.clone());
            }
        }
        if !blockers.is_empty() && !cascade {
            let relname = interp
                .pg_class
                .get(&relid)
                .map(|c| c.relname.clone())
                .unwrap_or_default();
            return Err(DdlError::DependencyError(format!(
                "cannot drop column {relname}.{} because constraint(s) {} depend on it",
                cmd.name,
                blockers.join(", "),
            )));
        }
        if cascade && !blockers.is_empty() {
            // CASCADE: drop the dependent constraints + their backing
            // indexes + any non-constraint indexes that reference this
            // column.
            let to_drop_constraints: Vec<_> = interp
                .pg_constraint
                .values()
                .filter(|c| {
                    (c.conrelid == relid && c.conkey.contains(&an))
                        || (matches!(c.contype, ConType::ForeignKey)
                            && c.confrelid == Some(relid)
                            && c.confkey.contains(&an))
                })
                .map(|c| c.oid)
                .collect();
            for oid in to_drop_constraints {
                interp.pg_constraint.remove(&oid);
            }
            for idx_oid in dependent_indexes {
                interp.remove_pg_index(idx_oid);
                interp.remove_pg_class(idx_oid);
                let obj = crate::oid::PgGenericOid::new(idx_oid.get()).unwrap();
                interp.remove_dependencies_of(crate::pg_catalog::PG_CLASS_RELID, obj);
                interp.remove_dependencies_on(crate::pg_catalog::PG_CLASS_RELID, obj);
            }
        }
    }

    if !dependent_views.is_empty() {
        views::drop_views(interp, &dependent_views);
    }

    if let Some(attrs) = interp.pg_attribute.get_mut(&relid) {
        attrs.retain(|a| a.attname != cmd.name);
    }

    // Cascade DROP COLUMN through pg_inherits: every direct (and transitive)
    // child that inherited the same column name loses it too. PG actually
    // tracks this with `attinhcount`; we approximate by walking pg_inherits
    // and removing the matching column from each descendant.
    cascade_drop_column_to_children(interp, relid, &cmd.name);

    Ok(())
}

/// Recursively remove a dropped column from every child relation that
/// inherits from `parent_oid`. Skips children that don't carry the column —
/// they may have it locally or have already had it dropped earlier in the
/// same operation.
fn cascade_drop_column_to_children(interp: &mut PgCatalog, parent_oid: PgClassOid, col_name: &str) {
    let direct_children: Vec<PgClassOid> = interp
        .pg_inherits
        .iter()
        .filter(|i| i.inhparent == parent_oid)
        .map(|i| i.inhrelid)
        .collect();
    for child in direct_children {
        let had_col = interp
            .attributes_of(child)
            .iter()
            .any(|a| a.attname == col_name);
        if !had_col {
            continue;
        }
        if let Some(attrs) = interp.pg_attribute.get_mut(&child) {
            attrs.retain(|a| a.attname != col_name);
        }
        cascade_drop_column_to_children(interp, child, col_name);
    }
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

    let new_typmod = match cd.type_name.as_ref() {
        Some(tn) => crate::typmod::encode(interp, new_type_oid, &tn.typmods)?,
        None => None,
    };

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
        col.atttypmod = new_typmod;
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
        let attnums: Vec<i16> = pk_cols
            .iter()
            .filter_map(|n| {
                interp
                    .attributes_of(relid)
                    .iter()
                    .find(|a| &a.attname == n)
                    .map(|a| a.attnum)
            })
            .collect();
        if !attnums.is_empty() {
            let relname = interp
                .pg_class
                .get(&relid)
                .map(|c| c.relname.clone())
                .unwrap_or_default();
            let conname = if c.conname.is_empty() {
                format!("{relname}_pkey")
            } else {
                c.conname.clone()
            };
            emit_constraint_with_backing_index(
                interp,
                relid,
                conname,
                ConType::PrimaryKey,
                attnums,
                None,
                Vec::new(),
            );
        }
    }

    if c.contype == ConstrType::ConstrUnique as i32 {
        let cols: Vec<String> = c
            .keys
            .iter()
            .filter_map(|k| match k.node.as_ref()? {
                node::Node::String(s) => Some(s.sval.clone()),
                _ => None,
            })
            .collect();
        let attnums: Vec<i16> = cols
            .iter()
            .filter_map(|n| {
                interp
                    .attributes_of(relid)
                    .iter()
                    .find(|a| &a.attname == n)
                    .map(|a| a.attnum)
            })
            .collect();
        if !attnums.is_empty() {
            let relname = interp
                .pg_class
                .get(&relid)
                .map(|c| c.relname.clone())
                .unwrap_or_default();
            let conname = if c.conname.is_empty() {
                format!("{relname}_{}_key", cols.join("_"))
            } else {
                c.conname.clone()
            };
            emit_constraint_with_backing_index(
                interp,
                relid,
                conname,
                ConType::Unique,
                attnums,
                None,
                Vec::new(),
            );
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

    if c.contype == ConstrType::ConstrCheck as i32
        && let Some(expr) = c.raw_expr.as_deref()
    {
        super::volatile::check_no_volatile(expr, super::volatile::ExprLocation::Check, interp)?;
        validate_check_expression_for_table(interp, relid, expr)?;
        // Emit pg_constraint row for the CHECK so future inspections see it.
        let conname = if c.conname.is_empty() {
            let relname = interp
                .pg_class
                .get(&relid)
                .map(|cls| cls.relname.clone())
                .unwrap_or_default();
            format!("{relname}_check")
        } else {
            c.conname.clone()
        };
        let oid = PgConstraintOid::new(interp.alloc_oid()).expect("alloc_oid is non-zero");
        interp.insert_pg_constraint(PgConstraint {
            oid,
            conname,
            conrelid: relid,
            contype: ConType::Check,
            conkey: Vec::new(),
            confrelid: None,
            confkey: Vec::new(),
        });
    }

    if c.contype == ConstrType::ConstrForeign as i32 {
        // FK column list lives in `fk_attrs`, not `keys` (which is empty
        // for FK constraints — `keys` is only used by PK/UNIQUE).
        let column_names: Vec<String> = c
            .fk_attrs
            .iter()
            .filter_map(|k| match k.node.as_ref()? {
                node::Node::String(s) => Some(s.sval.clone()),
                _ => None,
            })
            .collect();
        let attrs = interp.attributes_of(relid).to_vec();
        let local_types: Vec<PgTypeOid> = column_names
            .iter()
            .filter_map(|n| attrs.iter().find(|a| &a.attname == n).map(|a| a.atttypid))
            .collect();
        if local_types.len() != column_names.len() {
            return Err(DdlError::Parse(
                "ALTER TABLE ADD FOREIGN KEY references unknown local column".to_string(),
            ));
        }
        let attnums: Vec<i16> = column_names
            .iter()
            .filter_map(|n| attrs.iter().find(|a| &a.attname == n).map(|a| a.attnum))
            .collect();
        let relname_owned = interp
            .pg_class
            .get(&relid)
            .map(|c| c.relname.clone())
            .unwrap_or_default();
        let (target_oid, target_attnums) =
            resolve_fk_target(interp, c, &relname_owned, &local_types)?;
        let conname = if c.conname.is_empty() {
            format!("{relname_owned}_{}_fkey", column_names.join("_"))
        } else {
            c.conname.clone()
        };
        let oid = PgConstraintOid::new(interp.alloc_oid()).expect("alloc_oid is non-zero");
        interp.insert_pg_constraint(PgConstraint {
            oid,
            conname,
            conrelid: relid,
            contype: ConType::ForeignKey,
            conkey: attnums,
            confrelid: Some(target_oid),
            confkey: target_attnums,
        });
    }

    Ok(())
}

/// Run a CHECK expression through the analyzer in the scope of `relid`
/// and verify its result type is boolean. Used by ALTER TABLE ADD
/// CONSTRAINT (the CREATE TABLE path uses [`validate_constraint_expressions`]
/// which sees the full `CreateStmt` shape).
fn validate_check_expression_for_table(
    interp: &PgCatalog,
    relid: PgClassOid,
    expr: &pg_query::protobuf::Node,
) -> Result<(), DdlError> {
    use crate::expr::{TypeGoal, infer_expr};
    use crate::nullability::NullabilityContext;
    use crate::param_collector::ParamCollector;
    use crate::pg_catalog::oid;
    use crate::qualified_name::QualifiedName;
    use crate::scope::Scope;

    let class = interp
        .pg_class
        .get(&relid)
        .ok_or_else(|| DdlError::TableNotFound(format!("relation oid {relid}")))?;
    let nspname = interp
        .namespace_name(class.relnamespace)
        .map(str::to_owned)
        .unwrap_or_else(|| "public".to_owned());
    let relname = class.relname.clone();
    let attrs = interp.attributes_of(relid).to_vec();

    let mut scope = Scope::default();
    scope.add_dml_target(
        interp,
        &relname,
        QualifiedName::new(nspname, relname.clone()),
        &attrs,
    );
    let null_ctx = NullabilityContext::default();
    let mut params = ParamCollector::default();

    let result = infer_expr(expr, &scope, &null_ctx, interp, &mut params, TypeGoal::NONE)
        .map_err(|e| DdlError::UnsupportedDdl(format!("CHECK on \"{relname}\": {e}")))?;
    if result.type_oid != oid::BOOL && result.type_oid != oid::UNKNOWN {
        let typname = interp
            .get_type(result.type_oid)
            .map(|t| t.typname.clone())
            .unwrap_or_else(|| format!("oid {}", result.type_oid));
        return Err(DdlError::UnsupportedDdl(format!(
            "argument of CHECK constraint on \"{relname}\" must be type boolean, not type {typname}"
        )));
    }
    Ok(())
}
