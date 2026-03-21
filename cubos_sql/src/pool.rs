use std::ops::Deref;

use tokio_postgres::types::ToSql;
use tokio_postgres::Row;

use crate::executor::Executor;

/// [`Executor`] implementation for `deadpool_postgres::Pool`.
///
/// Each method call acquires a connection from the pool, executes the query,
/// and returns the connection to the pool when done. This is the most convenient
/// way to run queries -- just pass `&pool` to `query!` and connection management
/// is handled automatically.
///
/// If the pool is exhausted (no available connections), methods return
/// [`Error::Pool`](crate::Error::Pool).
impl Executor for deadpool_postgres::Pool {
    async fn query<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, crate::Error> {
        let client = self
            .get()
            .await
            .map_err(|e| crate::Error::Pool(e.to_string()))?;
        client.query(sql, params).await
    }

    async fn execute<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> Result<u64, crate::Error> {
        let client = self
            .get()
            .await
            .map_err(|e| crate::Error::Pool(e.to_string()))?;
        client.execute(sql, params).await
    }
}

/// [`Executor`] implementation for `deadpool_postgres::Object` (the pooled connection).
///
/// Delegates directly to the inner `tokio_postgres::Client` via `Deref`. Use this
/// when you need to hold a connection across multiple queries (e.g., to run them on
/// the same connection) without using a transaction.
impl Executor for deadpool_postgres::Object {
    async fn query<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, crate::Error> {
        Ok(self.deref().query(sql, params).await?)
    }

    async fn execute<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> Result<u64, crate::Error> {
        Ok(self.deref().execute(sql, params).await?)
    }
}
