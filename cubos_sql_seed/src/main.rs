//! Generates `seed.json` for the static analyzer from a live PostgreSQL instance.
//!
//! Usage:
//!   cargo run -p cubos_sql_seed
//!
//! Spins up a disposable `postgres:latest` container via the Docker daemon
//! (using `testcontainers`), waits for it to accept connections, exports the
//! schema, and then stops + removes the container (via `Drop`). The output is
//! written to `cubos_sql_analyzer/src/seed.json`.
//!
//! Run this when updating to a new PostgreSQL version (e.g. PG 19) to refresh
//! the baseline type catalog used by the DDL interpreter.

use std::collections::{BTreeMap, HashMap};

use cubos_sql_analyzer::schema::*;
use testcontainers::ImageExt;
use testcontainers::runners::SyncRunner;
use testcontainers_modules::postgres::Postgres;

fn main() {
    eprintln!("Pulling postgres:latest from registry...");
    // `pull_image()` forces a fresh pull on every run — otherwise
    // `start()` only pulls on 404, which would pin us to whatever
    // `postgres:latest` was when the image was first cached locally.
    let request = Postgres::default()
        .with_tag("latest")
        .pull_image()
        .expect("failed to pull postgres:latest");

    eprintln!("Starting postgres:latest container...");
    // `testcontainers_modules::postgres::Postgres` waits for the "database
    // system is ready to accept connections" log line before `start()`
    // returns, so no manual readiness polling is required.
    let container = request.start().expect("failed to start postgres container");

    let host = container.get_host().expect("failed to get container host");
    let port = container
        .get_host_port_ipv4(5432)
        .expect("failed to get container port");
    let conn_str =
        format!("host={host} port={port} user=postgres password=postgres dbname=postgres");

    eprintln!("Connecting to: {conn_str}");
    let mut client = postgres::Client::connect(&conn_str, postgres::NoTls)
        .expect("failed to connect to PostgreSQL");

    eprintln!("Exporting schema...");
    let mut snapshot = export_schema(&mut client).expect("failed to export schema");

    // Sort all Vec values for deterministic output.
    for fns in snapshot.functions_by_name.values_mut() {
        fns.sort_by_key(|f| f.oid);
    }
    for ops in snapshot.operators_by_name.values_mut() {
        ops.sort_by_key(|o| (o.left_type_oid.unwrap_or(0), o.right_type_oid));
    }

    // Serialize via BTreeMap for sorted keys.
    let ordered = OrderedSnapshot::from(snapshot);
    let json = serde_json::to_string(&ordered).expect("failed to serialize snapshot");

    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let out_path = std::path::Path::new(manifest_dir)
        .parent()
        .unwrap()
        .join("cubos_sql_analyzer/src/seed.json");
    std::fs::write(&out_path, &json).expect("failed to write seed.json");

    let num_types = ordered.types.len();
    let num_tables = ordered.tables.len();
    let num_functions: usize = ordered.functions_by_name.values().map(|v| v.len()).sum();
    let num_operators: usize = ordered.operators_by_name.values().map(|v| v.len()).sum();
    let num_casts = ordered.casts.len();
    let size_kb = json.len() / 1024;

    eprintln!("Wrote {out_path:?} ({size_kb} KB)");
    eprintln!("  types:     {num_types}");
    eprintln!("  tables:    {num_tables}");
    eprintln!("  functions: {num_functions}");
    eprintln!("  operators: {num_operators}");
    eprintln!("  casts:     {num_casts}");
}

// ─── Deterministic serialization ───────────────────────────────────────────────

/// Mirror of `SchemaSnapshot` with `BTreeMap` for deterministic key ordering.
#[derive(serde::Serialize)]
struct OrderedSnapshot {
    types: BTreeMap<u32, TypeEntry>,
    type_by_name: BTreeMap<String, u32>,
    tables: BTreeMap<u32, TableEntry>,
    table_by_name: BTreeMap<String, u32>,
    functions_by_name: BTreeMap<String, Vec<FunctionEntry>>,
    operators_by_name: BTreeMap<String, Vec<OperatorEntry>>,
    casts: BTreeMap<String, CastContext>,
    search_path: Vec<String>,
}

impl From<SchemaSnapshot> for OrderedSnapshot {
    fn from(s: SchemaSnapshot) -> Self {
        Self {
            types: s.types.into_iter().collect(),
            type_by_name: s.type_by_name.into_iter().collect(),
            tables: s.tables.into_iter().collect(),
            table_by_name: s.table_by_name.into_iter().collect(),
            functions_by_name: s.functions_by_name.into_iter().collect(),
            operators_by_name: s.operators_by_name.into_iter().collect(),
            casts: s.casts.into_iter().collect(),
            search_path: s.search_path,
        }
    }
}

// ─── Schema export ─────────────────────────────────────────────────────────────

fn export_schema(client: &mut postgres::Client) -> Result<SchemaSnapshot, postgres::Error> {
    let search_path = export_search_path(client)?;
    let (types, type_by_name) = export_types(client)?;
    let (tables, table_by_name) = export_tables(client)?;
    let functions = export_functions(client)?;
    let operators = export_operators(client)?;
    let casts = export_casts(client)?;

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

    // Derive the known schema set from everything we exported.
    let mut schemas: std::collections::HashSet<String> = std::collections::HashSet::new();
    for t in types.values() {
        schemas.insert(t.schema.clone());
    }
    for t in tables.values() {
        schemas.insert(t.schema.clone());
    }
    for fns in functions_by_name.values() {
        for f in fns {
            schemas.insert(f.schema.clone());
        }
    }
    for s in &search_path {
        schemas.insert(s.clone());
    }

    Ok(SchemaSnapshot {
        types,
        type_by_name,
        tables,
        table_by_name,
        functions_by_name,
        operators_by_name,
        casts: casts_map,
        search_path,
        schemas,
    })
}

fn export_search_path(client: &mut postgres::Client) -> Result<Vec<String>, postgres::Error> {
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

type TypeExport = (HashMap<u32, TypeEntry>, HashMap<String, u32>);

fn export_types(client: &mut postgres::Client) -> Result<TypeExport, postgres::Error> {
    let rows = client.query(
        "SELECT t.oid, t.typname, n.nspname, t.typtype, t.typbasetype, t.typelem, \
                t.typrelid, t.typlen, t.typcategory, t.typispreferred \
         FROM pg_catalog.pg_type t \
         JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
         ORDER BY t.oid",
        &[],
    )?;

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
        let typcategory: i8 = row.get(8);
        let typispreferred: bool = row.get(9);
        let category = typcategory as u8 as char;

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
                category,
                is_preferred: typispreferred,
                extension: None,
            },
        );
    }

    Ok((types, type_by_name))
}

type TableExport = (HashMap<u32, TableEntry>, HashMap<String, u32>);

fn export_tables(client: &mut postgres::Client) -> Result<TableExport, postgres::Error> {
    let rows = client.query(
        "SELECT c.oid, c.relname, n.nspname, c.relkind \
         FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind IN ('r', 'v', 'm', 'p') \
           AND n.nspname NOT IN ('pg_toast') \
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
           AND n.nspname NOT IN ('pg_toast') \
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
                view_def: None,
            },
        );
    }

    Ok((tables, table_by_name))
}

fn export_functions(client: &mut postgres::Client) -> Result<Vec<FunctionEntry>, postgres::Error> {
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
            is_procedure: false,
            agg_final_type_oid,
        });
    }

    Ok(functions)
}

fn export_operators(client: &mut postgres::Client) -> Result<Vec<OperatorEntry>, postgres::Error> {
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

struct CastEntry {
    source_type_oid: u32,
    target_type_oid: u32,
    context: CastContext,
}

fn export_casts(client: &mut postgres::Client) -> Result<Vec<CastEntry>, postgres::Error> {
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
