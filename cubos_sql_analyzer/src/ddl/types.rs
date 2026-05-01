//! CREATE TYPE / CREATE DOMAIN / ALTER TYPE DDL handlers.

use pg_query::protobuf::{
    AlterEnumStmt, CoercionContext, CompositeTypeStmt, ConstrType, CreateCastStmt,
    CreateDomainStmt, CreateEnumStmt, CreateRangeStmt, DefineStmt, ObjectType, node,
};

use crate::oid::{PgCastOid, PgClassOid, PgEnumOid, PgNamespaceOid, PgTypeOid};
use crate::pg_catalog::{
    CastContext, CastMethod, PgAttribute, PgCast, PgClass, PgEnum, PgRange, PgType, RelKind,
    TypCategory, TypType,
};

use super::DdlError;
use super::util::{
    ensure_namespace, ensure_qualified_name, names_key, node_string,
    register_composite_to_record_cast, resolve_type_name,
};
use crate::pg_catalog::PgCatalog;

// ─── CREATE DOMAIN ──────────────────────────────────────────────────────────

pub fn create_domain(interp: &mut PgCatalog, stmt: &CreateDomainStmt) -> Result<(), DdlError> {
    let (nsoid, name) = ensure_qualified_name(interp, &stmt.domainname);

    if interp.type_by_qname.contains_key(&(nsoid, name.clone())) {
        return Err(DdlError::DuplicateObject(format!(
            "type \"{name}\" already exists"
        )));
    }

    let base_type_name = stmt
        .type_name
        .as_ref()
        .ok_or_else(|| DdlError::TypeNotFound("domain base type".into()))?;
    let base_type_oid = resolve_type_name(base_type_name, interp)
        .ok_or_else(|| DdlError::TypeNotFound("domain base type".into()))?;
    let typtypmod = crate::typmod::encode(interp, base_type_oid, &base_type_name.typmods)?;

    // Domains inherit category/preferred from their base type.
    let (typcategory, typispreferred) = interp
        .pg_type
        .get(&base_type_oid)
        .map(|t| (t.typcategory, t.typispreferred))
        .unwrap_or((TypCategory::UserDefined, false));

    // `CREATE DOMAIN d AS T NOT NULL` lands in `stmt.constraints` as a
    // `Constraint { contype = CONSTR_NOTNULL }`. PG also forbids null defaults
    // on a NOT NULL domain, but the analyzer doesn't model defaults yet.
    let typnotnull = stmt.constraints.iter().any(|n| {
        matches!(
            n.node.as_ref(),
            Some(node::Node::Constraint(c)) if c.contype == ConstrType::ConstrNotnull as i32
        )
    });

    let oid = PgTypeOid::new(interp.alloc_oid()).expect("alloc_oid is non-zero");
    interp.insert_pg_type(PgType {
        oid,
        typname: name.clone(),
        typnamespace: nsoid,
        typtype: TypType::Domain,
        typcategory,
        typispreferred,
        typrelid: None,
        typelem: None,
        typarray: None,
        typbasetype: Some(base_type_oid),
        typnotnull,
        typtypmod,
    });

    register_array_type(interp, nsoid, &name, oid);
    Ok(())
}

// ─── CREATE TYPE AS ENUM ────────────────────────────────────────────────────

pub fn create_enum(interp: &mut PgCatalog, stmt: &CreateEnumStmt) -> Result<(), DdlError> {
    let (nsoid, name) = ensure_qualified_name(interp, &stmt.type_name);

    if interp.type_by_qname.contains_key(&(nsoid, name.clone())) {
        return Err(DdlError::DuplicateObject(format!(
            "type \"{name}\" already exists"
        )));
    }

    let labels: Vec<String> = stmt
        .vals
        .iter()
        .filter_map(|n| node_string(n).map(|s| s.to_owned()))
        .collect();

    let oid = PgTypeOid::new(interp.alloc_oid()).expect("alloc_oid is non-zero");
    interp.insert_pg_type(PgType {
        oid,
        typname: name.clone(),
        typnamespace: nsoid,
        typtype: TypType::Enum,
        typcategory: TypCategory::Enum,
        typispreferred: false,
        typrelid: None,
        typelem: None,
        typarray: None,
        typbasetype: None,
        typnotnull: false,
        typtypmod: None,
    });
    for (i, label) in labels.into_iter().enumerate() {
        let enum_oid = PgEnumOid::new(interp.alloc_oid()).expect("alloc_oid is non-zero");
        interp.insert_pg_enum(PgEnum {
            oid: enum_oid,
            enumtypid: oid,
            enumsortorder: (i + 1) as f32,
            enumlabel: label,
        });
    }

    register_array_type(interp, nsoid, &name, oid);
    Ok(())
}

// ─── CREATE TYPE AS (composite) ─────────────────────────────────────────────

pub fn create_composite(interp: &mut PgCatalog, stmt: &CompositeTypeStmt) -> Result<(), DdlError> {
    let rv = stmt
        .typevar
        .as_ref()
        .ok_or_else(|| DdlError::Parse("CREATE TYPE without name".into()))?;

    let schema = if rv.schemaname.is_empty() {
        interp
            .search_path
            .first()
            .and_then(|&oid| interp.namespace_name(oid).map(str::to_owned))
            .unwrap_or_else(|| "public".to_owned())
    } else {
        rv.schemaname.clone()
    };
    let nsoid = ensure_namespace(interp, &schema);
    let name = rv.relname.clone();

    if interp.type_by_qname.contains_key(&(nsoid, name.clone())) {
        return Err(DdlError::DuplicateObject(format!(
            "type \"{name}\" already exists"
        )));
    }

    // Collect column definitions before mutating, so we can resolve type
    // names against the catalog without holding a mutable borrow.
    let mut field_defs: Vec<(String, PgTypeOid, Option<i32>, bool)> = Vec::new();
    for col_node in &stmt.coldeflist {
        if let Some(node::Node::ColumnDef(cd)) = col_node.node.as_ref()
            && let Some(tn) = cd.type_name.as_ref()
            && let Some(type_oid) = resolve_type_name(tn, interp)
        {
            let typmod = crate::typmod::encode(interp, type_oid, &tn.typmods)?;
            field_defs.push((cd.colname.clone(), type_oid, typmod, cd.is_not_null));
        }
    }

    let class_oid = PgClassOid::new(interp.alloc_oid()).expect("alloc_oid is non-zero");
    let type_oid = PgTypeOid::new(interp.alloc_oid()).expect("alloc_oid is non-zero");

    interp.insert_pg_class(PgClass {
        oid: class_oid,
        relname: name.clone(),
        relnamespace: nsoid,
        relkind: RelKind::CompositeType,
        reltype: Some(type_oid),
    });
    for (i, (fname, ftype, ftypmod, fnotnull)) in field_defs.into_iter().enumerate() {
        interp.insert_pg_attribute(PgAttribute {
            attrelid: class_oid,
            attname: fname,
            atttypid: ftype,
            attnum: (i + 1) as i16,
            attnotnull: fnotnull,
            atthasdef: false,
            attgenerated: None,
            atttypmod: ftypmod,
            attidentity: None,
            attcollation: None,
        });
    }
    interp.insert_pg_type(PgType {
        oid: type_oid,
        typname: name.clone(),
        typnamespace: nsoid,
        typtype: TypType::Composite,
        typcategory: TypCategory::Composite,
        typispreferred: false,
        typrelid: Some(class_oid),
        typelem: None,
        typarray: None,
        typbasetype: None,
        typnotnull: false,
        typtypmod: None,
    });

    register_array_type(interp, nsoid, &name, type_oid);
    register_composite_to_record_cast(interp, type_oid);

    Ok(())
}

// ─── CREATE TYPE AS RANGE ───────────────────────────────────────────────────

pub fn create_range(interp: &mut PgCatalog, stmt: &CreateRangeStmt) -> Result<(), DdlError> {
    let (nsoid, name) = ensure_qualified_name(interp, &stmt.type_name);

    if interp.type_by_qname.contains_key(&(nsoid, name.clone())) {
        return Err(DdlError::DuplicateObject(format!(
            "type \"{name}\" already exists"
        )));
    }

    // Extract subtype from params (look for DefElem with defname="subtype").
    let mut subtype_oid: Option<PgTypeOid> = None;
    for param_node in &stmt.params {
        if let Some(node::Node::DefElem(de)) = param_node.node.as_ref()
            && de.defname == "subtype"
            && let Some(arg) = de.arg.as_deref()
            && let Some(node::Node::TypeName(tn)) = arg.node.as_ref()
        {
            subtype_oid = resolve_type_name(tn, interp);
        }
    }
    let Some(subtype_oid) = subtype_oid else {
        return Ok(());
    };

    let oid = PgTypeOid::new(interp.alloc_oid()).expect("alloc_oid is non-zero");
    interp.insert_pg_type(PgType {
        oid,
        typname: name.clone(),
        typnamespace: nsoid,
        typtype: TypType::Range,
        typcategory: TypCategory::Range,
        typispreferred: false,
        typrelid: None,
        typelem: None,
        typarray: None,
        typbasetype: None,
        typnotnull: false,
        typtypmod: None,
    });
    interp.insert_pg_range(PgRange {
        rngtypid: oid,
        rngsubtype: subtype_oid,
    });

    register_array_type(interp, nsoid, &name, oid);
    Ok(())
}

// ─── ALTER TYPE ... ADD VALUE (enum) ────────────────────────────────────────

pub fn alter_enum(interp: &mut PgCatalog, stmt: &AlterEnumStmt) -> Result<(), DdlError> {
    let key = names_key(&stmt.type_name, interp);
    let nsoid = match interp.namespace_oid(&key.schema) {
        Some(oid) => oid,
        None => return Err(DdlError::TypeNotFound(key.to_string())),
    };

    let Some(&oid) = interp.type_by_qname.get(&(nsoid, key.name.clone())) else {
        return Err(DdlError::TypeNotFound(key.to_string()));
    };

    if !matches!(
        interp.pg_type.get(&oid).map(|t| t.typtype),
        Some(TypType::Enum)
    ) {
        return Ok(());
    }

    let labels = interp.pg_enum.entry(oid).or_default();
    if labels.iter().any(|e| e.enumlabel == stmt.new_val) {
        if stmt.skip_if_new_val_exists {
            return Ok(());
        }
        return Err(DdlError::DuplicateObject(format!(
            "enum label \"{}\" already exists",
            stmt.new_val
        )));
    }

    let new_sortorder = if stmt.new_val_neighbor.is_empty() {
        labels
            .iter()
            .map(|e| e.enumsortorder)
            .fold(0.0_f32, f32::max)
            + 1.0
    } else if let Some(neighbor) = labels.iter().find(|e| e.enumlabel == stmt.new_val_neighbor) {
        let neighbor_order = neighbor.enumsortorder;
        if stmt.new_val_is_after {
            // Insert immediately after: midpoint with the next-higher
            // sortorder, or neighbor + 1 if neighbor is last.
            let next = labels
                .iter()
                .filter(|e| e.enumsortorder > neighbor_order)
                .map(|e| e.enumsortorder)
                .fold(f32::INFINITY, f32::min);
            if next.is_finite() {
                (neighbor_order + next) / 2.0
            } else {
                neighbor_order + 1.0
            }
        } else {
            // Insert immediately before: midpoint with the previous-lower
            // sortorder, or neighbor - 1 if neighbor is first.
            let prev = labels
                .iter()
                .filter(|e| e.enumsortorder < neighbor_order)
                .map(|e| e.enumsortorder)
                .fold(f32::NEG_INFINITY, f32::max);
            if prev.is_finite() {
                (neighbor_order + prev) / 2.0
            } else {
                neighbor_order - 1.0
            }
        }
    } else {
        labels
            .iter()
            .map(|e| e.enumsortorder)
            .fold(0.0_f32, f32::max)
            + 1.0
    };

    let enum_oid = PgEnumOid::new(interp.alloc_oid()).expect("alloc_oid is non-zero");
    interp.insert_pg_enum(PgEnum {
        oid: enum_oid,
        enumtypid: oid,
        enumsortorder: new_sortorder,
        enumlabel: stmt.new_val.clone(),
    });

    Ok(())
}

// ─── DefineStmt: CREATE TYPE name / CREATE TYPE name (...) ──────────────────

/// Handle `DefineStmt` which covers shell types (`CREATE TYPE citext;`) and
/// full type definitions (`CREATE TYPE citext (INPUT = ..., OUTPUT = ...)`).
pub fn define_type(interp: &mut PgCatalog, stmt: &DefineStmt) -> Result<(), DdlError> {
    let obj_type = ObjectType::try_from(stmt.kind).unwrap_or(ObjectType::Undefined);
    if obj_type != ObjectType::ObjectType {
        return Ok(());
    }

    let (nsoid, name) = ensure_qualified_name(interp, &stmt.defnames);

    if interp.type_by_qname.contains_key(&(nsoid, name.clone())) {
        // Full definition after shell type — just confirm it exists.
        return Ok(());
    }

    let oid = PgTypeOid::new(interp.alloc_oid()).expect("alloc_oid is non-zero");
    interp.insert_pg_type(PgType {
        oid,
        typname: name.clone(),
        typnamespace: nsoid,
        typtype: TypType::Base,
        typcategory: TypCategory::UserDefined,
        typispreferred: false,
        typrelid: None,
        typelem: None,
        typarray: None,
        typbasetype: None,
        typnotnull: false,
        typtypmod: None,
    });
    register_array_type(interp, nsoid, &name, oid);
    Ok(())
}

// ─── CREATE CAST ────────────────────────────────────────────────────────────

pub fn create_cast(interp: &mut PgCatalog, stmt: &CreateCastStmt) -> Result<(), DdlError> {
    let source_oid = stmt
        .sourcetype
        .as_ref()
        .and_then(|tn| resolve_type_name(tn, interp));
    let target_oid = stmt
        .targettype
        .as_ref()
        .and_then(|tn| resolve_type_name(tn, interp));

    let (Some(src), Some(tgt)) = (source_oid, target_oid) else {
        return Ok(());
    };

    let castcontext = match CoercionContext::try_from(stmt.context) {
        Ok(CoercionContext::CoercionImplicit) => CastContext::Implicit,
        Ok(CoercionContext::CoercionAssignment) => CastContext::Assignment,
        _ => CastContext::Explicit,
    };

    // Map `CREATE CAST` syntax to pg_cast.castmethod:
    // - WITH FUNCTION f(...)  → 'f' (Function)
    // - WITH INOUT            → 'i' (InOut)
    // - WITHOUT FUNCTION      → 'b' (Binary)
    let castmethod = if stmt.inout {
        CastMethod::InOut
    } else if stmt.func.is_some() {
        CastMethod::Function
    } else {
        CastMethod::Binary
    };

    let cast_oid = PgCastOid::new(interp.alloc_oid()).expect("alloc_oid is non-zero");
    interp.insert_pg_cast(PgCast {
        oid: cast_oid,
        castsource: src,
        casttarget: tgt,
        castcontext,
        castmethod,
    });
    Ok(())
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Suppress the auto array name when needed; we always use `_<name>`.
fn array_name(base_name: &str) -> String {
    format!("_{base_name}")
}

/// Register an array type (`_name`) for a base type, and back-link the
/// element type's `typarray` to it so `array_type_of(element)` resolves.
pub(crate) fn register_array_type(
    interp: &mut PgCatalog,
    nsoid: PgNamespaceOid,
    base_name: &str,
    element_oid: PgTypeOid,
) -> PgTypeOid {
    let array_oid = PgTypeOid::new(interp.alloc_oid()).expect("alloc_oid is non-zero");
    interp.insert_pg_type(PgType {
        oid: array_oid,
        typname: array_name(base_name),
        typnamespace: nsoid,
        typtype: TypType::Base,
        typcategory: TypCategory::Array,
        typispreferred: false,
        typrelid: None,
        typelem: Some(element_oid),
        typarray: None,
        typbasetype: None,
        typnotnull: false,
        typtypmod: None,
    });
    if let Some(elem) = interp.pg_type.get_mut(&element_oid) {
        elem.typarray = Some(array_oid);
    }
    array_oid
}
