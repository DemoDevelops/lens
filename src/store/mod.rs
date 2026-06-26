//! Reversible store: persist full blobs keyed by blake3 hash, return compact refs.
//!
//! Anything truncated or compressed elsewhere in lens is first written here,
//! so the agent can always recover the full version with `lens_recall`.

pub mod compress;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Handle to the reversible blob store and the stats counters (both live in
/// `store.db`). A new SQLite connection is opened per operation, which keeps
/// the store safe to use from multiple async tasks without sharing a handle.
#[derive(Clone)]
pub struct Store {
    db_path: PathBuf,
}

impl Store {
    /// Open (creating if needed) the store under `dir`.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating data dir {}", dir.display()))?;
        let db_path = dir.join("store.db");
        let store = Store { db_path };
        store.init()?;
        Ok(store)
    }

    fn conn(&self) -> Result<Connection> {
        let conn = Connection::open(&self.db_path)
            .with_context(|| format!("opening store db {}", self.db_path.display()))?;
        crate::obs::configure_conn(&conn)?;
        Ok(conn)
    }

    fn init(&self) -> Result<()> {
        let conn = self.conn()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS blobs (
                hash    TEXT PRIMARY KEY,
                content BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS stats (
                key   TEXT PRIMARY KEY,
                value INTEGER NOT NULL
             );",
        )?;
        Ok(())
    }

    /// Store a blob and return its content-addressed ref (blake3 hex). Storing
    /// identical content twice is a no-op that returns the same ref.
    pub fn put(&self, content: &str) -> Result<String> {
        let hash = blake3::hash(content.as_bytes()).to_hex().to_string();
        let conn = self.conn()?;
        conn.execute(
            "INSERT OR IGNORE INTO blobs (hash, content) VALUES (?1, ?2)",
            rusqlite::params![hash, content.as_bytes()],
        )?;
        Ok(hash)
    }

    /// Fetch a blob by its ref. Accepts the full blake3 hash or a unique short
    /// prefix of it (git-style), so callers can surface a cheap truncated ref and
    /// still recover the full blob. Returns `None` if the ref is empty or unknown.
    pub fn get(&self, reference: &str) -> Result<Option<String>> {
        if reference.is_empty() {
            return Ok(None);
        }
        let conn = self.conn()?;
        // Hashes are lowercase hex, so every hash starting with `reference` lies
        // in the range [reference, reference + 'g'). This is a prefix scan over
        // the PRIMARY KEY index; the full-hash case resolves to exactly itself.
        let upper = format!("{reference}g");
        let mut stmt = conn.prepare(
            "SELECT content FROM blobs WHERE hash >= ?1 AND hash < ?2 ORDER BY hash LIMIT 1",
        )?;
        let mut rows = stmt.query(rusqlite::params![reference, upper])?;
        match rows.next()? {
            Some(row) => {
                let bytes: Vec<u8> = row.get(0)?;
                Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
            }
            None => Ok(None),
        }
    }

    /// Add `delta` to a named counter, returning nothing. Counters are created
    /// on first use.
    pub fn bump_stat(&self, key: &str, delta: i64) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO stats (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = value + ?2",
            rusqlite::params![key, delta],
        )?;
        Ok(())
    }

    /// Overwrite a counter to an absolute value.
    pub fn set_stat(&self, key: &str, value: i64) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO stats (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = ?2",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    /// The DB's current journal mode (e.g. `"wal"`). Lets callers confirm the
    /// concurrency hardening is engaged without reaching for rusqlite directly.
    pub fn journal_mode(&self) -> Result<String> {
        let conn = self.conn()?;
        let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        Ok(mode)
    }

    /// Read a counter, defaulting to 0.
    pub fn get_stat(&self, key: &str) -> Result<i64> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare("SELECT value FROM stats WHERE key = ?1")?;
        let mut rows = stmt.query([key])?;
        match rows.next()? {
            Some(row) => Ok(row.get(0)?),
            None => Ok(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn put_get_roundtrip() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let content = "hello\nworld\n".repeat(100);
        let r = store.put(&content).unwrap();
        assert_eq!(store.get(&r).unwrap().unwrap(), content);
    }

    #[test]
    fn put_is_deterministic_and_dedups() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let a = store.put("same").unwrap();
        let b = store.put("same").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn unknown_ref_is_none() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert!(store.get("deadbeef").unwrap().is_none());
    }

    #[test]
    fn get_by_short_prefix() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let content = "fn x() { /* body */ }\n".repeat(20);
        let full = store.put(&content).unwrap();
        // A cheap 12-char prefix recovers the full blob, and so does the full hash.
        assert_eq!(store.get(&full[..12]).unwrap().unwrap(), content);
        assert_eq!(store.get(&full).unwrap().unwrap(), content);
        // An empty ref never resolves to a blob.
        assert!(store.get("").unwrap().is_none());
    }

    #[test]
    fn stats_bump_and_read() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.bump_stat("k", 5).unwrap();
        store.bump_stat("k", 3).unwrap();
        assert_eq!(store.get_stat("k").unwrap(), 8);
        store.set_stat("k", 1).unwrap();
        assert_eq!(store.get_stat("k").unwrap(), 1);
        assert_eq!(store.get_stat("missing").unwrap(), 0);
    }
}
