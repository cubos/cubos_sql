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
    /// When this column holds a `record` produced by an SRF / OUT-arg
    /// function, the named output columns live here so that downstream
    /// `(alias.col).field` can resolve through to the field's real type.
    pub record_fields: Option<Vec<crate::schema::CompositeField>>,
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
    pub fn find_source(&self, alias: &str) -> Option<&TableSource> {
        self.sources.iter().find(|s| s.alias == alias)
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
            return Err(AnalyzeError::UndefinedColumn(format!(
                "column \"{t}.{column}\" does not exist"
            )));
        }

        let mut found: Option<&ScopeColumn> = None;
        for source in &self.sources {
            if let Some(col) = source.columns.iter().find(|c| c.name == column) {
                if found.is_some() {
                    return Err(AnalyzeError::UndefinedColumn(format!(
                        "column reference \"{column}\" is ambiguous"
                    )));
                }
                found = Some(col);
            }
        }
        found.ok_or_else(|| {
            AnalyzeError::UndefinedColumn(format!("column \"{column}\" does not exist"))
        })
    }

    /// Get all columns (for SELECT *).
    pub fn all_columns(&self) -> Vec<&ScopeColumn> {
        self.sources.iter().flat_map(|s| s.columns.iter()).collect()
    }
}
