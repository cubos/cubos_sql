//! Shared setup for end-to-end tests.
//!
//! Each integration test file in `tests/` is compiled as its own binary.
//! Within a binary, `#[tokio::test]`s run in parallel threads, each with its
//! own Tokio runtime. We share the Postgres container (and make sure
//! migrations run only once) via `OnceCell`, but every test creates its own
//! `deadpool_postgres::Pool` — a pool built in one test's runtime would
//! leave behind connection tasks that die when that runtime shuts down,
//! surfacing as `kind: Closed` errors in sibling tests.

#![allow(dead_code)]

use std::path::PathBuf;

use cubos_sql::migrate::MigrationSource;
use cubos_sql_core::config::MigrationsConfig;
use deadpool_postgres::{Config, Pool, Runtime};
use testcontainers::ContainerAsync;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tokio::sync::OnceCell;
use tokio_postgres::NoTls;

pub struct TestEnv {
    host: String,
    port: u16,
    // Kept alive for the duration of the test binary — dropping this would
    // tear down the Docker container.
    _container: ContainerAsync<Postgres>,
}

static ENV: OnceCell<TestEnv> = OnceCell::const_new();

pub async fn setup() -> Pool {
    let env = ENV
        .get_or_init(|| async {
            let container = Postgres::default().start().await.expect("start postgres");
            let host = container
                .get_host()
                .await
                .expect("container host")
                .to_string();
            let port = container
                .get_host_port_ipv4(5432)
                .await
                .expect("container port");

            let bootstrap = build_pool(&host, port);
            let migrations_dir: PathBuf =
                [env!("CARGO_MANIFEST_DIR"), "migrations"].iter().collect();
            let source = MigrationSource::from_dir(&migrations_dir).expect("load migrations");
            let mig_cfg = MigrationsConfig::default();

            let mut client = bootstrap.get().await.expect("get client");
            cubos_sql::migrate::run(&mut *client, &source, &mig_cfg)
                .await
                .expect("run migrations");
            drop(client);
            drop(bootstrap);

            TestEnv {
                host,
                port,
                _container: container,
            }
        })
        .await;

    build_pool(&env.host, env.port)
}

fn build_pool(host: &str, port: u16) -> Pool {
    let mut cfg = Config::new();
    cfg.host = Some(host.to_string());
    cfg.port = Some(port);
    cfg.user = Some("postgres".into());
    cfg.password = Some("postgres".into());
    cfg.dbname = Some("postgres".into());

    cfg.create_pool(Some(Runtime::Tokio1), NoTls)
        .expect("build pool")
}
