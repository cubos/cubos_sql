use tokio_postgres::types::ToSql;
use tokio_postgres::Row;

/// Trait for types that can execute SQL queries.
///
/// Implemented for [`Pool`](crate::Pool), [`tokio_postgres::Client`],
/// and [`tokio_postgres::Transaction`]. The `query!` macro generates code
/// generic over this trait so the same query works with pools and transactions.
pub trait Executor: Sync {
    /// Execute a query and return all rows.
    fn query<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> impl std::future::Future<Output = Result<Vec<Row>, crate::Error>> + Send + 'a;

    /// Execute a statement and return the number of affected rows.
    fn execute<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> impl std::future::Future<Output = Result<u64, crate::Error>> + Send + 'a;
}

impl Executor for tokio_postgres::Client {
    async fn query<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, crate::Error> {
        Ok(tokio_postgres::Client::query(self, sql, params).await?)
    }

    async fn execute<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> Result<u64, crate::Error> {
        Ok(tokio_postgres::Client::execute(self, sql, params).await?)
    }
}

impl Executor for tokio_postgres::Transaction<'_> {
    async fn query<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, crate::Error> {
        Ok(tokio_postgres::Transaction::query(self, sql, params).await?)
    }

    async fn execute<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> Result<u64, crate::Error> {
        Ok(tokio_postgres::Transaction::execute(self, sql, params).await?)
    }
}
