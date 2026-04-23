//! Configuration types for `cubos_sql`.
//!
//! The configuration is read from the `[package.metadata.cubos_sql]` section
//! of the consumer's `Cargo.toml`. A complete example:
//!
//! ```toml
//! [package]
//! name = "my-app"
//! version = "0.1.0"
//! edition = "2021"
//!
//! [package.metadata.cubos_sql.database]
//! migrations = "./migrations"    # path to SQL migration files (required)
//! extra_migrations = ["../other-crate/migrations"]  # extra migrations for compile-time only
//!
//! [package.metadata.cubos_sql.migrations]
//! table = "public._migrations"   # tracking table name (default: "public._migrations")
//! lock_id = 713705               # advisory lock ID (default: 713705)
//! use_transaction = true         # wrap each migration in a transaction (default: true)
//!
//! [package.metadata.cubos_sql.domains]
//! user_preferences = "crate::domains::UserPreferences"
//! order_metadata = "crate::domains::OrderMetadata"
//!
//! [package.metadata.cubos_sql.types]
//! "public.citext" = "String"                   # custom PG type → Rust type
//! "extensions.ltree" = "String"                # schema-qualified lookup
//! citext = "String"                            # also works without schema
//! ```
//!
//! Custom types in `[types]` are looked up by `schema.name` (or just `name`)
//! in `pg_catalog` at compile time. The mapped Rust type must implement
//! `tokio_postgres::types::ToSql + FromSql`. Array versions (`type[]`) are
//! supported automatically as `Vec<RustType>`.
//!
//! All fields have sensible defaults. The `[package.metadata.cubos_sql]` section
//! itself is required, but every field within it is optional.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::qualified_name::{ParseQualifiedNameError, QualifiedName};

/// Top-level configuration for `cubos_sql`, read from `[package.metadata.cubos_sql]` in `Cargo.toml`.
///
/// Load this from a `Cargo.toml` file with [`Config::from_cargo_toml`], or
/// parse a TOML string directly via [`str::parse`].
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    /// Database-related settings (migrations path).
    #[serde(default)]
    pub database: DatabaseConfig,
    /// Migration runner settings (tracking table, lock ID, transaction behavior).
    #[serde(default)]
    pub migrations: MigrationsConfig,
    /// Custom JSONB domain mappings: PostgreSQL domain name to Rust type path.
    #[serde(default)]
    pub domains: HashMap<String, String>,
    /// Custom enum mappings: PostgreSQL enum type name to Rust type path.
    /// The Rust type must implement `ToString` + `FromStr` for serialization.
    #[serde(default)]
    pub enums: HashMap<String, String>,
    /// Custom type mappings: `"schema.type_name"` or `"type_name"` to Rust type path.
    /// The Rust type must implement `tokio_postgres::types::ToSql` + `FromSql`.
    /// Array versions are supported automatically as `Vec<RustType>`.
    #[serde(default)]
    pub types: HashMap<String, String>,
    /// Named database configurations for multi-database support.
    /// Each key maps to a complete database configuration.
    /// Used with `sql!(db = name, ...)` syntax.
    #[serde(default)]
    pub databases: HashMap<String, DatabaseEntry>,
}

/// A named database entry for multi-database support.
///
/// Configured under `[package.metadata.cubos_sql.databases.<name>]` in `Cargo.toml`.
/// Each entry has its own database settings, migrations, domain/enum/type mappings.
#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseEntry {
    /// Database-related settings (migrations path).
    #[serde(default)]
    pub database: DatabaseConfig,
    /// Migration runner settings (tracking table, lock ID, transaction behavior).
    #[serde(default)]
    pub migrations: MigrationsConfig,
    /// Custom JSONB domain mappings.
    #[serde(default)]
    pub domains: HashMap<String, String>,
    /// Custom enum mappings.
    #[serde(default)]
    pub enums: HashMap<String, String>,
    /// Custom type mappings.
    #[serde(default)]
    pub types: HashMap<String, String>,
}

/// Database-related configuration.
///
/// Specifies the path to the SQL migration files.
#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    /// Path to the migrations directory, relative to the project root or absolute.
    /// Default: `"./migrations"`. If the directory does not exist, it is treated
    /// as having zero migrations.
    #[serde(default = "DatabaseConfig::default_migrations")]
    pub migrations: PathBuf,
    /// Additional migration directories from other crates to include at compile
    /// time. These are used only for static analysis and type checking — they are
    /// NOT executed by the runtime migration runner.
    /// Paths are relative to the project root or absolute.
    #[serde(default)]
    pub extra_migrations: Vec<PathBuf>,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            migrations: Self::default_migrations(),
            extra_migrations: Vec::new(),
        }
    }
}

impl DatabaseConfig {
    fn default_migrations() -> PathBuf {
        PathBuf::from("./migrations")
    }
}

/// Configuration for the migration runner behavior.
///
/// Controls how migrations are tracked and executed. All fields have defaults,
/// so this entire section can be omitted from `Cargo.toml`.
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
        validate_qualified_name(&self.table).map_err(|msg| ConfigError::InvalidTable {
            table: self.table.clone(),
            reason: msg,
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
        if c == '"' && chars.next() != Some('"') {
            return false;
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
            return Err(format!("'{}' is not a valid PostgreSQL identifier", part));
        }
    }

    Ok(())
}

/// Splits a qualified name like `schema.name` or `"my schema"."my table"` into parts.
/// Handles escaped double-quotes (`""`) inside quoted identifiers.
fn split_qualified_name(name: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_quote = false;
    let bytes = name.as_bytes();

    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            if in_quote && i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                // Escaped double-quote inside quoted identifier — skip both
                i += 2;
                continue;
            }
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
#[derive(Debug, Default, Deserialize)]
struct CargoToml {
    #[serde(default)]
    package: Package,
}

#[derive(Debug, Default, Deserialize)]
struct Package {
    #[serde(default)]
    metadata: Metadata,
}

#[derive(Debug, Default, Deserialize)]
struct Metadata {
    #[serde(default)]
    cubos_sql: Config,
}

/// Errors that can occur when loading or parsing the `cubos_sql` configuration.
///
/// Returned by [`Config::from_cargo_toml`] and [`MigrationsConfig::validate`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse Cargo.toml: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid migration table name '{table}': {reason}")]
    InvalidTable { table: String, reason: String },
    #[error("unknown database '{0}' — not found in [package.metadata.cubos_sql.databases]")]
    UnknownDatabase(String),
    #[error("invalid qualified name in [{section}] key '{key}': {source}")]
    InvalidQualifiedName {
        section: &'static str,
        key: String,
        #[source]
        source: ParseQualifiedNameError,
    },
}

impl std::str::FromStr for Config {
    type Err = ConfigError;

    fn from_str(content: &str) -> Result<Self, Self::Err> {
        let cargo: CargoToml = toml::from_str(content)?;
        let config = cargo.package.metadata.cubos_sql;
        config.migrations.validate()?;
        Ok(config)
    }
}

impl Config {
    /// Load config from a `Cargo.toml` file.
    pub fn from_cargo_toml(path: &Path) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
            path: path.to_owned(),
            source: e,
        })?;
        content.parse()
    }

    /// Resolve the migrations path relative to a base directory.
    pub fn migrations_dir(&self, base: &Path) -> PathBuf {
        if self.database.migrations.is_absolute() {
            self.database.migrations.clone()
        } else {
            base.join(&self.database.migrations)
        }
    }

    /// Resolve extra migration paths relative to a base directory.
    /// These are migration directories from other crates, used only at compile time.
    pub fn extra_migrations_dirs(&self, base: &Path) -> Vec<PathBuf> {
        self.database
            .extra_migrations
            .iter()
            .map(|p| {
                if p.is_absolute() {
                    p.clone()
                } else {
                    base.join(p)
                }
            })
            .collect()
    }

    /// Resolve configuration for a specific database.
    ///
    /// - `None` returns the default (top-level) config.
    /// - `Some("name")` looks up `[databases.name]` and returns an error if not found.
    pub fn resolve(&self, db_name: Option<&str>) -> Result<ResolvedConfig<'_>, ConfigError> {
        match db_name {
            None => Ok(ResolvedConfig {
                database: &self.database,
                migrations: &self.migrations,
                domains: qualify_keys(&self.domains, "domains")?,
                enums: qualify_keys(&self.enums, "enums")?,
                types: qualify_keys(&self.types, "types")?,
            }),
            Some(name) => {
                let entry = self
                    .databases
                    .get(name)
                    .ok_or_else(|| ConfigError::UnknownDatabase(name.to_string()))?;
                Ok(ResolvedConfig {
                    database: &entry.database,
                    migrations: &entry.migrations,
                    domains: qualify_keys(&entry.domains, "domains")?,
                    enums: qualify_keys(&entry.enums, "enums")?,
                    types: qualify_keys(&entry.types, "types")?,
                })
            }
        }
    }
}

/// A resolved view of a single database's configuration.
///
/// Returned by [`Config::resolve`]. Borrows database/migration settings from the
/// parent [`Config`], but owns the type-mapping HashMaps because keys are normalized
/// into [`QualifiedName`]s. Bare names in Cargo.toml default to the `public` schema.
#[derive(Debug, Clone)]
pub struct ResolvedConfig<'a> {
    pub database: &'a DatabaseConfig,
    pub migrations: &'a MigrationsConfig,
    pub domains: HashMap<QualifiedName, String>,
    pub enums: HashMap<QualifiedName, String>,
    pub types: HashMap<QualifiedName, String>,
}

/// Parse type-mapping keys into [`QualifiedName`]s.
///
/// Respects PostgreSQL quoting rules (`"My Schema"."My Type"`). Bare names
/// without a dot are interpreted as living in the `public` schema.
fn qualify_keys(
    map: &HashMap<String, String>,
    section: &'static str,
) -> Result<HashMap<QualifiedName, String>, ConfigError> {
    map.iter()
        .map(|(k, v)| {
            let key = if k.contains('.') {
                k.parse::<QualifiedName>()
                    .map_err(|source| ConfigError::InvalidQualifiedName {
                        section,
                        key: k.clone(),
                        source,
                    })?
            } else {
                QualifiedName::new("public", k.clone())
            };
            Ok((key, v.clone()))
        })
        .collect()
}

impl ResolvedConfig<'_> {
    /// Resolve the migrations path relative to a base directory.
    pub fn migrations_dir(&self, base: &Path) -> PathBuf {
        if self.database.migrations.is_absolute() {
            self.database.migrations.clone()
        } else {
            base.join(&self.database.migrations)
        }
    }

    /// Resolve extra migration paths relative to a base directory.
    pub fn extra_migrations_dirs(&self, base: &Path) -> Vec<PathBuf> {
        self.database
            .extra_migrations
            .iter()
            .map(|p| {
                if p.is_absolute() {
                    p.clone()
                } else {
                    base.join(p)
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    const VALID_TOML: &str = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.cubos_sql]
[package.metadata.cubos_sql.database]
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
        assert!(config.domains.is_empty());
        assert_eq!(config.migrations.table, "public._migrations");
    }

    #[test]
    fn parse_bare_minimum_config() {
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.cubos_sql]
"#;
        let config = Config::from_str(toml).unwrap();
        assert_eq!(config.database.migrations, PathBuf::from("./migrations"));
        assert!(config.domains.is_empty());
    }

    #[test]
    fn parse_custom_migrations_config() {
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.cubos_sql.database]
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
    fn missing_section_uses_defaults() {
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"
"#;
        let config = Config::from_str(toml).unwrap();
        assert_eq!(config.database.migrations, PathBuf::from("./migrations"));
    }

    #[test]
    fn legacy_analysis_mode_ignored() {
        // Old Cargo.toml files with analysis_mode should still parse fine.
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.cubos_sql]
analysis_mode = "static"
"#;
        Config::from_str(toml).unwrap();
    }

    #[test]
    fn legacy_docker_image_ignored() {
        // Old Cargo.toml files with docker_image should still parse fine.
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.cubos_sql.database]
docker_image = "postgres:16"
migrations = "./migrations"
"#;
        Config::from_str(toml).unwrap();
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
        let config = MigrationsConfig {
            table: "_migrations".into(),
            ..Default::default()
        };
        config.validate().unwrap();
    }

    #[test]
    fn validate_qualified_table() {
        let config = MigrationsConfig {
            table: "my_schema._migrations".into(),
            ..Default::default()
        };
        config.validate().unwrap();
    }

    #[test]
    fn validate_quoted_table() {
        let config = MigrationsConfig {
            table: r#""my-schema"."my-migrations""#.into(),
            ..Default::default()
        };
        config.validate().unwrap();
    }

    #[test]
    fn validate_mixed_quoted_unquoted() {
        let config = MigrationsConfig {
            table: r#"public."my-migrations""#.into(),
            ..Default::default()
        };
        config.validate().unwrap();
    }

    #[test]
    fn validate_rejects_sql_injection() {
        let config = MigrationsConfig {
            table: "public; DROP TABLE users --".into(),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty() {
        let config = MigrationsConfig {
            table: "".into(),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_too_many_parts() {
        let config = MigrationsConfig {
            table: "a.b.c".into(),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_quoted() {
        let config = MigrationsConfig {
            table: r#""""#.into(),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn parse_multi_db_config() {
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.cubos_sql.database]
migrations = "./migrations/main"

[package.metadata.cubos_sql.databases.analytics]
[package.metadata.cubos_sql.databases.analytics.database]
migrations = "./migrations/analytics"

[package.metadata.cubos_sql.databases.analytics.migrations]
table = "public._analytics_migrations"

[package.metadata.cubos_sql.databases.analytics.domains]
event_data = "crate::EventData"
"#;
        let config = Config::from_str(toml).unwrap();
        assert_eq!(
            config.database.migrations,
            PathBuf::from("./migrations/main")
        );
        assert_eq!(config.databases.len(), 1);

        let analytics = config.databases.get("analytics").unwrap();
        assert_eq!(
            analytics.database.migrations,
            PathBuf::from("./migrations/analytics")
        );
        assert_eq!(analytics.migrations.table, "public._analytics_migrations");
        assert_eq!(
            analytics.domains.get("event_data").unwrap(),
            "crate::EventData"
        );
    }

    #[test]
    fn resolve_default_db() {
        let config = Config::from_str(MINIMAL_TOML).unwrap();
        let resolved = config.resolve(None).unwrap();
        assert_eq!(resolved.database.migrations, PathBuf::from("./migrations"));
    }

    #[test]
    fn resolve_named_db() {
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.cubos_sql.database]
migrations = "./migrations/main"

[package.metadata.cubos_sql.databases.analytics.database]
migrations = "./migrations/analytics"
"#;
        let config = Config::from_str(toml).unwrap();
        let resolved = config.resolve(Some("analytics")).unwrap();
        assert_eq!(
            resolved.database.migrations,
            PathBuf::from("./migrations/analytics")
        );
    }

    #[test]
    fn resolve_unknown_db_errors() {
        let config = Config::from_str(MINIMAL_TOML).unwrap();
        assert!(config.resolve(Some("nonexistent")).is_err());
    }

    #[test]
    fn resolve_qualifies_unqualified_domain_keys() {
        let config = Config::from_str(VALID_TOML).unwrap();
        let resolved = config.resolve(None).unwrap();
        // Unqualified "user_preferences" becomes "public.user_preferences"
        assert_eq!(
            resolved
                .domains
                .get(&QualifiedName::new("public", "user_preferences"))
                .unwrap(),
            "crate::domains::UserPreferences"
        );
        assert_eq!(
            resolved
                .domains
                .get(&QualifiedName::new("public", "order_metadata"))
                .unwrap(),
            "crate::domains::OrderMetadata"
        );
    }

    #[test]
    fn resolve_preserves_qualified_domain_keys() {
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.cubos_sql.database]
migrations = "./migrations"

[package.metadata.cubos_sql.domains]
"whatsapp.health_data" = "crate::domains::HealthData"
"whatsapp.qr_data" = "crate::domains::QrData"
user_preferences = "crate::domains::UserPreferences"
"#;
        let config = Config::from_str(toml).unwrap();
        let resolved = config.resolve(None).unwrap();
        // Schema-qualified keys are preserved as-is
        assert_eq!(
            resolved
                .domains
                .get(&QualifiedName::new("whatsapp", "health_data"))
                .unwrap(),
            "crate::domains::HealthData"
        );
        assert_eq!(
            resolved
                .domains
                .get(&QualifiedName::new("whatsapp", "qr_data"))
                .unwrap(),
            "crate::domains::QrData"
        );
        // Unqualified key gets "public." prefix
        assert_eq!(
            resolved
                .domains
                .get(&QualifiedName::new("public", "user_preferences"))
                .unwrap(),
            "crate::domains::UserPreferences"
        );
    }

    #[test]
    fn resolve_qualifies_enum_and_type_keys() {
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.cubos_sql.database]
migrations = "./migrations"

[package.metadata.cubos_sql.enums]
user_role = "crate::UserRole"
"custom_schema.status" = "crate::Status"

[package.metadata.cubos_sql.types]
point = "crate::Point"
"geo.polygon" = "crate::Polygon"
"#;
        let config = Config::from_str(toml).unwrap();
        let resolved = config.resolve(None).unwrap();

        assert_eq!(
            resolved
                .enums
                .get(&QualifiedName::new("public", "user_role"))
                .unwrap(),
            "crate::UserRole"
        );
        assert_eq!(
            resolved
                .enums
                .get(&QualifiedName::new("custom_schema", "status"))
                .unwrap(),
            "crate::Status"
        );

        assert_eq!(
            resolved
                .types
                .get(&QualifiedName::new("public", "point"))
                .unwrap(),
            "crate::Point"
        );
        assert_eq!(
            resolved
                .types
                .get(&QualifiedName::new("geo", "polygon"))
                .unwrap(),
            "crate::Polygon"
        );
    }

    #[test]
    fn resolve_qualifies_named_db_keys() {
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.cubos_sql.database]
migrations = "./migrations/main"

[package.metadata.cubos_sql.databases.analytics.database]
migrations = "./migrations/analytics"

[package.metadata.cubos_sql.databases.analytics.domains]
event_data = "crate::EventData"
"stats.metric" = "crate::Metric"
"#;
        let config = Config::from_str(toml).unwrap();
        let resolved = config.resolve(Some("analytics")).unwrap();
        assert_eq!(
            resolved
                .domains
                .get(&QualifiedName::new("public", "event_data"))
                .unwrap(),
            "crate::EventData"
        );
        assert_eq!(
            resolved
                .domains
                .get(&QualifiedName::new("stats", "metric"))
                .unwrap(),
            "crate::Metric"
        );
    }
}
