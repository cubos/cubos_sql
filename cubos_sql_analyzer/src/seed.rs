//! Static seed loading.
//!
//! The seed is a pre-exported snapshot of a clean PostgreSQL 18 instance —
//! every built-in row from `pg_namespace`, `pg_type`, `pg_class`,
//! `pg_attribute`, `pg_proc`, `pg_operator`, `pg_cast`, `pg_extension`, and
//! the supporting `pg_enum`/`pg_range`/`pg_aggregate`/`pg_depend` tables. It
//! is embedded at compile time via `include_str!` and deserialized into a
//! [`PgCatalogSeed`] DTO before being moved into a fresh [`crate::PgCatalog`].

use crate::pg_catalog::PgCatalogSeed;

const SEED_JSON: &str = include_str!("seed.json");

/// Load the embedded seed (clean PostgreSQL 18 catalog).
pub(crate) fn load_seed() -> PgCatalogSeed {
    serde_json::from_str(SEED_JSON).expect("embedded seed.json is invalid")
}
