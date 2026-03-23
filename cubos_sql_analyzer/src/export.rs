//! Schema export from a live PostgreSQL connection.
//!
//! Queries `pg_catalog` to build a [`SchemaSnapshot`] containing all types,
//! tables, functions, operators, and casts needed for static SQL analysis.

use std::collections::HashMap;

use crate::error::AnalyzeError;
use crate::schema::*;

/// Export a complete schema snapshot from a live PostgreSQL connection.
///
/// This should be called once per migration hash, after migrations are applied.
/// The returned snapshot is serializable and can be cached to disk.
pub fn export_schema(client: &mut postgres::Client) -> Result<SchemaSnapshot, AnalyzeError> {
    let search_path = export_search_path(client)?;
    let (types, type_by_name) = export_types(client)?;
    let (tables, table_by_name) = export_tables(client)?;
    let functions = export_functions(client)?;
    let operators = export_operators(client)?;
    let casts = export_casts(client)?;

    // Build indexed structures.
    let mut functions_by_name: HashMap<String, Vec<FunctionEntry>> = HashMap::new();
    for f in functions {
        functions_by_name.entry(f.name.clone()).or_default().push(f);
    }

    let mut operators_by_name: HashMap<String, Vec<OperatorEntry>> = HashMap::new();
    for o in operators {
        operators_by_name.entry(o.name.clone()).or_default().push(o);
    }

    let casts_map: HashMap<String, CastContext> = casts
        .into_iter()
        .map(|c| {
            let key = format!("{}:{}", c.source_type_oid, c.target_type_oid);
            (key, c.context)
        })
        .collect();

    Ok(SchemaSnapshot {
        types,
        type_by_name,
        tables,
        table_by_name,
        functions_by_name,
        operators_by_name,
        casts: casts_map,
        search_path,
    })
}

// ──────────────────────────────────────────────────────────────────────────────
// search_path
// ──────────────────────────────────────────────────────────────────────────────

fn export_search_path(client: &mut postgres::Client) -> Result<Vec<String>, AnalyzeError> {
    let row = client.query_one("SHOW search_path", &[])?;
    let raw: String = row.get(0);
    let schemas: Vec<String> = raw
        .split(',')
        .map(|s| {
            let s = s.trim().trim_matches('"');
            if s == "$user" || s == "\"$user\"" {
                "public".to_owned()
            } else {
                s.to_owned()
            }
        })
        .collect();
    Ok(schemas)
}

// ──────────────────────────────────────────────────────────────────────────────
// Types
// ──────────────────────────────────────────────────────────────────────────────

fn export_types(
    client: &mut postgres::Client,
) -> Result<(HashMap<u32, TypeEntry>, HashMap<String, u32>), AnalyzeError> {
    let rows = client.query(
        "SELECT t.oid, t.typname, n.nspname, t.typtype, t.typbasetype, t.typelem, \
                t.typrelid, t.typlen \
         FROM pg_catalog.pg_type t \
         JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
         ORDER BY t.oid",
        &[],
    )?;

    // Pre-fetch enum labels.
    let enum_rows = client.query(
        "SELECT enumtypid, enumlabel \
         FROM pg_catalog.pg_enum \
         ORDER BY enumtypid, enumsortorder",
        &[],
    )?;
    let mut enum_labels: HashMap<u32, Vec<String>> = HashMap::new();
    for row in &enum_rows {
        let typid: u32 = row.get(0);
        let label: String = row.get(1);
        enum_labels.entry(typid).or_default().push(label);
    }

    // Pre-fetch composite type fields.
    let comp_rows = client.query(
        "SELECT a.attrelid, a.attname, a.atttypid, a.attnotnull \
         FROM pg_catalog.pg_attribute a \
         WHERE a.attnum > 0 AND NOT a.attisdropped \
         ORDER BY a.attrelid, a.attnum",
        &[],
    )?;
    let mut composite_fields: HashMap<u32, Vec<CompositeField>> = HashMap::new();
    for row in &comp_rows {
        let relid: u32 = row.get(0);
        composite_fields
            .entry(relid)
            .or_default()
            .push(CompositeField {
                name: row.get(1),
                type_oid: row.get(2),
                not_null: row.get(3),
            });
    }

    let mut types = HashMap::new();
    let mut type_by_name = HashMap::new();

    for row in &rows {
        let oid: u32 = row.get(0);
        let name: String = row.get(1);
        let schema: String = row.get(2);
        let typtype: i8 = row.get(3);
        let basetype: u32 = row.get(4);
        let typelem: u32 = row.get(5);
        let typrelid: u32 = row.get(6);
        let typlen: i16 = row.get(7);

        let kind = match typtype as u8 as char {
            'd' => TypeKind::Domain {
                base_type_oid: basetype,
            },
            'e' => TypeKind::Enum {
                labels: enum_labels.remove(&oid).unwrap_or_default(),
            },
            'c' => TypeKind::Composite {
                fields: composite_fields.get(&typrelid).cloned().unwrap_or_default(),
            },
            'r' => TypeKind::Range {
                subtype_oid: basetype,
            },
            'p' => TypeKind::Pseudo,
            _ => {
                // Check if this is an array type (typlen == -1 and has typelem).
                if typelem != 0 && typlen == -1 && name.starts_with('_') {
                    TypeKind::Array {
                        element_type_oid: typelem,
                    }
                } else {
                    TypeKind::Base
                }
            }
        };

        type_by_name.insert(format!("{schema}.{name}"), oid);
        types.insert(
            oid,
            TypeEntry {
                oid,
                name,
                schema,
                kind,
            },
        );
    }

    Ok((types, type_by_name))
}

// ──────────────────────────────────────────────────────────────────────────────
// Tables and views
// ──────────────────────────────────────────────────────────────────────────────

fn export_tables(
    client: &mut postgres::Client,
) -> Result<(HashMap<u32, TableEntry>, HashMap<String, u32>), AnalyzeError> {
    let rows = client.query(
        "SELECT c.oid, c.relname, n.nspname, c.relkind \
         FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind IN ('r', 'v', 'm', 'p') \
           AND n.nspname NOT IN ('pg_catalog', 'information_schema', 'pg_toast') \
         ORDER BY c.oid",
        &[],
    )?;

    let col_rows = client.query(
        "SELECT a.attrelid, a.attname, a.atttypid, a.attnotnull, a.atthasdef, a.attnum \
         FROM pg_catalog.pg_attribute a \
         JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE a.attnum > 0 \
           AND NOT a.attisdropped \
           AND c.relkind IN ('r', 'v', 'm', 'p') \
           AND n.nspname NOT IN ('pg_catalog', 'information_schema', 'pg_toast') \
         ORDER BY a.attrelid, a.attnum",
        &[],
    )?;

    let mut columns_map: HashMap<u32, Vec<TableColumn>> = HashMap::new();
    for row in &col_rows {
        let relid: u32 = row.get(0);
        columns_map.entry(relid).or_default().push(TableColumn {
            name: row.get(1),
            type_oid: row.get(2),
            not_null: row.get(3),
            has_default: row.get(4),
            attnum: row.get(5),
        });
    }

    let mut tables = HashMap::new();
    let mut table_by_name = HashMap::new();

    for row in &rows {
        let oid: u32 = row.get(0);
        let name: String = row.get(1);
        let schema: String = row.get(2);
        let relkind: i8 = row.get(3);

        let kind = match relkind as u8 as char {
            'r' => RelationKind::Table,
            'v' => RelationKind::View,
            'm' => RelationKind::MaterializedView,
            'p' => RelationKind::Partitioned,
            _ => continue,
        };

        table_by_name.insert(format!("{schema}.{name}"), oid);
        tables.insert(
            oid,
            TableEntry {
                oid,
                name,
                schema,
                kind,
                columns: columns_map.remove(&oid).unwrap_or_default(),
            },
        );
    }

    Ok((tables, table_by_name))
}

// ──────────────────────────────────────────────────────────────────────────────
// Functions
// ──────────────────────────────────────────────────────────────────────────────

fn export_functions(client: &mut postgres::Client) -> Result<Vec<FunctionEntry>, AnalyzeError> {
    // Export: user-schema functions + pg_catalog aggregates/window functions +
    // common pg_catalog functions used in SQL queries.
    let rows = client.query(
        "SELECT p.oid, p.proname, n.nspname, \
                p.proargtypes::int4[]::int4[], p.prorettype, \
                p.proisstrict, p.proretset, p.provariadic != 0 as is_variadic, \
                p.prokind \
         FROM pg_catalog.pg_proc p \
         JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
         WHERE n.nspname NOT IN ('information_schema', 'pg_toast') \
         ORDER BY p.oid",
        &[],
    )?;

    // Pre-fetch aggregate info.
    let agg_rows = client.query(
        "SELECT a.aggfnoid::int4 as oid, \
                CASE WHEN a.aggfinalfn != 0 \
                     THEN (SELECT prorettype FROM pg_proc WHERE oid = a.aggfinalfn) \
                     ELSE NULL \
                END as final_type \
         FROM pg_catalog.pg_aggregate a",
        &[],
    )?;
    let mut agg_final: HashMap<u32, Option<u32>> = HashMap::new();
    for row in &agg_rows {
        let oid: i32 = row.get(0);
        let final_type: Option<u32> = row.get(1);
        agg_final.insert(oid as u32, final_type);
    }

    let mut functions = Vec::new();

    for row in &rows {
        let oid: u32 = row.get(0);
        let name: String = row.get(1);
        let schema: String = row.get(2);
        let arg_types_raw: Vec<i32> = row.get(3);
        let arg_types: Vec<u32> = arg_types_raw.iter().map(|&x| x as u32).collect();
        let return_type_oid: u32 = row.get(4);
        let is_strict: bool = row.get(5);
        let is_set_returning: bool = row.get(6);
        let is_variadic: bool = row.get(7);
        let prokind: i8 = row.get(8);

        let is_aggregate = prokind as u8 as char == 'a';
        let is_window = prokind as u8 as char == 'w';
        let agg_final_type_oid = if is_aggregate {
            agg_final.get(&oid).copied().flatten()
        } else {
            None
        };

        functions.push(FunctionEntry {
            oid,
            name,
            schema,
            arg_types,
            return_type_oid,
            is_aggregate,
            is_window,
            is_variadic,
            is_set_returning,
            is_strict,
            agg_final_type_oid,
        });
    }

    Ok(functions)
}

// ──────────────────────────────────────────────────────────────────────────────
// Operators
// ──────────────────────────────────────────────────────────────────────────────

fn export_operators(client: &mut postgres::Client) -> Result<Vec<OperatorEntry>, AnalyzeError> {
    let rows = client.query(
        "SELECT o.oprname, o.oprleft, o.oprright, o.oprresult \
         FROM pg_catalog.pg_operator o \
         WHERE o.oprresult != 0",
        &[],
    )?;

    let operators = rows
        .iter()
        .map(|row| {
            let left: u32 = row.get(1);
            OperatorEntry {
                name: row.get(0),
                left_type_oid: if left == 0 { None } else { Some(left) },
                right_type_oid: row.get(2),
                result_type_oid: row.get(3),
            }
        })
        .collect();

    Ok(operators)
}

// ──────────────────────────────────────────────────────────────────────────────
// Casts
// ──────────────────────────────────────────────────────────────────────────────

fn export_casts(client: &mut postgres::Client) -> Result<Vec<CastEntry>, AnalyzeError> {
    let rows = client.query(
        "SELECT c.castsource, c.casttarget, c.castcontext \
         FROM pg_catalog.pg_cast c",
        &[],
    )?;

    let casts = rows
        .iter()
        .map(|row| {
            let ctx: i8 = row.get(2);
            CastEntry {
                source_type_oid: row.get(0),
                target_type_oid: row.get(1),
                context: match ctx as u8 as char {
                    'i' => CastContext::Implicit,
                    'a' => CastContext::Assignment,
                    _ => CastContext::Explicit,
                },
            }
        })
        .collect();

    Ok(casts)
}

// Integration tests for export are in tests/compare.rs (use Docker container discovery).
