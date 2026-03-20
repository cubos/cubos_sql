use std::ops::Deref;

use tokio_postgres::types::ToSql;
use tokio_postgres::Row;

use crate::executor::Executor;

/// Implement `Executor` for `deadpool_postgres::Pool` directly.
///
/// Each call acquires a connection from the pool, runs the query, and returns
/// the connection automatically.
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

/// Implement `Executor` for `deadpool_postgres::Object` (the pooled client).
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
