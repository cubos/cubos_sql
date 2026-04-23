//! Aggregate `FILTER` clause, `WITHIN GROUP`, ordered-set aggregates —
//! **coverage gap**.
//!
//! TODO: `agg(x) FILTER (WHERE cond)`, `array_agg(x ORDER BY y)`,
//! `string_agg(x, sep ORDER BY y)`, `percentile_cont`/`percentile_disc`
//! WITHIN GROUP, nullability interaction with FILTER eliminating all rows.
