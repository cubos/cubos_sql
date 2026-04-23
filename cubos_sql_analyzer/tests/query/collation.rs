//! COLLATE clauses — **coverage gap**.
//!
//! TODO: `expr COLLATE "en_US"` in ORDER BY / WHERE / expressions, column
//! default collation, `CREATE COLLATION`, interaction with string
//! comparison operators. Today the analyzer doesn't model collations at all.
