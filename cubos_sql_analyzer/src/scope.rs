//! Scope tracking for table aliases, columns, and CTEs.

use crate::error::AnalyzeError;
use crate::oid::PgTypeOid;
use crate::pg_catalog::{PgAttribute, PgCatalog};
use crate::qualified_name::QualifiedName;

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

impl Scope {
    /// Add a table from the catalog.
    pub fn add_table(
        &mut self,
        snapshot: &PgCatalog,
        schema: Option<&str>,
        name: &str,
        alias: &str,
    ) -> Result<(), AnalyzeError> {
        let table = snapshot.resolve_table(schema, name).ok_or_else(|| {
            AnalyzeError::UndefinedTable(format!("relation \"{name}\" does not exist"))
        })?;
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
                return Err(AnalyzeError::UndefinedColumn(format!(
                    "invalid reference to FROM-clause entry for table \"{t}\""
                )));
            }
            return Err(AnalyzeError::UndefinedColumn(format!(
                "column \"{t}.{column}\" does not exist"
            )));
        }

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

    pub fn all_columns(&self) -> Vec<&ScopeColumn> {
        self.sources.iter().flat_map(|s| s.columns.iter()).collect()
    }
}
