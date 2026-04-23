//! Typed PostgreSQL access with compile-time query verification.
//!
//! `cubos_sql` checks your SQL queries against a real PostgreSQL schema at compile time,
//! generating type-safe Rust code with zero runtime overhead. Write plain SQL, get full
//! type safety.
//!
//! # Philosophy
//!
//! - **Postgres-only** -- no abstraction over multiple databases. Embraces PostgreSQL
//!   features like `JSONB`, advisory locks, and `CREATE DOMAIN`.
//! - **SQL-native** -- write real SQL, not a Rust DSL. The `sql!` macro takes a SQL
//!   string and verifies it at compile time.
//! - **Compile-time checked** -- the proc macro spins up a Docker container, runs your
//!   migrations, and introspects every query. Type mismatches are caught before your code
//!   ships.
//! - **Human-friendly syntax** -- use `$name` for parameters instead of `$1`. Use
//!   `$..spread` for bulk inserts. Get named fields on output structs, not positional
//!   indices.
//!
//! # Quick start
//!
//! Add to your `Cargo.toml`:
//!
//! ```toml
//! [dependencies]
//! cubos_sql = "0.1"
//! deadpool-postgres = "0.14"
//! tokio-postgres = "0.7"
//! tokio = { version = "1", features = ["full"] }
//!
//! [package.metadata.cubos_sql.database]
//! migrations = "./migrations"
//! ```
//!
//! Create `migrations/0001_create_users.sql`:
//!
//! ```sql
//! CREATE TABLE users (
//!     id    SERIAL PRIMARY KEY,
//!     name  TEXT NOT NULL,
//!     email TEXT NOT NULL UNIQUE
//! );
//! ```
//!
//! Use it:
//!
//! ```rust,ignore
//! use cubos_sql::query;
//!
//! let users = sql!(pool, "SELECT id, name, email FROM users")
//!     .fetch_all()
//!     .await?;
//!
//! for user in &users {
//!     println!("{}: {} ({})", user.id, user.name, user.email);
//! }
//! ```
//!
//! That's it. The macro spins up a Docker Postgres container at compile time,
//! runs your migrations, and type-checks every query.
//!
//! # Configuration
//!
//! The only required configuration is the migrations path. All other settings
//! have sensible defaults:
//!
//! ```toml
//! [package.metadata.cubos_sql.database]
//! migrations = "./migrations"             # required
//! docker_image = "postgres"               # optional, default: "postgres"
//!
//! [package.metadata.cubos_sql.migrations]
//! table = "public._migrations"            # optional, tracking table name
//! lock_id = 713705                        # optional, advisory lock ID
//! use_transaction = true                  # optional, wrap each migration in a tx
//!
//! [package.metadata.cubos_sql.domains]
//! user_preferences = "crate::UserPrefs"   # optional, JSONB domain mappings
//! ```
//!
//! # `.gitignore`
//!
//! The `sql!` macro creates a `.cubos_sql/` directory in your project root to
//! cache Docker container state and query introspection results. Add it to your
//! `.gitignore`:
//!
//! ```text
//! # cubos_sql compile-time cache
//! .cubos_sql/
//! ```
//!
//! # The `sql!` macro
//!
//! The macro verifies your SQL at compile time and generates an anonymous struct
//! for the result columns. It supports four terminal methods:
//!
//! ```rust,ignore
//! use cubos_sql::query;
//!
//! # async fn example(pool: &deadpool_postgres::Pool) -> Result<(), cubos_sql::Error> {
//! // fetch_all -- returns Vec<Row>. Use for SELECT queries expecting multiple rows.
//! let users = sql!(pool, "SELECT id, name FROM users")
//!     .fetch_all()
//!     .await?;
//!
//! // fetch_one -- returns a single Row. Returns Error::NoRows if empty.
//! let user = sql!(pool, "SELECT id, name FROM users WHERE id = $id", id = 1)
//!     .fetch_one()
//!     .await?;
//!
//! // fetch_optional -- returns Option<Row>. Use when the row might not exist.
//! let maybe_user = sql!(pool, "SELECT id, name FROM users WHERE id = $id", id = 42)
//!     .fetch_optional()
//!     .await?;
//!
//! // execute -- returns u64 (number of affected rows). Use for INSERT/UPDATE/DELETE.
//! let rows_affected = sql!(pool, "DELETE FROM users WHERE id = $id", id = 1)
//!     .execute()
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Named parameters
//!
//! Parameters use `$name` syntax in SQL. You can provide values with explicit
//! assignment or rely on scope capture (like closures):
//!
//! ```rust,ignore
//! use cubos_sql::query;
//!
//! # async fn example(pool: &deadpool_postgres::Pool) -> Result<(), cubos_sql::Error> {
//! // Explicit assignment
//! let user = sql!(
//!     pool,
//!     "SELECT id, name FROM users WHERE email = $email",
//!     email = "alice@example.com"
//! )
//!     .fetch_one()
//!     .await?;
//!
//! // Scope capture -- if a variable `email` is in scope, just use $email
//! let email = "alice@example.com".to_string();
//! let user = sql!(
//!     pool,
//!     "SELECT id, name FROM users WHERE email = $email"
//! )
//!     .fetch_one()
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Bulk insert with `$..spread`
//!
//! Insert multiple rows in a single statement using the spread syntax. The macro
//! expands `$..items { field1, field2 }` into a multi-row `VALUES` clause:
//!
//! ```rust,ignore
//! use cubos_sql::query;
//!
//! # struct NewUser { name: String, email: String }
//! # async fn example(pool: &deadpool_postgres::Pool) -> Result<(), cubos_sql::Error> {
//! let new_users = vec![
//!     NewUser { name: "Alice".into(), email: "alice@example.com".into() },
//!     NewUser { name: "Bob".into(),   email: "bob@example.com".into() },
//! ];
//!
//! let rows_affected = sql!(
//!     pool,
//!     "INSERT INTO users (name, email) VALUES $..new_users { name, email }"
//! )
//!     .execute()
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Domain types (JSONB)
//!
//! Map PostgreSQL `CREATE DOMAIN ... AS JSONB` types to Rust structs that
//! implement `serde::Serialize` and `serde::Deserialize`. Configure the mapping
//! in `Cargo.toml`:
//!
//! ```toml
//! [package.metadata.cubos_sql.domains]
//! user_preferences = "crate::domains::UserPreferences"
//! ```
//!
//! Then the `sql!` macro automatically serializes and deserializes through the
//! mapped Rust type instead of raw `serde_json::Value`.
//!
//! # Transactions
//!
//! The [`Executor`] trait is implemented for `tokio_postgres::Transaction`, so you
//! can pass a transaction directly to `sql!`:
//!
//! ```rust,ignore
//! use cubos_sql::query;
//!
//! # async fn example(pool: &deadpool_postgres::Pool) -> Result<(), cubos_sql::Error> {
//! let mut client = pool.get().await.map_err(|e| cubos_sql::Error::Pool(e.to_string()))?;
//! let tx = client.transaction().await?;
//!
//! sql!(&tx, "INSERT INTO users (name, email) VALUES ($name, $email)",
//!     name = "Charlie", email = "charlie@example.com")
//!     .execute()
//!     .await?;
//!
//! sql!(&tx, "UPDATE users SET name = $name WHERE email = $email",
//!     name = "Charles", email = "charlie@example.com")
//!     .execute()
//!     .await?;
//!
//! tx.commit().await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Migrations (programmatic API)
//!
//! Use the [`migrate`] module to run migrations at application startup:
//!
//! ```rust,no_run
//! use cubos_sql::migrate::{MigrationSource, run};
//! use cubos_sql_core::config::MigrationsConfig;
//! use std::path::Path;
//!
//! # async fn example() -> Result<(), cubos_sql::Error> {
//! // Connect directly with tokio-postgres for migrations
//! let (mut client, connection) =
//!     tokio_postgres::connect("host=localhost dbname=mydb user=postgres", tokio_postgres::NoTls).await?;
//! tokio::spawn(connection);
//!
//! let source = MigrationSource::from_dir(Path::new("./migrations"))?;
//! let config = MigrationsConfig::default();
//! let applied = run(&mut client, &source, &config).await?;
//! println!("Applied {} migrations", applied.len());
//! # Ok(())
//! # }
//! ```
//!
//! See the [`migrate`] module for details on [`migrate::status`] and [`migrate::revert`].
//!
//! # Pool setup
//!
//! `cubos_sql` does **not** create or manage connection pools — that is the
//! application's responsibility. This crate implements [`Executor`] for common
//! pool types so you can pass them directly to `sql!`.
//!
//! ## With `deadpool-postgres` (default feature: `deadpool`)
//!
//! ```rust,no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use deadpool_postgres::{Config, Runtime};
//! use tokio_postgres::NoTls;
//!
//! let mut cfg = Config::new();
//! cfg.url = Some(std::env::var("DATABASE_URL")?);
//! let pool = cfg.create_pool(Some(Runtime::Tokio1), NoTls)?;
//!
//! // Pass `&pool` directly to sql! -- acquires a connection automatically
//! // Or get a dedicated connection: pool.get().await?
//! # Ok(())
//! # }
//! ```
//!
//! ## With `bb8-postgres` (feature: `bb8`)
//!
//! ```toml
//! cubos_sql = { version = "0.1", default-features = false, features = ["bb8"] }
//! bb8-postgres = "0.9"
//! ```
//!
//! # The `Executor` trait
//!
//! The [`Executor`] trait abstracts over connection types. It is implemented for:
//!
//! | Type | Feature | Behavior |
//! |------|---------|----------|
//! | `deadpool_postgres::Pool` | `deadpool` (default) | Acquires a connection per query |
//! | `deadpool_postgres::Object` | `deadpool` (default) | Uses the pooled connection |
//! | `bb8::Pool<PostgresConnectionManager<Tls>>` | `bb8` | Acquires a connection per query |
//! | `tokio_postgres::Client` | always | Uses the raw client directly |
//! | `tokio_postgres::Transaction<'_>` | always | Executes within the transaction |
//!
//! You do not call `Executor` methods directly -- the `sql!` macro generates code
//! that is generic over any `Executor` implementation.
//!
//! # Type mapping
//!
//! The following PostgreSQL types are supported and mapped to Rust types automatically:
//!
//! | PostgreSQL type | Rust type |
//! |-----------------|-----------|
//! | `bool` | `bool` |
//! | `int2` / `smallint` | `i16` |
//! | `int4` / `integer` | `i32` |
//! | `int8` / `bigint` | `i64` |
//! | `float4` / `real` | `f32` |
//! | `float8` / `double precision` | `f64` |
//! | `text` | `String` |
//! | `varchar` / `char(n)` | `String` |
//! | `bytea` | `Vec<u8>` |
//! | `uuid` | `uuid::Uuid` |
//! | `date` | `chrono::NaiveDate` |
//! | `time` | `chrono::NaiveTime` |
//! | `timestamp` | `chrono::NaiveDateTime` |
//! | `timestamptz` | `chrono::DateTime<chrono::Utc>` |
//! | `json` | `serde_json::Value` |
//! | `jsonb` | `serde_json::Value` |
//! | `oid` | `u32` |
//! | `bool[]` | `Vec<bool>` |
//! | `int2[]` | `Vec<i16>` |
//! | `int4[]` | `Vec<i32>` |
//! | `int8[]` | `Vec<i64>` |
//! | `float4[]` | `Vec<f32>` |
//! | `float8[]` | `Vec<f64>` |
//! | `text[]` | `Vec<String>` |
//! | `uuid[]` | `Vec<uuid::Uuid>` |
//! | `jsonb[]` | `Vec<serde_json::Value>` |
//!
//! Nullable columns (no `NOT NULL` constraint) are wrapped in `Option<T>`.

pub mod error;
pub mod executor;
pub mod from_row;
pub mod migrate;
mod pool; // Executor impls for pool types (deadpool, bb8)

pub use error::Error;
pub use executor::Executor;
pub use from_row::FromRow;

pub use cubos_sql_macros::FromRow;
/// Re-export the `sql!` macro from `cubos_sql_macros`.
///
/// See the [`macro@sql`] documentation for full syntax, examples, and
/// configuration details.
pub use cubos_sql_macros::sql;

// Re-export rust_decimal so generated code can reference it.
pub use rust_decimal;

/// Re-exports used by the `sql!` macro generated code.
/// Not part of the public API — do not rely on these directly.
#[doc(hidden)]
pub mod __private {
    pub use tokio_postgres;

    use bytes::BytesMut;
    use tokio_postgres::types::{FromSql, IsNull, Kind, ToSql, Type, to_sql_checked};

    /// Bridge type for PostgreSQL enums.
    ///
    /// `tokio_postgres` will not decode/encode a PG enum as `String` directly:
    /// `FromSql`/`ToSql` for `String` only accept `TEXT`/`VARCHAR`/etc. Enum
    /// values travel on the wire as plain UTF-8 label bytes, so we expose a
    /// thin wrapper whose `accepts` matches any `Kind::Enum` type.
    #[derive(Debug)]
    pub struct EnumString(pub String);

    impl<'a> FromSql<'a> for EnumString {
        fn from_sql(
            _ty: &Type,
            raw: &'a [u8],
        ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
            Ok(EnumString(std::str::from_utf8(raw)?.to_owned()))
        }

        fn accepts(ty: &Type) -> bool {
            matches!(ty.kind(), Kind::Enum(_))
        }
    }

    impl ToSql for EnumString {
        fn to_sql(
            &self,
            _ty: &Type,
            out: &mut BytesMut,
        ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
            out.extend_from_slice(self.0.as_bytes());
            Ok(IsNull::No)
        }

        fn accepts(ty: &Type) -> bool {
            matches!(ty.kind(), Kind::Enum(_))
        }

        to_sql_checked!();
    }

    /// Converts a bare string-like value or an `Option<T: Into<String>>` into
    /// `Option<String>` so callers of `sql!` can pass either at a site that
    /// expects a nullable string parameter.
    ///
    /// Impls are concrete for common string types to avoid coherence
    /// conflicts with a hypothetical future `Option<T>: Into<String>`.
    pub trait IntoOptionString {
        fn into_option_string(self) -> Option<String>;
    }

    impl IntoOptionString for &str {
        fn into_option_string(self) -> Option<String> {
            Some(self.to_owned())
        }
    }

    impl IntoOptionString for String {
        fn into_option_string(self) -> Option<String> {
            Some(self)
        }
    }

    impl IntoOptionString for &String {
        fn into_option_string(self) -> Option<String> {
            Some(self.clone())
        }
    }

    impl<T: Into<String>> IntoOptionString for Option<T> {
        fn into_option_string(self) -> Option<String> {
            self.map(Into::into)
        }
    }

    /// Collect any `IntoIterator` whose items convert into `T` into a
    /// `Vec<T>`. Used by generated code for `Vec<T>` parameters so callers
    /// can pass `Vec<&str>`, `[&str; N]`, `[T; N]`, etc., in addition to an
    /// exact `Vec<T>`.
    pub fn into_flex_vec<T, I>(iter: I) -> Vec<T>
    where
        I: IntoIterator,
        I::Item: Into<T>,
    {
        iter.into_iter().map(Into::into).collect()
    }
}
