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
use cubos_sql_analyzer::{PgCatalog, QualifiedName, SchemaSeed};
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

    // Vec values are kept in the natural pg_proc order (the export query is
    // ordered by oid). That ordering determines overload resolution
    // tie-breaking, and pg_proc oids correlate with preferred overloads in
    // the way the analyzer expects (e.g. `length(text)` before
    // `length(bytea)`), so do not re-sort here.
    for ops in snapshot.operators_by_name.values_mut() {
        ops.sort_by_key(|o| (o.left_type_oid.unwrap_or(0), o.right_type_oid));
    }

    // Second pass: populate `view_def` on every relation that `pg_class`
    // reported as a view/matview by re-applying its `CREATE VIEW` through the
    // analyzer's own DDL pipeline. The pass-1 snapshot already has the view
    // columns from `pg_attribute`, so resolution of one view against another
    // works without ordering.
    eprintln!("Exporting view definitions...");
    let view_defs =
        export_view_definitions(&mut client).expect("failed to export view definitions");
    eprintln!("Populating view_def for {} view(s)...", view_defs.len());
    let snapshot = populate_view_defs(snapshot, view_defs);

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

/// Mirror of [`SchemaSeed`] with `BTreeMap` for deterministic key ordering.
#[derive(serde::Serialize)]
struct OrderedSnapshot {
    types: BTreeMap<u32, TypeEntry>,
    type_by_name: BTreeMap<String, u32>,
    tables: BTreeMap<String, TableEntry>,
    functions_by_name: BTreeMap<String, Vec<FunctionEntry>>,
    operators_by_name: BTreeMap<String, Vec<OperatorEntry>>,
    casts: BTreeMap<String, CastInfo>,
    search_path: Vec<String>,
}

impl From<SchemaSeed> for OrderedSnapshot {
    fn from(s: SchemaSeed) -> Self {
        Self {
            types: s.types.into_iter().collect(),
            type_by_name: s
                .type_by_name
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            tables: s
                .tables
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            functions_by_name: s
                .functions_by_name
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            operators_by_name: s
                .operators_by_name
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            casts: s.casts.into_iter().collect(),
            search_path: s.search_path,
        }
    }
}

// ─── Schema export ─────────────────────────────────────────────────────────────

fn export_schema(client: &mut postgres::Client) -> Result<SchemaSeed, postgres::Error> {
    let search_path = export_search_path(client)?;
    let (types, type_by_name) = export_types(client)?;
    let tables = export_tables(client)?;
    let functions = export_functions(client)?;
    let operators = export_operators(client)?;
    let casts = export_casts(client)?;

    let mut functions_by_name: HashMap<QualifiedName, Vec<FunctionEntry>> = HashMap::new();
    for f in functions {
        let key = QualifiedName::new(&f.schema, &f.name);
        functions_by_name.entry(key).or_default().push(f);
    }

    let mut operators_by_name: HashMap<QualifiedName, Vec<OperatorEntry>> = HashMap::new();
    for o in operators {
        let key = QualifiedName::new("pg_catalog", &o.name);
        operators_by_name.entry(key).or_default().push(o);
    }

    let casts_map: HashMap<String, CastInfo> = casts
        .into_iter()
        .map(|c| {
            let key = format!("{}:{}", c.source_type_oid, c.target_type_oid);
            (key, CastInfo::new(c.context, c.method))
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

    Ok(SchemaSeed {
        types,
        type_by_name,
        tables,
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
    // Both `$user` (session role) and `"$user"` get normalized to "public"
    // since we don't have a real user context at analysis time. Dedupe after
    // normalization so the same schema doesn't appear twice.
    let mut schemas: Vec<String> = Vec::new();
    for part in raw.split(',') {
        let part = part.trim().trim_matches('"');
        let name = if part == "$user" || part == "\"$user\"" {
            "public".to_owned()
        } else {
            part.to_owned()
        };
        if !schemas.contains(&name) {
            schemas.push(name);
        }
    }
    Ok(schemas)
}

type TypeExport = (HashMap<u32, TypeEntry>, HashMap<QualifiedName, u32>);

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
    let mut type_by_name: HashMap<QualifiedName, u32> = HashMap::new();

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

        type_by_name.insert(QualifiedName::new(&schema, &name), oid);
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

fn export_tables(
    client: &mut postgres::Client,
) -> Result<HashMap<QualifiedName, TableEntry>, postgres::Error> {
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
        "SELECT a.attrelid, a.attname, a.atttypid, a.attnotnull, a.atthasdef, \
                a.attgenerated \
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
        // `attgenerated` is one byte: 's' for STORED, 'v' for VIRTUAL
        // (PG18), '\0' for non-generated.
        let attgenerated: i8 = row.get(5);
        columns_map.entry(relid).or_default().push(TableColumn {
            name: row.get(1),
            type_oid: row.get(2),
            not_null: row.get(3),
            has_default: row.get(4),
            is_generated: attgenerated != 0,
        });
    }

    let mut tables: HashMap<QualifiedName, TableEntry> = HashMap::new();

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

        let key = QualifiedName::new(&schema, &name);
        tables.insert(
            key,
            TableEntry {
                name,
                schema,
                kind,
                columns: columns_map.remove(&oid).unwrap_or_default(),
                view_def: None,
            },
        );
    }

    Ok(tables)
}

fn export_functions(client: &mut postgres::Client) -> Result<Vec<FunctionEntry>, postgres::Error> {
    // `proallargtypes` / `proargmodes` / `proargnames` are NULL for plain
    // functions that only take IN args (PG sets them non-NULL only when at
    // least one arg is OUT/INOUT/TABLE/VARIADIC). Read them as nullable
    // arrays and fall back to an empty slice when absent.
    let rows = client.query(
        "SELECT p.oid, p.proname, n.nspname, \
                p.proargtypes::int4[]::int4[], p.prorettype, \
                p.proisstrict, p.proretset, p.provariadic != 0 as is_variadic, \
                p.prokind, \
                p.proallargtypes::int4[], p.proargmodes, p.proargnames, \
                p.pronargdefaults \
         FROM pg_catalog.pg_proc p \
         JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
         WHERE n.nspname NOT IN ('pg_toast') \
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
        let all_arg_types: Option<Vec<i32>> = row.get(9);
        // `proargmodes` is `char[]` → Vec<i8> with one byte per arg.
        let arg_modes: Option<Vec<i8>> = row.get(10);
        let arg_names: Option<Vec<String>> = row.get(11);
        // `pronargdefaults` — count of trailing IN args with DEFAULT
        // expressions. Lets the analyzer accept calls that omit the
        // tail (e.g. `jsonb_set(j, p, v)` against the 4-arg signature).
        let pronargdefaults: i16 = row.get(12);

        let is_aggregate = prokind as u8 as char == 'a';
        let is_window = prokind as u8 as char == 'w';
        let agg_final_type_oid = if is_aggregate {
            agg_final.get(&oid).copied().flatten()
        } else {
            None
        };

        let out_args = extract_out_args(
            all_arg_types.as_deref(),
            arg_modes.as_deref(),
            arg_names.as_deref(),
        );

        functions.push(FunctionEntry {
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
            out_args,
            num_default_args: pronargdefaults.max(0) as u8,
        });
    }

    Ok(functions)
}

/// Extract OUT/INOUT/TABLE args from `pg_proc.{proallargtypes, proargmodes,
/// proargnames}` into a list of `CompositeField`s keyed by name.
/// - All three arrays share one index per formal arg.
/// - `proargmodes`: `'i'` IN, `'o'` OUT, `'b'` INOUT, `'v'` VARIADIC, `'t'` TABLE.
/// - We only keep OUT, INOUT and TABLE entries — those are the columns
///   visible from `(func(...)).field` or `FROM func(...)`.
/// - `proargnames[i]` may be an empty string; skip those since they can't
///   appear in a named field lookup anyway.
fn extract_out_args(
    all_types: Option<&[i32]>,
    modes: Option<&[i8]>,
    names: Option<&[String]>,
) -> Vec<CompositeField> {
    let (Some(types), Some(modes), Some(names)) = (all_types, modes, names) else {
        return Vec::new();
    };
    let len = types.len().min(modes.len()).min(names.len());
    let mut out = Vec::new();
    for i in 0..len {
        let mode = modes[i] as u8 as char;
        if !matches!(mode, 'o' | 'b' | 't') {
            continue;
        }
        let name = &names[i];
        if name.is_empty() {
            continue;
        }
        out.push(CompositeField {
            name: name.clone(),
            type_oid: types[i] as u32,
            not_null: false,
        });
    }
    out
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
    method: CastMethod,
}

fn export_casts(client: &mut postgres::Client) -> Result<Vec<CastEntry>, postgres::Error> {
    let rows = client.query(
        "SELECT c.castsource, c.casttarget, c.castcontext, c.castmethod \
         FROM pg_catalog.pg_cast c",
        &[],
    )?;

    let casts = rows
        .iter()
        .map(|row| {
            let ctx: i8 = row.get(2);
            let method: i8 = row.get(3);
            CastEntry {
                source_type_oid: row.get(0),
                target_type_oid: row.get(1),
                context: match ctx as u8 as char {
                    'i' => CastContext::Implicit,
                    'a' => CastContext::Assignment,
                    _ => CastContext::Explicit,
                },
                method: match method as u8 as char {
                    'b' => CastMethod::Binary,
                    'i' => CastMethod::InOut,
                    _ => CastMethod::Function,
                },
            }
        })
        .collect();

    Ok(casts)
}

// ─── View definitions (second pass) ────────────────────────────────────────────

/// Read every view and materialized view definition from the pristine catalog.
/// `pg_views` / `pg_matviews` already cover `pg_catalog`, `information_schema`
/// and any other schema visible to the superuser.
fn export_view_definitions(
    client: &mut postgres::Client,
) -> Result<Vec<(String, String, String)>, postgres::Error> {
    let rows = client.query(
        "SELECT schemaname, viewname, definition FROM pg_catalog.pg_views \
         UNION ALL \
         SELECT schemaname, matviewname, definition FROM pg_catalog.pg_matviews \
         ORDER BY 1, 2",
        &[],
    )?;

    Ok(rows
        .iter()
        .map(|r| {
            let schema: String = r.get(0);
            let name: String = r.get(1);
            let definition: String = r.get(2);
            (schema, name, definition)
        })
        .collect())
}

/// Enrich `snapshot` by re-applying each view's `CREATE VIEW` through the
/// analyzer. This drives `ddl::views::create_view`, which populates
/// `TableEntry.view_def` (resolved AST + deps) the same way user migrations do.
///
/// Fail-fast: any analyzer failure or column drift aborts the seed with the
/// offending view's name, error, and full SQL definition. The expectation is
/// that every system view round-trips cleanly — otherwise the analyzer has
/// regressed or a new PG release introduced a construct we don't cover.
fn populate_view_defs(seed: SchemaSeed, defs: Vec<(String, String, String)>) -> SchemaSeed {
    let mut db = PgCatalog::from_seed(seed);

    for (schema, name, definition) in &defs {
        let qn = QualifiedName::new(schema.clone(), name.clone());

        // Keep the pass-1 column list so we can diff against the reanalyzed
        // version below.
        let before: Vec<TableColumn> = db
            .tables()
            .get(&qn)
            .map(|t| t.columns.clone())
            .unwrap_or_default();

        // `pg_views.definition` always contains just the SELECT body; wrap it
        // with CREATE OR REPLACE so the DDL pipeline treats it as a view
        // replacement (overwriting the pass-1 `TableEntry` with one that
        // carries `view_def`).
        let sql = format!(
            "CREATE OR REPLACE VIEW {qn} AS {body}",
            qn = qn,
            body = definition.trim_end().trim_end_matches(';'),
        );

        if let Err(err) = db.apply_sql(&sql) {
            abort_on_view_failure(&qn, &err.to_string(), definition);
        }

        let after: Vec<TableColumn> = db
            .tables()
            .get(&qn)
            .map(|t| t.columns.clone())
            .unwrap_or_default();

        if let Some(drift) = describe_column_drift(&before, &after) {
            abort_on_view_failure(&qn, &format!("column drift: {drift}"), definition);
        }
    }

    db.to_seed()
}

/// Print the offending view's error and SQL, then panic. Formatted the same
/// way regardless of whether the failure came from `apply_sql` or from the
/// column-drift check so the caller's log layout stays consistent.
fn abort_on_view_failure(qn: &QualifiedName, error: &str, definition: &str) -> ! {
    eprintln!();
    eprintln!("--- {qn} ---");
    eprintln!("  error: {error}");
    eprintln!("  definition:");
    for line in definition.lines() {
        eprintln!("    {line}");
    }
    panic!("view reanalysis failed for {qn}");
}

/// Diff pass-1 and post-reanalysis column lists on shape: count, order,
/// names, and type OIDs. `not_null` is intentionally skipped — our analyzer
/// derives stricter nullability from expression structure than
/// `pg_attribute.attnotnull`, so drift on that field is expected.
fn describe_column_drift(before: &[TableColumn], after: &[TableColumn]) -> Option<String> {
    if before.len() != after.len() {
        return Some(format!("column count {} → {}", before.len(), after.len()));
    }
    for (i, (b, a)) in before.iter().zip(after.iter()).enumerate() {
        if b.name != a.name {
            return Some(format!("column #{i} name {:?} → {:?}", b.name, a.name));
        }
        if b.type_oid != a.type_oid {
            return Some(format!(
                "column #{i} {:?} type_oid {} → {}",
                b.name, b.type_oid, a.type_oid
            ));
        }
    }
    None
}
