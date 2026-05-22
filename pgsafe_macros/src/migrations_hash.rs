//! Migration file hashing for cache key computation.

use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

/// Reads all `.sql` files in `path` (sorted by name) and returns the
/// hex-encoded SHA-256 hash of the concatenated `filename + content` strings.
///
/// If the directory does not exist, returns the hash of an empty input
/// (equivalent to zero migrations).
#[cfg(test)]
pub fn hash_migrations_dir(path: &Path) -> Result<String, std::io::Error> {
    hash_migrations_dirs(&[path])
}

/// Reads all `.sql` files across multiple directories (each sorted by name)
/// and returns the hex-encoded SHA-256 hash. Directories are processed in
/// order, and non-existent directories are skipped.
pub fn hash_migrations_dirs(paths: &[&Path]) -> Result<String, std::io::Error> {
    let mut hasher = Sha256::new();

    for path in paths {
        if !path.is_dir() {
            continue;
        }

        let mut entries: Vec<_> = fs::read_dir(path)?
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
            let file_name = entry.file_name();
            let name_str = file_name.to_string_lossy();

            let content = fs::read_to_string(&file_path)?;

            hasher.update(name_str.as_bytes());
            hasher.update(content.as_bytes());
        }
    }

    let result = hasher.finalize();
    Ok(result
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_migrations_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("0001_create_users.sql"),
            "CREATE TABLE test_users (id SERIAL PRIMARY KEY, name TEXT NOT NULL);",
        )
        .unwrap();

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
}
