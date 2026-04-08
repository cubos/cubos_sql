//! Schema snapshot data model.
//!
//! A [`SchemaSnapshot`] captures the complete type system of a PostgreSQL
//! database at a point in time (post-migration). It is exported once from
//! a live database and then used for offline static analysis.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ──────────────────────────────────────────────────────────────────────────────
// Top-level snapshot
// ──────────────────────────────────────────────────────────────────────────────

/// Complete snapshot of a PostgreSQL schema sufficient for static type inference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaSnapshot {
    /// All types indexed by OID.
    pub types: HashMap<u32, TypeEntry>,
    /// Type name lookup: `"schema.name" → OID`.
    pub type_by_name: HashMap<String, u32>,
    /// All tables and views indexed by OID.
    pub tables: HashMap<u32, TableEntry>,
    /// Table/view name lookup: `"schema.name" → OID`.
    pub table_by_name: HashMap<String, u32>,
    /// Functions indexed by name for fast lookup.
    pub functions_by_name: HashMap<String, Vec<FunctionEntry>>,
    /// Operators indexed by name for fast lookup.
    pub operators_by_name: HashMap<String, Vec<OperatorEntry>>,
    /// Cast rules: `"source_oid:target_oid"` → context for O(1) lookup.
    pub casts: HashMap<String, CastContext>,
    /// Current `search_path` schemas, in order.
    pub search_path: Vec<String>,
}

// ──────────────────────────────────────────────────────────────────────────────
// Types
// ──────────────────────────────────────────────────────────────────────────────

/// A PostgreSQL type from `pg_type`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeEntry {
    pub oid: u32,
    pub name: String,
    pub schema: String,
    pub kind: TypeKind,
}

/// The category of a PostgreSQL type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TypeKind {
    /// A built-in or extension base type (int4, text, uuid, etc.).
    Base,
    /// A domain type wrapping another type.
    Domain { base_type_oid: u32 },
    /// An array type.
    Array { element_type_oid: u32 },
    /// An enum type with its labels in order.
    Enum { labels: Vec<String> },
    /// A composite (row) type with named fields.
    Composite { fields: Vec<CompositeField> },
    /// A range type over a subtype.
    Range { subtype_oid: u32 },
    /// A pseudo-type (void, trigger, record, any, etc.).
    Pseudo,
}

/// A field in a composite type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositeField {
    pub name: String,
    pub type_oid: u32,
    pub not_null: bool,
}

// ──────────────────────────────────────────────────────────────────────────────
// Tables and views
// ──────────────────────────────────────────────────────────────────────────────

/// A table or view from `pg_class`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableEntry {
    pub oid: u32,
    pub name: String,
    pub schema: String,
    pub kind: RelationKind,
    pub columns: Vec<TableColumn>,
    /// For views: the definition needed to recreate the view after CASCADE.
    /// Contains the deparsed SQL query and column dependencies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_def: Option<ViewDef>,
}

/// View dependency tracking for CASCADE behavior.
///
/// Views never get automatically recreated — if a dependent column is altered
/// or dropped with CASCADE, the view is simply dropped (matching PostgreSQL).
/// The user must DROP + CREATE VIEW manually in their migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewDef {
    /// Tables this view depends on (by OID).
    pub depends_on_tables: Vec<u32>,
    /// Specific columns this view depends on: `(table_oid, column_name)`.
    pub depends_on_columns: Vec<(u32, String)>,
}

/// The kind of relation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelationKind {
    Table,
    View,
    MaterializedView,
    Partitioned,
}

/// A column in a table or view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableColumn {
    pub name: String,
    pub type_oid: u32,
    pub not_null: bool,
    pub has_default: bool,
    pub attnum: i16,
}

// ──────────────────────────────────────────────────────────────────────────────
// Functions
// ──────────────────────────────────────────────────────────────────────────────

/// A function or aggregate from `pg_proc`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionEntry {
    pub oid: u32,
    pub name: String,
    pub schema: String,
    pub arg_types: Vec<u32>,
    pub return_type_oid: u32,
    pub is_aggregate: bool,
    pub is_window: bool,
    pub is_variadic: bool,
    pub is_set_returning: bool,
    /// For strict functions: returns NULL if any input is NULL.
    pub is_strict: bool,
    /// For aggregates: the actual return type (may differ from prorettype
    /// when a finalfn transforms the transition state).
    pub agg_final_type_oid: Option<u32>,
}

// ──────────────────────────────────────────────────────────────────────────────
// Operators
// ──────────────────────────────────────────────────────────────────────────────

/// An operator from `pg_operator`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorEntry {
    pub name: String,
    /// Left operand type OID. `None` for prefix operators.
    pub left_type_oid: Option<u32>,
    /// Right operand type OID.
    pub right_type_oid: u32,
    /// Result type OID.
    pub result_type_oid: u32,
}

// ──────────────────────────────────────────────────────────────────────────────
// Casts
// ──────────────────────────────────────────────────────────────────────────────

/// A type cast rule from `pg_cast`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CastEntry {
    pub source_type_oid: u32,
    pub target_type_oid: u32,
    pub context: CastContext,
}

/// When a cast is allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CastContext {
    /// Cast is applied implicitly (e.g., int4 → int8 in expressions).
    Implicit,
    /// Cast is applied in assignment context (e.g., INSERT target column).
    Assignment,
    /// Cast requires explicit `::type` or `CAST(... AS type)`.
    Explicit,
}

// ──────────────────────────────────────────────────────────────────────────────
// Lookup methods
// ──────────────────────────────────────────────────────────────────────────────

impl SchemaSnapshot {
    /// Look up a table or view by name, searching the `search_path`.
    pub fn resolve_table(&self, schema: Option<&str>, name: &str) -> Option<&TableEntry> {
        if let Some(s) = schema {
            let key = format!("{s}.{name}");
            return self
                .table_by_name
                .get(&key)
                .and_then(|oid| self.tables.get(oid));
        }
        for s in &self.search_path {
            let key = format!("{s}.{name}");
            if let Some(oid) = self.table_by_name.get(&key)
                && let Some(entry) = self.tables.get(oid)
            {
                return Some(entry);
            }
        }
        None
    }

    /// Look up a type by name, searching the `search_path`.
    pub fn resolve_type_by_name(&self, schema: Option<&str>, name: &str) -> Option<&TypeEntry> {
        if let Some(s) = schema {
            let key = format!("{s}.{name}");
            return self
                .type_by_name
                .get(&key)
                .and_then(|oid| self.types.get(oid));
        }
        for s in &self.search_path {
            let key = format!("{s}.{name}");
            if let Some(oid) = self.type_by_name.get(&key)
                && let Some(entry) = self.types.get(oid)
            {
                return Some(entry);
            }
        }
        // Also check pg_catalog (builtins).
        let key = format!("pg_catalog.{name}");
        self.type_by_name
            .get(&key)
            .and_then(|oid| self.types.get(oid))
    }

    /// Look up a type by OID.
    pub fn get_type(&self, oid: u32) -> Option<&TypeEntry> {
        self.types.get(&oid)
    }

    /// Unwrap domains to find the base type OID.
    pub fn unwrap_domain(&self, oid: u32) -> u32 {
        let mut current = oid;
        for _ in 0..32 {
            match self.types.get(&current) {
                Some(TypeEntry {
                    kind: TypeKind::Domain { base_type_oid },
                    ..
                }) => current = *base_type_oid,
                _ => break,
            }
        }
        current
    }

    /// Check if an implicit cast exists from `source` to `target`.
    pub fn has_implicit_cast(&self, source: u32, target: u32) -> bool {
        if source == target {
            return true;
        }
        let key = format!("{source}:{target}");
        matches!(self.casts.get(&key), Some(CastContext::Implicit))
    }

    /// Find all functions matching a name, searching the `search_path`.
    pub fn find_functions(&self, schema: Option<&str>, name: &str) -> Vec<&FunctionEntry> {
        let Some(candidates) = self.functions_by_name.get(name) else {
            return Vec::new();
        };
        candidates
            .iter()
            .filter(|f| match schema {
                Some(s) => f.schema == s,
                None => self.search_path.contains(&f.schema) || f.schema == "pg_catalog",
            })
            .collect()
    }

    /// Find operators matching name and operand types.
    pub fn find_operator(
        &self,
        name: &str,
        left_oid: Option<u32>,
        right_oid: u32,
    ) -> Option<&OperatorEntry> {
        let candidates = self.operators_by_name.get(name)?;
        // Exact match first.
        if let Some(op) = candidates
            .iter()
            .find(|o| o.left_type_oid == left_oid && o.right_type_oid == right_oid)
        {
            return Some(op);
        }
        // Try with implicit casts on operands.
        candidates.iter().find(|o| {
            let left_ok = match (o.left_type_oid, left_oid) {
                (Some(expected), Some(actual)) => self.has_implicit_cast(actual, expected),
                (None, None) => true,
                _ => false,
            };
            left_ok && self.has_implicit_cast(right_oid, o.right_type_oid)
        })
    }
}
