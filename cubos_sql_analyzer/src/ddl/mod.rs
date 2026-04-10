//! DDL interpreter: applies DDL statements to a [`SchemaSnapshot`] in memory.
//!
//! This module parses SQL migration files using `pg_query` and mutates the
//! snapshot as if the DDL had been executed against a real PostgreSQL instance.

pub mod aggregates;
pub mod alter;
pub mod drop;
pub mod extensions;
pub mod functions;
pub mod operators;
pub mod schema_stmt;
pub mod tables;
pub mod types;
pub mod util;
pub mod views;

use std::collections::HashMap;

use pg_query::protobuf::node;

use crate::schema::SchemaSnapshot;
use crate::seed::DdlWarning;

// ─── Error type ─────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum DdlError {
    Parse(String),
    Migration {
        filename: String,
        source: Box<DdlError>,
    },
    UnsupportedDdl(String),
    TypeNotFound(String),
    TableNotFound(String),
    DuplicateObject(String),
    ExtensionError(String),
    DependencyError(String),
}

impl std::fmt::Display for DdlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DdlError::Parse(msg) => write!(f, "SQL parse error: {msg}"),
            DdlError::Migration { filename, source } => {
                write!(f, "in migration '{filename}': {source}")
            }
            DdlError::UnsupportedDdl(msg) => write!(f, "unsupported DDL: {msg}"),
            DdlError::TypeNotFound(name) => write!(f, "type not found: {name}"),
            DdlError::TableNotFound(name) => write!(f, "table not found: {name}"),
            DdlError::DuplicateObject(name) => write!(f, "duplicate object: {name}"),
            DdlError::ExtensionError(msg) => write!(f, "extension error: {msg}"),
            DdlError::DependencyError(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for DdlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DdlError::Migration { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

// ─── Installed extension state ──────────────────────────────────────────────

/// Tracks an installed extension's version, schema, and created objects.
#[derive(Debug, Clone)]
pub struct InstalledExtension {
    pub version: String,
    pub schema: String,
    /// OIDs of types created by this extension.
    pub type_oids: Vec<u32>,
    /// Names of functions created by this extension (for cleanup).
    pub function_names: Vec<String>,
    /// Keys of casts created by this extension.
    pub cast_keys: Vec<String>,
}

// ─── DdlInterpreter ─────────────────────────────────────────────────────────

/// Interprets DDL statements against a mutable [`SchemaSnapshot`].
pub struct DdlInterpreter {
    pub snapshot: SchemaSnapshot,
    next_oid: u32,
    warnings: Vec<DdlWarning>,
    /// Extensions installed during DDL interpretation, keyed by name.
    pub installed_extensions: HashMap<String, InstalledExtension>,
}

/// Starting OID for user-defined objects. Well above PG system OIDs (~16384).
const USER_OID_START: u32 = 100_000;

impl DdlInterpreter {
    /// Create a new interpreter starting from a seed snapshot.
    pub fn new(seed: SchemaSnapshot) -> Self {
        Self {
            snapshot: seed,
            next_oid: USER_OID_START,
            warnings: Vec::new(),
            installed_extensions: HashMap::new(),
        }
    }

    /// Allocate a fresh OID.
    pub fn alloc_oid(&mut self) -> u32 {
        let oid = self.next_oid;
        self.next_oid += 1;
        oid
    }

    /// Add a non-fatal warning.
    pub fn warn(&mut self, msg: impl Into<String>) {
        self.warnings.push(DdlWarning {
            message: msg.into(),
        });
    }

    /// Take collected warnings.
    pub fn take_warnings(&mut self) -> Vec<DdlWarning> {
        std::mem::take(&mut self.warnings)
    }

    /// Consume the interpreter and return the final snapshot.
    pub fn into_snapshot(self) -> SchemaSnapshot {
        self.snapshot
    }

    /// Parse and apply all DDL statements in a SQL string.
    pub fn apply_sql(&mut self, sql: &str) -> Result<(), DdlError> {
        let parsed = pg_query::parse(sql).map_err(|e| DdlError::Parse(e.to_string()))?;

        for raw_stmt in &parsed.protobuf.stmts {
            let Some(stmt) = raw_stmt.stmt.as_ref().and_then(|n| n.node.as_ref()) else {
                continue;
            };
            self.apply_statement(stmt)?;
        }

        Ok(())
    }

    /// Dispatch a single parsed statement.
    pub(crate) fn apply_statement(&mut self, stmt: &node::Node) -> Result<(), DdlError> {
        match stmt {
            // ── Tables ──────────────────────────────────────────────────
            node::Node::CreateStmt(s) => tables::create_table(self, s),
            node::Node::AlterTableStmt(s) => tables::alter_table(self, s),

            // ── Types ───────────────────────────────────────────────────
            node::Node::CreateDomainStmt(s) => types::create_domain(self, s),
            node::Node::CreateEnumStmt(s) => types::create_enum(self, s),
            node::Node::CompositeTypeStmt(s) => types::create_composite(self, s),
            node::Node::CreateRangeStmt(s) => types::create_range(self, s),
            node::Node::AlterEnumStmt(s) => types::alter_enum(self, s),

            // ── Drop ────────────────────────────────────────────────────
            node::Node::DropStmt(s) => drop::drop_objects(self, s),

            // ── Schema ──────────────────────────────────────────────────
            node::Node::CreateSchemaStmt(s) => schema_stmt::create_schema(self, s),

            // ── Functions ───────────────────────────────────────────────
            node::Node::CreateFunctionStmt(s) => functions::create_function(self, s),

            // ── Views ───────────────────────────────────────────────────
            node::Node::ViewStmt(s) => views::create_view(self, s),
            node::Node::CreateTableAsStmt(s) => views::create_table_as(self, s),

            // ── Extensions ──────────────────────────────────────────────
            node::Node::CreateExtensionStmt(s) => extensions::create_extension(self, s),
            node::Node::AlterExtensionStmt(s) => extensions::alter_extension(self, s),

            // ── Type definitions (CREATE TYPE name (...)) and casts ─────
            node::Node::DefineStmt(s) => {
                use pg_query::protobuf::ObjectType;
                match ObjectType::try_from(s.kind).unwrap_or(ObjectType::Undefined) {
                    ObjectType::ObjectType => types::define_type(self, s),
                    ObjectType::ObjectOperator => operators::define_operator(self, s),
                    ObjectType::ObjectAggregate => aggregates::define_aggregate(self, s),
                    // Other DefineStmt kinds (collation, text search, etc.)
                    // are irrelevant for static type analysis.
                    _ => Ok(()),
                }
            }
            node::Node::CreateCastStmt(s) => types::create_cast(self, s),

            // ── ALTER ... RENAME / SET SCHEMA ───────────────────────────
            node::Node::RenameStmt(s) => alter::rename(self, s),
            node::Node::AlterObjectSchemaStmt(s) => alter::set_schema(self, s),

            // ── No-ops (irrelevant for type analysis) ───────────────────
            node::Node::IndexStmt(_)
            | node::Node::CreateSeqStmt(_)
            | node::Node::AlterSeqStmt(_)
            | node::Node::GrantStmt(_)
            | node::Node::CommentStmt(_)
            | node::Node::CreateTrigStmt(_)
            | node::Node::RuleStmt(_)
            | node::Node::ConstraintsSetStmt(_)
            | node::Node::CreatePolicyStmt(_)
            | node::Node::AlterPolicyStmt(_)
            | node::Node::AlterOwnerStmt(_)
            | node::Node::AlterDefaultPrivilegesStmt(_)
            | node::Node::CreateRoleStmt(_)
            | node::Node::AlterRoleStmt(_)
            | node::Node::GrantRoleStmt(_)
            | node::Node::CreateOpClassStmt(_)
            | node::Node::AlterOpFamilyStmt(_)
            | node::Node::AlterOperatorStmt(_)
            | node::Node::SelectStmt(_)
            | node::Node::InsertStmt(_)
            | node::Node::UpdateStmt(_)
            | node::Node::DeleteStmt(_)
            | node::Node::TransactionStmt(_)
            | node::Node::DoStmt(_)
            | node::Node::TruncateStmt(_)
            | node::Node::CopyStmt(_)
            | node::Node::ClusterStmt(_)
            | node::Node::VacuumStmt(_)
            | node::Node::ReindexStmt(_)
            | node::Node::LockStmt(_)
            | node::Node::VariableSetStmt(_)
            | node::Node::VariableShowStmt(_)
            | node::Node::DiscardStmt(_)
            | node::Node::ExplainStmt(_)
            | node::Node::AlterFunctionStmt(_)
            | node::Node::AlterDomainStmt(_)
            | node::Node::NotifyStmt(_)
            | node::Node::ListenStmt(_)
            | node::Node::UnlistenStmt(_)
            | node::Node::AlterExtensionContentsStmt(_)
            | node::Node::CreateAmStmt(_) => Ok(()),

            // ── Unknown DDL — warn and continue ─────────────────────────
            other => {
                self.warn(format!("ignoring unsupported DDL: {other:?}"));
                Ok(())
            }
        }
    }
}
