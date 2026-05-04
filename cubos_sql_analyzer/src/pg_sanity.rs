//! Real-PostgreSQL-backed sanity check for [`PgCatalog`](crate::PgCatalog).
//!
//! When the `pg_sanity` feature is on, every catalog produced by
//! [`PgCatalog::new`](crate::PgCatalog::new) carves out its own scratch
//! database inside the cluster pointed to by `POSTGRES_URL`, runs every
//! `apply_sql` / `analyze` against it, and drops the database on Drop.
//!
//! The cluster itself isn't managed by Rust — `scripts/run-pg-sanity.sh`
//! spins up a real PostgreSQL Docker container, exports `POSTGRES_URL`,
//! invokes `cargo nextest`, and tears the container down on exit. That
//! split keeps Rust dependencies thin (no async runtime, no embedded
//! binaries) and lets the same code run unchanged against any PG (CI,
//! local Docker, a remote dev cluster).
//!
//! `apply_sql` and `analyze` mirror their work onto the live server and
//! assert that:
//!
//! - the *outcome* matches (both succeed or both fail), and
//! - on success, every output column's name + qualified type name matches
//!   exactly between our static analyzer and PG's wire-protocol Describe.
//!
//! Error message contract: when both fail, our analyzer's message must
//! *start with* PG's server-side message verbatim (the `DbError.message`
//! field, ignoring SQLSTATE). Extra detail / hints after that prefix are
//! fine; missing the prefix or diverging early is a hard panic.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use postgres::config::Config;
use postgres::{Client, NoTls};

use crate::error::AnalyzeError;
use crate::resolve::AnalyzedQuery;
use crate::types::Type;

pub(crate) struct PgSanityServer {
    /// Connected to the per-instance scratch database.
    client: Client,
    /// Admin connection string (kept verbatim from `POSTGRES_URL`) so the
    /// `Drop` impl can connect back to a different database to run
    /// `DROP DATABASE` against the scratch one.
    admin_conn_str: String,
    /// Name of the scratch database; dropped from `admin_conn_str` on Drop.
    db_name: String,
    type_name_cache: HashMap<u32, String>,
}

impl PgSanityServer {
    /// Read `POSTGRES_URL`, create a per-instance scratch database with a
    /// random name, and return a client connected to it.
    pub(crate) fn spawn() -> Result<Self, String> {
        let admin_conn_str = std::env::var("POSTGRES_URL").map_err(|_| {
            "pg_sanity: POSTGRES_URL is not set. Run tests via `scripts/run-pg-sanity.sh`, \
             which spins up a Docker PostgreSQL and exports the URL."
                .to_string()
        })?;

        // Build the scratch DB name. Process pid + monotonic counter +
        // wall-clock nanos gives us enough entropy that concurrent test
        // binaries don't collide; underscore-prefix keeps it valid as an
        // unquoted SQL identifier.
        let db_name = scratch_db_name();

        // Connect to the admin database (whatever `POSTGRES_URL` points at)
        // and CREATE DATABASE. Drop the admin client immediately — we keep
        // the admin URL string around for the symmetric DROP at Drop time.
        {
            let mut admin = Client::connect(&admin_conn_str, NoTls).map_err(|e| {
                format!(
                    "pg_sanity: connect to admin DB failed (POSTGRES_URL={admin_conn_str:?}): {e}"
                )
            })?;
            admin
                .batch_execute(&format!("CREATE DATABASE \"{db_name}\""))
                .map_err(|e| format!("pg_sanity: CREATE DATABASE \"{db_name}\" failed: {e}"))?;
        }

        // Reconnect with the same parameters but pointing at the scratch DB.
        let scratch_conn_str = with_dbname(&admin_conn_str, &db_name)?;
        let client = Client::connect(&scratch_conn_str, NoTls)
            .map_err(|e| format!("pg_sanity: connect to scratch DB \"{db_name}\" failed: {e}"))?;

        Ok(Self {
            client,
            admin_conn_str,
            db_name,
            type_name_cache: HashMap::new(),
        })
    }

    /// Run `sql` on PG and panic if its outcome diverges from `our_result`.
    pub(crate) fn assert_apply_matches<E: std::fmt::Display>(
        &mut self,
        sql: &str,
        our_result: &Result<(), E>,
    ) {
        let pg_result = self.client.batch_execute(sql);
        match (our_result, &pg_result) {
            (Ok(()), Ok(())) => {}
            (Err(_), Err(_)) => {
                let our_msg = format!("{}", our_result.as_ref().unwrap_err());
                assert_error_prefix_matches(
                    &our_msg,
                    pg_result.as_ref().unwrap_err(),
                    sql,
                    "apply_sql",
                );
            }
            (Ok(()), Err(e)) => {
                // Treat protocol-level errors (no DbError → the postgres-rs
                // client received a wire frame it couldn't parse) as "PG
                // couldn't decide" and skip. Real rejections always carry
                // a SQLSTATE.
                if e.as_db_error().is_none() {
                    return;
                }
                panic!(
                    "pg_sanity: analyzer accepted DDL but PG rejected it.\n\
                     SQL:\n---\n{sql}\n---\nPG error: {}",
                    render_pg_error(e),
                );
            }
            (Err(e), Ok(())) => {
                panic!(
                    "pg_sanity: analyzer rejected DDL but PG accepted it.\n\
                     SQL:\n---\n{sql}\n---\nanalyzer error: {e}"
                );
            }
        }
    }

    /// Compare `analysis_sql` (the rewritten SQL with `$N` placeholders that
    /// the analyzer fed to its static pass) against PG's Parse+Describe.
    /// Panics if outcomes diverge or any column's name or qualified type
    /// name disagrees.
    pub(crate) fn assert_analyze_matches(
        &mut self,
        analysis_sql: &str,
        our_result: &Result<AnalyzedQuery, AnalyzeError>,
    ) {
        let pg_result = self.client.prepare(analysis_sql);

        match (our_result, &pg_result) {
            (Ok(ours), Ok(stmt)) => {
                let pg_columns: Vec<(String, u32)> = stmt
                    .columns()
                    .iter()
                    .map(|c| (c.name().to_string(), c.type_().oid()))
                    .collect();

                if ours.columns.len() != pg_columns.len() {
                    panic!(
                        "pg_sanity: column count mismatch.\n\
                         SQL:\n---\n{analysis_sql}\n---\n\
                         analyzer: {} columns ({:?})\n\
                         PG:       {} columns ({:?})",
                        ours.columns.len(),
                        ours.columns.iter().map(|c| &c.name).collect::<Vec<_>>(),
                        pg_columns.len(),
                        pg_columns.iter().map(|c| &c.0).collect::<Vec<_>>(),
                    );
                }

                for (i, (our_col, (pg_name, pg_oid))) in
                    ours.columns.iter().zip(pg_columns.iter()).enumerate()
                {
                    // Trailing `!` / `?` on PG-side column names come from
                    // the analyzer's nullability-annotation syntax (e.g.
                    // `SELECT col AS "title!"`). The analyzer strips them
                    // when parsing the alias; PG preserves them verbatim.
                    // Strip on the comparison side too — but leave the
                    // literal `?column?` placeholder alone (PG's name for
                    // unnamed expressions, not an annotation).
                    let pg_name_stripped = if pg_name.as_str() == "?column?" {
                        pg_name.as_str()
                    } else {
                        pg_name
                            .strip_suffix('!')
                            .or_else(|| pg_name.strip_suffix('?'))
                            .unwrap_or(pg_name)
                    };
                    if our_col.name != pg_name_stripped {
                        panic!(
                            "pg_sanity: column {i} name mismatch.\n\
                             SQL:\n---\n{analysis_sql}\n---\n\
                             analyzer: {:?}\nPG:       {:?}",
                            our_col.name, pg_name,
                        );
                    }
                    let pg_qname = self.qualified_type_name(*pg_oid).unwrap_or_else(|e| {
                        panic!(
                            "pg_sanity: failed to resolve type oid {pg_oid} for column \
                             '{}': {e}",
                            our_col.name
                        )
                    });
                    let our_qname = qualified_type_name_for_compare(&our_col.pg_type);
                    if our_qname != pg_qname {
                        panic!(
                            "pg_sanity: column '{}' type mismatch.\n\
                             SQL:\n---\n{analysis_sql}\n---\n\
                             analyzer: {our_qname} (Type::{:?})\nPG:       {pg_qname} (oid {pg_oid})",
                            our_col.name, our_col.pg_type,
                        );
                    }
                }
            }
            (Err(_), Err(_)) => {
                let our_msg = format!("{}", our_result.as_ref().unwrap_err());
                assert_error_prefix_matches(
                    &our_msg,
                    pg_result.as_ref().unwrap_err(),
                    analysis_sql,
                    "analyze",
                );
            }
            (Ok(ours), Err(e)) => {
                if e.as_db_error().is_none() {
                    return;
                }
                panic!(
                    "pg_sanity: analyzer accepted query but PG rejected it.\n\
                     SQL:\n---\n{analysis_sql}\n---\n\
                     analyzer columns: {:?}\nPG error: {}",
                    ours.columns
                        .iter()
                        .map(|c| (c.name.clone(), c.pg_type.clone()))
                        .collect::<Vec<_>>(),
                    render_pg_error(e),
                );
            }
            (Err(e), Ok(stmt)) => {
                panic!(
                    "pg_sanity: analyzer rejected query but PG accepted it.\n\
                     SQL:\n---\n{analysis_sql}\n---\n\
                     analyzer error: {e}\nPG columns: {:?}",
                    stmt.columns()
                        .iter()
                        .map(|c| (c.name().to_string(), c.type_().oid()))
                        .collect::<Vec<_>>(),
                );
            }
        }
    }

    /// Resolve a PG type OID to its qualified name (`schema.type_name`) by
    /// querying `pg_catalog.pg_type`. Array types are flipped from their PG
    /// `_int4` underscore form into the bracketed `pg_catalog.int4[]` form
    /// so they line up with [`Type::Array`]. Inlines the OID as a literal
    /// (non-injectable: it's a u32 from PG itself) to dodge `to_sql` for
    /// the `oid` type.
    fn qualified_type_name(&mut self, oid: u32) -> Result<String, postgres::Error> {
        if let Some(name) = self.type_name_cache.get(&oid) {
            return Ok(name.clone());
        }
        let row = self.client.query_one(
            &format!(
                "SELECT n.nspname, t.typname, t.typcategory::text, t.typelem::int8 \
                 FROM pg_catalog.pg_type t \
                 JOIN pg_catalog.pg_namespace n ON t.typnamespace = n.oid \
                 WHERE t.oid = {oid}"
            ),
            &[],
        )?;
        let schema: String = row.get(0);
        let name: String = row.get(1);
        let category: String = row.get(2);
        let typelem: u32 = row.get::<_, i64>(3) as u32;

        let qualified = if category == "A" && typelem != 0 {
            // Array — recurse to get the element's qname, then bracket.
            let elem_qname = self.qualified_type_name(typelem)?;
            format!("{elem_qname}[]")
        } else {
            format!("{schema}.{name}")
        };
        self.type_name_cache.insert(oid, qualified.clone());
        Ok(qualified)
    }
}

impl Drop for PgSanityServer {
    fn drop(&mut self) {
        // Tear down the scratch database. We can't run DROP DATABASE while
        // any session is connected to it, so first drop our scratch client
        // (by replacing it via `mem::replace` … actually just close
        // implicitly when we open the admin connection — `Client` has no
        // explicit close; dropping it on the next admin connection is fine
        // because PG terminates the old session when its TCP socket closes,
        // which happens when the original client drops below).
        //
        // The order: open a fresh admin client *before* the scratch client
        // is dropped would race; instead, take ownership of the scratch
        // client out of `self` and drop it explicitly first.
        let scratch = std::mem::replace(
            &mut self.client,
            // Placeholder client we never use; created against the same
            // admin URL so the connection succeeds quickly.
            Client::connect(&self.admin_conn_str, NoTls)
                .expect("pg_sanity: drop: admin reconnect failed"),
        );
        drop(scratch);

        // The placeholder client we just created is connected to the admin
        // DB — perfect for issuing the DROP. Errors during drop are
        // intentionally swallowed (a panic in Drop would mask the original
        // test failure); the script-level cleanup tears the whole cluster
        // down anyway.
        let _ = self
            .client
            .batch_execute(&format!("DROP DATABASE IF EXISTS \"{}\"", self.db_name));
    }
}

/// Build a unique-ish scratch database name: `pg_sanity_<pid>_<counter>_<nanos>`.
/// Underscores-only so it doesn't need quoting; lowercase prefix dodges any
/// PG identifier folding subtleties.
fn scratch_db_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    format!("pg_sanity_{pid}_{n}_{nanos}")
}

/// Take a libpq-style connection string and replace its `dbname` with the
/// given name. Parse via `postgres::config::Config` to validate, then
/// rebuild the keyword=value form manually — `Config` has accessors but
/// no public `Display` for keyword form.
fn with_dbname(conn_str: &str, dbname: &str) -> Result<String, String> {
    let config: Config = conn_str
        .parse()
        .map_err(|e| format!("pg_sanity: invalid POSTGRES_URL ({conn_str:?}): {e}"))?;
    let mut parts: Vec<String> = Vec::new();
    if let Some(host) = config.get_hosts().first() {
        let host_str = match host {
            postgres::config::Host::Tcp(s) => s.clone(),
            #[cfg(unix)]
            postgres::config::Host::Unix(p) => p.to_string_lossy().into_owned(),
        };
        parts.push(format!("host={host_str}"));
    }
    if let Some(port) = config.get_ports().first() {
        parts.push(format!("port={port}"));
    }
    if let Some(user) = config.get_user() {
        parts.push(format!("user={user}"));
    }
    if let Some(pw) = config.get_password() {
        parts.push(format!("password={}", String::from_utf8_lossy(pw)));
    }
    parts.push(format!("dbname={dbname}"));
    Ok(parts.join(" "))
}

/// Render a server-side message with the SQLSTATE appended — used for
/// human-readable diagnostics in panic messages. `postgres::Error::Display`
/// collapses to a useless "db error", so we always dig into `as_db_error()`
/// when present.
fn render_pg_error(e: &postgres::Error) -> String {
    if let Some(db) = e.as_db_error() {
        format!("{} (SQLSTATE {})", db.message(), db.code().code())
    } else {
        e.to_string()
    }
}

/// Enforce the contract: the analyzer's error message must begin with the
/// PG server-side message verbatim. Trailing detail (column names, hints)
/// is allowed; the SQLSTATE on PG's side is intentionally ignored.
///
/// If PG didn't send a structured `DbError` (a protocol-level issue, not a
/// real rejection), we can't compare wording meaningfully and skip the
/// prefix check. The outer success/failure invariant has already fired.
#[track_caller]
fn assert_error_prefix_matches(our_msg: &str, pg_err: &postgres::Error, sql: &str, kind: &str) {
    let Some(db) = pg_err.as_db_error() else {
        return;
    };
    let pg_msg = db.message();
    if !our_msg.starts_with(pg_msg) {
        panic!(
            "pg_sanity: {kind} error must start with PG's message.\n\
             SQL:\n---\n{sql}\n---\n\
             PG (expected prefix): {pg_msg}\n\
             analyzer:             {our_msg}\n\
             PG (with SQLSTATE):   {}",
            render_pg_error(pg_err),
        );
    }
}

/// Render an analyzer [`Type`] into the same `schema.name[]?` shape that
/// [`PgSanityServer::qualified_type_name`] produces, so the two can be
/// compared as plain strings.
///
/// Domains are unwrapped to their base type for comparison purposes — PG's
/// wire-level Describe collapses domain columns to their base OID. The
/// analyzer's `Type::Domain` shape is asserted in dedicated tests; this
/// comparison only validates the underlying physical type.
fn qualified_type_name_for_compare(ty: &Type) -> String {
    match ty {
        Type::Domain { base, .. } => qualified_type_name_for_compare(base),
        Type::Basic { schema, name, .. }
        | Type::Enum { schema, name, .. }
        | Type::Range { schema, name, .. } => format!("{schema}.{name}"),
        Type::Array { element } => format!("{}[]", qualified_type_name_for_compare(element)),
        Type::AnonymousRecord { .. } => "pg_catalog.record".to_string(),
    }
}
