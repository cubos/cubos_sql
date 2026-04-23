//! The [`PgCatalog`] type: a mutable in-memory PostgreSQL schema snapshot.
//!
//! `PgCatalog` starts from the embedded PostgreSQL 18 seed catalog and evolves
//! by applying DDL statements via [`PgCatalog::apply_sql`]. It is the single
//! entry point for schema construction in the public API.

use std::collections::HashMap;

use crate::ddl::{DdlError, InstalledExtension, apply_sql_to};
use crate::error::AnalyzeError;
use crate::lexer::lex;
use crate::resolve::{AnalyzedQuery, analyze_static, build_spread_sample_sql, fuse};
use crate::schema::SchemaSnapshot;
use crate::seed::load_seed;

/// A mutable in-memory schema. Applies DDL statements on top of a seed
/// catalog and keeps the snapshot updated as each statement is processed.
#[derive(Clone)]
pub struct PgCatalog {
    pub(crate) snapshot: SchemaSnapshot,
    pub(crate) next_oid: u32,
    pub(crate) installed_extensions: HashMap<String, InstalledExtension>,
}

/// Starting OID for user-defined objects. Well above PG system OIDs (~16384).
pub(crate) const USER_OID_START: u32 = 100_000;

impl Default for PgCatalog {
    fn default() -> Self {
        Self::new()
    }
}

impl PgCatalog {
    /// Create a new catalog seeded with the PostgreSQL 18 built-in catalog.
    pub fn new() -> Self {
        Self {
            snapshot: load_seed(),
            next_oid: USER_OID_START,
            installed_extensions: HashMap::new(),
        }
    }

    /// Parse and apply all DDL statements in a SQL string, updating the schema
    /// in-place.
    pub fn apply_sql(&mut self, sql: &str) -> Result<(), DdlError> {
        apply_sql_to(self, sql)
    }

    /// Analyze a SQL query template against this catalog.
    ///
    /// Lexes `sql` to extract named parameters (`$name`), spreads (`$..name`),
    /// and nullability annotations (`$foo?`, `$foo!`); rewrites the SQL with
    /// positional placeholders; infers parameter and output column types; and
    /// returns everything combined in an [`AnalyzedQuery`].
    pub fn analyze(&self, sql: &str) -> Result<AnalyzedQuery, AnalyzeError> {
        let lex_output = lex(sql)?;

        // Collect explicit nullability annotations from the lexer, ordered by
        // positional parameter index (regular params first, then spread fields).
        let mut param_nullability: Vec<Option<bool>> =
            lex_output.params.iter().map(|p| p.nullable).collect();
        for spread in &lex_output.spreads {
            if let Some(fields) = &spread.fields {
                param_nullability.extend(
                    fields
                        .iter()
                        .map(|f| if f.nullable { Some(true) } else { None }),
                );
            }
        }

        // When the query has spreads, run analysis on a sample SQL where each
        // spread is materialized as a single row of placeholders, so the
        // analyzer can infer the field types from surrounding context.
        let analysis_sql = if lex_output.spreads.is_empty() {
            lex_output.sql.clone()
        } else {
            build_spread_sample_sql(&lex_output)
        };

        let (columns, mut info_params) =
            analyze_static(&self.snapshot, &analysis_sql, &param_nullability)?;

        // Merge explicit $foo? / $foo! annotations from the lexer on top of
        // the analyzer's inferred nullability (explicit always wins).
        for (pi, &lexer_nullable) in info_params.iter_mut().zip(param_nullability.iter()) {
            if let Some(explicit) = lexer_nullable {
                pi.nullable = explicit;
            }
        }

        Ok(fuse(lex_output, columns, info_params))
    }

    // ── Internal access for tests and the `internal` feature ────────────────

    /// Build a [`PgCatalog`] from an existing [`SchemaSnapshot`] (e.g. one
    /// restored from serialized JSON). Extension state is discarded.
    #[cfg(any(test, feature = "internal"))]
    pub fn from_snapshot(snapshot: SchemaSnapshot) -> Self {
        Self {
            snapshot,
            next_oid: USER_OID_START,
            installed_extensions: HashMap::new(),
        }
    }

    /// Consume the catalog and return its internal [`SchemaSnapshot`].
    #[cfg(any(test, feature = "internal"))]
    pub fn into_snapshot(self) -> SchemaSnapshot {
        self.snapshot
    }

    /// Borrow the catalog's internal [`SchemaSnapshot`].
    #[cfg(any(test, feature = "internal"))]
    pub fn snapshot(&self) -> &SchemaSnapshot {
        &self.snapshot
    }

    /// Mutably borrow the catalog's internal [`SchemaSnapshot`]. Exposed
    /// for tests that need to simulate legacy/partial snapshot states.
    #[cfg(any(test, feature = "internal"))]
    pub fn snapshot_mut(&mut self) -> &mut SchemaSnapshot {
        &mut self.snapshot
    }

    // ── Internal helpers used by the DDL submodules ─────────────────────────

    pub(crate) fn alloc_oid(&mut self) -> u32 {
        let oid = self.next_oid;
        self.next_oid += 1;
        oid
    }
}
