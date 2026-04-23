//! Scope tracking for table aliases, columns, and CTEs.

use crate::error::AnalyzeError;
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
}

/// A table-like source in the FROM clause.
#[derive(Debug, Clone)]
pub(crate) struct TableSource {
    pub alias: String,
    pub columns: Vec<ScopeColumn>,
}

/// Tracks all table sources visible in the current query scope.
#[derive(Debug, Clone, Default)]
pub(crate) struct Scope {
    pub sources: Vec<TableSource>,
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
        let table = snapshot
            .resolve_table(schema, name)
            .ok_or_else(|| AnalyzeError::UnknownRelation(name.to_owned()))?;

        let columns = table
            .columns
            .iter()
            .map(|c| ScopeColumn {
                name: c.name.clone(),
                type_oid: c.type_oid,
                base_not_null: c.not_null,
                table_alias: alias.to_owned(),
            })
            .collect();

        self.sources.push(TableSource {
            alias: alias.to_owned(),
            columns,
        });
        Ok(())
    }

    /// Add a virtual table (CTE, subquery result).
    pub fn add_virtual_table(&mut self, alias: &str, columns: Vec<ScopeColumn>) {
        self.sources.push(TableSource {
            alias: alias.to_owned(),
            columns,
        });
    }

    /// Add columns from a DML target table (for RETURNING).
    pub fn add_table_columns(&mut self, alias: &str, columns: &[TableColumn]) {
        let cols = columns
            .iter()
            .map(|c| ScopeColumn {
                name: c.name.clone(),
                type_oid: c.type_oid,
                base_not_null: c.not_null,
                table_alias: alias.to_owned(),
            })
            .collect();
        self.sources.push(TableSource {
            alias: alias.to_owned(),
            columns: cols,
        });
    }

    /// Resolve a column reference. If `table` is Some, look only in that alias.
    /// Otherwise search all sources (error if ambiguous).
    pub fn resolve_column(
        &self,
        table: Option<&str>,
        column: &str,
    ) -> Result<&ScopeColumn, AnalyzeError> {
        if let Some(t) = table {
            for source in &self.sources {
                if source.alias == t
                    && let Some(col) = source.columns.iter().find(|c| c.name == column)
                {
                    return Ok(col);
                }
            }
            return Err(AnalyzeError::UnknownColumn(format!("{t}.{column}")));
        }

        let mut found: Option<&ScopeColumn> = None;
        for source in &self.sources {
            if let Some(col) = source.columns.iter().find(|c| c.name == column) {
                if found.is_some() {
                    return Err(AnalyzeError::UnknownColumn(format!(
                        "ambiguous column: {column}"
                    )));
                }
                found = Some(col);
            }
        }
        found.ok_or_else(|| AnalyzeError::UnknownColumn(column.to_owned()))
    }

    /// Get all columns (for SELECT *).
    pub fn all_columns(&self) -> Vec<&ScopeColumn> {
        self.sources.iter().flat_map(|s| s.columns.iter()).collect()
    }
}
