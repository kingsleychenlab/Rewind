//! SQLite metadata store.
//!
//! Holds everything except file contents (those live in the content-addressed
//! object store). Schema changes go through numbered [`migrations`] guarded by
//! `PRAGMA user_version`, so an existing database upgrades in place.

use std::path::Path;

use rusqlite::Connection;

use crate::error::Result;

pub mod migrations;
pub mod models;

/// Owned SQLite connection plus Rewind conveniences.
pub struct Db {
    conn: Connection,
}

impl Db {
    /// Open (creating if needed) the database at `path`, apply pragmas, and run
    /// any pending migrations.
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path)?;
        Self::configure(&conn)?;
        migrations::run(&conn)?;
        Ok(Db { conn })
    }

    /// Open an in-memory database (used by tests).
    pub fn open_in_memory() -> Result<Db> {
        let conn = Connection::open_in_memory()?;
        Self::configure(&conn)?;
        migrations::run(&conn)?;
        Ok(Db { conn })
    }

    fn configure(conn: &Connection) -> Result<()> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(())
    }

    /// Borrow the underlying connection for ad-hoc queries.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Mutable access, used by model helpers that open transactions.
    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Run `f` inside a transaction, committing on `Ok` and rolling back on
    /// `Err`.
    pub fn transaction<T>(
        &mut self,
        f: impl FnOnce(&rusqlite::Transaction) -> Result<T>,
    ) -> Result<T> {
        let tx = self.conn.transaction()?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_and_migrates_in_memory() {
        let db = Db::open_in_memory().unwrap();
        let v: i64 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, migrations::LATEST_VERSION);
    }

    #[test]
    fn foreign_keys_enabled() {
        let db = Db::open_in_memory().unwrap();
        let on: i64 = db
            .conn()
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(on, 1);
    }
}
