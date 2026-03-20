use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Top-level configuration for `cubos_sql`, read from `[package.metadata.cubos_sql]` in `Cargo.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Database-related settings (Docker image, migrations path).
    pub database: DatabaseConfig,
    /// Migration runner settings (tracking table, lock ID, transaction behavior).
    #[serde(default)]
    pub migrations: MigrationsConfig,
    /// Custom JSONB domain mappings: PostgreSQL domain name to Rust type path.
    #[serde(default)]
    pub domains: HashMap<String, String>,
}

/// Database configuration section.
#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    /// Docker image to use for the compile-time PostgreSQL container.
    /// Default: `"postgres"`
    #[serde(default = "DatabaseConfig::default_docker_image")]
    pub docker_image: String,
    /// Path to the migrations directory, relative to the project root or absolute.
    pub migrations: PathBuf,
}

impl DatabaseConfig {
    fn default_docker_image() -> String {
        "postgres".to_string()
    }
}

/// Configuration for the migration runner behavior.
#[derive(Debug, Clone, Deserialize)]
pub struct MigrationsConfig {
    /// Fully qualified table name for tracking applied migrations.
    /// Default: "public._migrations"
    #[serde(default = "MigrationsConfig::default_table")]
    pub table: String,

    /// Advisory lock ID to prevent concurrent migration execution.
    /// Default: 713705
    #[serde(default = "MigrationsConfig::default_lock_id")]
    pub lock_id: i64,

    /// Whether migrations run inside a transaction by default.
    /// Individual migrations can override this with `-- no-transaction` on the first line.
    /// Default: true
    #[serde(default = "MigrationsConfig::default_use_transaction")]
    pub use_transaction: bool,
}

impl Default for MigrationsConfig {
    fn default() -> Self {
        Self {
            table: Self::default_table(),
            lock_id: Self::default_lock_id(),
            use_transaction: Self::default_use_transaction(),
        }
    }
}

impl MigrationsConfig {
    fn default_table() -> String {
        "public._migrations".to_string()
    }

    fn default_lock_id() -> i64 {
        713705
    }

    fn default_use_transaction() -> bool {
        true
    }

    /// Validates the `table` field as a safe PostgreSQL qualified identifier.
    ///
    /// Accepts `name` or `schema.name`, where each part matches `[a-zA-Z_][a-zA-Z0-9_]*`.
    /// This prevents SQL injection since the table name is interpolated into queries.
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_qualified_name(&self.table).map_err(|msg| {
            ConfigError::InvalidTable {
                table: self.table.clone(),
                reason: msg,
            }
        })
    }
}

/// Checks if a string is a valid unquoted PG identifier: `[a-zA-Z_][a-zA-Z0-9_]*`.
fn is_pg_ident_unquoted(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Checks if a string is a valid quoted PG identifier: `"..."` with no embedded
/// null bytes. Double quotes inside must be escaped as `""`.
fn is_pg_ident_quoted(s: &str) -> bool {
    if s.len() < 3 || !s.starts_with('"') || !s.ends_with('"') {
        return false;
    }
    let inner = &s[1..s.len() - 1];
    if inner.is_empty() || inner.contains('\0') {
        return false;
    }
    // Ensure no unescaped double quotes (consecutive `""` are fine)
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '"' {
            if chars.next() != Some('"') {
                return false;
            }
        }
    }
    true
}

fn is_pg_ident(s: &str) -> bool {
    is_pg_ident_unquoted(s) || is_pg_ident_quoted(s)
}

fn validate_qualified_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("table name cannot be empty".into());
    }

    // Split on '.' but respect quoted identifiers
    let parts = split_qualified_name(name);
    if parts.is_empty() || parts.len() > 2 {
        return Err("expected format: 'name' or 'schema.name'".into());
    }

    for part in &parts {
        if !is_pg_ident(part) {
            return Err(format!(
                "'{}' is not a valid PostgreSQL identifier",
                part
            ));
        }
    }

    Ok(())
}

/// Splits a qualified name like `schema.name` or `"my schema"."my table"` into parts.
fn split_qualified_name(name: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_quote = false;
    let bytes = name.as_bytes();

    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            in_quote = !in_quote;
        } else if bytes[i] == b'.' && !in_quote {
            parts.push(&name[start..i]);
            start = i + 1;
        }
        i += 1;
    }
    parts.push(&name[start..]);
    parts
}

/// Wrapper to extract `[package.metadata.cubos_sql]` from a full Cargo.toml.
#[derive(Debug, Deserialize)]
struct CargoToml {
    package: Package,
}

#[derive(Debug, Deserialize)]
struct Package {
    metadata: Metadata,
}

#[derive(Debug, Deserialize)]
struct Metadata {
    cubos_sql: Config,
}

/// Errors that can occur when loading or parsing the `cubos_sql` configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse Cargo.toml: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("missing [package.metadata.cubos_sql] section in Cargo.toml")]
    MissingSection,
    #[error("invalid migration table name '{table}': {reason}")]
    InvalidTable { table: String, reason: String },
}

impl Config {
    /// Load config from a `Cargo.toml` file.
    pub fn from_cargo_toml(path: &Path) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
            path: path.to_owned(),
            source: e,
        })?;
        Self::from_str(&content)
    }

    /// Parse config from a TOML string (the contents of a Cargo.toml).
    pub fn from_str(content: &str) -> Result<Self, ConfigError> {
        let cargo: CargoToml = toml::from_str(content)?;
        Ok(cargo.package.metadata.cubos_sql)
    }

    /// Resolve the migrations path relative to a base directory.
    pub fn migrations_dir(&self, base: &Path) -> PathBuf {
        if self.database.migrations.is_absolute() {
            self.database.migrations.clone()
        } else {
            base.join(&self.database.migrations)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_TOML: &str = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.cubos_sql]
[package.metadata.cubos_sql.database]
docker_image = "postgres:16"
migrations = "./migrations"

[package.metadata.cubos_sql.domains]
user_preferences = "crate::domains::UserPreferences"
order_metadata = "crate::domains::OrderMetadata"
"#;

    const MINIMAL_TOML: &str = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.cubos_sql.database]
migrations = "./migrations"
"#;

    #[test]
    fn parse_full_config() {
        let config = Config::from_str(VALID_TOML).unwrap();
        assert_eq!(config.database.docker_image, "postgres:16");
        assert_eq!(config.database.migrations, PathBuf::from("./migrations"));
        assert_eq!(config.domains.len(), 2);
        assert_eq!(
            config.domains.get("user_preferences").unwrap(),
            "crate::domains::UserPreferences"
        );
        // Defaults for migrations config
        assert_eq!(config.migrations.table, "public._migrations");
        assert_eq!(config.migrations.lock_id, 713705);
        assert!(config.migrations.use_transaction);
    }

    #[test]
    fn parse_minimal_config() {
        let config = Config::from_str(MINIMAL_TOML).unwrap();
        assert_eq!(config.database.docker_image, "postgres");
        assert!(config.domains.is_empty());
        assert_eq!(config.migrations.table, "public._migrations");
    }

    #[test]
    fn parse_custom_migrations_config() {
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.cubos_sql.database]
docker_image = "postgres:16"
migrations = "./migrations"

[package.metadata.cubos_sql.migrations]
table = "myschema._my_migrations"
lock_id = 999
use_transaction = false
"#;
        let config = Config::from_str(toml).unwrap();
        assert_eq!(config.migrations.table, "myschema._my_migrations");
        assert_eq!(config.migrations.lock_id, 999);
        assert!(!config.migrations.use_transaction);
    }

    #[test]
    fn missing_section_errors() {
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"
"#;
        let result = Config::from_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn migrations_dir_relative() {
        let config = Config::from_str(MINIMAL_TOML).unwrap();
        let dir = config.migrations_dir(Path::new("/home/user/project"));
        assert_eq!(dir, PathBuf::from("/home/user/project/./migrations"));
    }

    #[test]
    fn migrations_dir_absolute() {
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.cubos_sql.database]
docker_image = "postgres:16"
migrations = "/opt/migrations"
"#;
        let config = Config::from_str(toml).unwrap();
        let dir = config.migrations_dir(Path::new("/home/user/project"));
        assert_eq!(dir, PathBuf::from("/opt/migrations"));
    }

    #[test]
    fn validate_default_table() {
        let config = MigrationsConfig::default();
        config.validate().unwrap();
    }

    #[test]
    fn validate_unqualified_table() {
        let config = MigrationsConfig { table: "_migrations".into(), ..Default::default() };
        config.validate().unwrap();
    }

    #[test]
    fn validate_qualified_table() {
        let config = MigrationsConfig { table: "my_schema._migrations".into(), ..Default::default() };
        config.validate().unwrap();
    }

    #[test]
    fn validate_quoted_table() {
        let config = MigrationsConfig { table: r#""my-schema"."my-migrations""#.into(), ..Default::default() };
        config.validate().unwrap();
    }

    #[test]
    fn validate_mixed_quoted_unquoted() {
        let config = MigrationsConfig { table: r#"public."my-migrations""#.into(), ..Default::default() };
        config.validate().unwrap();
    }

    #[test]
    fn validate_rejects_sql_injection() {
        let config = MigrationsConfig { table: "public; DROP TABLE users --".into(), ..Default::default() };
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty() {
        let config = MigrationsConfig { table: "".into(), ..Default::default() };
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_too_many_parts() {
        let config = MigrationsConfig { table: "a.b.c".into(), ..Default::default() };
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_quoted() {
        let config = MigrationsConfig { table: r#""""#.into(), ..Default::default() };
        assert!(config.validate().is_err());
    }
}
