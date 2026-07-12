//! Safe restoration of files from a snapshot.
//!
//! Every restore is preceded by a safety snapshot (created by the engine) and a
//! dry-run plan the user must confirm. Writes are atomic (temp file + rename),
//! strictly confined to the repository root, and never touch `.git`. Undo is
//! achieved by restoring the safety snapshot.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use crate::db::Db;
use crate::error::{Result, RewindError};
use crate::objects::{self, ObjectStore};
use crate::snapshot;
use crate::tracking::{self, IgnoreRules};

/// Which files a restore targets.
#[derive(Debug, Clone)]
pub enum Selection {
    /// Restore the entire snapshot (may create, overwrite, and delete).
    All,
    /// Restore only these relative paths.
    Paths(BTreeSet<String>),
}

impl Selection {
    /// Build a selection from an iterator of paths; empty means "all".
    pub fn from_paths<I: IntoIterator<Item = String>>(paths: I) -> Selection {
        let set: BTreeSet<String> = paths.into_iter().collect();
        if set.is_empty() {
            Selection::All
        } else {
            Selection::Paths(set)
        }
    }
}

/// One file to be written during a restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEntry {
    pub path: String,
    pub hash: String,
    pub size: i64,
    pub mode: u32,
}

/// A dry-run description of a restore. Nothing is written until [`apply`].
#[derive(Debug, Clone, Default)]
pub struct RestorePlan {
    pub snapshot_id: i64,
    /// Files that don't exist in the working tree and will be created.
    pub create: Vec<PlanEntry>,
    /// Files that exist but differ and will be overwritten.
    pub overwrite: Vec<PlanEntry>,
    /// Tracked files absent from the snapshot that will be deleted.
    pub delete: Vec<String>,
}

impl RestorePlan {
    pub fn is_empty(&self) -> bool {
        self.create.is_empty() && self.overwrite.is_empty() && self.delete.is_empty()
    }

    pub fn total_changes(&self) -> usize {
        self.create.len() + self.overwrite.len() + self.delete.len()
    }
}

/// Result of applying a plan.
#[derive(Debug, Clone, Copy, Default)]
pub struct RestoreStats {
    pub written: usize,
    pub deleted: usize,
}

/// Build a restore plan comparing a stored snapshot against the current working
/// tree. Does not modify anything.
pub fn plan(
    db: &Db,
    store: &ObjectStore,
    repo_root: &Path,
    rules: &IgnoreRules,
    snapshot_id: i64,
    selection: &Selection,
) -> Result<RestorePlan> {
    // Confirm the snapshot exists (an empty manifest is legal for an empty
    // repo, so we can't infer existence from the manifest alone).
    if crate::db::models::get_snapshot(db.conn(), snapshot_id)?.is_none() {
        return Err(RewindError::SnapshotMissing(snapshot_id.to_string()));
    }
    let target = snapshot::manifest_map(db, snapshot_id)?;
    let current = current_manifest(store, repo_root, rules)?;

    let candidates: BTreeSet<String> = match selection {
        Selection::All => target.keys().chain(current.keys()).cloned().collect(),
        Selection::Paths(set) => set.clone(),
    };

    let mut out = RestorePlan {
        snapshot_id,
        ..Default::default()
    };
    for path in candidates {
        // Validate every path we might touch, up front.
        validate_rel_path(repo_root, &path)?;
        match target.get(&path) {
            Some(t) => match current.get(&path) {
                None => out.create.push(PlanEntry {
                    path: path.clone(),
                    hash: t.hash.clone(),
                    size: t.size,
                    mode: t.mode,
                }),
                Some(cur) if cur.0 != t.hash => out.overwrite.push(PlanEntry {
                    path: path.clone(),
                    hash: t.hash.clone(),
                    size: t.size,
                    mode: t.mode,
                }),
                Some(_) => {} // unchanged
            },
            None => {
                if current.contains_key(&path) {
                    out.delete.push(path.clone());
                }
            }
        }
    }
    out.create.sort_by(|a, b| a.path.cmp(&b.path));
    out.overwrite.sort_by(|a, b| a.path.cmp(&b.path));
    out.delete.sort();
    Ok(out)
}

/// Apply a plan, writing and deleting files atomically. Every path is
/// re-validated; any escape aborts before touching the filesystem is complete
/// for that entry (writes are per-file atomic).
pub fn apply(store: &ObjectStore, repo_root: &Path, plan: &RestorePlan) -> Result<RestoreStats> {
    // Re-validate everything first so a bad path fails the whole operation
    // before we write anything.
    for e in plan.create.iter().chain(plan.overwrite.iter()) {
        validate_rel_path(repo_root, &e.path)?;
    }
    for p in &plan.delete {
        validate_rel_path(repo_root, p)?;
    }

    let mut stats = RestoreStats::default();
    for e in plan.create.iter().chain(plan.overwrite.iter()) {
        let abs = safe_join(repo_root, &e.path)?;
        let bytes = store.read(&e.hash)?;
        atomic_write(&abs, &bytes, e.mode)?;
        stats.written += 1;
    }
    for p in &plan.delete {
        let abs = safe_join(repo_root, p)?;
        match std::fs::remove_file(&abs) {
            Ok(()) => stats.deleted += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(stats)
}

/// Compute the current working-tree manifest as `path -> (hash, size, mode)`.
fn current_manifest(
    store: &ObjectStore,
    repo_root: &Path,
    rules: &IgnoreRules,
) -> Result<BTreeMap<String, (String, i64, u32)>> {
    let _ = store; // hashing does not require the store
    let files = tracking::scan(repo_root, rules)?;
    let mut map = BTreeMap::new();
    for (path, tf) in files {
        let (hash, size) = objects::hash_file(&tf.abs_path)?;
        map.insert(path, (hash, size as i64, tf.mode));
    }
    Ok(map)
}

/// Validate that `rel` is a safe in-repo relative path: not empty, not
/// absolute, no `..`/`.`/root components, and never inside `.git`.
pub fn validate_rel_path(repo_root: &Path, rel: &str) -> Result<()> {
    if rel.is_empty() {
        return Err(RewindError::other("empty restore path"));
    }
    let rp = Path::new(rel);
    for comp in rp.components() {
        match comp {
            Component::Normal(seg) => {
                if seg == std::ffi::OsStr::new(".git") {
                    return Err(RewindError::PathEscapesRoot {
                        path: rp.to_path_buf(),
                        root: repo_root.to_path_buf(),
                    });
                }
            }
            // ParentDir, CurDir, RootDir, Prefix are all disallowed.
            _ => {
                return Err(RewindError::PathEscapesRoot {
                    path: rp.to_path_buf(),
                    root: repo_root.to_path_buf(),
                })
            }
        }
    }
    Ok(())
}

/// Join a validated relative path onto the root. Assumes [`validate_rel_path`]
/// already passed.
fn safe_join(repo_root: &Path, rel: &str) -> Result<PathBuf> {
    validate_rel_path(repo_root, rel)?;
    Ok(repo_root.join(rel))
}

/// Write `data` to `path` atomically, creating parent directories and setting
/// Unix mode bits.
fn atomic_write(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    use std::io::Write;
    let dir = path
        .parent()
        .ok_or_else(|| RewindError::other("restore path has no parent"))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".rewind-tmp-{}", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    set_mode(&tmp, mode);
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e.into())
        }
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    if mode != 0 {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::models;
    use crate::repo::Repo;
    use crate::snapshot::SnapshotContext;
    use std::fs;
    use std::process::Command;

    #[test]
    fn rejects_path_traversal_and_git() {
        let root = Path::new("/repo");
        assert!(validate_rel_path(root, "src/main.rs").is_ok());
        assert!(validate_rel_path(root, "../escape").is_err());
        assert!(validate_rel_path(root, "/etc/passwd").is_err());
        assert!(validate_rel_path(root, ".git/config").is_err());
        assert!(validate_rel_path(root, "a/../../b").is_err());
        assert!(validate_rel_path(root, "").is_err());
    }

    fn git_init(dir: &Path) {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .arg("init")
            .output()
            .unwrap();
    }

    #[test]
    fn plan_and_apply_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        let root = tmp.path().canonicalize().unwrap();
        let repo = Repo { root: root.clone() };
        let store = ObjectStore::new(root.join(".rewind/objects"));
        store.ensure().unwrap();
        let mut db = Db::open_in_memory().unwrap();
        let repo_id = models::upsert_repository(db.conn(), &root.to_string_lossy(), "h").unwrap();
        let rules = IgnoreRules::new(&root, &Config::default()).unwrap();
        let ctx = SnapshotContext {
            repo: &repo,
            repo_id,
            session_id: None,
            rules: &rules,
        };

        // State A: two files.
        fs::write(root.join("keep.txt"), b"original").unwrap();
        fs::write(root.join("gone.txt"), b"temporary").unwrap();
        let snap = snapshot::create(
            &mut db,
            &store,
            &ctx,
            models::snapshot_kind::CHECKPOINT,
            "A",
        )
        .unwrap();

        // Mutate: change keep, delete gone, add extra.
        fs::write(root.join("keep.txt"), b"modified").unwrap();
        fs::remove_file(root.join("gone.txt")).unwrap();
        fs::write(root.join("extra.txt"), b"new").unwrap();

        let p = plan(&db, &store, &root, &rules, snap.id, &Selection::All).unwrap();
        assert_eq!(p.overwrite.len(), 1, "keep.txt overwritten");
        assert_eq!(p.create.len(), 1, "gone.txt re-created");
        assert_eq!(p.delete.len(), 1, "extra.txt deleted");

        let stats = apply(&store, &root, &p).unwrap();
        assert_eq!(stats.written, 2);
        assert_eq!(stats.deleted, 1);

        assert_eq!(fs::read(root.join("keep.txt")).unwrap(), b"original");
        assert_eq!(fs::read(root.join("gone.txt")).unwrap(), b"temporary");
        assert!(!root.join("extra.txt").exists());
    }

    #[test]
    fn selected_file_restore_only_touches_that_file() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        let root = tmp.path().canonicalize().unwrap();
        let repo = Repo { root: root.clone() };
        let store = ObjectStore::new(root.join(".rewind/objects"));
        store.ensure().unwrap();
        let mut db = Db::open_in_memory().unwrap();
        let repo_id = models::upsert_repository(db.conn(), &root.to_string_lossy(), "h").unwrap();
        let rules = IgnoreRules::new(&root, &Config::default()).unwrap();
        let ctx = SnapshotContext {
            repo: &repo,
            repo_id,
            session_id: None,
            rules: &rules,
        };
        fs::write(root.join("a.txt"), b"a1").unwrap();
        fs::write(root.join("b.txt"), b"b1").unwrap();
        let snap = snapshot::create(
            &mut db,
            &store,
            &ctx,
            models::snapshot_kind::CHECKPOINT,
            "A",
        )
        .unwrap();
        fs::write(root.join("a.txt"), b"a2").unwrap();
        fs::write(root.join("b.txt"), b"b2").unwrap();

        let sel = Selection::from_paths(["a.txt".to_string()]);
        let p = plan(&db, &store, &root, &rules, snap.id, &sel).unwrap();
        apply(&store, &root, &p).unwrap();

        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"a1", "a restored");
        assert_eq!(fs::read(root.join("b.txt")).unwrap(), b"b2", "b untouched");
    }
}
