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
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

use cubos_sql::migrate::MigrationSource;
use cubos_sql_core::config::MigrationsConfig;
use deadpool_postgres::{Config, Pool, Runtime};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;
use tokio::sync::OnceCell;
use tokio_postgres::NoTls;

/// Container IDs that an `atexit` hook force-removes before the test process
/// exits. The `ContainerAsync` below lives inside a `static OnceCell` and
/// `Drop` is never invoked for `static`s, so without this fallback the
/// testcontainers-rs remove-on-drop path never runs and containers leak on
/// any environment without Ryuk.
static TRACKED_CONTAINERS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

extern "C" fn reap_containers() {
    let Some(ids) = TRACKED_CONTAINERS.get() else {
        return;
    };
    let ids = match ids.lock() {
        Ok(mut g) => std::mem::take(&mut *g),
        Err(_) => return,
    };
    for id in ids {
        let _ = Command::new("docker")
            .args(["rm", "-f", &id])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn track_container(id: String) {
    let slot = TRACKED_CONTAINERS.get_or_init(|| {
        // SAFETY: `reap_containers` has the required C ABI and only touches
        // the static `Mutex`; registering it once is safe on every POSIX
        // target we support.
        unsafe {
            libc::atexit(reap_containers);
        }
        Mutex::new(Vec::new())
    });
    if let Ok(mut ids) = slot.lock() {
        ids.push(id);
    }
}

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
            // Run on the `pgvector/pgvector` image — it ships the official
            // Postgres image plus the `vector` extension, which migration
            // `0006_vectors.sql` needs. The plain `postgres` image has no
            // `vector` control files, so `CREATE EXTENSION vector` there
            // fails; since every e2e test shares this one migration set,
            // the image has to satisfy all of them. Docker's `--rm` flag
            // deletes the container as soon as it exits — the fallback for
            // environments where the Ryuk reaper isn't running and would
            // otherwise leak containers on panic or abort.
            let container = Postgres::default()
                .with_name("pgvector/pgvector")
                .with_tag("pg18")
                .with_host_config_modifier(|cfg| cfg.auto_remove = Some(true))
                .start()
                .await
                .expect("start postgres");
            track_container(container.id().to_string());
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
            cubos_sql::migrate::run(&mut client, &source, &mig_cfg)
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
