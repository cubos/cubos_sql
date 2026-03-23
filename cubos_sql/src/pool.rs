use std::ops::Deref;

use tokio_postgres::types::ToSql;
use tokio_postgres::Row;

use crate::executor::Executor;

// ── deadpool-postgres ────────────────────────────────────────────────────────

/// [`Executor`] implementation for `deadpool_postgres::Pool`.
///
/// Each method call acquires a connection from the pool, executes the query,
/// and returns the connection to the pool when done. This is the most convenient
/// way to run queries -- just pass `&pool` to `sql!` and connection management
/// is handled automatically.
///
/// If the pool is exhausted (no available connections), methods return
/// [`Error::Pool`](crate::Error::Pool).
#[cfg(feature = "deadpool")]
impl Executor for deadpool_postgres::Pool {
    async fn query<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, crate::Error> {
        let client = self.get().await?;
        Ok(client.deref().query(sql, params).await?)
    }

    async fn execute<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> Result<u64, crate::Error> {
        let client = self.get().await?;
        Ok(client.deref().execute(sql, params).await?)
    }
}

/// [`Executor`] implementation for `deadpool_postgres::Object` (the pooled connection).
///
/// Delegates directly to the inner `tokio_postgres::Client` via `Deref`. Use this
/// when you need to hold a connection across multiple queries (e.g., to run them on
/// the same connection) without using a transaction.
#[cfg(feature = "deadpool")]
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

// ── bb8-postgres ─────────────────────────────────────────────────────────────

/// [`Executor`] implementation for `bb8::Pool` with `bb8_postgres::PostgresConnectionManager`.
///
/// Works identically to the deadpool implementation: acquires a connection per
/// method call and returns it when done. Enable the `bb8` feature to use this.
#[cfg(feature = "bb8")]
impl<Tls> Executor for bb8::Pool<bb8_postgres::PostgresConnectionManager<Tls>>
where
    Tls:
        tokio_postgres::tls::MakeTlsConnect<tokio_postgres::Socket> + Clone + Send + Sync + 'static,
    <Tls as tokio_postgres::tls::MakeTlsConnect<tokio_postgres::Socket>>::Stream: Send + Sync,
    <Tls as tokio_postgres::tls::MakeTlsConnect<tokio_postgres::Socket>>::TlsConnect: Send,
{
    async fn query<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, crate::Error> {
        let client = self.get().await?;
        Ok(client.query(sql, params).await?)
    }

    async fn execute<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> Result<u64, crate::Error> {
        let client = self.get().await?;
        Ok(client.execute(sql, params).await?)
    }
}
