//! Nullability propagation through JOINs and expressions.

use std::collections::HashSet;

/// Tracks which table aliases are on the nullable side of an outer JOIN,
/// and whether the current SELECT has a GROUP BY clause.
#[derive(Debug, Clone, Default)]
pub(crate) struct NullabilityContext {
    /// Aliases that are on the nullable side of some JOIN.
    nullable_aliases: HashSet<String>,
    /// Whether the current SELECT has a GROUP BY clause.
    /// When true, each group has ≥1 row, so aggregates with NOT NULL inputs
    /// produce NOT NULL results.
    pub has_group_by: bool,
}

impl NullabilityContext {
    /// Mark all aliases from a list as nullable (used for FULL JOIN).
    pub fn mark_all_nullable(&mut self, aliases: &[String]) {
        for a in aliases {
            self.nullable_aliases.insert(a.clone());
        }
    }

    /// Check if a column is nullable, considering both the column's base
    /// definition and whether its source table is on a nullable JOIN side.
    pub fn is_nullable(&self, table_alias: &str, base_not_null: bool) -> bool {
        if self.nullable_aliases.contains(table_alias) {
            // Table is on nullable side of JOIN → column is always nullable.
            true
        } else {
            // Column nullability comes from table definition.
            !base_not_null
        }
    }
}

/// Collect all table aliases from a scope source list.
pub(crate) fn collect_aliases(sources: &[crate::scope::TableSource]) -> Vec<String> {
    sources.iter().map(|s| s.alias.clone()).collect()
}
