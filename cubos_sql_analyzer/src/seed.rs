//! Static seed loading.
//!
//! The seed is a pre-exported [`SchemaSnapshot`] from a clean PostgreSQL 18
//! instance containing all built-in types, functions, operators, and casts.
//! It is embedded at compile time via `include_str!`.

use crate::qualified_name::QualifiedName;
use crate::schema::SchemaSnapshot;

const SEED_JSON: &str = include_str!("seed.json");

/// Load the embedded seed snapshot (clean PostgreSQL 18 catalog).
pub(crate) fn load_seed() -> SchemaSnapshot {
    let mut snap: SchemaSnapshot =
        serde_json::from_str(SEED_JSON).expect("embedded seed.json is invalid");
    apply_default_arg_overrides(&mut snap);
    snap
}

/// Patch in `num_default_args` for pg_catalog functions that ship with
/// `pronargdefaults > 0` in PG. The seed exporter does not yet emit this
/// field, so we keep a small allowlist here. Each entry is
/// `(schema, name, total_arg_count, default_count)`.
fn apply_default_arg_overrides(snap: &mut SchemaSnapshot) {
    const OVERRIDES: &[(&str, &str, usize, u8)] = &[
        // jsonb_set / jsonb_insert: trailing bool with default true/false.
        ("pg_catalog", "jsonb_set", 4, 1),
        // jsonb_set_lax: trailing (bool, text) — 2 defaults.
        ("pg_catalog", "jsonb_set_lax", 5, 2),
        ("pg_catalog", "jsonb_insert", 4, 1),
    ];

    for &(schema, name, total, defaults) in OVERRIDES {
        let key = QualifiedName::new(schema, name);
        if let Some(entries) = snap.functions_by_name.get_mut(&key) {
            for f in entries.iter_mut() {
                if f.arg_types.len() == total {
                    f.num_default_args = defaults;
                }
            }
        }
    }
}
