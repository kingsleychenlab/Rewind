//! Differences between snapshots.
//!
//! Two layers:
//!
//! * Manifest diff — which paths were added, modified, deleted, or renamed
//!   between two snapshots (rename = a delete and an add sharing content).
//! * Text diff — a unified line diff and add/remove counts for an individual
//!   file, when both versions are UTF-8 text.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};

use crate::db::models::SnapshotFile;
use crate::db::Db;
use crate::error::Result;
use crate::objects::ObjectStore;
use crate::snapshot;

/// How a path changed between two snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ChangeStatus {
    Added,
    Modified,
    Deleted,
    Renamed { from: String },
}

/// A single path-level change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub status: ChangeStatus,
    pub old_hash: Option<String>,
    pub new_hash: Option<String>,
    pub old_size: i64,
    pub new_size: i64,
}

impl FileChange {
    /// A short status letter for compact display (`A`/`M`/`D`/`R`).
    pub fn letter(&self) -> char {
        match self.status {
            ChangeStatus::Added => 'A',
            ChangeStatus::Modified => 'M',
            ChangeStatus::Deleted => 'D',
            ChangeStatus::Renamed { .. } => 'R',
        }
    }
}

/// Line add/remove counts for a text diff.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineStat {
    pub added: usize,
    pub removed: usize,
}

/// Compute path-level changes between two stored snapshots (`old` → `new`).
pub fn diff_snapshots(db: &Db, old_id: i64, new_id: i64) -> Result<Vec<FileChange>> {
    let old = snapshot::manifest_map(db, old_id)?;
    let new = snapshot::manifest_map(db, new_id)?;
    Ok(diff_manifests(&old, &new))
}

/// Pure manifest diff with rename detection. Results are ordered by path with
/// renames attributed to their new path.
pub fn diff_manifests(
    old: &BTreeMap<String, SnapshotFile>,
    new: &BTreeMap<String, SnapshotFile>,
) -> Vec<FileChange> {
    let mut added: Vec<&SnapshotFile> = Vec::new();
    let mut deleted: Vec<&SnapshotFile> = Vec::new();
    let mut changes: Vec<FileChange> = Vec::new();

    for (path, nf) in new {
        match old.get(path) {
            None => added.push(nf),
            Some(of) => {
                if of.hash != nf.hash {
                    changes.push(FileChange {
                        path: path.clone(),
                        status: ChangeStatus::Modified,
                        old_hash: Some(of.hash.clone()),
                        new_hash: Some(nf.hash.clone()),
                        old_size: of.size,
                        new_size: nf.size,
                    });
                }
            }
        }
    }
    for (path, of) in old {
        if !new.contains_key(path) {
            deleted.push(of);
        }
    }

    // Rename detection: pair a deleted file with an added file of identical
    // content. Each deletion consumes at most one addition.
    let mut consumed_added = vec![false; added.len()];
    for df in &deleted {
        let mut matched = None;
        for (i, af) in added.iter().enumerate() {
            if !consumed_added[i] && af.hash == df.hash {
                matched = Some(i);
                break;
            }
        }
        match matched {
            Some(i) => {
                consumed_added[i] = true;
                let af = added[i];
                changes.push(FileChange {
                    path: af.path.clone(),
                    status: ChangeStatus::Renamed {
                        from: df.path.clone(),
                    },
                    old_hash: Some(df.hash.clone()),
                    new_hash: Some(af.hash.clone()),
                    old_size: df.size,
                    new_size: af.size,
                });
            }
            None => {
                changes.push(FileChange {
                    path: df.path.clone(),
                    status: ChangeStatus::Deleted,
                    old_hash: Some(df.hash.clone()),
                    new_hash: None,
                    old_size: df.size,
                    new_size: 0,
                });
            }
        }
    }
    for (i, af) in added.iter().enumerate() {
        if !consumed_added[i] {
            changes.push(FileChange {
                path: af.path.clone(),
                status: ChangeStatus::Added,
                old_hash: None,
                new_hash: Some(af.hash.clone()),
                old_size: 0,
                new_size: af.size,
            });
        }
    }

    changes.sort_by(|a, b| a.path.cmp(&b.path));
    changes
}

/// Load an object's bytes as UTF-8 text, or `None` if binary/non-UTF-8.
fn load_text(store: &ObjectStore, hash: &str) -> Option<String> {
    let bytes = store.read(hash).ok()?;
    if bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Add/remove line counts between two object versions. Missing or binary
/// content yields `None`.
pub fn line_stat(
    store: &ObjectStore,
    old_hash: Option<&str>,
    new_hash: Option<&str>,
) -> Option<LineStat> {
    let old = old_hash
        .and_then(|h| load_text(store, h))
        .unwrap_or_default();
    let new = new_hash
        .and_then(|h| load_text(store, h))
        .unwrap_or_default();
    if old.is_empty() && new.is_empty() {
        return None;
    }
    Some(line_stat_text(&old, &new))
}

/// Add/remove counts between two in-memory strings.
pub fn line_stat_text(old: &str, new: &str) -> LineStat {
    let diff = TextDiff::from_lines(old, new);
    let mut stat = LineStat::default();
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => stat.added += 1,
            ChangeTag::Delete => stat.removed += 1,
            ChangeTag::Equal => {}
        }
    }
    stat
}

/// 1-based line numbers in the new file that were inserted (used as failure
/// evidence). Empty when content is unavailable or binary.
pub fn changed_new_lines(
    store: &ObjectStore,
    old_hash: Option<&str>,
    new_hash: Option<&str>,
) -> Vec<usize> {
    let old = old_hash
        .and_then(|h| load_text(store, h))
        .unwrap_or_default();
    let new = match new_hash.and_then(|h| load_text(store, h)) {
        Some(t) => t,
        None => return Vec::new(),
    };
    let diff = TextDiff::from_lines(&old, &new);
    let mut lines = Vec::new();
    for change in diff.iter_all_changes() {
        if change.tag() == ChangeTag::Insert {
            if let Some(idx) = change.new_index() {
                lines.push(idx + 1);
            }
        }
    }
    lines
}

/// A unified diff between two object versions, with the file path in the
/// header. Returns `None` if either side is binary.
pub fn unified_diff(
    store: &ObjectStore,
    path: &str,
    old_hash: Option<&str>,
    new_hash: Option<&str>,
) -> Option<String> {
    let old = old_hash
        .and_then(|h| load_text(store, h))
        .unwrap_or_default();
    let new = new_hash
        .and_then(|h| load_text(store, h))
        .unwrap_or_default();
    if old_hash.is_some() && load_text(store, old_hash.unwrap()).is_none() {
        return None;
    }
    if new_hash.is_some() && load_text(store, new_hash.unwrap()).is_none() {
        return None;
    }
    Some(unified_diff_text(path, &old, &new))
}

/// Unified diff between two strings.
pub fn unified_diff_text(path: &str, old: &str, new: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    diff.unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sf(path: &str, hash: &str, size: i64) -> SnapshotFile {
        SnapshotFile {
            path: path.into(),
            hash: hash.into(),
            size,
            mode: 0o644,
        }
    }

    fn map(items: &[SnapshotFile]) -> BTreeMap<String, SnapshotFile> {
        items.iter().map(|f| (f.path.clone(), f.clone())).collect()
    }

    #[test]
    fn detects_add_modify_delete() {
        let old = map(&[sf("keep.txt", "h1", 1), sf("gone.txt", "h2", 2)]);
        let new = map(&[sf("keep.txt", "h1b", 3), sf("added.txt", "h3", 4)]);
        let changes = diff_manifests(&old, &new);
        let by_path: BTreeMap<_, _> = changes.iter().map(|c| (c.path.as_str(), c)).collect();
        assert_eq!(by_path["added.txt"].status, ChangeStatus::Added);
        assert_eq!(by_path["keep.txt"].status, ChangeStatus::Modified);
        assert_eq!(by_path["gone.txt"].status, ChangeStatus::Deleted);
    }

    #[test]
    fn detects_rename() {
        let old = map(&[sf("old/name.rs", "same", 10)]);
        let new = map(&[sf("new/name.rs", "same", 10)]);
        let changes = diff_manifests(&old, &new);
        assert_eq!(changes.len(), 1);
        assert_eq!(
            changes[0].status,
            ChangeStatus::Renamed {
                from: "old/name.rs".into()
            }
        );
        assert_eq!(changes[0].path, "new/name.rs");
    }

    #[test]
    fn line_stats_count_changes() {
        let stat = line_stat_text("a\nb\nc\n", "a\nB\nc\nd\n");
        assert_eq!(stat.added, 2); // "B" and "d"
        assert_eq!(stat.removed, 1); // "b"
    }

    #[test]
    fn unified_diff_has_headers() {
        let d = unified_diff_text("x.txt", "one\n", "two\n");
        assert!(d.contains("a/x.txt"));
        assert!(d.contains("b/x.txt"));
        assert!(d.contains("-one"));
        assert!(d.contains("+two"));
    }
}
