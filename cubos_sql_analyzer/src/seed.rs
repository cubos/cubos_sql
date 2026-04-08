//! Static seed loading and schema-from-migrations builder.
//!
//! The seed is a pre-exported [`SchemaSnapshot`] from a clean PostgreSQL 18
//! instance containing all built-in types, functions, operators, and casts.
//! It is embedded at compile time via `include_str!`.

use crate::ddl::{DdlError, DdlInterpreter};
use crate::schema::SchemaSnapshot;

const SEED_JSON: &str = include_str!("seed.json");

/// Load the embedded seed snapshot (clean PostgreSQL 18 catalog).
pub fn load_seed() -> SchemaSnapshot {
    serde_json::from_str(SEED_JSON).expect("embedded seed.json is invalid")
}

/// Build a [`SchemaSnapshot`] by applying DDL migrations on top of the seed.
///
/// `migrations` is a sorted list of `(filename, sql_content)` pairs.
/// Returns the final snapshot after all migrations are applied.
///
/// Views are resolved at creation time (matching PostgreSQL behavior), with
/// `SELECT *` expanded and column types fixed at that point. Dependency
/// tracking enables proper CASCADE behavior for ALTER TABLE / DROP TABLE.
pub fn build_schema_from_migrations(
    migrations: &[(String, String)],
) -> Result<(SchemaSnapshot, Vec<DdlWarning>), DdlError> {
    let seed = load_seed();
    let mut interp = DdlInterpreter::new(seed);
    for (filename, sql) in migrations {
        interp.apply_sql(sql).map_err(|e| DdlError::Migration {
            filename: filename.clone(),
            source: Box::new(e),
        })?;
    }
    let warnings = interp.take_warnings();
    Ok((interp.into_snapshot(), warnings))
}

/// A non-fatal warning from DDL interpretation.
#[derive(Debug, Clone)]
pub struct DdlWarning {
    pub message: String,
}

impl std::fmt::Display for DdlWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
