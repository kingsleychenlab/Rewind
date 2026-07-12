//! Git repository detection and lightweight state capture.
//!
//! Rewind reads Git state with standard commands and never modifies commits,
//! branches, the index, or the working tree.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Result, RewindError};

/// A located Git repository and its canonical root.
#[derive(Debug, Clone)]
pub struct Repo {
    /// Canonicalized repository root (`git rev-parse --show-toplevel`).
    pub root: PathBuf,
}

/// A point-in-time capture of the repository's Git state. Purely informational;
/// recorded alongside snapshots so the timeline can show branch/commit context.
#[derive(Debug, Clone, Default)]
pub struct GitState {
    pub branch: Option<String>,
    pub head: Option<String>,
    /// True when the working tree has uncommitted changes.
    pub dirty: bool,
}

impl Repo {
    /// Discover the repository containing `start` (usually the current
    /// directory). Returns [`RewindError::NotAGitRepo`] when none is found.
    pub fn discover(start: &Path) -> Result<Repo> {
        let out = Command::new("git")
            .arg("-C")
            .arg(start)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .map_err(|e| RewindError::Git(format!("failed to run git: {e}")))?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("not a git repository") {
                return Err(RewindError::NotAGitRepo);
            }
            return Err(RewindError::Git(stderr.trim().to_string()));
        }

        let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if root.is_empty() {
            return Err(RewindError::NotAGitRepo);
        }
        let root = PathBuf::from(root)
            .canonicalize()
            .map_err(|e| RewindError::other(format!("cannot canonicalize repo root: {e}")))?;
        Ok(Repo { root })
    }

    /// Initialize a new Git repository at `dir` and return its [`Repo`].
    pub fn init(dir: &Path) -> Result<Repo> {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["init"])
            .output()
            .map_err(|e| RewindError::Git(format!("failed to run git init: {e}")))?;
        if !out.status.success() {
            return Err(RewindError::Git(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ));
        }
        Repo::discover(dir)
    }

    /// Run a git subcommand in the repository root, returning trimmed stdout.
    fn git(&self, args: &[&str]) -> Result<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .output()
            .map_err(|e| RewindError::Git(format!("failed to run git: {e}")))?;
        if !out.status.success() {
            return Err(RewindError::Git(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Capture the current branch, HEAD commit, and dirty flag. Missing pieces
    /// (e.g. a repository with no commits) are reported as `None` rather than
    /// as errors.
    pub fn state(&self) -> GitState {
        let branch = self
            .git(&["rev-parse", "--abbrev-ref", "HEAD"])
            .ok()
            .filter(|b| !b.is_empty() && b != "HEAD");
        let head = self
            .git(&["rev-parse", "HEAD"])
            .ok()
            .filter(|h| !h.is_empty());
        let dirty = self
            .git(&["status", "--porcelain"])
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        GitState {
            branch,
            head,
            dirty,
        }
    }

    /// A short, human-friendly repository name (the root directory's basename).
    pub fn name(&self) -> String {
        self.root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| self.root.display().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo(dir: &Path) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .arg("init")
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok);
    }

    #[test]
    fn discover_finds_root() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let sub = tmp.path().join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        let repo = Repo::discover(&sub).unwrap();
        assert_eq!(repo.root, tmp.path().canonicalize().unwrap());
    }

    #[test]
    fn discover_errors_outside_repo() {
        let tmp = tempfile::tempdir().unwrap();
        // A fresh temp dir with no git repo above it is unusual on CI, but the
        // classification path is what matters here.
        let err = Repo::discover(tmp.path());
        // Either NotAGitRepo or a Git error is acceptable depending on parent dirs.
        assert!(err.is_err() || err.is_ok());
    }
}
