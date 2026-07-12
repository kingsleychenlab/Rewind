//! The engine: high-level operations shared by the CLI and the TUI.
//!
//! [`Engine`] owns the repository handle, configuration, storage paths, SQLite
//! connection, object store, and ignore rules, and exposes the verbs users
//! actually perform: snapshot, checkpoint, run, test, investigate, and restore.
//! Both the interactive interface and the non-interactive commands drive these
//! same code paths.

use crate::config::Config;
use crate::db::models::{self, event_kind, snapshot_kind, Snapshot, TestRun};
use crate::db::Db;
use crate::error::{Result, RewindError};
use crate::exec::{self, CancelToken, CommandOutcome, OutputChunk, RunSpec};
use crate::investigate::{self, Investigation};
use crate::objects::ObjectStore;
use crate::paths::StoragePaths;
use crate::repo::Repo;
use crate::restore::{self, RestorePlan, RestoreStats, Selection};
use crate::snapshot::{self, SnapshotContext};
use crate::tracking::IgnoreRules;
use crate::util::now_millis;

/// Owns all per-repository state and provides Rewind's operations.
pub struct Engine {
    pub repo: Repo,
    pub repo_id: i64,
    pub config: Config,
    pub storage: StoragePaths,
    pub db: Db,
    pub store: ObjectStore,
    pub rules: IgnoreRules,
    pub session_id: Option<i64>,
}

/// Result of running a tracked command.
pub struct CommandResult {
    pub command_id: i64,
    pub outcome: CommandOutcome,
    pub pre_snapshot: i64,
    pub post_snapshot: i64,
}

/// Result of running the test command.
pub struct TestResult {
    pub test_run_id: i64,
    pub outcome: CommandOutcome,
    pub pre_snapshot: i64,
    pub post_snapshot: i64,
    pub passed: bool,
    /// Present when this run turned a prior pass into a failure.
    pub investigation: Option<Investigation>,
}

/// Result of executing a restore.
pub struct RestoreOutcome {
    pub restore_id: i64,
    pub safety_snapshot_id: i64,
    pub stats: RestoreStats,
}

impl Engine {
    /// Open the engine for a repository, loading `.rewind.toml` if present.
    pub fn open(repo: Repo) -> Result<Engine> {
        let config = Config::load(&repo.root)?;
        Engine::open_with_config(repo, config)
    }

    /// Open with an explicit configuration.
    pub fn open_with_config(repo: Repo, config: Config) -> Result<Engine> {
        let storage = StoragePaths::for_repo(&repo.root)?;
        Engine::open_with_storage(repo, config, storage)
    }

    /// Open with explicit configuration and storage location. Lets tests and
    /// embedders control where data lives without the environment.
    pub fn open_with_storage(repo: Repo, config: Config, storage: StoragePaths) -> Result<Engine> {
        storage.ensure_dirs()?;
        let db = Db::open(&storage.db)?;
        let repo_id =
            models::upsert_repository(db.conn(), &repo.root.to_string_lossy(), &storage.repo_hash)?;
        let store = ObjectStore::new(&storage.objects);
        store.ensure()?;
        let rules = IgnoreRules::new(&repo.root, &config)?;
        Ok(Engine {
            repo,
            repo_id,
            config,
            storage,
            db,
            store,
            rules,
            session_id: None,
        })
    }

    /// Begin an interactive session, recording it and an initial snapshot.
    pub fn start_session(&mut self) -> Result<i64> {
        let shell = exec::resolve_shell();
        let id = models::start_session(self.db.conn(), self.repo_id, std::process::id(), &shell)?;
        self.session_id = Some(id);
        models::insert_event(
            self.db.conn(),
            self.repo_id,
            Some(id),
            event_kind::SESSION_START,
            Some(id),
            "session started",
        )?;
        self.snapshot(snapshot_kind::SESSION_START, "session start")?;
        Ok(id)
    }

    /// End the current session, if any.
    pub fn end_session(&mut self) -> Result<()> {
        if let Some(id) = self.session_id.take() {
            models::end_session(self.db.conn(), id)?;
            models::insert_event(
                self.db.conn(),
                self.repo_id,
                Some(id),
                event_kind::SESSION_END,
                Some(id),
                "session ended",
            )?;
        }
        Ok(())
    }

    /// Create a snapshot of the current working tree.
    pub fn snapshot(&mut self, kind: &str, label: &str) -> Result<Snapshot> {
        let ctx = SnapshotContext {
            repo: &self.repo,
            repo_id: self.repo_id,
            session_id: self.session_id,
            rules: &self.rules,
        };
        snapshot::create(&mut self.db, &self.store, &ctx, kind, label)
    }

    /// Create a named checkpoint snapshot.
    pub fn checkpoint(&mut self, name: Option<&str>) -> Result<Snapshot> {
        let label = match name {
            Some(n) if !n.trim().is_empty() => format!("checkpoint: {}", n.trim()),
            _ => format!("checkpoint {}", crate::util::format_clock(now_millis())),
        };
        let snap = self.snapshot(snapshot_kind::CHECKPOINT, &label)?;
        models::insert_event(
            self.db.conn(),
            self.repo_id,
            self.session_id,
            event_kind::CHECKPOINT,
            Some(snap.id),
            &label,
        )?;
        Ok(snap)
    }

    /// Run a tracked command: snapshot before, execute (streaming), snapshot
    /// after, and record the result.
    pub fn run_command<F>(
        &mut self,
        command: &str,
        cancel: &CancelToken,
        on_chunk: F,
    ) -> Result<CommandResult>
    where
        F: FnMut(&OutputChunk),
    {
        let pre = self.snapshot(snapshot_kind::PRE_COMMAND, &format!("before `{command}`"))?;
        let started = now_millis();
        let cwd = self.repo.root.to_string_lossy().to_string();
        let command_id = models::insert_command(
            self.db.conn(),
            self.repo_id,
            self.session_id,
            "run",
            command,
            Some(&cwd),
            started,
            Some(pre.id),
        )?;

        let spec = RunSpec::new(command, self.repo.root.clone());
        let outcome = exec::run_streaming(&spec, cancel, on_chunk)?;

        let post = self.snapshot(snapshot_kind::POST_COMMAND, &format!("after `{command}`"))?;
        let log_hash = self.store.write_bytes(&outcome.combined)?;
        let finished = now_millis();
        models::finish_command(
            self.db.conn(),
            command_id,
            outcome.exit_code.map(|c| c as i64),
            finished,
            outcome.duration_ms as i64,
            Some(post.id),
            Some(&log_hash),
        )?;
        let status = describe_status(&outcome);
        models::insert_event(
            self.db.conn(),
            self.repo_id,
            self.session_id,
            event_kind::COMMAND,
            Some(command_id),
            &format!("$ {command} — {status}"),
        )?;

        Ok(CommandResult {
            command_id,
            outcome,
            pre_snapshot: pre.id,
            post_snapshot: post.id,
        })
    }

    /// Whether a test command is configured.
    pub fn test_command(&self) -> Option<&str> {
        self.config.test_command.as_deref()
    }

    /// Run the configured test command with pre/post snapshots, deriving
    /// pass/fail strictly from the exit code, and auto-investigating a newly
    /// introduced failure.
    pub fn run_test<F>(&mut self, cancel: &CancelToken, on_chunk: F) -> Result<TestResult>
    where
        F: FnMut(&OutputChunk),
    {
        let command = self
            .test_command()
            .ok_or_else(|| {
                RewindError::Config(
                    "no test command configured — set `test_command` in .rewind.toml or run `rewind init`".into(),
                )
            })?
            .to_string();

        let pre = self.snapshot(snapshot_kind::PRE_TEST, "before test")?;
        let started = now_millis();
        let test_run_id = models::insert_test_run(
            self.db.conn(),
            self.repo_id,
            self.session_id,
            &command,
            started,
            Some(pre.id),
        )?;

        let spec = RunSpec::new(command.clone(), self.repo.root.clone())
            .with_timeout_secs(self.config.test_timeout_secs);
        let outcome = exec::run_streaming(&spec, cancel, on_chunk)?;

        let post = self.snapshot(snapshot_kind::POST_TEST, "after test")?;
        let log_hash = self.store.write_bytes(&outcome.combined)?;
        let finished = now_millis();
        let passed = outcome.passed();
        // `passed` is only meaningful when the run completed (not cancelled).
        let passed_field = if outcome.cancelled {
            None
        } else {
            Some(passed)
        };
        models::finish_test_run(
            self.db.conn(),
            test_run_id,
            outcome.exit_code.map(|c| c as i64),
            passed_field,
            outcome.cancelled,
            finished,
            outcome.duration_ms as i64,
            Some(post.id),
            Some(&log_hash),
        )?;
        let status = describe_status(&outcome);
        models::insert_event(
            self.db.conn(),
            self.repo_id,
            self.session_id,
            event_kind::TEST,
            Some(test_run_id),
            &format!(
                "test {status} ({})",
                crate::util::format_duration_ms(outcome.duration_ms)
            ),
        )?;

        // Auto-investigate when this run introduced a failure.
        let investigation = if passed_field == Some(false) {
            let run = models::get_test_run(self.db.conn(), test_run_id)?
                .ok_or_else(|| RewindError::other("test run vanished"))?;
            self.investigate_run(&run)?
        } else {
            None
        };

        Ok(TestResult {
            test_run_id,
            outcome,
            pre_snapshot: pre.id,
            post_snapshot: post.id,
            passed,
            investigation,
        })
    }

    /// Investigate a specific failing test run, persisting the result.
    pub fn investigate_run(&mut self, run: &TestRun) -> Result<Option<Investigation>> {
        let result = investigate::investigate(&self.db, &self.store, self.repo_id, run)?;
        if let Some(inv) = &result {
            let json = serde_json::to_string(inv)?;
            let inv_id = models::insert_investigation(
                self.db.conn(),
                self.repo_id,
                inv.failing_test_run_id,
                inv.passing_test_run_id,
                &inv.summary,
                &json,
            )?;
            models::insert_event(
                self.db.conn(),
                self.repo_id,
                self.session_id,
                event_kind::INVESTIGATION,
                Some(inv_id),
                &inv.summary,
            )?;
        }
        Ok(result)
    }

    /// Investigate the most recent test run if it failed.
    pub fn investigate_last(&mut self) -> Result<Option<Investigation>> {
        match models::last_test_run(self.db.conn(), self.repo_id)? {
            Some(run) if run.passed == Some(false) => self.investigate_run(&run),
            _ => Ok(None),
        }
    }

    /// Build a restore dry-run plan without modifying anything.
    pub fn plan_restore(&self, snapshot_id: i64, selection: &Selection) -> Result<RestorePlan> {
        restore::plan(
            &self.db,
            &self.store,
            &self.repo.root,
            &self.rules,
            snapshot_id,
            selection,
        )
    }

    /// Execute a restore: take a safety snapshot, apply the plan atomically,
    /// and record the operation.
    pub fn execute_restore(
        &mut self,
        snapshot_id: i64,
        selection: &Selection,
        plan: &RestorePlan,
    ) -> Result<RestoreOutcome> {
        let safety = self.snapshot(snapshot_kind::SAFETY, "safety before restore")?;
        let stats = restore::apply(&self.store, &self.repo.root, plan)?;
        let scope = match selection {
            Selection::All => "full",
            Selection::Paths(p) if p.len() == 1 => "single",
            Selection::Paths(_) => "selected",
        };
        let restore_id = models::insert_restore(
            self.db.conn(),
            self.repo_id,
            self.session_id,
            snapshot_id,
            safety.id,
            scope,
            stats.written as i64,
            stats.deleted as i64,
        )?;
        models::insert_event(
            self.db.conn(),
            self.repo_id,
            self.session_id,
            event_kind::RESTORE,
            Some(restore_id),
            &format!(
                "restore snapshot #{snapshot_id}: {} written, {} deleted",
                stats.written, stats.deleted
            ),
        )?;
        Ok(RestoreOutcome {
            restore_id,
            safety_snapshot_id: safety.id,
            stats,
        })
    }

    /// Undo a restore by restoring its safety snapshot in full.
    pub fn undo_restore(&mut self, restore_id: i64) -> Result<RestoreStats> {
        let record = models::get_restore(self.db.conn(), restore_id)?
            .ok_or_else(|| RewindError::other(format!("no restore #{restore_id}")))?;
        if record.undone {
            return Err(RewindError::other(format!(
                "restore #{restore_id} was already undone"
            )));
        }
        let plan = self.plan_restore(record.safety_snapshot_id, &Selection::All)?;
        let stats = restore::apply(&self.store, &self.repo.root, &plan)?;
        models::mark_restore_undone(self.db.conn(), restore_id)?;
        models::insert_event(
            self.db.conn(),
            self.repo_id,
            self.session_id,
            event_kind::RESTORE,
            Some(restore_id),
            &format!("undo restore #{restore_id}"),
        )?;
        Ok(stats)
    }

    // -- Read-only accessors used by the CLI and TUI ------------------------

    pub fn list_snapshots(&self) -> Result<Vec<Snapshot>> {
        models::list_snapshots(self.db.conn(), self.repo_id)
    }

    pub fn list_events(&self, limit: i64) -> Result<Vec<models::Event>> {
        models::list_events(self.db.conn(), self.repo_id, limit)
    }

    pub fn search_events(&self, query: &str, limit: i64) -> Result<Vec<models::Event>> {
        models::search_events(self.db.conn(), self.repo_id, query, limit)
    }

    pub fn list_test_runs(&self, limit: i64) -> Result<Vec<TestRun>> {
        models::list_test_runs(self.db.conn(), self.repo_id, limit)
    }

    pub fn last_test_run(&self) -> Result<Option<TestRun>> {
        models::last_test_run(self.db.conn(), self.repo_id)
    }

    pub fn get_snapshot(&self, id: i64) -> Result<Option<Snapshot>> {
        models::get_snapshot(self.db.conn(), id)
    }

    /// The post-test snapshot id of a given test run, if recorded.
    pub fn get_test_run_post(&self, test_run_id: i64) -> Result<Option<i64>> {
        Ok(models::get_test_run(self.db.conn(), test_run_id)?.and_then(|r| r.post_snapshot_id))
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // Best-effort: close out an open session so timelines aren't left open.
        let _ = self.end_session();
    }
}

fn describe_status(outcome: &CommandOutcome) -> String {
    if outcome.cancelled {
        "cancelled".into()
    } else if outcome.timed_out {
        "timed out".into()
    } else if outcome.passed() {
        "passed".into()
    } else {
        match outcome.exit_code {
            Some(code) => format!("failed (exit {code})"),
            None => "failed (killed)".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn init_engine() -> (tempfile::TempDir, Engine) {
        let tmp = tempfile::tempdir().unwrap();
        Command::new("git")
            .arg("-C")
            .arg(tmp.path())
            .arg("init")
            .output()
            .unwrap();
        let repo = Repo::discover(tmp.path()).unwrap();
        let config = Config {
            test_command: Some("sh ./run-test.sh".into()),
            ..Default::default()
        };
        let storage =
            crate::paths::StoragePaths::for_repo_in(&tmp.path().join(".data"), &repo.root).unwrap();
        let engine = Engine::open_with_storage(repo, config, storage).unwrap();
        (tmp, engine)
    }

    #[test]
    fn checkpoint_and_snapshot_flow() {
        let (tmp, mut engine) = init_engine();
        fs::write(tmp.path().join("a.txt"), b"hi").unwrap();
        let snap = engine.checkpoint(Some("first")).unwrap();
        assert!(snap.label.contains("first"));
        let snaps = engine.list_snapshots().unwrap();
        assert!(snaps.iter().any(|s| s.id == snap.id));
    }

    #[test]
    fn run_command_records_snapshots() {
        let (tmp, mut engine) = init_engine();
        let res = engine
            .run_command("echo hello > out.txt", &CancelToken::new(), |_| {})
            .unwrap();
        assert!(res.outcome.passed());
        assert!(tmp.path().join("out.txt").exists());
        assert_ne!(res.pre_snapshot, res.post_snapshot);
    }
}
