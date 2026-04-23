//! Error types for the SQL analyzer.

use thiserror::Error;

use crate::lexer::LexError;

/// Errors that can occur during static SQL analysis.
#[derive(Debug, Error)]
pub enum AnalyzeError {
    /// The SQL could not be lexed (unclosed string, comment, etc.).
    #[error("SQL lex error: {0}")]
    Lex(String),

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

    /// A type mismatch: an expression's type cannot be coerced to the expected type.
    #[error("type mismatch: {actual} cannot be coerced to {expected} ({context})")]
    TypeMismatch {
        actual: String,
        expected: String,
        context: String,
    },

    /// The analyzer encountered an AST node or SQL feature it does not yet support.
    #[error("unsupported SQL feature: {0}")]
    Unsupported(String),

    /// The parser reported a JOIN kind the analyzer does not recognize.
    /// Returned instead of silently falling back to INNER JOIN semantics,
    /// which would produce incorrect nullability.
    #[error("unsupported join type: {0}")]
    UnsupportedJoinType(i32),

    /// JSON serialization/deserialization error.
    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),

    /// IO error (reading/writing snapshot files).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<LexError> for AnalyzeError {
    fn from(err: LexError) -> Self {
        AnalyzeError::Lex(err.to_string())
    }
}
