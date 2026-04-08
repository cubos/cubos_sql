//! Generates the `seed.json` file from a live PostgreSQL instance.
//!
//! Usage:
//!   cargo run -p cubos_sql_analyzer --example generate_seed -- <connection_string>
//!
//! Example:
//!   cargo run -p cubos_sql_analyzer --example generate_seed -- \
//!     "host=127.0.0.1 port=5432 user=postgres password=postgres dbname=postgres"
//!
//! The output is written to `cubos_sql_analyzer/src/seed.json`.

fn main() {
    let conn_str = std::env::args().nth(1).unwrap_or_else(|| {
        "host=127.0.0.1 port=5432 user=postgres password=postgres dbname=postgres".to_string()
    });

    eprintln!("Connecting to: {conn_str}");
    let mut client = postgres::Client::connect(&conn_str, postgres::NoTls)
        .expect("failed to connect to PostgreSQL");

    eprintln!("Exporting schema...");
    let snapshot =
        cubos_sql_analyzer::export::export_schema(&mut client).expect("failed to export schema");

    let json = serde_json::to_string(&snapshot).expect("failed to serialize snapshot");

    let out_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/seed.json");
    std::fs::write(&out_path, &json).expect("failed to write seed.json");

    let size_kb = json.len() / 1024;
    let num_types = snapshot.types.len();
    let num_tables = snapshot.tables.len();
    let num_functions: usize = snapshot.functions_by_name.values().map(|v| v.len()).sum();
    let num_operators: usize = snapshot.operators_by_name.values().map(|v| v.len()).sum();
    let num_casts = snapshot.casts.len();

    eprintln!("Wrote {out_path:?} ({size_kb} KB)");
    eprintln!("  types:     {num_types}");
    eprintln!("  tables:    {num_tables}");
    eprintln!("  functions: {num_functions}");
    eprintln!("  operators: {num_operators}");
    eprintln!("  casts:     {num_casts}");
}
