use cubos_sql_core::config::MigrationsConfig;
use tokio_postgres::Client;

use super::source::MigrationSource;

/// Status of a migration, indicating whether it has been applied to the database.
#[derive(Debug, Clone)]
pub struct MigrationStatus {
    /// The migration name (file stem), e.g. `"0001_create_users"`.
    pub name: String,
    /// Whether this migration has been applied to the database.
    pub applied: bool,
    /// Timestamp when the migration was applied, or `None` if it is still pending.
    pub applied_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Applies all pending migrations in order.
///
/// Uses an advisory lock to prevent concurrent execution.
/// By default each migration runs inside a transaction, configurable via `MigrationsConfig`.
/// Individual migrations can disable the transaction with `-- no-transaction` on the first line.
/// Returns the list of migration names applied in this run.
pub async fn run(
    client: &mut Client,
    source: &MigrationSource,
    config: &MigrationsConfig,
) -> Result<Vec<String>, crate::Error> {
    ensure_table(client, config).await?;
    acquire_lock(client, config).await?;

    let result = run_inner(client, source, config).await;

    // Always release lock, even if run_inner failed.
    // If release itself fails, prefer returning the original error.
    let release = release_lock(client, config).await;
    match result {
        Ok(v) => {
            release?;
            Ok(v)
        }
        Err(e) => Err(e),
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
            client
                .batch_execute(&migration.sql)
                .await
                .map_err(|e| {
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

/// Returns the status of all migrations (applied and pending).
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

    let applied: std::collections::HashMap<String, chrono::DateTime<chrono::Utc>> = rows
        .iter()
        .map(|row| {
            let name: String = row.get(0);
            let applied_at: chrono::DateTime<chrono::Utc> = row.get(1);
            (name, applied_at)
        })
        .collect();

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

/// Reverts a migration by name.
///
/// If the migration has a `.down.sql` file, executes the rollback SQL.
/// If it does not, returns an error — unless `force` is true, in which case
/// it only removes the record from the tracking table without executing SQL.
///
/// The down SQL runs in a transaction following the same rules as the up migration
/// (`config.use_transaction` and `-- no-transaction`).
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
    match result {
        Ok(v) => {
            release?;
            Ok(v)
        }
        Err(e) => Err(e),
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
                    crate::Error::Migration(format!(
                        "failed to revert migration {}: {}",
                        name, e
                    ))
                })?;

                tx.execute(
                    &format!("DELETE FROM {} WHERE name = $1", config.table),
                    &[&name.to_string()],
                )
                .await?;

                tx.commit().await?;
            } else {
                client.batch_execute(down_sql).await.map_err(|e| {
                    crate::Error::Migration(format!(
                        "failed to revert migration {}: {}",
                        name, e
                    ))
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

