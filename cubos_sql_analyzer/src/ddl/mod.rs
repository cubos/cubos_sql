//! DDL interpreter: applies DDL statements to a [`Database`](crate::Database)
//! in memory.
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

use pg_query::protobuf::node;

use crate::database::Database;

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
    ViewAnalysis {
        view: String,
        source: Box<crate::error::AnalyzeError>,
    },
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
            DdlError::ViewAnalysis { view, source } => {
                write!(f, "cannot analyze view '{view}': {source}")
            }
        }
    }
}

impl std::error::Error for DdlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DdlError::Migration { source, .. } => Some(source.as_ref()),
            DdlError::ViewAnalysis { source, .. } => Some(source.as_ref()),
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
    /// Schema-qualified names of functions created by this extension
    /// (for cleanup on DROP EXTENSION).
    pub function_names: Vec<crate::qualified_name::QualifiedName>,
    /// Keys of casts created by this extension.
    pub cast_keys: Vec<String>,
}

// ─── Dispatcher ─────────────────────────────────────────────────────────────

/// Parse and apply all DDL statements in a SQL string.
pub(crate) fn apply_sql_to(db: &mut Database, sql: &str) -> Result<(), DdlError> {
    let parsed = pg_query::parse(sql).map_err(|e| DdlError::Parse(e.to_string()))?;

    for raw_stmt in &parsed.protobuf.stmts {
        let Some(stmt) = raw_stmt.stmt.as_ref().and_then(|n| n.node.as_ref()) else {
            continue;
        };
        apply_statement(db, stmt)?;
    }

    Ok(())
}

/// Dispatch a single parsed statement.
pub(crate) fn apply_statement(db: &mut Database, stmt: &node::Node) -> Result<(), DdlError> {
    match stmt {
        // ── Tables ──────────────────────────────────────────────────
        node::Node::CreateStmt(s) => tables::create_table(db, s),
        node::Node::AlterTableStmt(s) => tables::alter_table(db, s),

        // ── Types ───────────────────────────────────────────────────
        node::Node::CreateDomainStmt(s) => types::create_domain(db, s),
        node::Node::CreateEnumStmt(s) => types::create_enum(db, s),
        node::Node::CompositeTypeStmt(s) => types::create_composite(db, s),
        node::Node::CreateRangeStmt(s) => types::create_range(db, s),
        node::Node::AlterEnumStmt(s) => types::alter_enum(db, s),

        // ── Drop ────────────────────────────────────────────────────
        node::Node::DropStmt(s) => drop::drop_objects(db, s),

        // ── Schema ──────────────────────────────────────────────────
        node::Node::CreateSchemaStmt(s) => schema_stmt::create_schema(db, s),

        // ── Functions ───────────────────────────────────────────────
        node::Node::CreateFunctionStmt(s) => functions::create_function(db, s),

        // ── Views ───────────────────────────────────────────────────
        node::Node::ViewStmt(s) => views::create_view(db, s),
        node::Node::CreateTableAsStmt(s) => views::create_table_as(db, s),

        // ── Extensions ──────────────────────────────────────────────
        node::Node::CreateExtensionStmt(s) => extensions::create_extension(db, s),
        node::Node::AlterExtensionStmt(s) => extensions::alter_extension(db, s),

        // ── Type definitions (CREATE TYPE name (...)) and casts ─────
        node::Node::DefineStmt(s) => {
            use pg_query::protobuf::ObjectType;
            match ObjectType::try_from(s.kind).unwrap_or(ObjectType::Undefined) {
                ObjectType::ObjectType => types::define_type(db, s),
                ObjectType::ObjectOperator => operators::define_operator(db, s),
                ObjectType::ObjectAggregate => aggregates::define_aggregate(db, s),
                // Other DefineStmt kinds (collation, text search, etc.)
                // are irrelevant for static type analysis.
                _ => Ok(()),
            }
        }
        node::Node::CreateCastStmt(s) => types::create_cast(db, s),

        // ── ALTER ... RENAME / SET SCHEMA ───────────────────────────
        node::Node::RenameStmt(s) => alter::rename(db, s),
        node::Node::AlterObjectSchemaStmt(s) => alter::set_schema(db, s),

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

        // ── Unknown DDL — surface as an error ───────────────────────
        other => Err(DdlError::UnsupportedDdl(format!("{other:?}"))),
    }
}
