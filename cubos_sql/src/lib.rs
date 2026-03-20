//! Main runtime crate for `cubos_sql` — typed PostgreSQL access with compile-time verification.
//!
//! This crate provides:
//!
//! - **Migration runner** ([`migrate`]) — apply, revert, and inspect database migrations
//!   with advisory-lock protection and optional transaction wrapping.
//! - **Connection pool** — coming soon (via `deadpool-postgres`).
//! - **Query execution** — coming soon (integrates with the `query!` proc macro).
//!
//! # Example: running migrations
//!
//! ```rust,no_run
//! use cubos_sql::migrate::{MigrationSource, run};
//! use cubos_sql_core::config::MigrationsConfig;
//! use std::path::Path;
//!
//! # async fn example() -> Result<(), cubos_sql::Error> {
//! let source = MigrationSource::from_dir(Path::new("./migrations"))?;
//! let config = MigrationsConfig::default();
//!
//! let (client, connection) =
//!     tokio_postgres::connect("host=localhost dbname=mydb", tokio_postgres::NoTls).await?;
//! tokio::spawn(connection);
//! let mut client = client;
//!
//! let applied: Vec<String> = run(&mut client, &source, &config).await?;
//! println!("Applied {} migration(s)", applied.len());
//! # Ok(())
//! # }
//! ```

pub mod error;
pub mod migrate;

pub use error::Error;
