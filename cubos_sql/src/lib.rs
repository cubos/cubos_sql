//! Main runtime crate for `cubos_sql` — typed PostgreSQL access with compile-time verification.
//!
//! This crate provides:
//!
//! - **Migration runner** ([`migrate`]) — apply, revert, and inspect database migrations
//!   with advisory-lock protection and optional transaction wrapping.
//! - **Executor trait** ([`Executor`]) — abstraction over pooled clients and transactions,
//!   used by the `query!` macro.
//!
//! # Pool setup
//!
//! Create a `deadpool_postgres::Pool` directly — this crate implements
//! [`Executor`] for `deadpool_postgres::Object` (the pooled client):
//!
//! ```rust,no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use deadpool_postgres::{Config, Runtime};
//! use tokio_postgres::NoTls;
//!
//! let mut cfg = Config::new();
//! cfg.host = Some("localhost".into());
//! cfg.dbname = Some("mydb".into());
//! cfg.user = Some("postgres".into());
//! let pool = cfg.create_pool(Some(Runtime::Tokio1), NoTls)?;
//!
//! let client = pool.get().await?;
//! // `client` implements `cubos_sql::Executor` — pass it to `query!`
//! # Ok(())
//! # }
//! ```

pub mod error;
pub mod executor;
pub mod migrate;
mod pool; // Executor impls for deadpool types

pub use error::Error;
pub use executor::Executor;

/// Re-export the `query!` macro from `cubos_sql_macros`.
pub use cubos_sql_macros::query;

/// Re-exports used by the `query!` macro generated code.
/// Not part of the public API — do not rely on these directly.
#[doc(hidden)]
pub mod __private {
    pub use serde_json;
    pub use tokio_postgres;
}
