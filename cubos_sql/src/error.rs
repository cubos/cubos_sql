/// Errors returned by cubos_sql operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Database(#[from] tokio_postgres::Error),

    #[error("migration error: {0}")]
    Migration(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("pool error: {0}")]
    Pool(String),

    #[error("query returned no rows")]
    NoRows,
}
