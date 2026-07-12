//! Typed rows and query helpers over the SQLite schema.
//!
//! Functions take a `&Connection` (or `&Transaction`, which derefs to it) so
//! callers control transaction boundaries.

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::util::now_millis;

/// String constants for `snapshots.kind`.
pub mod snapshot_kind {
    pub const SESSION_START: &str = "session_start";
    pub const PRE_COMMAND: &str = "pre_command";
    pub const POST_COMMAND: &str = "post_command";
    pub const PRE_TEST: &str = "pre_test";
    pub const POST_TEST: &str = "post_test";
    pub const CHECKPOINT: &str = "checkpoint";
    pub const PRE_RESTORE: &str = "pre_restore";
    pub const SAFETY: &str = "safety";
    pub const MANUAL: &str = "manual";
}

/// String constants for `events.kind`.
pub mod event_kind {
    pub const SESSION_START: &str = "session_start";
    pub const SESSION_END: &str = "session_end";
    pub const COMMAND: &str = "command";
    pub const TEST: &str = "test";
    pub const SNAPSHOT: &str = "snapshot";
    pub const CHECKPOINT: &str = "checkpoint";
    pub const RESTORE: &str = "restore";
    pub const INVESTIGATION: &str = "investigation";
}

// ---------------------------------------------------------------------------
// repositories
// ---------------------------------------------------------------------------

/// Insert the repository row if absent and return its id.
pub fn upsert_repository(conn: &Connection, path: &str, hash: &str) -> Result<i64> {
    conn.execute(
        "INSERT INTO repositories(path, hash, created_at) VALUES(?1, ?2, ?3)
         ON CONFLICT(path) DO NOTHING",
        params![path, hash, now_millis()],
    )?;
    let id: i64 = conn.query_row(
        "SELECT id FROM repositories WHERE path = ?1",
        params![path],
        |r| r.get(0),
    )?;
    Ok(id)
}

// ---------------------------------------------------------------------------
// sessions
// ---------------------------------------------------------------------------

/// Begin a session and return its id.
pub fn start_session(conn: &Connection, repo_id: i64, pid: u32, shell: &str) -> Result<i64> {
    conn.execute(
        "INSERT INTO sessions(repo_id, started_at, pid, shell) VALUES(?1, ?2, ?3, ?4)",
        params![repo_id, now_millis(), pid, shell],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Mark a session ended.
pub fn end_session(conn: &Connection, session_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET ended_at = ?1 WHERE id = ?2",
        params![now_millis(), session_id],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// snapshots
// ---------------------------------------------------------------------------

/// A snapshot manifest header.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: i64,
    pub repo_id: i64,
    pub session_id: Option<i64>,
    pub parent_id: Option<i64>,
    pub kind: String,
    pub label: String,
    pub created_at: i64,
    pub git_branch: Option<String>,
    pub git_head: Option<String>,
    pub git_dirty: bool,
    pub file_count: i64,
    pub total_bytes: i64,
}

/// Fields required to create a snapshot header.
#[derive(Debug, Clone)]
pub struct NewSnapshot {
    pub repo_id: i64,
    pub session_id: Option<i64>,
    pub parent_id: Option<i64>,
    pub kind: String,
    pub label: String,
    pub created_at: i64,
    pub git_branch: Option<String>,
    pub git_head: Option<String>,
    pub git_dirty: bool,
    pub file_count: i64,
    pub total_bytes: i64,
}

fn row_to_snapshot(r: &Row) -> rusqlite::Result<Snapshot> {
    Ok(Snapshot {
        id: r.get("id")?,
        repo_id: r.get("repo_id")?,
        session_id: r.get("session_id")?,
        parent_id: r.get("parent_id")?,
        kind: r.get("kind")?,
        label: r.get("label")?,
        created_at: r.get("created_at")?,
        git_branch: r.get("git_branch")?,
        git_head: r.get("git_head")?,
        git_dirty: r.get::<_, i64>("git_dirty")? != 0,
        file_count: r.get("file_count")?,
        total_bytes: r.get("total_bytes")?,
    })
}

const SNAPSHOT_COLS: &str = "id, repo_id, session_id, parent_id, kind, label, created_at, \
                             git_branch, git_head, git_dirty, file_count, total_bytes";

/// Insert a snapshot header, returning its id.
pub fn insert_snapshot(conn: &Connection, s: &NewSnapshot) -> Result<i64> {
    conn.execute(
        "INSERT INTO snapshots(repo_id, session_id, parent_id, kind, label, created_at,
             git_branch, git_head, git_dirty, file_count, total_bytes)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        params![
            s.repo_id,
            s.session_id,
            s.parent_id,
            s.kind,
            s.label,
            s.created_at,
            s.git_branch,
            s.git_head,
            s.git_dirty as i64,
            s.file_count,
            s.total_bytes,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Fetch a snapshot header by id.
pub fn get_snapshot(conn: &Connection, id: i64) -> Result<Option<Snapshot>> {
    let s = conn
        .query_row(
            &format!("SELECT {SNAPSHOT_COLS} FROM snapshots WHERE id = ?1"),
            params![id],
            row_to_snapshot,
        )
        .optional()?;
    Ok(s)
}

/// All snapshots for a repository, newest first.
pub fn list_snapshots(conn: &Connection, repo_id: i64) -> Result<Vec<Snapshot>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {SNAPSHOT_COLS} FROM snapshots WHERE repo_id = ?1 ORDER BY created_at DESC, id DESC"
    ))?;
    let rows = stmt.query_map(params![repo_id], row_to_snapshot)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Most recent snapshot for a repository, if any.
pub fn latest_snapshot(conn: &Connection, repo_id: i64) -> Result<Option<Snapshot>> {
    let s = conn
        .query_row(
            &format!(
                "SELECT {SNAPSHOT_COLS} FROM snapshots WHERE repo_id = ?1 \
                 ORDER BY created_at DESC, id DESC LIMIT 1"
            ),
            params![repo_id],
            row_to_snapshot,
        )
        .optional()?;
    Ok(s)
}

// ---------------------------------------------------------------------------
// snapshot_files
// ---------------------------------------------------------------------------

/// One tracked file within a snapshot manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotFile {
    pub path: String,
    pub hash: String,
    pub size: i64,
    pub mode: u32,
}

/// Insert a manifest entry.
pub fn insert_snapshot_file(conn: &Connection, snapshot_id: i64, f: &SnapshotFile) -> Result<()> {
    conn.execute(
        "INSERT INTO snapshot_files(snapshot_id, path, hash, size, mode)
         VALUES(?1,?2,?3,?4,?5)",
        params![snapshot_id, f.path, f.hash, f.size, f.mode as i64],
    )?;
    Ok(())
}

/// All files in a snapshot, ordered by path.
pub fn list_snapshot_files(conn: &Connection, snapshot_id: i64) -> Result<Vec<SnapshotFile>> {
    let mut stmt = conn.prepare(
        "SELECT path, hash, size, mode FROM snapshot_files WHERE snapshot_id = ?1 ORDER BY path",
    )?;
    let rows = stmt.query_map(params![snapshot_id], |r| {
        Ok(SnapshotFile {
            path: r.get("path")?,
            hash: r.get("hash")?,
            size: r.get("size")?,
            mode: r.get::<_, i64>("mode")? as u32,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

/// A captured command execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandRun {
    pub id: i64,
    pub kind: String,
    pub command: String,
    pub exit_code: Option<i64>,
    pub duration_ms: Option<i64>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub pre_snapshot_id: Option<i64>,
    pub post_snapshot_id: Option<i64>,
    pub output_object: Option<String>,
}

/// Insert a starting command row and return its id.
#[allow(clippy::too_many_arguments)]
pub fn insert_command(
    conn: &Connection,
    repo_id: i64,
    session_id: Option<i64>,
    kind: &str,
    command: &str,
    cwd: Option<&str>,
    started_at: i64,
    pre_snapshot_id: Option<i64>,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO commands(repo_id, session_id, kind, command, cwd, started_at, pre_snapshot_id)
         VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![
            repo_id,
            session_id,
            kind,
            command,
            cwd,
            started_at,
            pre_snapshot_id
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Record a command's result.
pub fn finish_command(
    conn: &Connection,
    id: i64,
    exit_code: Option<i64>,
    finished_at: i64,
    duration_ms: i64,
    post_snapshot_id: Option<i64>,
    output_object: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE commands SET exit_code=?1, finished_at=?2, duration_ms=?3,
             post_snapshot_id=?4, output_object=?5 WHERE id=?6",
        params![
            exit_code,
            finished_at,
            duration_ms,
            post_snapshot_id,
            output_object,
            id
        ],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// test_runs
// ---------------------------------------------------------------------------

/// A test execution and its outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestRun {
    pub id: i64,
    pub repo_id: i64,
    pub command: String,
    pub exit_code: Option<i64>,
    pub passed: Option<bool>,
    pub cancelled: bool,
    pub duration_ms: Option<i64>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub pre_snapshot_id: Option<i64>,
    pub post_snapshot_id: Option<i64>,
    pub log_object: Option<String>,
}

fn row_to_test_run(r: &Row) -> rusqlite::Result<TestRun> {
    Ok(TestRun {
        id: r.get("id")?,
        repo_id: r.get("repo_id")?,
        command: r.get("command")?,
        exit_code: r.get("exit_code")?,
        passed: r.get::<_, Option<i64>>("passed")?.map(|v| v != 0),
        cancelled: r.get::<_, i64>("cancelled")? != 0,
        duration_ms: r.get("duration_ms")?,
        started_at: r.get("started_at")?,
        finished_at: r.get("finished_at")?,
        pre_snapshot_id: r.get("pre_snapshot_id")?,
        post_snapshot_id: r.get("post_snapshot_id")?,
        log_object: r.get("log_object")?,
    })
}

const TEST_COLS: &str = "id, repo_id, command, exit_code, passed, cancelled, duration_ms, \
                         started_at, finished_at, pre_snapshot_id, post_snapshot_id, log_object";

/// Insert a starting test-run row and return its id.
pub fn insert_test_run(
    conn: &Connection,
    repo_id: i64,
    session_id: Option<i64>,
    command: &str,
    started_at: i64,
    pre_snapshot_id: Option<i64>,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO test_runs(repo_id, session_id, command, started_at, pre_snapshot_id)
         VALUES(?1,?2,?3,?4,?5)",
        params![repo_id, session_id, command, started_at, pre_snapshot_id],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Record a test run's outcome. `passed` is derived from the exit code by the
/// caller (0 == pass) and is only meaningful when not cancelled.
#[allow(clippy::too_many_arguments)]
pub fn finish_test_run(
    conn: &Connection,
    id: i64,
    exit_code: Option<i64>,
    passed: Option<bool>,
    cancelled: bool,
    finished_at: i64,
    duration_ms: i64,
    post_snapshot_id: Option<i64>,
    log_object: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE test_runs SET exit_code=?1, passed=?2, cancelled=?3, finished_at=?4,
             duration_ms=?5, post_snapshot_id=?6, log_object=?7 WHERE id=?8",
        params![
            exit_code,
            passed.map(|p| p as i64),
            cancelled as i64,
            finished_at,
            duration_ms,
            post_snapshot_id,
            log_object,
            id
        ],
    )?;
    Ok(())
}

/// Fetch a test run by id.
pub fn get_test_run(conn: &Connection, id: i64) -> Result<Option<TestRun>> {
    Ok(conn
        .query_row(
            &format!("SELECT {TEST_COLS} FROM test_runs WHERE id = ?1"),
            params![id],
            row_to_test_run,
        )
        .optional()?)
}

/// Test runs for a repository, newest first (up to `limit`).
pub fn list_test_runs(conn: &Connection, repo_id: i64, limit: i64) -> Result<Vec<TestRun>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TEST_COLS} FROM test_runs WHERE repo_id = ?1 \
         ORDER BY started_at DESC, id DESC LIMIT ?2"
    ))?;
    let rows = stmt.query_map(params![repo_id, limit], row_to_test_run)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The most recent completed test run, if any.
pub fn last_test_run(conn: &Connection, repo_id: i64) -> Result<Option<TestRun>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {TEST_COLS} FROM test_runs WHERE repo_id = ?1 AND finished_at IS NOT NULL \
                 ORDER BY started_at DESC, id DESC LIMIT 1"
            ),
            params![repo_id],
            row_to_test_run,
        )
        .optional()?)
}

/// The most recent passing test run strictly before `before_started_at`.
pub fn last_passing_before(
    conn: &Connection,
    repo_id: i64,
    before_started_at: i64,
) -> Result<Option<TestRun>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {TEST_COLS} FROM test_runs WHERE repo_id = ?1 AND passed = 1 \
                 AND started_at < ?2 ORDER BY started_at DESC, id DESC LIMIT 1"
            ),
            params![repo_id, before_started_at],
            row_to_test_run,
        )
        .optional()?)
}

// ---------------------------------------------------------------------------
// investigations
// ---------------------------------------------------------------------------

/// Persist an investigation result (the ranked causes are JSON in
/// `result_json`).
pub fn insert_investigation(
    conn: &Connection,
    repo_id: i64,
    failing_test_run_id: i64,
    passing_test_run_id: Option<i64>,
    summary: &str,
    result_json: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO investigations(repo_id, failing_test_run_id, passing_test_run_id,
             created_at, summary, result_json) VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            repo_id,
            failing_test_run_id,
            passing_test_run_id,
            now_millis(),
            summary,
            result_json
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

// ---------------------------------------------------------------------------
// restores
// ---------------------------------------------------------------------------

/// Record a restore operation.
#[allow(clippy::too_many_arguments)]
pub fn insert_restore(
    conn: &Connection,
    repo_id: i64,
    session_id: Option<i64>,
    source_snapshot_id: i64,
    safety_snapshot_id: i64,
    scope: &str,
    files_written: i64,
    files_deleted: i64,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO restores(repo_id, session_id, source_snapshot_id, safety_snapshot_id,
             created_at, scope, files_written, files_deleted) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            repo_id,
            session_id,
            source_snapshot_id,
            safety_snapshot_id,
            now_millis(),
            scope,
            files_written,
            files_deleted
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Mark a restore as undone.
pub fn mark_restore_undone(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("UPDATE restores SET undone = 1 WHERE id = ?1", params![id])?;
    Ok(())
}

/// A recorded restore operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Restore {
    pub id: i64,
    pub source_snapshot_id: i64,
    pub safety_snapshot_id: i64,
    pub scope: String,
    pub files_written: i64,
    pub files_deleted: i64,
    pub undone: bool,
    pub created_at: i64,
}

/// Fetch a restore by id.
pub fn get_restore(conn: &Connection, id: i64) -> Result<Option<Restore>> {
    Ok(conn
        .query_row(
            "SELECT id, source_snapshot_id, safety_snapshot_id, scope, files_written,
                 files_deleted, undone, created_at FROM restores WHERE id = ?1",
            params![id],
            |r| {
                Ok(Restore {
                    id: r.get("id")?,
                    source_snapshot_id: r.get("source_snapshot_id")?,
                    safety_snapshot_id: r.get("safety_snapshot_id")?,
                    scope: r.get("scope")?,
                    files_written: r.get("files_written")?,
                    files_deleted: r.get("files_deleted")?,
                    undone: r.get::<_, i64>("undone")? != 0,
                    created_at: r.get("created_at")?,
                })
            },
        )
        .optional()?)
}

/// The most recent restore that has not been undone, if any.
pub fn last_undoable_restore(conn: &Connection, repo_id: i64) -> Result<Option<Restore>> {
    Ok(conn
        .query_row(
            "SELECT id, source_snapshot_id, safety_snapshot_id, scope, files_written,
                 files_deleted, undone, created_at FROM restores
             WHERE repo_id = ?1 AND undone = 0 ORDER BY created_at DESC, id DESC LIMIT 1",
            params![repo_id],
            |r| {
                Ok(Restore {
                    id: r.get("id")?,
                    source_snapshot_id: r.get("source_snapshot_id")?,
                    safety_snapshot_id: r.get("safety_snapshot_id")?,
                    scope: r.get("scope")?,
                    files_written: r.get("files_written")?,
                    files_deleted: r.get("files_deleted")?,
                    undone: r.get::<_, i64>("undone")? != 0,
                    created_at: r.get("created_at")?,
                })
            },
        )
        .optional()?)
}

// ---------------------------------------------------------------------------
// events
// ---------------------------------------------------------------------------

/// A timeline event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    pub kind: String,
    pub ref_id: Option<i64>,
    pub summary: String,
    pub created_at: i64,
}

/// Append a timeline event.
pub fn insert_event(
    conn: &Connection,
    repo_id: i64,
    session_id: Option<i64>,
    kind: &str,
    ref_id: Option<i64>,
    summary: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO events(repo_id, session_id, kind, ref_id, summary, created_at)
         VALUES(?1,?2,?3,?4,?5,?6)",
        params![repo_id, session_id, kind, ref_id, summary, now_millis()],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Recent events for a repository, newest first.
pub fn list_events(conn: &Connection, repo_id: i64, limit: i64) -> Result<Vec<Event>> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, ref_id, summary, created_at FROM events \
         WHERE repo_id = ?1 ORDER BY created_at DESC, id DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![repo_id, limit], |r| {
        Ok(Event {
            id: r.get("id")?,
            kind: r.get("kind")?,
            ref_id: r.get("ref_id")?,
            summary: r.get("summary")?,
            created_at: r.get("created_at")?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Case-insensitive substring search over event summaries.
pub fn search_events(
    conn: &Connection,
    repo_id: i64,
    query: &str,
    limit: i64,
) -> Result<Vec<Event>> {
    let like = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
    let mut stmt = conn.prepare(
        "SELECT id, kind, ref_id, summary, created_at FROM events \
         WHERE repo_id = ?1 AND summary LIKE ?2 ESCAPE '\\' \
         ORDER BY created_at DESC, id DESC LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![repo_id, like, limit], |r| {
        Ok(Event {
            id: r.get("id")?,
            kind: r.get("kind")?,
            ref_id: r.get("ref_id")?,
            summary: r.get("summary")?,
            created_at: r.get("created_at")?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn setup() -> (Db, i64) {
        let db = Db::open_in_memory().unwrap();
        let repo_id = upsert_repository(db.conn(), "/tmp/repo", "hash").unwrap();
        (db, repo_id)
    }

    #[test]
    fn upsert_is_idempotent() {
        let (db, id1) = setup();
        let id2 = upsert_repository(db.conn(), "/tmp/repo", "hash").unwrap();
        assert_eq!(id1, id2);
    }

    #[test]
    fn snapshot_roundtrip() {
        let (db, repo_id) = setup();
        let sid = insert_snapshot(
            db.conn(),
            &NewSnapshot {
                repo_id,
                session_id: None,
                parent_id: None,
                kind: snapshot_kind::MANUAL.into(),
                label: "manual".into(),
                created_at: now_millis(),
                git_branch: Some("main".into()),
                git_head: None,
                git_dirty: true,
                file_count: 1,
                total_bytes: 3,
            },
        )
        .unwrap();
        insert_snapshot_file(
            db.conn(),
            sid,
            &SnapshotFile {
                path: "a.txt".into(),
                hash: "deadbeef".into(),
                size: 3,
                mode: 0o644,
            },
        )
        .unwrap();
        let got = get_snapshot(db.conn(), sid).unwrap().unwrap();
        assert!(got.git_dirty);
        let files = list_snapshot_files(db.conn(), sid).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "a.txt");
        assert_eq!(files[0].mode, 0o644);
    }

    #[test]
    fn test_run_pass_fail_queries() {
        let (db, repo_id) = setup();
        let t1 = insert_test_run(db.conn(), repo_id, None, "cargo test", 100, None).unwrap();
        finish_test_run(
            db.conn(),
            t1,
            Some(0),
            Some(true),
            false,
            200,
            100,
            None,
            None,
        )
        .unwrap();
        let t2 = insert_test_run(db.conn(), repo_id, None, "cargo test", 300, None).unwrap();
        finish_test_run(
            db.conn(),
            t2,
            Some(1),
            Some(false),
            false,
            400,
            100,
            None,
            None,
        )
        .unwrap();

        let last = last_test_run(db.conn(), repo_id).unwrap().unwrap();
        assert_eq!(last.id, t2);
        assert_eq!(last.passed, Some(false));

        let prev_pass = last_passing_before(db.conn(), repo_id, 300)
            .unwrap()
            .unwrap();
        assert_eq!(prev_pass.id, t1);
    }

    #[test]
    fn event_search() {
        let (db, repo_id) = setup();
        insert_event(
            db.conn(),
            repo_id,
            None,
            event_kind::TEST,
            None,
            "ran cargo test",
        )
        .unwrap();
        insert_event(
            db.conn(),
            repo_id,
            None,
            event_kind::COMMAND,
            None,
            "npm install",
        )
        .unwrap();
        let hits = search_events(db.conn(), repo_id, "cargo", 10).unwrap();
        assert_eq!(hits.len(), 1);
    }
}
