//! Snapshot creation.
//!
//! A snapshot is a manifest mapping every tracked relative path to the BLAKE3
//! hash of its content at that instant. File contents live in the
//! content-addressed [`ObjectStore`], so identical content across snapshots is
//! stored exactly once. The SQLite `snapshots` / `snapshot_files` tables hold
//! the manifest headers and entries.

use std::collections::BTreeMap;

use crate::db::models::{self, NewSnapshot, SnapshotFile};
use crate::db::Db;
use crate::error::Result;
use crate::objects::ObjectStore;
use crate::repo::Repo;
use crate::tracking::{self, IgnoreRules, TrackedFile};
use crate::util::now_millis;

/// Inputs shared across snapshot operations for one repository/session.
pub struct SnapshotContext<'a> {
    pub repo: &'a Repo,
    pub repo_id: i64,
    pub session_id: Option<i64>,
    pub rules: &'a IgnoreRules,
}

/// Create a snapshot of the current working tree.
///
/// Performs a full reconciliation scan, stores each tracked file's content in
/// the object store (deduplicated), then records the manifest in a single
/// transaction. Returns the persisted [`models::Snapshot`].
pub fn create(
    db: &mut Db,
    store: &ObjectStore,
    ctx: &SnapshotContext<'_>,
    kind: &str,
    label: &str,
) -> Result<models::Snapshot> {
    let files = tracking::scan(&ctx.repo.root, ctx.rules)?;
    create_from_files(db, store, ctx, kind, label, &files)
}

/// Like [`create`], but using an already-computed set of tracked files (avoids
/// re-scanning when the caller just did).
pub fn create_from_files(
    db: &mut Db,
    store: &ObjectStore,
    ctx: &SnapshotContext<'_>,
    kind: &str,
    label: &str,
    files: &BTreeMap<String, TrackedFile>,
) -> Result<models::Snapshot> {
    store.ensure()?;

    // Store contents first (idempotent, outside the DB transaction).
    let mut manifest: Vec<SnapshotFile> = Vec::with_capacity(files.len());
    let mut total_bytes: i64 = 0;
    for tf in files.values() {
        let (hash, size) = store.write_from_path(&tf.abs_path)?;
        total_bytes += size as i64;
        manifest.push(SnapshotFile {
            path: tf.rel_path.clone(),
            hash,
            size: size as i64,
            mode: tf.mode,
        });
    }

    let git = ctx.repo.state();
    let created_at = now_millis();
    let parent = models::latest_snapshot(db.conn(), ctx.repo_id)?.map(|s| s.id);
    let file_count = manifest.len() as i64;

    let header = NewSnapshot {
        repo_id: ctx.repo_id,
        session_id: ctx.session_id,
        parent_id: parent,
        kind: kind.to_string(),
        label: label.to_string(),
        created_at,
        git_branch: git.branch.clone(),
        git_head: git.head.clone(),
        git_dirty: git.dirty,
        file_count,
        total_bytes,
    };

    let snapshot_id = db.transaction(|tx| {
        let id = models::insert_snapshot(tx, &header)?;
        for f in &manifest {
            models::insert_snapshot_file(tx, id, f)?;
        }
        models::insert_event(
            tx,
            ctx.repo_id,
            ctx.session_id,
            models::event_kind::SNAPSHOT,
            Some(id),
            &format!("snapshot {label} ({file_count} files)"),
        )?;
        Ok(id)
    })?;

    models::get_snapshot(db.conn(), snapshot_id)?
        .ok_or_else(|| crate::error::RewindError::SnapshotMissing(snapshot_id.to_string()))
}

/// Reconstruct the manifest of a stored snapshot as a path→[`SnapshotFile`] map.
pub fn manifest_map(db: &Db, snapshot_id: i64) -> Result<BTreeMap<String, SnapshotFile>> {
    let files = models::list_snapshot_files(db.conn(), snapshot_id)?;
    Ok(files.into_iter().map(|f| (f.path.clone(), f)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::fs;
    use std::process::Command;

    fn git_init(dir: &std::path::Path) {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .arg("init")
            .output()
            .unwrap();
    }

    fn setup() -> (tempfile::TempDir, Db, ObjectStore, Repo, i64, IgnoreRules) {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        let root = tmp.path().canonicalize().unwrap();
        let repo = Repo { root: root.clone() };
        let store = ObjectStore::new(root.join(".objects-test"));
        store.ensure().unwrap();
        let db = Db::open_in_memory().unwrap();
        let repo_id = models::upsert_repository(db.conn(), &root.to_string_lossy(), "h").unwrap();
        let rules = IgnoreRules::new(&root, &Config::default()).unwrap();
        (tmp, db, store, repo, repo_id, rules)
    }

    #[test]
    fn snapshot_records_manifest_and_dedups() {
        let (tmp, mut db, store, repo, repo_id, rules) = setup();
        fs::write(tmp.path().join("a.txt"), b"same").unwrap();
        fs::write(tmp.path().join("b.txt"), b"same").unwrap();
        let ctx = SnapshotContext {
            repo: &repo,
            repo_id,
            session_id: None,
            rules: &rules,
        };
        let snap = create(
            &mut db,
            &store,
            &ctx,
            models::snapshot_kind::MANUAL,
            "first",
        )
        .unwrap();
        assert_eq!(snap.file_count, 2);
        let manifest = manifest_map(&db, snap.id).unwrap();
        // Identical content => identical hash => single object.
        assert_eq!(manifest["a.txt"].hash, manifest["b.txt"].hash);
        assert!(store.exists(&manifest["a.txt"].hash));
    }

    #[test]
    fn second_snapshot_links_parent_and_reuses_objects() {
        let (tmp, mut db, store, repo, repo_id, rules) = setup();
        fs::write(tmp.path().join("a.txt"), b"one").unwrap();
        let ctx = SnapshotContext {
            repo: &repo,
            repo_id,
            session_id: None,
            rules: &rules,
        };
        let s1 = create(&mut db, &store, &ctx, models::snapshot_kind::MANUAL, "s1").unwrap();
        fs::write(tmp.path().join("a.txt"), b"two").unwrap();
        let s2 = create(&mut db, &store, &ctx, models::snapshot_kind::MANUAL, "s2").unwrap();
        assert_eq!(s2.parent_id, Some(s1.id));
        let m1 = manifest_map(&db, s1.id).unwrap();
        let m2 = manifest_map(&db, s2.id).unwrap();
        assert_ne!(m1["a.txt"].hash, m2["a.txt"].hash);
        // Both versions retained in the store.
        assert!(store.exists(&m1["a.txt"].hash));
        assert!(store.exists(&m2["a.txt"].hash));
    }
}
