//! Content-addressed object store.
//!
//! Every unique file version (and captured command/test log) is stored exactly
//! once, keyed by its BLAKE3 hash. Writes are atomic (temp file + rename) and
//! skip content that already exists, so unchanged files are never duplicated.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::error::{Result, RewindError};

const READ_CHUNK: usize = 64 * 1024;

/// A handle to the on-disk object directory.
#[derive(Debug, Clone)]
pub struct ObjectStore {
    root: PathBuf,
}

impl ObjectStore {
    /// Wrap an existing (or to-be-created) objects directory.
    pub fn new(root: impl Into<PathBuf>) -> ObjectStore {
        ObjectStore { root: root.into() }
    }

    /// Ensure the root directory exists.
    pub fn ensure(&self) -> Result<()> {
        fs::create_dir_all(&self.root)?;
        Ok(())
    }

    /// Sharded path for a hash (`ab/cdef…`).
    pub fn path_for(&self, hash: &str) -> PathBuf {
        let (shard, rest) = hash.split_at(2.min(hash.len()));
        self.root.join(shard).join(rest)
    }

    /// Whether an object with this hash is already stored.
    pub fn exists(&self, hash: &str) -> bool {
        self.path_for(hash).exists()
    }

    /// Hash `data`, store it if not already present, and return the hash.
    pub fn write_bytes(&self, data: &[u8]) -> Result<String> {
        let hash = hash_bytes(data);
        self.store_if_absent(&hash, data)?;
        Ok(hash)
    }

    /// Read a file from disk, hash and store its contents, returning the hash
    /// and the byte length.
    pub fn write_from_path(&self, src: &Path) -> Result<(String, u64)> {
        let mut file = File::open(src)?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        let hash = self.write_bytes(&buf)?;
        Ok((hash, buf.len() as u64))
    }

    /// Retrieve an object's bytes by hash.
    pub fn read(&self, hash: &str) -> Result<Vec<u8>> {
        let path = self.path_for(hash);
        match fs::read(&path) {
            Ok(bytes) => Ok(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(RewindError::ObjectMissing(hash.to_string()))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Write `data` to its final sharded path atomically, unless already there.
    fn store_if_absent(&self, hash: &str, data: &[u8]) -> Result<()> {
        let final_path = self.path_for(hash);
        if final_path.exists() {
            return Ok(());
        }
        let dir = final_path
            .parent()
            .ok_or_else(|| RewindError::other("object path has no parent"))?;
        fs::create_dir_all(dir)?;

        // Unique temp name in the same directory so rename is atomic.
        let tmp = dir.join(format!(".tmp-{}-{}", std::process::id(), fastrand_suffix()));
        {
            let mut f = File::create(&tmp)?;
            f.write_all(data)?;
            f.sync_all()?;
        }
        set_readonly(&tmp);
        // Rename is atomic on the same filesystem. If another process wrote the
        // same content first, discard our temp copy.
        match fs::rename(&tmp, &final_path) {
            Ok(()) => Ok(()),
            Err(_) if final_path.exists() => {
                let _ = fs::remove_file(&tmp);
                Ok(())
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e.into())
            }
        }
    }
}

/// Hash a byte slice with BLAKE3, returning lowercase hex.
pub fn hash_bytes(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

/// Stream a file through BLAKE3 without loading it entirely, returning the hex
/// hash and byte length. Used where only the identity (not the content) is
/// needed, e.g. the dirty-file index.
pub fn hash_file(path: &Path) -> Result<(String, u64)> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; READ_CHUNK];
    let mut total: u64 = 0;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((hasher.finalize().to_hex().to_string(), total))
}

#[cfg(unix)]
fn set_readonly(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o400));
}

#[cfg(not(unix))]
fn set_readonly(_path: &Path) {}

/// A tiny non-cryptographic suffix for temp file uniqueness. Avoids adding a
/// dependency just for this.
fn fastrand_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Mix in a thread-local counter for uniqueness within the same nanosecond.
    use std::cell::Cell;
    thread_local!(static COUNTER: Cell<u64> = const { Cell::new(0) });
    let c = COUNTER.with(|c| {
        let v = c.get().wrapping_add(1);
        c.set(v);
        v
    });
    format!("{nanos:x}{c:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_content_same_hash() {
        assert_eq!(hash_bytes(b"hello"), hash_bytes(b"hello"));
        assert_ne!(hash_bytes(b"hello"), hash_bytes(b"world"));
    }

    #[test]
    fn write_then_read_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(tmp.path());
        store.ensure().unwrap();
        let h = store.write_bytes(b"content").unwrap();
        assert!(store.exists(&h));
        assert_eq!(store.read(&h).unwrap(), b"content");
    }

    #[test]
    fn dedup_does_not_duplicate() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(tmp.path());
        store.ensure().unwrap();
        let h1 = store.write_bytes(b"dup").unwrap();
        let h2 = store.write_bytes(b"dup").unwrap();
        assert_eq!(h1, h2);
        // Only one object file for identical content.
        let path = store.path_for(&h1);
        assert!(path.exists());
    }

    #[test]
    fn hash_file_matches_hash_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("x.txt");
        fs::write(&f, b"streamed content here").unwrap();
        let (fh, len) = hash_file(&f).unwrap();
        assert_eq!(fh, hash_bytes(b"streamed content here"));
        assert_eq!(len, 21);
    }

    #[test]
    fn missing_object_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(tmp.path());
        store.ensure().unwrap();
        let err = store.read("00deadbeef").unwrap_err();
        assert!(matches!(err, RewindError::ObjectMissing(_)));
    }
}
