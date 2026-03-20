use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

// ─── Error type ─────────────────────────────────────────────────────────────

/// Errors that can occur while managing the compile-time Docker container.
#[derive(Debug)]
pub enum DockerError {
    Io(std::io::Error),
    Docker(String),
    Migration(String),
    Postgres(postgres::Error),
    Hash(String),
}

impl fmt::Display for DockerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DockerError::Io(e) => write!(f, "I/O error: {}", e),
            DockerError::Docker(msg) => write!(f, "Docker error: {}", msg),
            DockerError::Migration(msg) => write!(f, "Migration error: {}", msg),
            DockerError::Postgres(e) => write!(f, "Postgres error: {}", e),
            DockerError::Hash(msg) => write!(f, "Hash error: {}", msg),
        }
    }
}

impl std::error::Error for DockerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DockerError::Io(e) => Some(e),
            DockerError::Postgres(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for DockerError {
    fn from(e: std::io::Error) -> Self {
        DockerError::Io(e)
    }
}

impl From<postgres::Error> for DockerError {
    fn from(e: postgres::Error) -> Self {
        DockerError::Postgres(e)
    }
}

// ─── ContainerInfo ───────────────────────────────────────────────────────────

/// Connection details for the running compile-time PostgreSQL container.
#[derive(Debug, Clone)]
pub struct ContainerInfo {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub dbname: String,
}

impl ContainerInfo {
    /// Returns a libpq-style connection string.
    pub fn connection_string(&self) -> String {
        format!(
            "host={} port={} user={} password={} dbname={}",
            self.host, self.port, self.user, self.password, self.dbname
        )
    }
}

// ─── Persisted container state ───────────────────────────────────────────────

/// State persisted to disk so we can recover from crashes and reuse containers.
#[derive(Debug, Serialize, Deserialize)]
struct ContainerState {
    container_id: String,
    port: u16,
    /// Whether migrations ran successfully. If `false`, the container is
    /// partially migrated and must be discarded.
    ready: bool,
}

// ─── Hashing ─────────────────────────────────────────────────────────────────

/// Reads all `.sql` files in `path` (sorted by name) and returns the
/// hex-encoded SHA-256 hash of the concatenated `filename + content` strings.
pub fn hash_migrations_dir(path: &Path) -> Result<String, DockerError> {
    let mut entries: Vec<_> = fs::read_dir(path)
        .map_err(|e| {
            DockerError::Hash(format!(
                "failed to read migrations directory '{}': {}",
                path.display(),
                e
            ))
        })?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .path()
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext == "sql")
                .unwrap_or(false)
        })
        .collect();

    entries.sort_by_key(|e| e.file_name());

    let mut hasher = Sha256::new();

    for entry in entries {
        let file_path = entry.path();
        let file_name = entry.file_name();
        let name_str = file_name.to_string_lossy();

        let content = fs::read_to_string(&file_path).map_err(|e| {
            DockerError::Hash(format!(
                "failed to read migration file '{}': {}",
                file_path.display(),
                e
            ))
        })?;

        hasher.update(name_str.as_bytes());
        hasher.update(content.as_bytes());
    }

    let result = hasher.finalize();
    Ok(format!("{:x}", result))
}

// ─── Cache directory ─────────────────────────────────────────────────────────

/// Returns the `.cubos_sql/` directory inside the project root.
///
/// This directory stores container state and query introspection cache.
/// It survives `cargo clean` (unlike `target/`) so running containers
/// are not orphaned.
pub fn cubos_sql_dir(manifest_dir: &Path) -> PathBuf {
    manifest_dir.join(".cubos_sql")
}

/// Returns `.cubos_sql/<hash>/`.
fn cache_dir(base_dir: &Path, hash: &str) -> PathBuf {
    base_dir.join(hash)
}

fn state_path(base_dir: &Path, hash: &str) -> PathBuf {
    cache_dir(base_dir, hash).join("container.json")
}

fn read_state(path: &Path) -> Option<ContainerState> {
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn write_state(path: &Path, state: &ContainerState) -> Result<(), DockerError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| DockerError::Io(std::io::Error::other(e)))?;
    fs::write(path, json)?;
    Ok(())
}

// ─── Docker helpers ──────────────────────────────────────────────────────────

/// Check whether a container is currently running.
fn is_container_running(container_id: &str) -> bool {
    Command::new("docker")
        .args(["inspect", "-f", "{{.State.Running}}", container_id])
        .output()
        .map(|o| {
            o.status.success()
                && String::from_utf8_lossy(&o.stdout).trim() == "true"
        })
        .unwrap_or(false)
}

/// Force-remove a container (ignore errors).
pub(crate) fn remove_container(container_id: &str) {
    let _ = Command::new("docker")
        .args(["rm", "-f", container_id])
        .output();
}

/// Queries Docker for the host port mapped to container port 5432.
fn get_mapped_port(container_id: &str) -> Result<u16, DockerError> {
    let output = Command::new("docker")
        .args(["port", container_id, "5432"])
        .output()?;

    if !output.status.success() {
        return Err(DockerError::Docker(format!(
            "docker port failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().next().ok_or_else(|| {
        DockerError::Docker("docker port returned empty output".to_string())
    })?;

    let port_str = line.trim().rsplit(':').next().ok_or_else(|| {
        DockerError::Docker(format!("unexpected docker port output: {}", line))
    })?;

    port_str.trim().parse::<u16>().map_err(|e| {
        DockerError::Docker(format!("failed to parse port '{}': {}", port_str.trim(), e))
    })
}

/// Waits up to 30 seconds for PostgreSQL inside the container to accept connections.
fn wait_for_postgres(container_id: &str) -> Result<(), DockerError> {
    let deadline = Instant::now() + Duration::from_secs(30);

    while Instant::now() < deadline {
        let output = Command::new("docker")
            .args([
                "exec",
                container_id,
                "pg_isready",
                "-U",
                "postgres",
                "-d",
                "cubos_sql",
            ])
            .output()?;

        if output.status.success() {
            return Ok(());
        }

        thread::sleep(Duration::from_millis(500));
    }

    Err(DockerError::Docker(
        "PostgreSQL did not become ready within 30 seconds".to_string(),
    ))
}

// ─── Migration execution ──────────────────────────────────────────────────────

/// Connects to the running container and runs all `.sql` up-migrations in order.
pub fn run_migrations(info: &ContainerInfo, migrations_dir: &Path) -> Result<(), DockerError> {
    let conn_str = info.connection_string();

    // Retry connection a few times — pg_isready can return success slightly
    // before the server is fully accepting client connections.
    let mut client = None;
    for _ in 0..5 {
        match postgres::Client::connect(&conn_str, postgres::NoTls) {
            Ok(c) => { client = Some(c); break; }
            Err(_) => thread::sleep(Duration::from_millis(500)),
        }
    }
    let mut client = match client {
        Some(c) => c,
        None => postgres::Client::connect(&conn_str, postgres::NoTls)?,
    };

    let mut entries: Vec<_> = fs::read_dir(migrations_dir)
        .map_err(|e| {
            DockerError::Migration(format!(
                "failed to read migrations directory '{}': {}",
                migrations_dir.display(),
                e
            ))
        })?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            let path = entry.path();
            let name = path.to_string_lossy();
            name.ends_with(".sql") && !name.ends_with(".down.sql")
        })
        .collect();

    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let file_path = entry.path();
        let sql = fs::read_to_string(&file_path).map_err(|e| {
            DockerError::Migration(format!(
                "failed to read migration file '{}': {}",
                file_path.display(),
                e
            ))
        })?;

        client.batch_execute(&sql).map_err(|e| {
            DockerError::Migration(format!(
                "failed to execute migration '{}': {}",
                file_path.display(),
                e
            ))
        })?;
    }

    Ok(())
}

// ─── Container management ─────────────────────────────────────────────────────

/// Ensures a PostgreSQL Docker container is running with the right schema.
///
/// Lifecycle:
/// 1. Compute migration hash.
/// 2. Acquire file lock at `target/cubos_sql/<hash>/lock`.
/// 3. Check persisted state in `target/cubos_sql/<hash>/container.json`:
///    - If `ready: true` and container still running → reuse.
///    - If `ready: false` (interrupted migration) → remove and start fresh.
///    - If no state → start fresh.
/// 4. Start new container → write state with `ready: false` → run migrations
///    → update state to `ready: true`.
/// 5. On migration failure → remove container, delete state.
pub fn ensure_container(
    config: &cubos_sql_core::config::Config,
    manifest_dir: &Path,
) -> Result<(ContainerInfo, String), DockerError> {
    let migrations_dir = config.migrations_dir(manifest_dir);
    let hash = hash_migrations_dir(&migrations_dir)?;
    let base_dir = cubos_sql_dir(manifest_dir);

    let dir = cache_dir(&base_dir, &hash);
    fs::create_dir_all(&dir)?;

    // Acquire exclusive file lock.
    let lock_file_path = dir.join("lock");
    let lock_file = File::create(&lock_file_path)?;
    lock_file.lock_exclusive().map_err(|e| {
        DockerError::Io(std::io::Error::new(
            e.kind(),
            format!("failed to acquire lock: {}", e),
        ))
    })?;

    let sp = state_path(&base_dir, &hash);
    let result = ensure_container_inner(config, &migrations_dir, &base_dir, &hash, &sp);

    // Always release lock.
    let _ = lock_file.unlock();

    let info = result?;
    Ok((info, hash))
}

fn ensure_container_inner(
    config: &cubos_sql_core::config::Config,
    migrations_dir: &Path,
    base_dir: &Path,
    hash: &str,
    sp: &Path,
) -> Result<ContainerInfo, DockerError> {
    // Check existing state.
    if let Some(state) = read_state(sp) {
        if state.ready && is_container_running(&state.container_id) {
            // Verify port is still valid.
            match get_mapped_port(&state.container_id) {
                Ok(port) => {
                    // Update port in state in case it changed (shouldn't, but be safe).
                    if port != state.port {
                        let _ = write_state(sp, &ContainerState {
                            container_id: state.container_id.clone(),
                            port,
                            ready: true,
                        });
                    }
                    return Ok(ContainerInfo {
                        host: "127.0.0.1".to_string(),
                        port,
                        user: "postgres".to_string(),
                        password: "postgres".to_string(),
                        dbname: "cubos_sql".to_string(),
                    });
                }
                Err(_) => {
                    // Port query failed — container is in a bad state.
                    remove_container(&state.container_id);
                    let _ = fs::remove_file(sp);
                }
            }
        } else {
            // Not ready (interrupted migration) or not running — discard.
            remove_container(&state.container_id);
            let _ = fs::remove_file(sp);
            // Also clear cached queries since the schema is invalid.
            let queries_dir = cache_dir(base_dir, hash).join("queries");
            let _ = fs::remove_dir_all(&queries_dir);
        }
    }

    // Start a new container.
    let image = &config.database.docker_image;
    let hash_label = format!("cubos_sql_hash={}", hash);

    let run_output = Command::new("docker")
        .args([
            "run",
            "-d",
            "--label",
            "cubos_sql=true",
            "--label",
            &hash_label,
            "-e",
            "POSTGRES_PASSWORD=postgres",
            "-e",
            "POSTGRES_DB=cubos_sql",
            "-p",
            "0:5432",
            image,
        ])
        .output()?;

    if !run_output.status.success() {
        return Err(DockerError::Docker(format!(
            "docker run failed: {}",
            String::from_utf8_lossy(&run_output.stderr)
        )));
    }

    let container_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    if container_id.is_empty() {
        return Err(DockerError::Docker(
            "docker run produced no container ID".to_string(),
        ));
    }

    // Write state with ready=false BEFORE migrating.
    write_state(sp, &ContainerState {
        container_id: container_id.clone(),
        port: 0,
        ready: false,
    })?;

    // Wait for PostgreSQL to be ready.
    if let Err(e) = wait_for_postgres(&container_id) {
        remove_container(&container_id);
        let _ = fs::remove_file(sp);
        return Err(e);
    }

    // Get port.
    let port = match get_mapped_port(&container_id) {
        Ok(p) => p,
        Err(e) => {
            remove_container(&container_id);
            let _ = fs::remove_file(sp);
            return Err(e);
        }
    };

    let info = ContainerInfo {
        host: "127.0.0.1".to_string(),
        port,
        user: "postgres".to_string(),
        password: "postgres".to_string(),
        dbname: "cubos_sql".to_string(),
    };

    // Run migrations. On failure: remove container + clean up.
    if let Err(e) = run_migrations(&info, migrations_dir) {
        remove_container(&container_id);
        let _ = fs::remove_file(sp);
        return Err(e);
    }

    // Success — mark as ready.
    write_state(sp, &ContainerState {
        container_id,
        port,
        ready: true,
    })?;

    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_migrations_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("0001_create_users.sql"),
            "CREATE TABLE test_users (id SERIAL PRIMARY KEY, name TEXT NOT NULL);",
        ).unwrap();
        dir
    }

    fn test_config(migrations_dir: &Path) -> cubos_sql_core::config::Config {
        cubos_sql_core::config::Config {
            database: cubos_sql_core::config::DatabaseConfig {
                docker_image: "postgres".to_string(),
                migrations: migrations_dir.to_path_buf(),
            },
            migrations: cubos_sql_core::config::MigrationsConfig::default(),
            domains: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn hash_migrations_deterministic() {
        let dir = test_migrations_dir();
        let h1 = hash_migrations_dir(dir.path()).unwrap();
        let h2 = hash_migrations_dir(dir.path()).unwrap();
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // SHA-256 hex
    }

    #[test]
    fn hash_changes_with_content() {
        let dir1 = tempfile::tempdir().unwrap();
        std::fs::write(dir1.path().join("0001_a.sql"), "SELECT 1;").unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        std::fs::write(dir2.path().join("0001_a.sql"), "SELECT 2;").unwrap();

        let h1 = hash_migrations_dir(dir1.path()).unwrap();
        let h2 = hash_migrations_dir(dir2.path()).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn cubos_sql_dir_is_deterministic() {
        let dir = cubos_sql_dir(Path::new("/home/user/project"));
        assert_eq!(dir, Path::new("/home/user/project/.cubos_sql"));
    }

    #[test]
    #[ignore] // Requires Docker
    fn ensure_container_starts_and_reuses() {
        let mig_dir = test_migrations_dir();
        let config = test_config(mig_dir.path());

        // First call: should start a new container.
        // ensure_container uses cubos_sql_dir(manifest_dir) = mig_dir/.cubos_sql/
        let (info1, hash1) = ensure_container(&config, mig_dir.path()).unwrap();
        assert!(!hash1.is_empty());
        assert!(info1.port > 0);

        // Verify state file exists and is ready.
        let base = cubos_sql_dir(mig_dir.path());
        let sp = state_path(&base, &hash1);
        let state = read_state(&sp).unwrap();
        assert!(state.ready);

        // Second call: should reuse the container.
        let (info2, hash2) = ensure_container(&config, mig_dir.path()).unwrap();
        assert_eq!(hash1, hash2);
        assert_eq!(info1.port, info2.port);

        // Clean up.
        remove_container(&state.container_id);
    }

    #[test]
    #[ignore] // Requires Docker
    fn recovery_from_not_ready_container() {
        let mig_dir = test_migrations_dir();
        let config = test_config(mig_dir.path());

        // Start a container normally.
        let (_, hash) = ensure_container(&config, mig_dir.path()).unwrap();

        // Simulate crash: mark state as not ready.
        let base = cubos_sql_dir(mig_dir.path());
        let sp = state_path(&base, &hash);
        let mut state = read_state(&sp).unwrap();
        let old_id = state.container_id.clone();
        state.ready = false;
        write_state(&sp, &state).unwrap();

        // Next call should detect not-ready, remove old container, start fresh.
        let (info, _) = ensure_container(&config, mig_dir.path()).unwrap();
        assert!(info.port > 0);

        let new_state = read_state(&sp).unwrap();
        assert!(new_state.ready);
        assert_ne!(new_state.container_id, old_id);

        // Clean up.
        remove_container(&new_state.container_id);
    }

    #[test]
    #[ignore] // Requires Docker
    fn migration_failure_cleans_up() {
        let mig_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            mig_dir.path().join("0001_bad.sql"),
            "THIS IS NOT VALID SQL!!!",
        ).unwrap();

        let config = test_config(mig_dir.path());

        // Should fail.
        let result = ensure_container(&config, mig_dir.path());
        assert!(result.is_err());

        // State file should not exist (cleaned up after failure).
        let hash = hash_migrations_dir(mig_dir.path()).unwrap();
        let base = cubos_sql_dir(mig_dir.path());
        let sp = state_path(&base, &hash);
        assert!(!sp.exists(), "state file should be cleaned up after failure");
    }
}
