//! Error types for the SQL analyzer.
//!
//! Variant names mirror PostgreSQL error categories (SQLSTATE class 42):
//! `UndefinedTable` = 42P01, `UndefinedColumn` = 42703, `UndefinedFunction` /
//! `UndefinedOperator` = 42883, `IndeterminateType` = 42P18, and so on. This
//! lets consumers map our errors to PG error codes without an extra translation
//! table, and helps us stay honest about what each variant actually represents.

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

    /// A table or view referenced in the query was not found in the schema
    /// snapshot. Equivalent to PG `undefined_table` (SQLSTATE 42P01).
    #[error("{0}")]
    UndefinedTable(String),

    /// A column referenced in the query was not found in scope. Equivalent to
    /// PG `undefined_column` (SQLSTATE 42703).
    #[error("{0}")]
    UndefinedColumn(String),

    /// A type OID was not found in the schema snapshot or type map.
    #[error("unknown type OID {oid} for {context}")]
    UndefinedType { oid: u32, context: String },

    /// A function does not exist for the given argument types. Equivalent to
    /// PG `undefined_function` (SQLSTATE 42883). In PG the same SQLSTATE covers
    /// missing operators; here we keep operators in their own variant for
    /// clarity.
    #[error("{0}")]
    UndefinedFunction(String),

    /// An operator does not exist for the given operand types. Shares PG
    /// SQLSTATE 42883 with `UndefinedFunction`.
    #[error("{0}")]
    UndefinedOperator(String),

    /// The type of an expression could not be determined — typically a bare
    /// parameter with no context, or an operator with UNKNOWN on both sides
    /// that resolves ambiguously. Equivalent to PG `indeterminate_datatype`
    /// (SQLSTATE 42P18).
    #[error("{0}")]
    IndeterminateType(String),

    /// A type mismatch: an expression's type cannot be coerced to the expected
    /// type. Equivalent to PG `datatype_mismatch` (SQLSTATE 42804) or
    /// `cannot_coerce` (42846) depending on context.
    #[error("type mismatch: {actual} cannot be coerced to {expected} ({context})")]
    TypeMismatch {
        actual: String,
        expected: String,
        context: String,
    },

    /// The analyzer encountered an AST node or SQL feature it does not yet support.
    #[error("unsupported SQL feature: {0}")]
    Unsupported(String),

    /// The query violates PostgreSQL's placement rules for a construct
    /// (aggregate in WHERE, window function in WHERE, nested aggregates,
    /// INSERT/SELECT arity mismatch, etc.). Maps to a mix of PG SQLSTATEs —
    /// primarily `grouping_error` (42803) and `syntax_error` (42601) — that we
    /// don't yet split further.
    #[error("invalid SQL: {0}")]
    Invalid(String),

    /// The parser reported a JOIN kind the analyzer does not recognize.
    /// Returned instead of silently falling back to INNER JOIN semantics,
    /// which would produce incorrect nullability.
    #[error("unsupported join type: {0}")]
    UnsupportedJoinType(i32),

    /// An analyzer invariant was violated — typically because a placeholder
    /// survived lexing but was not walked during type inference (e.g. it sat
    /// inside an AST node the analyzer does not yet traverse). Surfaced as an
    /// error instead of a panic so callers can report the offending SQL
    /// without crashing the macro host process.
    #[error("internal analyzer error: {0}")]
    Internal(String),

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
