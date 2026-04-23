//! Static seed loading.
//!
//! The seed is a pre-exported [`SchemaSnapshot`] from a clean PostgreSQL 18
//! instance containing all built-in types, functions, operators, and casts.
//! It is embedded at compile time via `include_str!`.

use crate::schema::SchemaSnapshot;

const SEED_JSON: &str = include_str!("seed.json");

/// Load the embedded seed snapshot (clean PostgreSQL 18 catalog).
pub(crate) fn load_seed() -> SchemaSnapshot {
    serde_json::from_str(SEED_JSON).expect("embedded seed.json is invalid")
}
