//! Scope tracking for table aliases, columns, and CTEs.

use crate::error::{AnalyzeError, RawError, SourceSpan};
use crate::oid::PgTypeOid;
use crate::pg_catalog::{PgAttribute, PgCatalog};
use crate::qualified_name::QualifiedName;
use crate::suggest::suggest_similar;

/// A resolved column with its type and base nullability (from table definition).
#[derive(Debug, Clone)]
pub(crate) struct ScopeColumn {
    pub name: String,
    pub type_oid: PgTypeOid,
    /// NOT NULL from the table definition (before JOIN effects).
    pub base_not_null: bool,
    /// `pg_attribute.atttypmod`-shaped modifier (`varchar(n)` length, etc.),
    /// optionally inherited from the column's type chain (e.g. a domain over
    /// `varchar(20)`). `None` matches PG's `-1`.
    pub typmod: Option<i32>,
    /// `pg_attribute.attcollation` of the source column, if any. Threaded
    /// through `infer_column_ref` into `ExprType.collation`.
    pub collation: Option<crate::oid::PgCollationOid>,
    /// The alias of the table this column belongs to.
    pub table_alias: String,
    /// Named-field structure when the column holds a record value: SRF /
    /// OUT-arg functions populate this from `out_args`, ROW constructors fill
    /// it from the inferred shape, subqueries propagate it through.
    pub record_fields: Option<Vec<crate::expr::RecordField>>,
}

/// A table-like source in the FROM clause.
#[derive(Debug, Clone)]
pub(crate) struct TableSource {
    pub alias: String,
    pub columns: Vec<ScopeColumn>,
    /// Qualified name of the backing relation, or `None` for derived sources
    /// (CTE, subquery). Set for real tables/views so that `alias.*` in an
    /// expression context can look up the composite type of the relation.
    pub source_qn: Option<QualifiedName>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Scope {
    pub sources: Vec<TableSource>,
    pub outer_sources: Vec<TableSource>,
    /// Aliases that exist in the enclosing scope but are *not* visible here
    /// — same shape PG uses for non-LATERAL subqueries: a reference like
    /// `t.col` against a `t` in this list produces the diagnostic `invalid
    /// reference to FROM-clause entry for table "t"` instead of the generic
    /// `column "t.col" does not exist`. Never consulted for resolution.
    pub shadowed_sources: Vec<TableSource>,
}

/// Build the public-facing `UndefinedTable` error for a missing relation,
/// picking up the snippet location from TLS and computing a "did you mean"
/// hint against the catalog's visible relations.
///
/// Used by `Scope::add_table` and by the DML statement handlers in
/// `resolve.rs` (INSERT/UPDATE/DELETE/MERGE) so the error rendering is
/// consistent across all sites.
pub(crate) fn undefined_table_error(
    snapshot: &PgCatalog,
    schema: Option<&str>,
    name: &str,
    span: Option<SourceSpan>,
) -> AnalyzeError {
    let hint = suggest_similar(name, snapshot.visible_relnames(schema))
        .map(|c| format!("did you mean \"{c}\"?"));
    RawError::undefined_table(name, span, hint).finalize_implicit()
}

/// Build an `UndefinedColumn` error for a DML target column (INSERT col
/// list, UPDATE SET col). Uses the table's attributes for the suggestion
/// rather than a `Scope` (no scope exists yet during DML target validation).
pub(crate) fn undefined_dml_column_error(
    column: &str,
    table_relname: &str,
    table_attrs: &[crate::pg_catalog::PgAttribute],
    span: Option<SourceSpan>,
) -> AnalyzeError {
    let hint = suggest_similar(column, table_attrs.iter().map(|a| a.attname.as_str()))
        .map(|c| format!("did you mean \"{c}\"?"));
    RawError::undefined_column(
        format!("column \"{column}\" of relation \"{table_relname}\" does not exist"),
        span,
        hint,
    )
    .finalize_implicit()
}

/// Build the public-facing `UndefinedColumn` error.
///
/// `message` is the PG-verbatim first line — callers pick the wording
/// matching PG's behavior (bare column, qualified column, ambiguous, or
/// "invalid reference to FROM-clause entry"). `column` is the bare column
/// name used for the "did you mean" suggestion; `scope` is searched for
/// candidate column names.
pub(crate) fn undefined_column_error(
    scope: &Scope,
    column: &str,
    message: String,
    span: Option<SourceSpan>,
) -> AnalyzeError {
    let candidates: Vec<&str> = scope
        .all_columns()
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    let hint = suggest_similar(column, candidates.iter().copied())
        .map(|c| format!("did you mean \"{c}\"?"));
    RawError::undefined_column(message, span, hint).finalize_implicit()
}

impl Scope {
    /// Add a table from the catalog.
    ///
    /// `span` covers the relation reference in the original SQL — usually
    /// produced by `SourceSpan::from_node_qname(RangeVar.location)`. Pass
    /// `None` when no AST location is available; the resulting
    /// `UndefinedTable` error then carries no snippet (but `did you mean`
    /// still works).
    pub fn add_table(
        &mut self,
        snapshot: &PgCatalog,
        schema: Option<&str>,
        name: &str,
        alias: &str,
        span: Option<SourceSpan>,
    ) -> Result<(), AnalyzeError> {
        let table = snapshot
            .resolve_table(schema, name)
            .ok_or_else(|| undefined_table_error(snapshot, schema, name, span))?;
        let table_oid = table.oid;
        let nspname = snapshot
            .namespace_name(table.relnamespace)
            .map(str::to_owned)
            .unwrap_or_else(|| "public".to_owned());
        let relname = table.relname.clone();

        let columns: Vec<ScopeColumn> = snapshot
            .attributes_of(table_oid)
            .iter()
            .map(|c| ScopeColumn {
                name: c.attname.clone(),
                type_oid: c.atttypid,
                base_not_null: c.attnotnull || snapshot.type_is_not_null(c.atttypid),
                typmod: snapshot.effective_typmod(c.atttypid, c.atttypmod),
                collation: c.attcollation,
                table_alias: alias.to_owned(),
                record_fields: None,
            })
            .collect();

        self.sources.push(TableSource {
            alias: alias.to_owned(),
            columns,
            source_qn: Some(QualifiedName::new(nspname, relname)),
        });
        Ok(())
    }

    /// Add a virtual table (CTE, subquery result).
    pub fn add_virtual_table(&mut self, alias: &str, columns: Vec<ScopeColumn>) {
        self.sources.push(TableSource {
            alias: alias.to_owned(),
            columns,
            source_qn: None,
        });
    }

    /// Add columns from a DML target table (for RETURNING).
    pub fn add_dml_target(
        &mut self,
        snapshot: &PgCatalog,
        alias: &str,
        qn: QualifiedName,
        columns: &[PgAttribute],
    ) {
        let cols = columns
            .iter()
            .map(|c| ScopeColumn {
                name: c.attname.clone(),
                type_oid: c.atttypid,
                base_not_null: c.attnotnull || snapshot.type_is_not_null(c.atttypid),
                typmod: snapshot.effective_typmod(c.atttypid, c.atttypmod),
                collation: c.attcollation,
                table_alias: alias.to_owned(),
                record_fields: None,
            })
            .collect();
        self.sources.push(TableSource {
            alias: alias.to_owned(),
            columns: cols,
            source_qn: Some(qn),
        });
    }

    pub fn find_source(&self, alias: &str) -> Option<&TableSource> {
        self.sources
            .iter()
            .chain(self.outer_sources.iter())
            .find(|s| s.alias == alias)
    }

    pub fn resolve_column(
        &self,
        table: Option<&str>,
        column: &str,
        span: Option<SourceSpan>,
    ) -> Result<&ScopeColumn, AnalyzeError> {
        if let Some(t) = table {
            for source in self.sources.iter().chain(self.outer_sources.iter()) {
                if source.alias == t
                    && let Some(col) = source.columns.iter().find(|c| c.name == column)
                {
                    return Ok(col);
                }
            }
            // Match PG's wording for the non-LATERAL outer-reference case:
            // when `t` is visible in the enclosing FROM but not here, point
            // at the FROM-clause-entry visibility rule rather than the
            // generic missing-column message. The hint mirrors PG's HINT.
            if self.shadowed_sources.iter().any(|s| s.alias == t) {
                return Err(undefined_column_error(
                    self,
                    column,
                    format!("invalid reference to FROM-clause entry for table \"{t}\""),
                    span,
                ));
            }
            // PG formats qualified missing columns through identifier
            // quoting rules (`column t.col does not exist`, but
            // `column "T".col does not exist` when `T` needs quoting).
            // Bare names use the simple `"col"` form just below.
            return Err(undefined_column_error(
                self,
                column,
                format!("column {} does not exist", QualifiedName::new(t, column)),
                span,
            ));
        }

        for tier in [&self.sources, &self.outer_sources] {
            let mut matches: Vec<&ScopeColumn> = Vec::new();
            for source in tier {
                if let Some(col) = source.columns.iter().find(|c| c.name == column) {
                    matches.push(col);
                }
            }
            if matches.len() > 1 {
                let candidates = matches
                    .iter()
                    .map(|c| QualifiedName::new(&c.table_alias, column).to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(undefined_column_error(
                    self,
                    column,
                    format!("column reference \"{column}\" is ambiguous (could be: {candidates})"),
                    span,
                ));
            }
            if let Some(col) = matches.first() {
                return Ok(col);
            }
        }
        Err(undefined_column_error(
            self,
            column,
            format!("column \"{column}\" does not exist"),
            span,
        ))
    }

    pub fn all_columns(&self) -> Vec<&ScopeColumn> {
        self.sources.iter().flat_map(|s| s.columns.iter()).collect()
    }
}
