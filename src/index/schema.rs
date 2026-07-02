//! Index handle: a Tantivy FTS store plus a small SQLite `index.db` that holds only
//! the mtime manifest (incremental re-index key) and a backend version marker.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};

use super::tantivy_index::TantivyStore;

/// Bump to force a clean full rebuild after an on-disk format change. A mismatch
/// clears the manifest so the next `index_path` re-reads every file into a fresh
/// Tantivy index (also discards a stale SQLite-FTS5-era index).
const FTS_BACKEND_VERSION: &str = "tantivy1";

/// Handle to the content index. Cloneable and cheap to share across async tasks:
/// the Tantivy store is behind an `Arc`, and the SQLite manifest opens a connection
/// per operation.
#[derive(Clone)]
pub struct Index {
    db_path: PathBuf,
    /// Base that stored chunk/manifest keys are relativized against — the repo
    /// root. Files under it are stored as repo-relative paths (portable across
    /// checkouts, no machine layout leaked into search output); files outside it
    /// fall back to absolute. Canonicalized so it matches the canonicalized walk
    /// paths in [`Index::index_path`].
    repo_root: PathBuf,
    store: Arc<TantivyStore>,
}

impl Index {
    /// Open (creating if needed) the index under `dir`. Stored paths default to
    /// being relativized against `dir`; the server overrides this with the repo
    /// root via [`Index::with_repo_root`].
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating data dir {}", dir.display()))?;
        let store = TantivyStore::open(&dir.join("fts")).context("opening tantivy fts")?;
        let idx = Index {
            db_path: dir.join("index.db"),
            repo_root: dir.to_path_buf(),
            store: Arc::new(store),
        };
        idx.init()?;
        Ok(idx)
    }

    /// Set the repo root that stored paths are relativized against. Canonicalized
    /// so it matches `index_path`'s canonicalized walk (on macOS `current_dir()`
    /// may be `/var/...` while the walk yields `/private/var/...`).
    pub fn with_repo_root(mut self, root: &Path) -> Self {
        self.repo_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        self
    }

    /// Storage key for an absolute file path: repo-root-relative when under the
    /// root, else the absolute path unchanged. Round-trips back to the absolute
    /// path via `repo_root.join(key)` (joining an absolute key discards the base).
    pub(crate) fn rel_key(&self, abs: &Path) -> String {
        abs.strip_prefix(&self.repo_root)
            .unwrap_or(abs)
            .to_string_lossy()
            .into_owned()
    }

    /// Resolve a stored key back to the absolute path to read from disk.
    pub(crate) fn abs_path(&self, key: &str) -> PathBuf {
        self.repo_root.join(key)
    }

    /// The Tantivy FTS store (build + search primitives).
    pub(crate) fn store(&self) -> &TantivyStore {
        &self.store
    }

    pub(crate) fn conn(&self) -> Result<Connection> {
        let conn = Connection::open(&self.db_path)
            .with_context(|| format!("opening index db {}", self.db_path.display()))?;
        crate::obs::configure_conn(&conn)?;
        Ok(conn)
    }

    fn init(&self) -> Result<()> {
        let conn = self.conn()?;
        // The SQLite side now holds only the mtime manifest + a version marker. Drop
        // any FTS5-era tables left by a pre-Tantivy index (harmless no-op otherwise).
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS file_manifest (
                 path TEXT PRIMARY KEY,
                 mtime INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS meta (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             DROP TABLE IF EXISTS chunks;
             DROP TABLE IF EXISTS chunks_tri;",
        )?;
        // Version gate: on a mismatch (fresh dir, or a stale/SQLite-era index), clear
        // the manifest so the next index_path does a full build into the Tantivy index.
        let ver: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = 'fts_backend'", [], |r| {
                r.get(0)
            })
            .optional()?;
        if ver.as_deref() != Some(FTS_BACKEND_VERSION) {
            conn.execute("DELETE FROM file_manifest", [])?;
            conn.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('fts_backend', ?1)",
                [FTS_BACKEND_VERSION],
            )?;
        }
        Ok(())
    }

    /// Total chunks currently indexed (live Tantivy docs).
    pub fn chunk_count(&self) -> Result<i64> {
        Ok(self.store.num_docs()? as i64)
    }
}
