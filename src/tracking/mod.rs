//! Workspace file tracking.
//!
//! Two concerns live here:
//!
//! * [`IgnoreRules`] — the deterministic decision of whether a path is tracked,
//!   layering the repository's `.gitignore` with Rewind's built-in exclusions
//!   (VCS/Rewind internals, dependency and build directories, caches, secrets)
//!   and any extra patterns from `.rewind.toml`.
//! * [`scan`] — a full reconciliation walk of the working tree, used at startup
//!   because filesystem watchers can miss changes made while Rewind was not
//!   running.
//!
//! The live incremental watcher lives in [`watcher`].

pub mod watcher;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::WalkBuilder;

use crate::config::{Config, DEFAULT_IGNORED_DIRS, SECRET_PATTERNS};
use crate::error::{Result, RewindError};

/// Number of leading bytes inspected for binary detection.
const BINARY_SNIFF_LEN: usize = 8192;

/// A file selected for tracking, with the metadata needed to snapshot it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedFile {
    /// Path relative to the repository root, using `/` separators.
    pub rel_path: String,
    /// Absolute path on disk.
    pub abs_path: PathBuf,
    /// Size in bytes.
    pub size: u64,
    /// Unix mode bits (permissions); `0o644` on platforms without them.
    pub mode: u32,
}

/// Why a path was excluded from tracking. Used for reporting/warnings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreReason {
    Tracked,
    DefaultDir,
    GitIgnored,
    Secret,
    Extra,
    TooLarge,
    Binary,
}

/// Compiled ignore decision logic for a single repository.
#[derive(Clone)]
pub struct IgnoreRules {
    root: PathBuf,
    default_dirs: Arc<Gitignore>,
    secrets: Arc<Gitignore>,
    extra: Arc<Gitignore>,
    repo_gitignore: Arc<Gitignore>,
    max_file_size: u64,
    track_secrets: bool,
}

impl IgnoreRules {
    /// Build the rule set for `repo_root` from `config`.
    pub fn new(repo_root: &Path, config: &Config) -> Result<IgnoreRules> {
        let default_dirs = build_gitignore(
            repo_root,
            DEFAULT_IGNORED_DIRS.iter().map(|d| format!("{d}/")),
        )?;
        let secrets = build_gitignore(repo_root, SECRET_PATTERNS.iter().map(|s| s.to_string()))?;
        let extra = build_gitignore(repo_root, config.ignore.iter().cloned())?;

        // Root-level .gitignore + git exclude, for incremental (single-path)
        // decisions. The full scan additionally honors nested .gitignore files
        // via the `ignore` crate's walker.
        let mut gi = GitignoreBuilder::new(repo_root);
        let _ = gi.add(repo_root.join(".gitignore"));
        let _ = gi.add(repo_root.join(".git/info/exclude"));
        let repo_gitignore = gi
            .build()
            .map_err(|e| RewindError::other(format!("invalid .gitignore: {e}")))?;

        Ok(IgnoreRules {
            root: repo_root.to_path_buf(),
            default_dirs: Arc::new(default_dirs),
            secrets: Arc::new(secrets),
            extra: Arc::new(extra),
            repo_gitignore: Arc::new(repo_gitignore),
            max_file_size: config.max_file_size,
            track_secrets: config.track_secrets,
        })
    }

    /// Whether a relative path is excluded by the pattern-based rules (default
    /// dirs, extra patterns, and secrets unless opted in). Does not consult
    /// `.gitignore` or inspect file contents; used to prune directory descent.
    pub fn is_custom_ignored(&self, rel: &Path, is_dir: bool) -> bool {
        if self
            .default_dirs
            .matched_path_or_any_parents(rel, is_dir)
            .is_ignore()
        {
            return true;
        }
        if self
            .extra
            .matched_path_or_any_parents(rel, is_dir)
            .is_ignore()
        {
            return true;
        }
        if !self.track_secrets
            && self
                .secrets
                .matched_path_or_any_parents(rel, is_dir)
                .is_ignore()
        {
            return true;
        }
        false
    }

    /// Whether a path matches a built-in secret pattern (regardless of the
    /// `track_secrets` opt-in). Used to warn the user.
    pub fn matches_secret(&self, rel: &Path) -> bool {
        self.secrets.matched(rel, false).is_ignore()
    }

    /// Full classification of a relative path, consulting the root `.gitignore`
    /// as well. `size` and `is_binary` are provided by the caller when known.
    pub fn classify(&self, rel: &Path, is_dir: bool, size: Option<u64>) -> IgnoreReason {
        if self
            .default_dirs
            .matched_path_or_any_parents(rel, is_dir)
            .is_ignore()
        {
            return IgnoreReason::DefaultDir;
        }
        if !self.track_secrets
            && self
                .secrets
                .matched_path_or_any_parents(rel, is_dir)
                .is_ignore()
        {
            return IgnoreReason::Secret;
        }
        if self
            .extra
            .matched_path_or_any_parents(rel, is_dir)
            .is_ignore()
        {
            return IgnoreReason::Extra;
        }
        if self
            .repo_gitignore
            .matched_path_or_any_parents(rel, is_dir)
            .is_ignore()
        {
            return IgnoreReason::GitIgnored;
        }
        if let Some(sz) = size {
            if sz > self.max_file_size {
                return IgnoreReason::TooLarge;
            }
        }
        IgnoreReason::Tracked
    }

    /// Convenience: should this relative path (a file) be tracked, ignoring
    /// size and binary checks?
    pub fn should_track(&self, rel: &Path) -> bool {
        matches!(self.classify(rel, false, None), IgnoreReason::Tracked)
    }

    /// Should a filesystem-watcher event for `rel` be forwarded?
    ///
    /// Watcher events may name either a file or a directory (macOS FSEvents in
    /// particular reports directory paths). A path is dropped if it is ignored
    /// under *either* interpretation, so a directory event for e.g.
    /// `node_modules` is correctly excluded even though `node_modules/` is a
    /// directory-only pattern.
    pub fn should_track_event(&self, rel: &Path) -> bool {
        matches!(self.classify(rel, false, None), IgnoreReason::Tracked)
            && matches!(self.classify(rel, true, None), IgnoreReason::Tracked)
    }

    /// The repository root these rules apply to.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The configured maximum tracked file size.
    pub fn max_file_size(&self) -> u64 {
        self.max_file_size
    }
}

fn build_gitignore(root: &Path, patterns: impl Iterator<Item = String>) -> Result<Gitignore> {
    let mut b = GitignoreBuilder::new(root);
    for p in patterns {
        b.add_line(None, &p)
            .map_err(|e| RewindError::other(format!("invalid ignore pattern {p:?}: {e}")))?;
    }
    b.build()
        .map_err(|e| RewindError::other(format!("failed to compile ignore rules: {e}")))
}

/// Perform a full reconciliation scan of the working tree, returning tracked
/// files keyed by relative path. Honors `.gitignore` (including nested files),
/// Rewind's default exclusions, the size limit, and binary detection.
pub fn scan(repo_root: &Path, rules: &IgnoreRules) -> Result<BTreeMap<String, TrackedFile>> {
    let root = repo_root.to_path_buf();
    let rules_filter = rules.clone();
    let filter_root = root.clone();

    let walk = WalkBuilder::new(&root)
        .hidden(false)
        .parents(true)
        .ignore(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .filter_entry(move |entry| {
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            match entry.path().strip_prefix(&filter_root) {
                Ok(rel) if !rel.as_os_str().is_empty() => {
                    !rules_filter.is_custom_ignored(rel, is_dir)
                }
                _ => true,
            }
        })
        .build();

    let mut out: BTreeMap<String, TrackedFile> = BTreeMap::new();
    for entry in walk {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue, // permission errors etc. — skip, don't abort scan
        };
        let ft = match entry.file_type() {
            Some(ft) => ft,
            None => continue,
        };
        if !ft.is_file() {
            continue;
        }
        let abs = entry.path();
        let rel = match rel_string(&root, abs) {
            Some(r) => r,
            None => continue,
        };
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let size = meta.len();
        if size > rules.max_file_size {
            continue;
        }
        if is_binary(abs) {
            continue;
        }
        out.insert(
            rel.clone(),
            TrackedFile {
                rel_path: rel,
                abs_path: abs.to_path_buf(),
                size,
                mode: file_mode(&meta),
            },
        );
    }
    Ok(out)
}

/// Relative path as a `/`-separated string, or `None` if `path` is not under
/// `root`.
pub fn rel_string(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let mut s = String::new();
    for (i, comp) in rel.components().enumerate() {
        if i > 0 {
            s.push('/');
        }
        s.push_str(&comp.as_os_str().to_string_lossy());
    }
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Heuristic binary detection: a NUL byte in the first [`BINARY_SNIFF_LEN`]
/// bytes marks the file as binary (matching Git's approach).
pub fn is_binary(path: &Path) -> bool {
    use std::io::Read;
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut buf = [0u8; BINARY_SNIFF_LEN];
    let n = match f.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return false,
    };
    buf[..n].contains(&0)
}

#[cfg(unix)]
fn file_mode(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    meta.mode() & 0o777
}

#[cfg(not(unix))]
fn file_mode(_meta: &std::fs::Metadata) -> u32 {
    0o644
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn rules_for(dir: &Path, cfg: Config) -> IgnoreRules {
        IgnoreRules::new(dir, &cfg).unwrap()
    }

    #[test]
    fn ignores_default_dirs_and_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        let rules = rules_for(tmp.path(), Config::default());
        assert!(rules.is_custom_ignored(Path::new("node_modules"), true));
        assert!(rules.is_custom_ignored(Path::new("target"), true));
        assert!(rules.is_custom_ignored(Path::new(".git"), true));
        assert!(rules.matches_secret(Path::new(".env")));
        assert!(rules.matches_secret(Path::new("server.pem")));
        assert!(rules.matches_secret(Path::new("config/.env.production")));
        assert!(!rules.should_track(Path::new(".env")));
        assert!(rules.should_track(Path::new("src/main.rs")));
    }

    #[test]
    fn secrets_can_be_opted_in() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config {
            track_secrets: true,
            ..Config::default()
        };
        let rules = rules_for(tmp.path(), cfg);
        assert!(rules.should_track(Path::new(".env")));
        // still reported as a secret for warning purposes
        assert!(rules.matches_secret(Path::new(".env")));
    }

    #[test]
    fn extra_patterns_apply() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config {
            ignore: vec!["coverage/".into(), "*.generated.rs".into()],
            ..Config::default()
        };
        let rules = rules_for(tmp.path(), cfg);
        assert!(rules.is_custom_ignored(Path::new("coverage"), true));
        assert!(!rules.should_track(Path::new("x.generated.rs")));
        assert!(rules.should_track(Path::new("x.rs")));
    }

    #[test]
    fn scan_respects_rules_and_size() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join("src/main.rs"), b"fn main() {}").unwrap();
        fs::write(root.join("node_modules/pkg/index.js"), b"x").unwrap();
        fs::write(root.join(".env"), b"SECRET=1").unwrap();
        fs::write(root.join("big.txt"), vec![b'a'; 2048]).unwrap();
        fs::write(root.join("bin.dat"), [0u8, 1, 2, 0]).unwrap();

        let cfg = Config {
            max_file_size: 1024,
            ..Config::default()
        };
        let rules = rules_for(root, cfg);
        let found = scan(root, &rules).unwrap();

        assert!(found.contains_key("src/main.rs"));
        assert!(!found.contains_key("node_modules/pkg/index.js"));
        assert!(!found.contains_key(".env"), "secret must be excluded");
        assert!(!found.contains_key("big.txt"), "oversize must be excluded");
        assert!(!found.contains_key("bin.dat"), "binary must be excluded");
    }

    #[test]
    fn scan_respects_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join(".gitignore"), b"ignored.txt\nlogs/\n").unwrap();
        fs::write(root.join("ignored.txt"), b"x").unwrap();
        fs::write(root.join("kept.txt"), b"y").unwrap();
        fs::create_dir_all(root.join("logs")).unwrap();
        fs::write(root.join("logs/a.log"), b"z").unwrap();

        let rules = rules_for(root, Config::default());
        let found = scan(root, &rules).unwrap();
        assert!(found.contains_key("kept.txt"));
        assert!(found.contains_key(".gitignore"));
        assert!(!found.contains_key("ignored.txt"));
        assert!(!found.contains_key("logs/a.log"));
    }
}
