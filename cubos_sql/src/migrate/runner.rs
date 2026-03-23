use cubos_sql_core::config::MigrationsConfig;
use tokio_postgres::Client;

use super::source::MigrationSource;

/// Status of a single migration, indicating whether it has been applied.
///
/// Returned by [`status`] for each migration found in the [`MigrationSource`].
///
/// # Example
///
/// ```rust,no_run
/// # async fn example(client: &tokio_postgres::Client,
/// #     source: &cubos_sql::migrate::MigrationSource,
/// #     config: &cubos_sql_core::config::MigrationsConfig) -> Result<(), cubos_sql::Error> {
/// let statuses = cubos_sql::migrate::status(client, source, config).await?;
/// for s in &statuses {
///     if !s.applied {
///         println!("Pending: {}", s.name);
///     }
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct MigrationStatus {
    /// The migration name (file stem), e.g. `"0001_create_users"`.
    pub name: String,
    /// `true` if this migration has been applied to the database.
    pub applied: bool,
    /// Timestamp when the migration was applied, or `None` if it is still pending.
    pub applied_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Applies all pending migrations in order.
///
/// Acquires a PostgreSQL advisory lock (using `config.lock_id`) to prevent
/// concurrent migration runs, then applies each pending migration in version order.
/// The lock is released when the function returns, even on failure.
///
/// By default each migration runs inside a transaction. This can be configured
/// globally via [`MigrationsConfig::use_transaction`](cubos_sql_core::config::MigrationsConfig),
/// or disabled per-migration with `-- no-transaction` on the first line of the SQL file.
///
/// Returns the list of migration names that were applied in this run. If all
/// migrations are already applied, returns an empty `Vec`.
///
/// # Errors
///
/// - [`Error::Migration`](crate::Error::Migration) if a migration's SQL fails to execute.
/// - [`Error::Database`](crate::Error::Database) on connection or lock errors.
///
/// # Example
///
/// ```rust,no_run
/// use cubos_sql::migrate::{MigrationSource, run};
/// use cubos_sql_core::config::MigrationsConfig;
/// use std::path::Path;
///
/// # async fn example() -> Result<(), cubos_sql::Error> {
/// let (mut client, conn) =
///     tokio_postgres::connect("host=localhost dbname=mydb", tokio_postgres::NoTls).await?;
/// tokio::spawn(conn);
///
/// let source = MigrationSource::from_dir(Path::new("./migrations"))?;
/// let applied = run(&mut client, &source, &MigrationsConfig::default()).await?;
/// println!("Applied {} migrations", applied.len());
/// # Ok(())
/// # }
/// ```
pub async fn run(
    client: &mut Client,
    source: &MigrationSource,
    config: &MigrationsConfig,
) -> Result<Vec<String>, crate::Error> {
    ensure_table(client, config).await?;
    acquire_lock(client, config).await?;

    let result = run_inner(client, source, config).await;

    // Always release lock, even if run_inner failed.
    let release = release_lock(client, config).await;
    match (&result, release) {
        (Ok(_), Ok(_)) => result,
        (Ok(_), Err(rel_err)) => Err(rel_err),
        (Err(_), Err(rel_err)) => {
            eprintln!("cubos_sql: failed to release advisory lock: {rel_err}");
            result
        }
        (Err(_), Ok(_)) => result,
    }
}

async fn run_inner(
    client: &mut Client,
    source: &MigrationSource,
    config: &MigrationsConfig,
) -> Result<Vec<String>, crate::Error> {
    let applied = get_applied_names(client, config).await?;
    let mut newly_applied = Vec::new();

    for migration in source.migrations() {
        if applied.contains(&migration.name) {
            continue;
        }

        let use_tx = config.use_transaction && !migration.no_transaction;

        if use_tx {
            let tx = client.transaction().await?;

            tx.batch_execute(&migration.sql).await.map_err(|e| {
                crate::Error::Migration(format!(
                    "failed to apply migration {}: {}",
                    migration.name, e
                ))
            })?;

            tx.execute(
                &format!("INSERT INTO {} (name) VALUES ($1)", config.table),
                &[&migration.name],
            )
            .await?;

            tx.commit().await?;
        } else {
            client.batch_execute(&migration.sql).await.map_err(|e| {
                crate::Error::Migration(format!(
                    "failed to apply migration {}: {}",
                    migration.name, e
                ))
            })?;

            client
                .execute(
                    &format!("INSERT INTO {} (name) VALUES ($1)", config.table),
                    &[&migration.name],
                )
                .await?;
        }

        newly_applied.push(migration.name.clone());
    }

    Ok(newly_applied)
}

/// Returns the status of all known migrations (applied and pending).
///
/// Queries the migrations tracking table and cross-references with the
/// [`MigrationSource`] to produce a [`MigrationStatus`] for each migration.
/// The result is ordered by migration version.
///
/// Unlike [`run`] and [`revert`], this function does **not** acquire an advisory
/// lock -- it is a read-only operation.
///
/// # Errors
///
/// - [`Error::Database`](crate::Error::Database) if the tracking table query fails.
///
/// # Example
///
/// ```rust,no_run
/// use cubos_sql::migrate::{MigrationSource, status};
/// use cubos_sql_core::config::MigrationsConfig;
/// use std::path::Path;
///
/// # async fn example(client: &tokio_postgres::Client) -> Result<(), cubos_sql::Error> {
/// let source = MigrationSource::from_dir(Path::new("./migrations"))?;
/// let statuses = status(client, &source, &MigrationsConfig::default()).await?;
/// for s in &statuses {
///     let mark = if s.applied { "+" } else { " " };
///     println!("[{}] {}", mark, s.name);
/// }
/// # Ok(())
/// # }
/// ```
pub async fn status(
    client: &Client,
    source: &MigrationSource,
    config: &MigrationsConfig,
) -> Result<Vec<MigrationStatus>, crate::Error> {
    ensure_table(client, config).await?;

    let rows = client
        .query(
            &format!(
                "SELECT name, applied_at FROM {} ORDER BY name",
                config.table
            ),
            &[],
        )
        .await?;

    let mut applied: std::collections::HashMap<String, chrono::DateTime<chrono::Utc>> =
        std::collections::HashMap::with_capacity(rows.len());
    for row in &rows {
        let name: String = row
            .try_get(0)
            .map_err(|e| crate::Error::Migration(format!("failed to read migration name: {e}")))?;
        let applied_at: chrono::DateTime<chrono::Utc> = row
            .try_get(1)
            .map_err(|e| crate::Error::Migration(format!("failed to read applied_at: {e}")))?;
        applied.insert(name, applied_at);
    }

    let statuses = source
        .migrations()
        .iter()
        .map(|m| {
            let info = applied.get(&m.name);
            MigrationStatus {
                name: m.name.clone(),
                applied: info.is_some(),
                applied_at: info.copied(),
            }
        })
        .collect();

    Ok(statuses)
}

/// Reverts a single migration by name.
///
/// If the migration has a `.down.sql` file, executes the rollback SQL and removes the
/// record from the tracking table. If there is no down file, returns
/// [`Error::Migration`](crate::Error::Migration) -- unless `force` is `true`, in which
/// case it removes the tracking record without executing any SQL.
///
/// The down SQL runs in a transaction following the same rules as the up migration
/// (`config.use_transaction` and the `-- no-transaction` marker).
///
/// Acquires an advisory lock for the duration of the operation.
///
/// # Errors
///
/// - [`Error::Migration`](crate::Error::Migration) if the migration is not currently
///   applied, not found in the source, has no down file (and `force` is `false`),
///   or if the down SQL fails.
/// - [`Error::Database`](crate::Error::Database) on connection or lock errors.
///
/// # Example
///
/// ```rust,no_run
/// use cubos_sql::migrate::{MigrationSource, revert};
/// use cubos_sql_core::config::MigrationsConfig;
/// use std::path::Path;
///
/// # async fn example(client: &mut tokio_postgres::Client) -> Result<(), cubos_sql::Error> {
/// let source = MigrationSource::from_dir(Path::new("./migrations"))?;
/// let config = MigrationsConfig::default();
///
/// // Revert a specific migration (requires a .down.sql file)
/// revert(client, &source, "0002_add_email", false, &config).await?;
///
/// // Force-remove a migration record without running down SQL
/// revert(client, &source, "0003_create_index", true, &config).await?;
/// # Ok(())
/// # }
/// ```
pub async fn revert(
    client: &mut Client,
    source: &MigrationSource,
    name: &str,
    force: bool,
    config: &MigrationsConfig,
) -> Result<(), crate::Error> {
    ensure_table(client, config).await?;
    acquire_lock(client, config).await?;

    let result = revert_inner(client, source, name, force, config).await;

    let release = release_lock(client, config).await;
    match (&result, release) {
        (Ok(_), Ok(_)) => result,
        (Ok(_), Err(rel_err)) => Err(rel_err),
        (Err(_), Err(rel_err)) => {
            eprintln!("cubos_sql: failed to release advisory lock: {rel_err}");
            result
        }
        (Err(_), Ok(_)) => result,
    }
}

async fn revert_inner(
    client: &mut Client,
    source: &MigrationSource,
    name: &str,
    force: bool,
    config: &MigrationsConfig,
) -> Result<(), crate::Error> {
    // Check if the migration is actually applied
    let applied = get_applied_names(client, config).await?;
    if !applied.contains(name) {
        return Err(crate::Error::Migration(format!(
            "migration '{}' is not applied",
            name
        )));
    }

    let migration = source.find(name).ok_or_else(|| {
        crate::Error::Migration(format!("migration '{}' not found in source", name))
    })?;

    match &migration.down_sql {
        Some(down_sql) => {
            let use_tx = config.use_transaction && !migration.no_transaction;

            if use_tx {
                let tx = client.transaction().await?;

                tx.batch_execute(down_sql).await.map_err(|e| {
                    crate::Error::Migration(format!("failed to revert migration {}: {}", name, e))
                })?;

                tx.execute(
                    &format!("DELETE FROM {} WHERE name = $1", config.table),
                    &[&name.to_string()],
                )
                .await?;

                tx.commit().await?;
            } else {
                client.batch_execute(down_sql).await.map_err(|e| {
                    crate::Error::Migration(format!("failed to revert migration {}: {}", name, e))
                })?;

                client
                    .execute(
                        &format!("DELETE FROM {} WHERE name = $1", config.table),
                        &[&name.to_string()],
                    )
                    .await?;
            }
        }
        None => {
            if !force {
                return Err(crate::Error::Migration(format!(
                    "migration '{}' has no down file ({}.down.sql). Use force to remove the record without running SQL.",
                    name, name
                )));
            }

            client
                .execute(
                    &format!("DELETE FROM {} WHERE name = $1", config.table),
                    &[&name.to_string()],
                )
                .await?;
        }
    }

    Ok(())
}

async fn ensure_table(client: &Client, config: &MigrationsConfig) -> Result<(), crate::Error> {
    config
        .validate()
        .map_err(|e| crate::Error::Migration(e.to_string()))?;
    let sql = format!(
        "CREATE TABLE IF NOT EXISTS {} (
            name       TEXT PRIMARY KEY,
            applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
        );",
        config.table
    );
    client.batch_execute(&sql).await?;
    Ok(())
}

async fn acquire_lock(client: &Client, config: &MigrationsConfig) -> Result<(), crate::Error> {
    client
        .execute("SELECT pg_advisory_lock($1)", &[&config.lock_id])
        .await?;
    Ok(())
}

async fn release_lock(client: &Client, config: &MigrationsConfig) -> Result<(), crate::Error> {
    client
        .execute("SELECT pg_advisory_unlock($1)", &[&config.lock_id])
        .await?;
    Ok(())
}

async fn get_applied_names(
    client: &Client,
    config: &MigrationsConfig,
) -> Result<std::collections::HashSet<String>, crate::Error> {
    let rows = client
        .query(&format!("SELECT name FROM {}", config.table), &[])
        .await?;

    let names = rows.iter().map(|row| row.get::<_, String>(0)).collect();

    Ok(names)
}
