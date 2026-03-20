use cubos_sql::migrate::MigrationSource;
use cubos_sql_core::config::MigrationsConfig;
use std::fs;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tokio_postgres::NoTls;

async fn connect_to_container(
    container: &testcontainers::ContainerAsync<Postgres>,
) -> tokio_postgres::Client {
    let host = container.get_host().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let conn_str = format!(
        "host={host} port={port} user=postgres password=postgres dbname=postgres"
    );
    let (client, conn) = tokio_postgres::connect(&conn_str, NoTls).await.unwrap();
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            eprintln!("connection error: {e}");
        }
    });
    client
}

fn create_test_migrations(dir: &std::path::Path) {
    fs::write(
        dir.join("20260318120000_create_users.sql"),
        "CREATE TABLE users (
            id   BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            name TEXT NOT NULL
        );",
    )
    .unwrap();

    fs::write(
        dir.join("20260319120000_create_orders.sql"),
        "CREATE TABLE orders (
            id      BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            user_id BIGINT NOT NULL REFERENCES users(id)
        );",
    )
    .unwrap();
}

fn create_test_migrations_with_down(dir: &std::path::Path) {
    create_test_migrations(dir);

    fs::write(
        dir.join("20260318120000_create_users.down.sql"),
        "DROP TABLE users;",
    )
    .unwrap();

    fs::write(
        dir.join("20260319120000_create_orders.down.sql"),
        "DROP TABLE orders;",
    )
    .unwrap();
}

#[tokio::test]
async fn run_applies_all_pending_migrations() {
    let container = Postgres::default().start().await.unwrap();
    let mut client = connect_to_container(&container).await;

    let dir = tempfile::tempdir().unwrap();
    create_test_migrations(dir.path());

    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();

    let applied = cubos_sql::migrate::run(&mut client, &source, &config)
        .await
        .unwrap();

    assert_eq!(applied.len(), 2);
    assert_eq!(applied[0], "20260318120000_create_users");
    assert_eq!(applied[1], "20260319120000_create_orders");

    let rows = client
        .query(
            "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public' AND table_name IN ('users', 'orders') ORDER BY table_name",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
}

#[tokio::test]
async fn run_is_idempotent() {
    let container = Postgres::default().start().await.unwrap();
    let mut client = connect_to_container(&container).await;

    let dir = tempfile::tempdir().unwrap();
    create_test_migrations(dir.path());

    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();

    let first = cubos_sql::migrate::run(&mut client, &source, &config)
        .await
        .unwrap();
    assert_eq!(first.len(), 2);

    let second = cubos_sql::migrate::run(&mut client, &source, &config)
        .await
        .unwrap();
    assert!(second.is_empty());
}

#[tokio::test]
async fn status_shows_applied_and_pending() {
    let container = Postgres::default().start().await.unwrap();
    let mut client = connect_to_container(&container).await;

    let dir = tempfile::tempdir().unwrap();
    create_test_migrations(dir.path());

    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();

    let statuses = cubos_sql::migrate::status(&client, &source, &config)
        .await
        .unwrap();
    assert_eq!(statuses.len(), 2);
    assert!(!statuses[0].applied);
    assert!(!statuses[1].applied);

    let partial_dir = tempfile::tempdir().unwrap();
    fs::write(
        partial_dir.path().join("20260318120000_create_users.sql"),
        "CREATE TABLE users (id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, name TEXT NOT NULL);",
    )
    .unwrap();
    let partial_source = MigrationSource::from_dir(partial_dir.path()).unwrap();
    cubos_sql::migrate::run(&mut client, &partial_source, &config)
        .await
        .unwrap();

    let statuses = cubos_sql::migrate::status(&client, &source, &config)
        .await
        .unwrap();
    assert!(statuses[0].applied);
    assert!(statuses[0].applied_at.is_some());
    assert!(!statuses[1].applied);
}

#[tokio::test]
async fn revert_with_down_sql() {
    let container = Postgres::default().start().await.unwrap();
    let mut client = connect_to_container(&container).await;

    let dir = tempfile::tempdir().unwrap();
    create_test_migrations_with_down(dir.path());

    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();

    cubos_sql::migrate::run(&mut client, &source, &config)
        .await
        .unwrap();

    // Revert orders (must revert before users due to FK)
    cubos_sql::migrate::revert(
        &mut client,
        &source,
        "20260319120000_create_orders",
        false,
        &config,
    )
    .await
    .unwrap();

    // orders table should be gone
    let rows = client
        .query(
            "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public' AND table_name = 'orders'",
            &[],
        )
        .await
        .unwrap();
    assert!(rows.is_empty());

    // users should still exist
    let rows = client
        .query(
            "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public' AND table_name = 'users'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);

    // Status should reflect the revert
    let statuses = cubos_sql::migrate::status(&client, &source, &config)
        .await
        .unwrap();
    assert!(statuses[0].applied);
    assert!(!statuses[1].applied);
}

#[tokio::test]
async fn revert_without_down_sql_errors() {
    let container = Postgres::default().start().await.unwrap();
    let mut client = connect_to_container(&container).await;

    let dir = tempfile::tempdir().unwrap();
    create_test_migrations(dir.path()); // no down files

    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();

    cubos_sql::migrate::run(&mut client, &source, &config)
        .await
        .unwrap();

    let result = cubos_sql::migrate::revert(
        &mut client,
        &source,
        "20260319120000_create_orders",
        false,
        &config,
    )
    .await;

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("no down file"), "unexpected error: {err}");
}

#[tokio::test]
async fn revert_force_without_down_sql() {
    let container = Postgres::default().start().await.unwrap();
    let mut client = connect_to_container(&container).await;

    let dir = tempfile::tempdir().unwrap();
    create_test_migrations(dir.path()); // no down files

    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();

    cubos_sql::migrate::run(&mut client, &source, &config)
        .await
        .unwrap();

    // force=true should succeed, just removing the record
    cubos_sql::migrate::revert(
        &mut client,
        &source,
        "20260319120000_create_orders",
        true,
        &config,
    )
    .await
    .unwrap();

    // Record removed but table still exists (no SQL ran)
    let statuses = cubos_sql::migrate::status(&client, &source, &config)
        .await
        .unwrap();
    assert!(statuses[0].applied);
    assert!(!statuses[1].applied);

    let rows = client
        .query(
            "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public' AND table_name = 'orders'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1); // table still exists
}

#[tokio::test]
async fn revert_not_applied_errors() {
    let container = Postgres::default().start().await.unwrap();
    let mut client = connect_to_container(&container).await;

    let dir = tempfile::tempdir().unwrap();
    create_test_migrations(dir.path());

    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();

    // Don't run migrations — try to revert directly
    let result = cubos_sql::migrate::revert(
        &mut client,
        &source,
        "20260318120000_create_users",
        false,
        &config,
    )
    .await;

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("not applied"), "unexpected error: {err}");
}

#[tokio::test]
async fn failed_migration_rolls_back() {
    let container = Postgres::default().start().await.unwrap();
    let mut client = connect_to_container(&container).await;

    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("20260318120000_create_users.sql"),
        "CREATE TABLE users (id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY);",
    )
    .unwrap();
    fs::write(
        dir.path().join("20260319120000_bad_migration.sql"),
        "THIS IS NOT VALID SQL;",
    )
    .unwrap();

    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();

    let result = cubos_sql::migrate::run(&mut client, &source, &config).await;
    assert!(result.is_err());

    let statuses = cubos_sql::migrate::status(&client, &source, &config)
        .await
        .unwrap();
    assert!(statuses[0].applied);
    assert!(!statuses[1].applied);
}

#[tokio::test]
async fn no_transaction_migration() {
    let container = Postgres::default().start().await.unwrap();
    let mut client = connect_to_container(&container).await;

    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("20260318120000_create_users.sql"),
        "CREATE TABLE users (id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, name TEXT NOT NULL);",
    )
    .unwrap();
    fs::write(
        dir.path().join("20260319120000_add_index.sql"),
        "-- no-transaction\nCREATE INDEX CONCURRENTLY idx_users_name ON users(name);",
    )
    .unwrap();

    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();

    let applied = cubos_sql::migrate::run(&mut client, &source, &config)
        .await
        .unwrap();
    assert_eq!(applied.len(), 2);

    let rows = client
        .query(
            "SELECT indexname FROM pg_indexes WHERE tablename = 'users' AND indexname = 'idx_users_name'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
}

#[tokio::test]
async fn custom_table_name() {
    let container = Postgres::default().start().await.unwrap();
    let mut client = connect_to_container(&container).await;

    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("20260318120000_create_users.sql"),
        "CREATE TABLE users (id SERIAL PRIMARY KEY);",
    )
    .unwrap();

    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig {
        table: "public._my_custom_migrations".to_string(),
        ..Default::default()
    };

    cubos_sql::migrate::run(&mut client, &source, &config)
        .await
        .unwrap();

    let rows = client
        .query("SELECT * FROM public._my_custom_migrations", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(0), "20260318120000_create_users");
}
