//! Scope tracking for table aliases, columns, and CTEs.

use crate::error::AnalyzeError;
use crate::qualified_name::QualifiedName;
use crate::schema::{SchemaSnapshot, TableColumn};

/// A resolved column with its type and base nullability (from table definition).
#[derive(Debug, Clone)]
pub(crate) struct ScopeColumn {
    pub name: String,
    pub type_oid: u32,
    /// NOT NULL from the table definition (before JOIN effects).
    pub base_not_null: bool,
    /// The alias of the table this column belongs to.
    pub table_alias: String,
    /// Named-field structure when the column holds a record value: SRF /
    /// OUT-arg functions populate this from `out_args`, ROW constructors fill
    /// it from the inferred shape, subqueries propagate it through. Lets
    /// `(alias.col).field` resolve through to the field's real type without
    /// needing a registered composite OID.
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

/// Tracks all table sources visible in the current query scope.
///
/// `sources` holds the local FROM-clause aliases. `outer_sources` mirrors
/// PG's correlated-reference fallback: a subquery's column lookup tries the
/// local sources first, and only falls back to outer sources when the local
/// search fails (so an inner `FROM users` shadowing an outer `users` works
/// the same way it does in PG).
#[derive(Debug, Clone, Default)]
pub(crate) struct Scope {
    pub sources: Vec<TableSource>,
    pub outer_sources: Vec<TableSource>,
}

impl Scope {
    /// Add a table from the schema snapshot.
    pub fn add_table(
        &mut self,
        snapshot: &SchemaSnapshot,
        schema: Option<&str>,
        name: &str,
        alias: &str,
    ) -> Result<(), AnalyzeError> {
        let table = snapshot.resolve_table(schema, name).ok_or_else(|| {
            AnalyzeError::UndefinedTable(format!("relation \"{name}\" does not exist"))
        })?;

        let columns = table
            .columns
            .iter()
            .map(|c| ScopeColumn {
                name: c.name.clone(),
                type_oid: c.type_oid,
                base_not_null: c.not_null,
                table_alias: alias.to_owned(),
                record_fields: None,
            })
            .collect();

        self.sources.push(TableSource {
            alias: alias.to_owned(),
            columns,
            source_qn: Some(QualifiedName::new(&table.schema, &table.name)),
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

    /// Add columns from a DML target table (for RETURNING), recording the
    /// relation's qualified name so `alias.*` expressions resolve to its
    /// composite type OID.
    pub fn add_dml_target(&mut self, alias: &str, qn: QualifiedName, columns: &[TableColumn]) {
        let cols = columns
            .iter()
            .map(|c| ScopeColumn {
                name: c.name.clone(),
                type_oid: c.type_oid,
                base_not_null: c.not_null,
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

    /// Look up a `TableSource` by its alias (user alias or generated relname).
    /// Tries local sources first, then outer (correlated) sources.
    pub fn find_source(&self, alias: &str) -> Option<&TableSource> {
        self.sources
            .iter()
            .chain(self.outer_sources.iter())
            .find(|s| s.alias == alias)
    }

    /// Resolve a column reference. If `table` is Some, look only in that alias.
    /// Otherwise search all sources (error if ambiguous). Outer (correlated)
    /// sources are only consulted when the local lookup turns up nothing —
    /// this matches PG's lexical rule that inner `FROM` aliases shadow outer
    /// ones with the same name.
    pub fn resolve_column(
        &self,
        table: Option<&str>,
        column: &str,
    ) -> Result<&ScopeColumn, AnalyzeError> {
        if let Some(t) = table {
            for source in self.sources.iter().chain(self.outer_sources.iter()) {
                if source.alias == t
                    && let Some(col) = source.columns.iter().find(|c| c.name == column)
                {
                    return Ok(col);
                }
            }
            return Err(AnalyzeError::UndefinedColumn(format!(
                "column \"{t}.{column}\" does not exist"
            )));
        }

        // Try local sources first, then outer (correlated) sources. Within
        // each tier we still detect ambiguous matches.
        for tier in [&self.sources, &self.outer_sources] {
            let mut found: Option<&ScopeColumn> = None;
            for source in tier {
                if let Some(col) = source.columns.iter().find(|c| c.name == column) {
                    if found.is_some() {
                        return Err(AnalyzeError::UndefinedColumn(format!(
                            "column reference \"{column}\" is ambiguous"
                        )));
                    }
                    found = Some(col);
                }
            }
            if let Some(col) = found {
                return Ok(col);
            }
        }
        Err(AnalyzeError::UndefinedColumn(format!(
            "column \"{column}\" does not exist"
        )))
    }

    /// Get all columns (for SELECT *) from the local FROM clause only —
    /// outer correlated sources don't expand under `*`.
    pub fn all_columns(&self) -> Vec<&ScopeColumn> {
        self.sources.iter().flat_map(|s| s.columns.iter()).collect()
    }
}
