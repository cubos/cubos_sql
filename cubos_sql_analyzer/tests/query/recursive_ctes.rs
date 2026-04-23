//! Recursive CTEs — **coverage gap**.
//!
//! TODO: `WITH RECURSIVE t AS (seed UNION ALL recursive) SELECT …`,
//! type unification between seed and recursive branches, nullability of
//! the recursive column, `SEARCH DEPTH FIRST / BREADTH FIRST`,
//! `CYCLE` clause.
