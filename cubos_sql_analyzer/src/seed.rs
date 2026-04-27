//! Static seed loading.
//!
//! The seed is a pre-exported snapshot of a clean PostgreSQL 18 instance
//! containing all built-in types, functions, operators, and casts. It is
//! embedded at compile time via `include_str!` and deserialized into a
//! [`SchemaSeed`] DTO before being moved into a fresh [`crate::PgCatalog`].

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::qualified_name::QualifiedName;
use crate::schema::{CastInfo, FunctionEntry, OperatorEntry, TableEntry, TypeEntry};

const SEED_JSON: &str = include_str!("seed.json");

/// On-disk shape of `seed.json`. Holds the eight schema-level fields the
/// embedded PG18 catalog dump produces, but none of the runtime-only state
/// ([`crate::PgCatalog::next_oid`], `installed_extensions`).
///
/// `Serialize` is kept so `to_seed()`/`from_seed()` can round-trip a live
/// catalog through JSON in tests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaSeed {
    pub types: HashMap<u32, TypeEntry>,
    pub type_by_name: HashMap<QualifiedName, u32>,
    pub tables: HashMap<QualifiedName, TableEntry>,
    pub functions_by_name: HashMap<QualifiedName, Vec<FunctionEntry>>,
    pub operators_by_name: HashMap<QualifiedName, Vec<OperatorEntry>>,
    pub casts: HashMap<String, CastInfo>,
    pub search_path: Vec<String>,
    #[serde(default)]
    pub schemas: HashSet<String>,
}

/// Load the embedded seed (clean PostgreSQL 18 catalog).
pub(crate) fn load_seed() -> SchemaSeed {
    let mut seed: SchemaSeed =
        serde_json::from_str(SEED_JSON).expect("embedded seed.json is invalid");
    apply_default_arg_overrides(&mut seed);
    seed
}

/// Patch in `num_default_args` for pg_catalog functions that ship with
/// `pronargdefaults > 0` in PG. The seed exporter does not yet emit this
/// field, so we keep a small allowlist here. Each entry is
/// `(schema, name, total_arg_count, default_count)`.
fn apply_default_arg_overrides(seed: &mut SchemaSeed) {
    const OVERRIDES: &[(&str, &str, usize, u8)] = &[
        // jsonb_set / jsonb_insert: trailing bool with default true/false.
        ("pg_catalog", "jsonb_set", 4, 1),
        // jsonb_set_lax: trailing (bool, text) — 2 defaults.
        ("pg_catalog", "jsonb_set_lax", 5, 2),
        ("pg_catalog", "jsonb_insert", 4, 1),
    ];

    for &(schema, name, total, defaults) in OVERRIDES {
        let key = QualifiedName::new(schema, name);
        if let Some(entries) = seed.functions_by_name.get_mut(&key) {
            for f in entries.iter_mut() {
                if f.arg_types.len() == total {
                    f.num_default_args = defaults;
                }
            }
        }
    }
}
