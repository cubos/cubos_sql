/// Errors returned by `cubos_sql` operations.
///
/// This enum covers all failure modes you may encounter when using the library:
/// database communication errors, migration problems, connection pool issues,
/// I/O failures when reading migration files, and empty query results.
///
/// All variants implement [`std::fmt::Display`] and [`std::error::Error`], so they
/// integrate naturally with `?` and error-reporting crates like `anyhow` or `eyre`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A PostgreSQL protocol or query execution error.
    ///
    /// This wraps [`tokio_postgres::Error`] and surfaces issues such as syntax errors
    /// in SQL, constraint violations, connection drops, or authentication failures.
    /// Inspect the inner error for the PostgreSQL error code and message.
    #[error("database error: {0}")]
    Database(#[from] tokio_postgres::Error),

    /// A migration-specific error.
    ///
    /// Returned when a migration fails to apply or revert, when the migration
    /// directory is missing, when a migration file has an invalid name format,
    /// or when attempting to revert a migration that has no `.down.sql` file
    /// without the `force` flag.
    #[error("migration error: {0}")]
    Migration(String),

    /// An I/O error, typically from reading migration files from disk.
    ///
    /// Check file permissions and that the migrations directory path is correct
    /// in your `[package.metadata.cubos_sql.database]` configuration.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Failed to acquire a connection from the `deadpool_postgres` pool.
    ///
    /// This usually means the pool is exhausted (all connections are in use) or
    /// the database is unreachable. Consider increasing the pool size or checking
    /// network connectivity.
    #[error("pool error: {0}")]
    Pool(String),

    /// A `fetch_one()` call returned zero rows.
    ///
    /// Use `fetch_optional()` instead if the row might not exist, or verify
    /// your query's `WHERE` clause.
    #[error("query returned no rows")]
    NoRows,
}
