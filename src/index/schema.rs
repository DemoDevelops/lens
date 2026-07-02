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
            // The server's on-disk staleness cache (`index.manifest.json`, a sibling of
            // index.db) still describes the pre-migration index. Remove it so the first
            // `ensure_index` after this wipe cannot match that stale snapshot against a
            // now-empty (or session-only) index and wrongly skip the mandatory full
            // rebuild, leaving `lens_search` with no code. Best-effort: absent on a fresh dir.
            if let Some(parent) = self.db_path.parent() {
                let _ = std::fs::remove_file(parent.join("index.manifest.json"));
            }
            // Reclaim the pages freed by dropping the pre-Tantivy FTS5 tables above: a
            // real SQLite-era index.db can be tens of MB of trigram postings, and a bare
            // DROP leaves them as free pages so the file never shrinks. Best-effort and
            // before the marker write: a crash mid-VACUUM just retries on the next open,
            // and a slow or contended VACUUM can never fail or block server startup.
            let _ = conn.execute_batch("VACUUM");
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// A pre-Tantivy `index.db` (FTS5 `chunks`/`chunks_tri` + a populated
    /// `file_manifest`, no `fts_backend` marker) must migrate cleanly the first time
    /// the Tantivy build opens it: the FTS5 tables are dropped, the manifest is wiped
    /// so the server's chunk-count gate forces a full rebuild, the version marker is
    /// set, and the pages freed by the drop are reclaimed rather than left as dead disk.
    #[test]
    fn upgrades_a_pre_tantivy_fts5_index_cleanly() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("index.db");

        // Realistic SQLite-era index.db: the old FTS5 vtables with enough rows to
        // allocate many pages, plus a manifest row, and no meta table.
        {
            let mut conn = Connection::open(&db).unwrap();
            crate::obs::configure_conn(&conn).unwrap();
            conn.execute_batch(
                "CREATE VIRTUAL TABLE chunks USING fts5(
                     path UNINDEXED, chunk_id UNINDEXED, symbols, content, tokenize='porter');
                 CREATE VIRTUAL TABLE chunks_tri USING fts5(
                     path UNINDEXED, chunk_id UNINDEXED, content, tokenize='trigram');
                 CREATE TABLE file_manifest(path TEXT PRIMARY KEY, mtime INTEGER NOT NULL);",
            )
            .unwrap();
            let filler = "fn handle_request() { dispatch(); } ".repeat(8);
            let tx = conn.transaction().unwrap();
            for i in 0..300 {
                let path = format!("src/f{i}.rs");
                tx.execute(
                    "INSERT INTO chunks(path, chunk_id, symbols, content) VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![path, i, "handle_request", filler],
                )
                .unwrap();
                tx.execute(
                    "INSERT INTO chunks_tri(path, chunk_id, content) VALUES (?1, ?2, ?3)",
                    rusqlite::params![path, i, filler],
                )
                .unwrap();
            }
            tx.execute(
                "INSERT INTO file_manifest(path, mtime) VALUES ('src/f0.rs', 123)",
                [],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let pages_before: i64 = Connection::open(&db)
            .unwrap()
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .unwrap();

        // First open by the Tantivy build runs the migration.
        let idx = Index::open(dir.path()).unwrap();

        let conn = Connection::open(&db).unwrap();
        assert!(
            conn.prepare("SELECT 1 FROM chunks LIMIT 0").is_err(),
            "old FTS5 `chunks` table must be dropped"
        );
        assert!(conn.prepare("SELECT 1 FROM chunks_tri LIMIT 0").is_err());
        let manifest_rows: i64 = conn
            .query_row("SELECT count(*) FROM file_manifest", [], |r| r.get(0))
            .unwrap();
        assert_eq!(manifest_rows, 0, "manifest must be cleared on the version bump");
        let ver: String = conn
            .query_row("SELECT value FROM meta WHERE key='fts_backend'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(ver, FTS_BACKEND_VERSION);
        let freelist: i64 = conn
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))
            .unwrap();
        assert_eq!(freelist, 0, "VACUUM must reclaim the pages freed by the DROP");
        let pages_after: i64 = conn
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .unwrap();
        assert!(
            pages_after < pages_before,
            "index.db must shrink after migration (was {pages_before}, now {pages_after})"
        );

        // Fresh Tantivy index is empty (so ensure_index rebuilds), and a rebuild over a
        // real corpus is searchable end to end.
        assert_eq!(idx.chunk_count().unwrap(), 0);
        let corpus = tempdir().unwrap();
        std::fs::write(
            corpus.path().join("server.rs"),
            "fn handle_request() { dispatch(); }\n",
        )
        .unwrap();
        idx.index_path(corpus.path(), true).unwrap();
        let out = idx.search(&["handle_request".into()], 5).unwrap();
        assert!(
            !out.results[0].hits.is_empty(),
            "search must work after the migration rebuild"
        );
    }
}
