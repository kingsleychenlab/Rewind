//! Debounced filesystem watcher.
//!
//! Wraps [`notify`] and coalesces raw events into batches of changed relative
//! paths, filtered by [`IgnoreRules`]. Editors frequently emit several events
//! per save (write, chmod, rename-of-tempfile); debouncing collapses those into
//! one logical change per path.
//!
//! The watcher is best-effort: it can miss events while the process is busy or
//! not running, which is why Rewind always performs a full [`super::scan`] at
//! startup.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::error::{Result, RewindError};
use crate::tracking::{rel_string, IgnoreRules};

/// Default quiet period before a batch of changes is emitted.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(300);

/// A running filesystem watcher. Dropping it stops watching.
pub struct Watcher {
    _inner: RecommendedWatcher,
    _debounce: JoinHandle<()>,
}

impl Watcher {
    /// Start watching `repo_root` recursively. Returns the watcher handle and a
    /// receiver of debounced batches; each batch is a sorted set of tracked
    /// relative paths that changed.
    pub fn start(
        repo_root: &Path,
        rules: IgnoreRules,
        debounce: Duration,
    ) -> Result<(Watcher, Receiver<Vec<String>>)> {
        let (raw_tx, raw_rx) = mpsc::channel::<Vec<PathBuf>>();
        let mut inner = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                if !event.paths.is_empty() {
                    let _ = raw_tx.send(event.paths);
                }
            }
        })
        .map_err(|e| RewindError::other(format!("failed to create watcher: {e}")))?;

        inner
            .watch(repo_root, RecursiveMode::Recursive)
            .map_err(|e| RewindError::other(format!("failed to watch {repo_root:?}: {e}")))?;

        let (out_tx, out_rx) = crossbeam_channel::unbounded::<Vec<String>>();
        let root = repo_root.to_path_buf();
        let debounce_handle = std::thread::Builder::new()
            .name("rewind-debounce".into())
            .spawn(move || debounce_loop(raw_rx, out_tx, root, rules, debounce))
            .map_err(RewindError::Io)?;

        Ok((
            Watcher {
                _inner: inner,
                _debounce: debounce_handle,
            },
            out_rx,
        ))
    }
}

/// Collect raw path events and emit filtered, de-duplicated batches after a
/// quiet period. Exits when the raw sender is dropped and the buffer is empty.
fn debounce_loop(
    raw_rx: mpsc::Receiver<Vec<PathBuf>>,
    out_tx: Sender<Vec<String>>,
    root: PathBuf,
    rules: IgnoreRules,
    debounce: Duration,
) {
    let mut pending: BTreeSet<String> = BTreeSet::new();
    let mut last_event: Option<Instant> = None;

    loop {
        let timeout = if pending.is_empty() {
            Duration::from_millis(500)
        } else {
            debounce
        };
        match raw_rx.recv_timeout(timeout) {
            Ok(paths) => {
                for p in paths {
                    if let Some(rel) = filtered_rel(&root, &p, &rules) {
                        pending.insert(rel);
                    }
                }
                last_event = Some(Instant::now());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let quiet = last_event.map(|t| t.elapsed() >= debounce).unwrap_or(true);
                if !pending.is_empty() && quiet {
                    let batch: Vec<String> = std::mem::take(&mut pending).into_iter().collect();
                    if out_tx.send(batch).is_err() {
                        return; // consumer gone
                    }
                    last_event = None;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if !pending.is_empty() {
                    let batch: Vec<String> = pending.into_iter().collect();
                    let _ = out_tx.send(batch);
                }
                return;
            }
        }
    }
}

/// Map an absolute event path to a tracked relative path, or `None` if it is
/// outside the repo or excluded by the pattern rules.
fn filtered_rel(root: &Path, path: &Path, rules: &IgnoreRules) -> Option<String> {
    let rel = rel_string(root, path)?;
    // We cannot always stat (deletions), and the path may be a file or a
    // directory, so decide on patterns under both interpretations. Ignored
    // directory events (node_modules, .git) are dropped here too.
    let rel_path = Path::new(&rel);
    if rules.should_track_event(rel_path) {
        Some(rel)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::fs;

    #[test]
    fn emits_batch_for_tracked_change() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let rules = IgnoreRules::new(&root, &Config::default()).unwrap();
        let (_w, rx) = Watcher::start(&root, rules, Duration::from_millis(80)).unwrap();

        // Give the watcher a moment to initialize before writing.
        std::thread::sleep(Duration::from_millis(150));
        fs::write(root.join("hello.txt"), b"hi").unwrap();

        // Wait for a debounced batch mentioning our file.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw = false;
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(batch) => {
                    if batch.iter().any(|p| p == "hello.txt") {
                        saw = true;
                        break;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            }
        }
        assert!(saw, "expected a debounced batch containing hello.txt");
    }

    #[test]
    fn ignores_untracked_change() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("node_modules")).unwrap();
        let rules = IgnoreRules::new(&root, &Config::default()).unwrap();
        let (_w, rx) = Watcher::start(&root, rules, Duration::from_millis(80)).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        fs::write(root.join("node_modules/x.js"), b"x").unwrap();

        // Should not receive a batch for the ignored path within a short window.
        std::thread::sleep(Duration::from_millis(400));
        let mut leaked = false;
        while let Ok(batch) = rx.try_recv() {
            if batch.iter().any(|p| p.starts_with("node_modules")) {
                leaked = true;
            }
        }
        assert!(!leaked, "ignored directory changes must not be emitted");
    }
}
