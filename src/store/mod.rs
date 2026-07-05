//! Reversible store: persist full blobs keyed by blake3 hash, return compact refs.
//!
//! Anything truncated or compressed elsewhere in lens is first written here,
//! so the agent can always recover the full version with `lens_recall`.

pub mod compress;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Handle to the reversible blob store, the stats counters, and the blob
/// provenance table (all live in `store.db`). A new SQLite connection is
/// opened per operation, which keeps the store safe to use from multiple
/// async tasks without sharing a handle.
#[derive(Clone)]
pub struct Store {
    db_path: PathBuf,
}

/// Provenance of a blob that is a byte-for-byte snapshot of a source file:
/// the blob's full hash plus the file it was captured from. Because the blob
/// is the file's exact bytes, `hash` doubles as the captured content hash —
/// staleness is simply `blake3(current file) != hash`.
#[derive(Debug)]
pub struct BlobSource {
    pub hash: String,
    pub path: String,
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
             );
             CREATE TABLE IF NOT EXISTS blob_sources (
                hash TEXT PRIMARY KEY,
                path TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS blob_sources_path ON blob_sources(path);",
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

    /// Record that the blob at `hash` (full hash, as returned by [`Store::put`])
    /// is a snapshot of the file at `path`, so a later edit to the file can be
    /// surfaced: `lens_recall` flags the ref stale and the session hook posts a
    /// supersession notice. Re-recording a hash overwrites its path (the same
    /// content captured from a new location: last capture wins).
    pub fn record_source(&self, hash: &str, path: &str) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO blob_sources (hash, path) VALUES (?1, ?2)
             ON CONFLICT(hash) DO UPDATE SET path = ?2",
            rusqlite::params![hash, path],
        )?;
        Ok(())
    }

    /// The recorded source of a blob, if any. Accepts the full hash or a unique
    /// short prefix (the same git-style resolution as [`Store::get`]) and always
    /// returns the full hash, so the caller can compare it against the live
    /// file's content hash.
    pub fn source(&self, reference: &str) -> Result<Option<BlobSource>> {
        if reference.is_empty() {
            return Ok(None);
        }
        let conn = self.conn()?;
        // Same prefix-scan trick as `get`: lowercase hex keys make
        // [reference, reference + 'g') cover every hash with that prefix.
        let upper = format!("{reference}g");
        let mut stmt = conn.prepare(
            "SELECT hash, path FROM blob_sources WHERE hash >= ?1 AND hash < ?2 ORDER BY hash LIMIT 1",
        )?;
        let mut rows = stmt.query(rusqlite::params![reference, upper])?;
        match rows.next()? {
            Some(row) => Ok(Some(BlobSource {
                hash: row.get(0)?,
                path: row.get(1)?,
            })),
            None => Ok(None),
        }
    }

    /// Every snapshot recorded for `path`, so an edit to the file can count how
    /// many previously handed-out refs it supersedes.
    pub fn sources_for_path(&self, path: &str) -> Result<Vec<BlobSource>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare("SELECT hash, path FROM blob_sources WHERE path = ?1")?;
        let rows = stmt.query_map([path], |row| {
            Ok(BlobSource {
                hash: row.get(0)?,
                path: row.get(1)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
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
    fn source_roundtrip_and_prefix() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let hash = store.put("fn a() {}\n").unwrap();
        store.record_source(&hash, "/repo/src/a.rs").unwrap();
        // The full hash and a short prefix both resolve to the recorded source,
        // and the returned hash is always the full one (callers compare it
        // against the live file's content hash).
        for r in [hash.as_str(), &hash[..12]] {
            let src = store.source(r).unwrap().unwrap();
            assert_eq!(src.hash, hash);
            assert_eq!(src.path, "/repo/src/a.rs");
        }
        // A blob without provenance, an unknown ref, and an empty ref have none.
        let plain = store.put("no provenance").unwrap();
        assert!(store.source(&plain).unwrap().is_none());
        assert!(store.source("deadbeef").unwrap().is_none());
        assert!(store.source("").unwrap().is_none());
    }

    #[test]
    fn sources_for_path_lists_recorded_snapshots() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let v1 = store.put("v1").unwrap();
        let v2 = store.put("v2").unwrap();
        store.record_source(&v1, "/repo/a.rs").unwrap();
        store.record_source(&v2, "/repo/a.rs").unwrap();
        let other = store.put("other").unwrap();
        store.record_source(&other, "/repo/b.rs").unwrap();

        let a = store.sources_for_path("/repo/a.rs").unwrap();
        assert_eq!(a.len(), 2);
        assert!(a.iter().any(|s| s.hash == v1));
        assert!(a.iter().any(|s| s.hash == v2));
        assert!(store
            .sources_for_path("/repo/missing.rs")
            .unwrap()
            .is_empty());

        // The blob hash is the key: re-recording a hash moves it to the new path.
        store.record_source(&v1, "/repo/b.rs").unwrap();
        assert_eq!(store.sources_for_path("/repo/a.rs").unwrap().len(), 1);
        assert_eq!(store.sources_for_path("/repo/b.rs").unwrap().len(), 2);
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
