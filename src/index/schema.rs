//! FTS5 schema for the content index.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};

/// Handle to the FTS5 index DB. A connection is opened per operation so the
/// index is safe to use from multiple async tasks.
#[derive(Clone)]
pub struct Index {
    db_path: PathBuf,
    /// Base that stored chunk/manifest keys are relativized against — the repo
    /// root. Files under it are stored as repo-relative paths (portable across
    /// checkouts, no machine layout leaked into search output); files outside it
    /// fall back to absolute. Canonicalized so it matches the canonicalized walk
    /// paths in [`Index::index_path`].
    repo_root: PathBuf,
}

impl Index {
    /// Open (creating if needed) the index under `dir`. Stored paths default to
    /// being relativized against `dir`; the server overrides this with the repo
    /// root via [`Index::with_repo_root`].
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating data dir {}", dir.display()))?;
        let idx = Index {
            db_path: dir.join("index.db"),
            repo_root: dir.to_path_buf(),
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

    pub(crate) fn conn(&self) -> Result<Connection> {
        let conn = Connection::open(&self.db_path)
            .with_context(|| format!("opening index db {}", self.db_path.display()))?;
        crate::obs::configure_conn(&conn)?;
        Ok(conn)
    }

    fn init(&self) -> Result<()> {
        let conn = self.conn()?;
        // BM25F migration: an index built before the `symbols` column has the wrong
        // shape, so drop it (and its manifest) to recreate fresh. The next
        // index_path re-reads every file and repopulates both.
        let table_exists = conn.prepare("SELECT 1 FROM chunks LIMIT 0").is_ok();
        let stale_schema =
            table_exists && conn.prepare("SELECT symbols FROM chunks LIMIT 0").is_err();
        // Path migration: indexes built before paths were relativized stored absolute
        // keys (leaking machine layout and breaking portability across checkouts).
        // Wipe so the next index_path repopulates with repo-root-relative keys; the
        // manifest goes too, forcing a full re-read.
        let has_abs_paths = table_exists
            && conn
                .query_row(
                    "SELECT 1 FROM chunks WHERE path GLOB '/*' OR path GLOB '?:\\*' LIMIT 1",
                    [],
                    |_| Ok(()),
                )
                .optional()
                .unwrap_or(None)
                .is_some();
        if stale_schema || has_abs_paths {
            conn.execute_batch(
                "DROP TABLE IF EXISTS chunks; DROP TABLE IF EXISTS chunks_tri; DROP TABLE IF EXISTS file_manifest;",
            )?;
        }
        // `path` and `chunk_id` are stored but not tokenized; `symbols` (the symbol
        // names defined in the chunk) and `content` are searchable, with `symbols`
        // field-weighted higher in `bm25()` so a query that names a symbol ranks the
        // file that defines it above files that merely mention it. Porter stemming
        // improves recall on code/prose.
        //
        // `chunks_tri` mirrors the same content under a trigram tokenizer so
        // structural/operator queries (`std::fs`, `->`) the porter path strips can be
        // matched as literal substrings. Kept in sync with `chunks` on every write.
        conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS chunks USING fts5(
                path UNINDEXED,
                chunk_id UNINDEXED,
                symbols,
                content,
                tokenize = 'porter'
             );
             CREATE VIRTUAL TABLE IF NOT EXISTS chunks_tri USING fts5(
                path UNINDEXED,
                chunk_id UNINDEXED,
                content,
                tokenize = 'trigram'
             );
             CREATE TABLE IF NOT EXISTS file_manifest (
                 path TEXT PRIMARY KEY,
                 mtime INTEGER NOT NULL
             );",
        )?;
        Ok(())
    }

    /// Total chunks currently indexed.
    pub fn chunk_count(&self) -> Result<i64> {
        let conn = self.conn()?;
        let n: i64 = conn.query_row("SELECT count(*) FROM chunks", [], |r| r.get(0))?;
        Ok(n)
    }
}
