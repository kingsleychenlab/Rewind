//! Schema migrations, applied in order and tracked via `PRAGMA user_version`.
//!
//! Each migration is an idempotent-at-its-version SQL script. To evolve the
//! schema, append a new `&str` to [`MIGRATIONS`] — never edit an existing one.

use rusqlite::Connection;

use crate::error::Result;

/// Ordered migration scripts. Index + 1 is the resulting `user_version`.
pub const MIGRATIONS: &[&str] = &[
    // v1 — initial schema.
    r#"
    CREATE TABLE repositories (
        id          INTEGER PRIMARY KEY,
        path        TEXT    NOT NULL UNIQUE,
        hash        TEXT    NOT NULL,
        created_at  INTEGER NOT NULL
    );

    CREATE TABLE sessions (
        id          INTEGER PRIMARY KEY,
        repo_id     INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
        started_at  INTEGER NOT NULL,
        ended_at    INTEGER,
        pid         INTEGER,
        shell       TEXT
    );

    CREATE TABLE snapshots (
        id          INTEGER PRIMARY KEY,
        repo_id     INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
        session_id  INTEGER REFERENCES sessions(id) ON DELETE SET NULL,
        parent_id   INTEGER REFERENCES snapshots(id) ON DELETE SET NULL,
        kind        TEXT    NOT NULL,
        label       TEXT    NOT NULL,
        created_at  INTEGER NOT NULL,
        git_branch  TEXT,
        git_head    TEXT,
        git_dirty   INTEGER NOT NULL DEFAULT 0,
        file_count  INTEGER NOT NULL DEFAULT 0,
        total_bytes INTEGER NOT NULL DEFAULT 0
    );

    CREATE TABLE snapshot_files (
        id          INTEGER PRIMARY KEY,
        snapshot_id INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
        path        TEXT    NOT NULL,
        hash        TEXT    NOT NULL,
        size        INTEGER NOT NULL,
        mode        INTEGER NOT NULL DEFAULT 0,
        UNIQUE(snapshot_id, path)
    );
    CREATE INDEX idx_snapshot_files_snapshot ON snapshot_files(snapshot_id);
    CREATE INDEX idx_snapshot_files_hash ON snapshot_files(hash);

    CREATE TABLE commands (
        id              INTEGER PRIMARY KEY,
        repo_id         INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
        session_id      INTEGER REFERENCES sessions(id) ON DELETE SET NULL,
        kind            TEXT    NOT NULL,
        command         TEXT    NOT NULL,
        cwd             TEXT,
        exit_code       INTEGER,
        duration_ms     INTEGER,
        started_at      INTEGER NOT NULL,
        finished_at     INTEGER,
        pre_snapshot_id  INTEGER REFERENCES snapshots(id) ON DELETE SET NULL,
        post_snapshot_id INTEGER REFERENCES snapshots(id) ON DELETE SET NULL,
        output_object   TEXT
    );

    CREATE TABLE test_runs (
        id              INTEGER PRIMARY KEY,
        repo_id         INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
        session_id      INTEGER REFERENCES sessions(id) ON DELETE SET NULL,
        command         TEXT    NOT NULL,
        exit_code       INTEGER,
        passed          INTEGER,
        cancelled       INTEGER NOT NULL DEFAULT 0,
        duration_ms     INTEGER,
        started_at      INTEGER NOT NULL,
        finished_at     INTEGER,
        pre_snapshot_id  INTEGER REFERENCES snapshots(id) ON DELETE SET NULL,
        post_snapshot_id INTEGER REFERENCES snapshots(id) ON DELETE SET NULL,
        log_object      TEXT
    );
    CREATE INDEX idx_test_runs_repo_time ON test_runs(repo_id, started_at);

    CREATE TABLE investigations (
        id                  INTEGER PRIMARY KEY,
        repo_id             INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
        failing_test_run_id INTEGER NOT NULL REFERENCES test_runs(id) ON DELETE CASCADE,
        passing_test_run_id INTEGER REFERENCES test_runs(id) ON DELETE SET NULL,
        created_at          INTEGER NOT NULL,
        summary             TEXT    NOT NULL,
        result_json         TEXT    NOT NULL
    );

    CREATE TABLE restores (
        id                  INTEGER PRIMARY KEY,
        repo_id             INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
        session_id          INTEGER REFERENCES sessions(id) ON DELETE SET NULL,
        source_snapshot_id  INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
        safety_snapshot_id  INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
        created_at          INTEGER NOT NULL,
        scope               TEXT    NOT NULL,
        files_written       INTEGER NOT NULL DEFAULT 0,
        files_deleted       INTEGER NOT NULL DEFAULT 0,
        undone              INTEGER NOT NULL DEFAULT 0
    );

    CREATE TABLE events (
        id          INTEGER PRIMARY KEY,
        repo_id     INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
        session_id  INTEGER REFERENCES sessions(id) ON DELETE SET NULL,
        kind        TEXT    NOT NULL,
        ref_id      INTEGER,
        summary     TEXT    NOT NULL,
        created_at  INTEGER NOT NULL
    );
    CREATE INDEX idx_events_repo_time ON events(repo_id, created_at);
    CREATE INDEX idx_events_kind ON events(kind);
    "#,
];

/// The schema version produced by applying every migration.
pub const LATEST_VERSION: i64 = MIGRATIONS.len() as i64;

/// Apply any migrations whose version is greater than the database's current
/// `user_version`, inside a transaction each.
pub fn run(conn: &Connection) -> Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (idx, script) in MIGRATIONS.iter().enumerate() {
        let version = (idx + 1) as i64;
        if version <= current {
            continue;
        }
        conn.execute_batch("BEGIN")?;
        match conn.execute_batch(script) {
            Ok(()) => {
                // PRAGMA cannot be parameterized; version is a trusted integer.
                conn.execute_batch(&format!("PRAGMA user_version = {version}; COMMIT"))?;
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e.into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_bring_fresh_db_to_latest() {
        let conn = Connection::open_in_memory().unwrap();
        run(&conn).unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, LATEST_VERSION);
    }

    #[test]
    fn migrations_are_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        run(&conn).unwrap();
        // Running again should be a no-op (no "table already exists" error).
        run(&conn).unwrap();
    }

    #[test]
    fn expected_tables_exist() {
        let conn = Connection::open_in_memory().unwrap();
        run(&conn).unwrap();
        for table in [
            "repositories",
            "sessions",
            "snapshots",
            "snapshot_files",
            "commands",
            "test_runs",
            "investigations",
            "restores",
            "events",
        ] {
            let count: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "missing table {table}");
        }
    }
}
