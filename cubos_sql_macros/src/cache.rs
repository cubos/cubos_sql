//! Query introspection cache backed by the filesystem.
//!
//! Results are stored under `target/cubos_sql/<migration_hash>/queries/<query_hash>.json`.
//! A cache hit avoids connecting to PostgreSQL entirely, making repeated
//! compilations near-instantaneous.

use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

use cubos_sql_core::query_info::QueryInfo;

/// Returns the cache file path for a query.
///
/// The path incorporates the migration hash, the SQL text, and a config hash
/// (covering `[domains]`, `[enums]`, `[types]`) so that changes to type
/// mappings correctly invalidate cached introspection results.
pub fn query_cache_path(
    target_dir: &Path,
    migration_hash: &str,
    sql: &str,
    config_hash: &str,
) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(sql.as_bytes());
    hasher.update(config_hash.as_bytes());
    let query_hash = format!("{:x}", hasher.finalize());

    target_dir
        .join(migration_hash)
        .join("queries")
        .join(format!("{}.json", &query_hash[..32]))
}

/// Try to read a cached `QueryInfo` from disk.
pub fn get(path: &Path) -> Option<QueryInfo> {
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Persist a `QueryInfo` to disk.
pub fn put(path: &Path, info: &QueryInfo) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string(info).map_err(std::io::Error::other)?;
    fs::write(path, json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cubos_sql_core::query_info::{ColumnInfo, ParamInfo, QueryInfo};

    #[test]
    fn cache_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = query_cache_path(dir.path(), "abc123", "SELECT 1", "");

        let info = QueryInfo {
            params: vec![ParamInfo {
                pg_type_oid: 23,
                rust_type: "i32".to_string(),
                nullable: false,
                domain_rust_type: None,
                enum_rust_type: None,
            }],
            columns: vec![ColumnInfo {
                name: "id".to_string(),
                pg_type_oid: 20,
                rust_type: "i64".to_string(),
                nullable: false,
                domain_rust_type: None,
                enum_rust_type: None,
            }],
        };

        assert!(get(&path).is_none());
        put(&path, &info).unwrap();

        let cached = get(&path).unwrap();
        assert_eq!(cached.params.len(), 1);
        assert_eq!(cached.columns.len(), 1);
        assert_eq!(cached.columns[0].name, "id");
        assert_eq!(cached.params[0].rust_type, "i32");
    }

    #[test]
    fn different_queries_different_paths() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = query_cache_path(dir.path(), "hash1", "SELECT 1", "cfg1");
        let p2 = query_cache_path(dir.path(), "hash1", "SELECT 2", "cfg1");
        let p3 = query_cache_path(dir.path(), "hash2", "SELECT 1", "cfg1");
        let p4 = query_cache_path(dir.path(), "hash1", "SELECT 1", "cfg2");
        assert_ne!(p1, p2, "different SQL → different path");
        assert_ne!(p1, p3, "different migration hash → different path");
        assert_ne!(p1, p4, "different config hash → different path");
    }

    #[test]
    fn missing_cache_returns_none() {
        let path = std::path::Path::new("/tmp/nonexistent_cubos_sql_cache.json");
        assert!(get(path).is_none());
    }
}
