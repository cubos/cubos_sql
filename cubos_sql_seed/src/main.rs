//! Generates `seed.json` for the static analyzer from a live PostgreSQL instance.
//!
//! Usage:
//!   cargo run -p cubos_sql_seed
//!
//! Spins up a disposable `postgres:latest` container via the Docker daemon
//! (using `testcontainers`), waits for it to accept connections, exports each
//! `pg_catalog` table almost 1:1 into the analyzer's `PgCatalogSeed`, then
//! stops + removes the container (via `Drop`). The output is written to
//! `cubos_sql_analyzer/src/seed.json`.

use std::collections::HashMap;

use cubos_sql_analyzer::{
    ArgMode, AttGenerated, AttIdentity, CastContext, CastMethod, ConType, DepType, PgAggregate,
    PgAttribute, PgCast, PgCastOid, PgCatalog, PgCatalogSeed, PgClass, PgClassOid, PgConstraint,
    PgConstraintOid, PgDepend, PgEnum, PgEnumOid, PgExtension, PgExtensionOid, PgGenericOid,
    PgInherits, PgNamespace, PgNamespaceOid, PgOperator, PgOperatorOid, PgProc, PgProcOid, PgRange,
    PgType, PgTypeOid, ProKind, ProVolatile, QualifiedName, RelKind, TypCategory, TypType,
};
use testcontainers::ImageExt;
use testcontainers::runners::SyncRunner;
use testcontainers_modules::postgres::Postgres;

fn main() {
    eprintln!("Pulling postgres:latest from registry...");
    let request = Postgres::default()
        .with_tag("latest")
        .pull_image()
        .expect("failed to pull postgres:latest");

    eprintln!("Starting postgres:latest container...");
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

    eprintln!("Exporting catalog...");
    let snapshot = export_catalog(&mut client).expect("failed to export schema");

    eprintln!("Exporting view definitions...");
    let view_defs =
        export_view_definitions(&mut client).expect("failed to export view definitions");
    eprintln!("Populating relviewdef for {} view(s)...", view_defs.len());
    let snapshot = populate_view_defs(snapshot, view_defs);

    let json = serde_json::to_string(&snapshot).expect("failed to serialize snapshot");

    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let out_path = std::path::Path::new(manifest_dir)
        .parent()
        .unwrap()
        .join("cubos_sql_analyzer/src/seed.json");
    std::fs::write(&out_path, &json).expect("failed to write seed.json");

    let size_kb = json.len() / 1024;
    eprintln!("Wrote {out_path:?} ({size_kb} KB)");
    eprintln!("  pg_namespace: {}", snapshot.pg_namespace.len());
    eprintln!("  pg_type:      {}", snapshot.pg_type.len());
    eprintln!("  pg_enum:      {}", snapshot.pg_enum.len());
    eprintln!("  pg_range:     {}", snapshot.pg_range.len());
    eprintln!("  pg_class:     {}", snapshot.pg_class.len());
    eprintln!("  pg_attribute: {}", snapshot.pg_attribute.len());
    eprintln!("  pg_proc:      {}", snapshot.pg_proc.len());
    eprintln!("  pg_aggregate: {}", snapshot.pg_aggregate.len());
    eprintln!("  pg_operator:  {}", snapshot.pg_operator.len());
    eprintln!("  pg_cast:      {}", snapshot.pg_cast.len());
    eprintln!("  pg_extension: {}", snapshot.pg_extension.len());
    eprintln!("  pg_depend:    {}", snapshot.pg_depend.len());
    eprintln!("  pg_inherits:  {}", snapshot.pg_inherits.len());
    eprintln!("  pg_constraint:{}", snapshot.pg_constraint.len());
}

// ─── Schema export ─────────────────────────────────────────────────────────────

fn export_catalog(client: &mut postgres::Client) -> Result<PgCatalogSeed, postgres::Error> {
    let pg_namespace = export_namespaces(client)?;
    let nsname_by_oid: HashMap<PgNamespaceOid, String> = pg_namespace
        .iter()
        .map(|n| (n.oid, n.nspname.clone()))
        .collect();

    let pg_type = export_types(client)?;
    let pg_enum = export_enums(client)?;
    let pg_range = export_ranges(client)?;
    let pg_class = export_classes(client)?;
    let pg_attribute = export_attributes(client)?;
    let pg_proc = export_procs(client)?;
    let pg_aggregate = export_aggregates(client)?;
    let pg_operator = export_operators(client)?;
    let pg_cast = export_casts(client)?;
    let pg_extension = export_extensions(client)?;
    let pg_depend = export_depends(client)?;
    let pg_inherits = export_inherits(client)?;
    let pg_constraint = export_constraints(client)?;
    let search_path = export_search_path(client, &pg_namespace)?;

    let _ = nsname_by_oid;

    Ok(PgCatalogSeed {
        pg_namespace,
        pg_type,
        pg_enum,
        pg_range,
        pg_class,
        pg_attribute,
        pg_proc,
        pg_aggregate,
        pg_operator,
        pg_cast,
        pg_extension,
        pg_depend,
        pg_inherits,
        pg_constraint,
        search_path,
    })
}

fn export_search_path(
    client: &mut postgres::Client,
    namespaces: &[PgNamespace],
) -> Result<Vec<PgNamespaceOid>, postgres::Error> {
    let row = client.query_one("SHOW search_path", &[])?;
    let raw: String = row.get(0);
    let by_name: HashMap<&str, PgNamespaceOid> = namespaces
        .iter()
        .map(|n| (n.nspname.as_str(), n.oid))
        .collect();
    let mut oids: Vec<PgNamespaceOid> = Vec::new();
    let mut seen: std::collections::HashSet<PgNamespaceOid> = std::collections::HashSet::new();
    for part in raw.split(',') {
        let part = part.trim().trim_matches('"');
        let name = if part == "$user" || part == "\"$user\"" {
            "public"
        } else {
            part
        };
        if let Some(&oid) = by_name.get(name)
            && seen.insert(oid)
        {
            oids.push(oid);
        }
    }
    Ok(oids)
}

fn export_namespaces(client: &mut postgres::Client) -> Result<Vec<PgNamespace>, postgres::Error> {
    let rows = client.query(
        "SELECT oid, nspname FROM pg_catalog.pg_namespace ORDER BY oid",
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|r| {
            let oid: u32 = r.get(0);
            PgNamespace {
                oid: PgNamespaceOid::new(oid).expect("namespace oid is non-zero"),
                nspname: r.get(1),
            }
        })
        .collect())
}

fn export_types(client: &mut postgres::Client) -> Result<Vec<PgType>, postgres::Error> {
    let rows = client.query(
        "SELECT oid, typname, typnamespace, typtype, typcategory, typispreferred, \
                typrelid, typelem, typarray, typbasetype, typnotnull, typtypmod \
         FROM pg_catalog.pg_type ORDER BY oid",
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|r| {
            let typtype: i8 = r.get(3);
            let typcategory: i8 = r.get(4);
            let oid: u32 = r.get(0);
            let typnamespace: u32 = r.get(2);
            let typrelid: u32 = r.get(6);
            let typelem: u32 = r.get(7);
            let typarray: u32 = r.get(8);
            let typbasetype: u32 = r.get(9);
            let typtypmod_raw: i32 = r.get(11);
            PgType {
                oid: PgTypeOid::new(oid).expect("pg_type.oid is non-zero"),
                typname: r.get(1),
                typnamespace: PgNamespaceOid::new(typnamespace).expect("typnamespace is non-zero"),
                typtype: char_to_typtype(typtype as u8 as char),
                typcategory: char_to_typcategory(typcategory as u8 as char),
                typispreferred: r.get(5),
                typrelid: PgClassOid::new(typrelid),
                typelem: PgTypeOid::new(typelem),
                typarray: PgTypeOid::new(typarray),
                typbasetype: PgTypeOid::new(typbasetype),
                typnotnull: r.get(10),
                typtypmod: (typtypmod_raw >= 0).then_some(typtypmod_raw),
            }
        })
        .collect())
}

fn export_enums(client: &mut postgres::Client) -> Result<Vec<PgEnum>, postgres::Error> {
    let rows = client.query(
        "SELECT oid, enumtypid, enumsortorder, enumlabel \
         FROM pg_catalog.pg_enum ORDER BY enumtypid, enumsortorder",
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|r| {
            let oid: u32 = r.get(0);
            let enumtypid: u32 = r.get(1);
            PgEnum {
                oid: PgEnumOid::new(oid).expect("pg_enum.oid is non-zero"),
                enumtypid: PgTypeOid::new(enumtypid).expect("enumtypid is non-zero"),
                enumsortorder: r.get(2),
                enumlabel: r.get(3),
            }
        })
        .collect())
}

fn export_ranges(client: &mut postgres::Client) -> Result<Vec<PgRange>, postgres::Error> {
    let rows = client.query(
        "SELECT rngtypid, rngsubtype FROM pg_catalog.pg_range ORDER BY rngtypid",
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|r| {
            let rngtypid: u32 = r.get(0);
            let rngsubtype: u32 = r.get(1);
            PgRange {
                rngtypid: PgTypeOid::new(rngtypid).expect("rngtypid is non-zero"),
                rngsubtype: PgTypeOid::new(rngsubtype).expect("rngsubtype is non-zero"),
            }
        })
        .collect())
}

fn export_classes(client: &mut postgres::Client) -> Result<Vec<PgClass>, postgres::Error> {
    let rows = client.query(
        "SELECT oid, relname, relnamespace, relkind, reltype \
         FROM pg_catalog.pg_class \
         WHERE relkind IN ('r', 'v', 'm', 'p', 'c') \
         ORDER BY oid",
        &[],
    )?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let relkind: i8 = r.get(3);
            let relkind = char_to_relkind(relkind as u8 as char)?;
            let oid: u32 = r.get(0);
            let relnamespace: u32 = r.get(2);
            let reltype: u32 = r.get(4);
            Some(PgClass {
                oid: PgClassOid::new(oid).expect("pg_class.oid is non-zero"),
                relname: r.get(1),
                relnamespace: PgNamespaceOid::new(relnamespace).expect("relnamespace is non-zero"),
                relkind,
                reltype: PgTypeOid::new(reltype),
                relviewdef: Vec::new(),
                viewbindings: Vec::new(),
            })
        })
        .collect())
}

fn export_attributes(client: &mut postgres::Client) -> Result<Vec<PgAttribute>, postgres::Error> {
    let rows = client.query(
        "SELECT a.attrelid, a.attname, a.atttypid, a.attnum, a.attnotnull, a.atthasdef, \
                a.attgenerated, a.atttypmod, a.attidentity \
         FROM pg_catalog.pg_attribute a \
         JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
         WHERE a.attnum > 0 \
           AND NOT a.attisdropped \
           AND c.relkind IN ('r', 'v', 'm', 'p', 'c') \
         ORDER BY a.attrelid, a.attnum",
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|r| {
            let attgenerated: i8 = r.get(6);
            let attidentity: i8 = r.get(8);
            let attrelid: u32 = r.get(0);
            let atttypid: u32 = r.get(2);
            let atttypmod_raw: i32 = r.get(7);
            PgAttribute {
                attrelid: PgClassOid::new(attrelid).expect("attrelid is non-zero"),
                attname: r.get(1),
                atttypid: PgTypeOid::new(atttypid).expect("atttypid is non-zero"),
                attnum: r.get(3),
                attnotnull: r.get(4),
                atthasdef: r.get(5),
                attgenerated: char_to_attgenerated(attgenerated as u8 as char),
                atttypmod: (atttypmod_raw >= 0).then_some(atttypmod_raw),
                attidentity: char_to_attidentity(attidentity as u8 as char),
            }
        })
        .collect())
}

fn export_procs(client: &mut postgres::Client) -> Result<Vec<PgProc>, postgres::Error> {
    let rows = client.query(
        "SELECT p.oid, p.proname, p.pronamespace, p.prokind, \
                p.proargtypes::int4[]::int4[], p.prorettype, \
                p.proretset, p.provariadic, p.proisstrict, p.pronargdefaults, \
                p.proallargtypes::int4[], p.proargmodes, p.proargnames, \
                p.provolatile \
         FROM pg_catalog.pg_proc p \
         ORDER BY p.oid",
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|r| {
            let prokind: i8 = r.get(3);
            let arg_types_raw: Vec<i32> = r.get(4);
            let proargtypes: Vec<PgTypeOid> = arg_types_raw
                .iter()
                .filter_map(|&x| PgTypeOid::new(x as u32))
                .collect();
            let proallargtypes_raw: Option<Vec<i32>> = r.get(10);
            let proallargtypes: Vec<PgTypeOid> = proallargtypes_raw
                .map(|v| {
                    v.into_iter()
                        .filter_map(|x| PgTypeOid::new(x as u32))
                        .collect()
                })
                .unwrap_or_default();
            let arg_modes: Option<Vec<i8>> = r.get(11);
            let proargmodes: Vec<ArgMode> = arg_modes
                .map(|v| {
                    v.into_iter()
                        .map(|c| char_to_argmode(c as u8 as char))
                        .collect()
                })
                .unwrap_or_default();
            let arg_names: Option<Vec<String>> = r.get(12);
            let provolatile: i8 = r.get(13);
            let oid: u32 = r.get(0);
            let pronamespace: u32 = r.get(2);
            let prorettype: u32 = r.get(5);
            let provariadic: u32 = r.get(7);
            PgProc {
                oid: PgProcOid::new(oid).expect("pg_proc.oid is non-zero"),
                proname: r.get(1),
                pronamespace: PgNamespaceOid::new(pronamespace).expect("pronamespace is non-zero"),
                prokind: char_to_prokind(prokind as u8 as char),
                proargtypes,
                prorettype: PgTypeOid::new(prorettype).expect("prorettype is non-zero"),
                proretset: r.get(6),
                provariadic: PgTypeOid::new(provariadic),
                proisstrict: r.get(8),
                pronargdefaults: r.get(9),
                proallargtypes,
                proargmodes,
                proargnames: arg_names.unwrap_or_default(),
                provolatile: char_to_provolatile(provolatile as u8 as char),
            }
        })
        .collect())
}

fn char_to_provolatile(c: char) -> ProVolatile {
    match c {
        'i' => ProVolatile::Immutable,
        's' => ProVolatile::Stable,
        _ => ProVolatile::Volatile,
    }
}

fn export_aggregates(client: &mut postgres::Client) -> Result<Vec<PgAggregate>, postgres::Error> {
    let rows = client.query(
        "SELECT a.aggfnoid::int4 as oid, \
                CASE WHEN a.aggfinalfn != 0 \
                     THEN (SELECT prorettype FROM pg_proc WHERE oid = a.aggfinalfn) \
                     ELSE 0::oid \
                END as final_type \
         FROM pg_aggregate a \
         ORDER BY a.aggfnoid",
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|r| {
            let oid: i32 = r.get(0);
            let final_type: u32 = r.get(1);
            PgAggregate {
                aggfnoid: PgProcOid::new(oid as u32).expect("aggfnoid is non-zero"),
                aggfinaltype: PgTypeOid::new(final_type),
            }
        })
        .collect())
}

fn export_operators(client: &mut postgres::Client) -> Result<Vec<PgOperator>, postgres::Error> {
    let rows = client.query(
        "SELECT oid, oprname, oprnamespace, oprleft, oprright, oprresult \
         FROM pg_catalog.pg_operator \
         WHERE oprresult != 0 \
         ORDER BY oid",
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|r| {
            let oid: u32 = r.get(0);
            let oprnamespace: u32 = r.get(2);
            let oprleft: u32 = r.get(3);
            let oprright: u32 = r.get(4);
            let oprresult: u32 = r.get(5);
            PgOperator {
                oid: PgOperatorOid::new(oid).expect("pg_operator.oid is non-zero"),
                oprname: r.get(1),
                oprnamespace: PgNamespaceOid::new(oprnamespace).expect("oprnamespace is non-zero"),
                oprleft: PgTypeOid::new(oprleft),
                oprright: PgTypeOid::new(oprright).expect("oprright is non-zero"),
                oprresult: PgTypeOid::new(oprresult).expect("oprresult is non-zero"),
            }
        })
        .collect())
}

fn export_casts(client: &mut postgres::Client) -> Result<Vec<PgCast>, postgres::Error> {
    let rows = client.query(
        "SELECT oid, castsource, casttarget, castcontext, castmethod \
         FROM pg_catalog.pg_cast \
         ORDER BY oid",
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|r| {
            let ctx: i8 = r.get(3);
            let method: i8 = r.get(4);
            let oid: u32 = r.get(0);
            let castsource: u32 = r.get(1);
            let casttarget: u32 = r.get(2);
            PgCast {
                oid: PgCastOid::new(oid).expect("pg_cast.oid is non-zero"),
                castsource: PgTypeOid::new(castsource).expect("castsource is non-zero"),
                casttarget: PgTypeOid::new(casttarget).expect("casttarget is non-zero"),
                castcontext: char_to_castcontext(ctx as u8 as char),
                castmethod: char_to_castmethod(method as u8 as char),
            }
        })
        .collect())
}

fn export_extensions(client: &mut postgres::Client) -> Result<Vec<PgExtension>, postgres::Error> {
    let rows = client.query(
        "SELECT oid, extname, extnamespace, extversion \
         FROM pg_catalog.pg_extension \
         ORDER BY oid",
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|r| {
            let oid: u32 = r.get(0);
            let extnamespace: u32 = r.get(2);
            PgExtension {
                oid: PgExtensionOid::new(oid).expect("pg_extension.oid is non-zero"),
                extname: r.get(1),
                extnamespace: PgNamespaceOid::new(extnamespace).expect("extnamespace is non-zero"),
                extversion: r.get(3),
            }
        })
        .collect())
}

fn export_constraints(client: &mut postgres::Client) -> Result<Vec<PgConstraint>, postgres::Error> {
    // Only export PRIMARY KEY / UNIQUE / FOREIGN KEY / CHECK / EXCLUSION
    // constraints — the analyzer does not consult anything else (e.g.
    // PG18 NOT NULL constraints land in pg_constraint too, but `attnotnull`
    // already records them).
    let rows = client.query(
        "SELECT oid, conname, conrelid, contype, conkey, confrelid, confkey \
         FROM pg_catalog.pg_constraint \
         WHERE contype IN ('p','u','f','c','x') \
         ORDER BY oid",
        &[],
    )?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let oid: u32 = r.get(0);
            let conname: String = r.get(1);
            let conrelid: u32 = r.get(2);
            let contype: i8 = r.get(3);
            let conkey: Option<Vec<i16>> = r.get(4);
            let confrelid: u32 = r.get(5);
            let confkey: Option<Vec<i16>> = r.get(6);
            Some(PgConstraint {
                oid: PgConstraintOid::new(oid)?,
                conname,
                conrelid: PgClassOid::new(conrelid)?,
                contype: char_to_contype(contype as u8 as char),
                conkey: conkey.unwrap_or_default(),
                confrelid: PgClassOid::new(confrelid),
                confkey: confkey.unwrap_or_default(),
            })
        })
        .collect())
}

fn char_to_contype(c: char) -> ConType {
    match c {
        'p' => ConType::PrimaryKey,
        'u' => ConType::Unique,
        'f' => ConType::ForeignKey,
        'c' => ConType::Check,
        'x' => ConType::Exclusion,
        _ => ConType::Other,
    }
}

fn export_inherits(client: &mut postgres::Client) -> Result<Vec<PgInherits>, postgres::Error> {
    let rows = client.query(
        "SELECT inhrelid, inhparent, inhseqno \
         FROM pg_catalog.pg_inherits \
         ORDER BY inhrelid, inhseqno",
        &[],
    )?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let inhrelid: u32 = r.get(0);
            let inhparent: u32 = r.get(1);
            Some(PgInherits {
                inhrelid: PgClassOid::new(inhrelid)?,
                inhparent: PgClassOid::new(inhparent)?,
                inhseqno: r.get(2),
            })
        })
        .collect())
}

fn export_depends(client: &mut postgres::Client) -> Result<Vec<PgDepend>, postgres::Error> {
    // Only export deptype = 'e' (extension) — the only type the analyzer
    // tracks in the seed. Normal/auto deps from the DDL pipeline are
    // re-derived when `apply_sql` runs against a clean catalog.
    let rows = client.query(
        "SELECT classid::int4, objid::int4, objsubid, refclassid::int4, refobjid::int4, \
                refobjsubid, deptype \
         FROM pg_catalog.pg_depend \
         WHERE deptype = 'e' \
         ORDER BY classid, objid, objsubid, refclassid, refobjid",
        &[],
    )?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let classid: i32 = r.get(0);
            let objid: i32 = r.get(1);
            let objsubid: i32 = r.get(2);
            let refclassid: i32 = r.get(3);
            let refobjid: i32 = r.get(4);
            let refobjsubid: i32 = r.get(5);
            let deptype: i8 = r.get(6);
            Some(PgDepend {
                classid: PgClassOid::new(classid as u32)?,
                objid: PgGenericOid::new(objid as u32)?,
                objsubid: objsubid as i16,
                refclassid: PgClassOid::new(refclassid as u32)?,
                refobjid: PgGenericOid::new(refobjid as u32)?,
                refobjsubid: refobjsubid as i16,
                deptype: char_to_deptype(deptype as u8 as char),
            })
        })
        .collect())
}

// ─── View definitions (second pass) ────────────────────────────────────────────

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

/// Re-apply each view's `CREATE VIEW` through the analyzer's DDL pipeline so
/// `PgClass.relviewdef` is populated with the resolved AST. Continues to use
/// `to_seed()`/`from_seed()` for round-trip.
///
/// Views that the analyzer can't yet handle (typically polymorphic /
/// information_schema oddities) are logged and skipped — `relviewdef` stays
/// empty for those, but the rest of the seed is still produced.
fn populate_view_defs(seed: PgCatalogSeed, defs: Vec<(String, String, String)>) -> PgCatalogSeed {
    let mut db = PgCatalog::from_seed(seed);
    let mut skipped = Vec::new();

    for (schema, name, definition) in &defs {
        let qn = QualifiedName::new(schema.clone(), name.clone());

        let before: Vec<(String, PgTypeOid)> = db
            .resolve_table(Some(schema), name)
            .map(|c| {
                db.attributes_of(c.oid)
                    .iter()
                    .map(|a| (a.attname.clone(), a.atttypid))
                    .collect()
            })
            .unwrap_or_default();

        let sql = format!(
            "CREATE OR REPLACE VIEW {qn} AS {body}",
            qn = qn,
            body = definition.trim_end().trim_end_matches(';'),
        );

        if let Err(err) = db.apply_sql(&sql) {
            log_view_failure(&qn, &err.to_string(), &sql);
            skipped.push(qn.to_string());
            continue;
        }

        let after: Vec<(String, PgTypeOid)> = db
            .resolve_table(Some(schema), name)
            .map(|c| {
                db.attributes_of(c.oid)
                    .iter()
                    .map(|a| (a.attname.clone(), a.atttypid))
                    .collect()
            })
            .unwrap_or_default();

        if let Some(drift) = describe_column_drift(&before, &after) {
            log_view_failure(&qn, &format!("column drift: {drift}"), &sql);
            skipped.push(qn.to_string());
        }
    }

    if !skipped.is_empty() {
        eprintln!(
            "Skipped {} view(s) that the analyzer couldn't reanalyze:",
            skipped.len()
        );
        for s in &skipped {
            eprintln!("  - {s}");
        }
    }

    db.to_seed()
}

fn log_view_failure(qn: &QualifiedName, error: &str, sql: &str) {
    eprintln!("--- {qn} ---");
    eprintln!("  error: {error}");
    eprintln!("  sql: {sql}");
}

fn describe_column_drift(
    before: &[(String, PgTypeOid)],
    after: &[(String, PgTypeOid)],
) -> Option<String> {
    if before.len() != after.len() {
        return Some(format!("column count {} → {}", before.len(), after.len()));
    }
    for (i, (b, a)) in before.iter().zip(after.iter()).enumerate() {
        if b.0 != a.0 {
            return Some(format!("column #{i} name {:?} → {:?}", b.0, a.0));
        }
        if b.1 != a.1 {
            return Some(format!(
                "column #{i} type_oid {} → {}",
                b.1.get(),
                a.1.get()
            ));
        }
    }
    None
}

// ─── Char → enum mapping ───────────────────────────────────────────────────────

fn char_to_typtype(c: char) -> TypType {
    match c {
        'b' => TypType::Base,
        'c' => TypType::Composite,
        'd' => TypType::Domain,
        'e' => TypType::Enum,
        'p' => TypType::Pseudo,
        'r' => TypType::Range,
        'm' => TypType::Multirange,
        _ => TypType::Base,
    }
}

fn char_to_typcategory(c: char) -> TypCategory {
    match c {
        'A' => TypCategory::Array,
        'B' => TypCategory::Boolean,
        'C' => TypCategory::Composite,
        'D' => TypCategory::DateTime,
        'E' => TypCategory::Enum,
        'G' => TypCategory::Geometric,
        'I' => TypCategory::Network,
        'N' => TypCategory::Numeric,
        'P' => TypCategory::Pseudo,
        'R' => TypCategory::Range,
        'S' => TypCategory::String,
        'T' => TypCategory::Timespan,
        'U' => TypCategory::UserDefined,
        'V' => TypCategory::BitString,
        'X' => TypCategory::Unknown,
        'Z' => TypCategory::Internal,
        _ => TypCategory::UserDefined,
    }
}

fn char_to_relkind(c: char) -> Option<RelKind> {
    Some(match c {
        'r' => RelKind::Table,
        'v' => RelKind::View,
        'm' => RelKind::MaterializedView,
        'p' => RelKind::Partitioned,
        'c' => RelKind::CompositeType,
        _ => return None,
    })
}

fn char_to_prokind(c: char) -> ProKind {
    match c {
        'a' => ProKind::Aggregate,
        'w' => ProKind::Window,
        'p' => ProKind::Procedure,
        _ => ProKind::Function,
    }
}

fn char_to_argmode(c: char) -> ArgMode {
    match c {
        'o' => ArgMode::Out,
        'b' => ArgMode::InOut,
        'v' => ArgMode::Variadic,
        't' => ArgMode::Table,
        _ => ArgMode::In,
    }
}

fn char_to_attgenerated(c: char) -> Option<AttGenerated> {
    match c {
        's' => Some(AttGenerated::Stored),
        'v' => Some(AttGenerated::Virtual),
        _ => None,
    }
}

fn char_to_attidentity(c: char) -> Option<AttIdentity> {
    match c {
        'a' => Some(AttIdentity::Always),
        'd' => Some(AttIdentity::ByDefault),
        _ => None,
    }
}

fn char_to_castcontext(c: char) -> CastContext {
    match c {
        'i' => CastContext::Implicit,
        'a' => CastContext::Assignment,
        _ => CastContext::Explicit,
    }
}

fn char_to_castmethod(c: char) -> CastMethod {
    match c {
        'b' => CastMethod::Binary,
        'i' => CastMethod::InOut,
        _ => CastMethod::Function,
    }
}

fn char_to_deptype(c: char) -> DepType {
    match c {
        'a' => DepType::Auto,
        'i' => DepType::Internal,
        'e' => DepType::Extension,
        'x' => DepType::AutoExtension,
        'p' => DepType::Pin,
        _ => DepType::Normal,
    }
}
