//! Storage layout resolution.
//!
//! Rewind never writes snapshot data inside the tracked repository. Instead it
//! keeps a per-repository directory under the operating system's
//! application-data directory, keyed by a stable hash of the repository's
//! canonical path:
//!
//! ```text
//! <app-data>/rewind/<repository-hash>/
//!   rewind.db
//!   objects/
//!   logs/
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Result, RewindError};

/// Environment variable that overrides the base data directory. Primarily used
/// by the test-suite and by users who want an alternate location.
pub const DATA_DIR_ENV: &str = "REWIND_DATA_DIR";

/// Resolved on-disk locations for a single repository's Rewind data.
#[derive(Debug, Clone)]
pub struct StoragePaths {
    /// The repository root this storage belongs to (canonicalized).
    pub repo_root: PathBuf,
    /// The stable hash derived from `repo_root`.
    pub repo_hash: String,
    /// `<app-data>/rewind/<hash>/`
    pub root: PathBuf,
    /// SQLite database file.
    pub db: PathBuf,
    /// Content-addressed object store directory.
    pub objects: PathBuf,
    /// Structured log directory.
    pub logs: PathBuf,
}

impl StoragePaths {
    /// Compute the storage layout for a canonical repository root.
    ///
    /// The base directory is `$REWIND_DATA_DIR` when set, otherwise the
    /// platform data directory (e.g. `~/.local/share` on Linux,
    /// `~/Library/Application Support` on macOS) with a `rewind` subdirectory.
    pub fn for_repo(repo_root: &Path) -> Result<Self> {
        let base = base_data_dir()?;
        Self::for_repo_in(&base, repo_root)
    }

    /// Like [`StoragePaths::for_repo`], but with an explicit base directory,
    /// ignoring the environment. Used by tests and embedders.
    pub fn for_repo_in(base: &Path, repo_root: &Path) -> Result<Self> {
        let canonical = repo_root
            .canonicalize()
            .map_err(|e| RewindError::other(format!("cannot canonicalize {repo_root:?}: {e}")))?;
        let repo_hash = repo_hash(&canonical);
        let root = base.join("rewind").join(&repo_hash);
        Ok(StoragePaths {
            db: root.join("rewind.db"),
            objects: root.join("objects"),
            logs: root.join("logs"),
            root,
            repo_root: canonical,
            repo_hash,
        })
    }

    /// Create the directory tree, applying restrictive permissions where the
    /// platform supports it. Safe to call repeatedly.
    pub fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.root)?;
        fs::create_dir_all(&self.objects)?;
        fs::create_dir_all(&self.logs)?;
        restrict_permissions(&self.root)?;
        Ok(())
    }

    /// Path at which a given object hash is stored, sharded by the first two
    /// hex characters to avoid oversized directories.
    pub fn object_path(&self, hash: &str) -> PathBuf {
        let (shard, rest) = hash.split_at(2.min(hash.len()));
        self.objects.join(shard).join(rest)
    }
}

/// Resolve the base data directory, honoring [`DATA_DIR_ENV`].
pub fn base_data_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV) {
        let p = PathBuf::from(dir);
        if p.as_os_str().is_empty() {
            return Err(RewindError::Config(format!(
                "{DATA_DIR_ENV} is set but empty"
            )));
        }
        return Ok(p);
    }
    dirs::data_dir()
        .ok_or_else(|| RewindError::other("could not determine the platform data directory"))
}

/// Compute the stable repository hash from a canonical path.
///
/// Uses BLAKE3 over the canonical path bytes and keeps the first 16 bytes as
/// hex (32 characters) — short enough for a directory name, wide enough to
/// avoid collisions in practice.
pub fn repo_hash(canonical: &Path) -> String {
    let bytes = path_bytes(canonical);
    let hash = blake3::hash(&bytes);
    hash.to_hex()[..32].to_string()
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().as_bytes().to_vec()
}

/// Apply `0700` permissions on Unix. A no-op elsewhere.
#[cfg(unix)]
pub fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = fs::Permissions::from_mode(0o700);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
pub fn restrict_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_hash_is_stable_and_hex() {
        let p = Path::new("/tmp/example/repo");
        let a = repo_hash(p);
        let b = repo_hash(p);
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn repo_hash_differs_by_path() {
        assert_ne!(repo_hash(Path::new("/a")), repo_hash(Path::new("/b")));
    }

    #[test]
    fn object_path_is_sharded() {
        let sp = StoragePaths {
            repo_root: PathBuf::from("/x"),
            repo_hash: "deadbeef".into(),
            root: PathBuf::from("/data/rewind/deadbeef"),
            db: PathBuf::from("/data/rewind/deadbeef/rewind.db"),
            objects: PathBuf::from("/data/rewind/deadbeef/objects"),
            logs: PathBuf::from("/data/rewind/deadbeef/logs"),
        };
        let op = sp.object_path("abcdef0123");
        assert_eq!(
            op,
            PathBuf::from("/data/rewind/deadbeef/objects/ab/cdef0123")
        );
    }
}
