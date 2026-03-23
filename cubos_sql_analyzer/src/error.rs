//! Error types for the SQL analyzer.

use thiserror::Error;

/// Errors that can occur during static SQL analysis.
#[derive(Debug, Error)]
pub enum AnalyzeError {
    /// The SQL could not be parsed.
    #[error("SQL parse error: {0}")]
    Parse(String),

    /// A table or view referenced in the query was not found in the schema snapshot.
    #[error("unknown relation: {0}")]
    UnknownRelation(String),

    /// A column referenced in the query was not found in scope.
    #[error("unknown column: {0}")]
    UnknownColumn(String),

    /// A type OID was not found in the schema snapshot or type map.
    #[error("unknown type OID {oid} for {context}")]
    UnknownType { oid: u32, context: String },

    /// A function could not be resolved (not found or ambiguous overload).
    #[error("cannot resolve function: {0}")]
    UnresolvedFunction(String),

    /// An operator could not be resolved for the given operand types.
    #[error("cannot resolve operator: {0}")]
    UnresolvedOperator(String),

    /// The analyzer encountered an AST node or SQL feature it does not yet support.
    #[error("unsupported SQL feature: {0}")]
    Unsupported(String),

    /// A PostgreSQL error during schema export.
    #[error("postgres error: {0}")]
    Postgres(#[from] postgres::Error),

    /// JSON serialization/deserialization error.
    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),

    /// IO error (reading/writing snapshot files).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
