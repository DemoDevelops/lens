//! FTS5 schema for the content index.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Handle to the FTS5 index DB. A connection is opened per operation so the
/// index is safe to use from multiple async tasks.
#[derive(Clone)]
pub struct Index {
    db_path: PathBuf,
}

impl Index {
    /// Open (creating if needed) the index under `dir`.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating data dir {}", dir.display()))?;
        let idx = Index {
            db_path: dir.join("index.db"),
        };
        idx.init()?;
        Ok(idx)
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
        let stale_schema = conn.prepare("SELECT 1 FROM chunks LIMIT 0").is_ok()
            && conn.prepare("SELECT symbols FROM chunks LIMIT 0").is_err();
        if stale_schema {
            conn.execute_batch(
                "DROP TABLE chunks; DROP TABLE IF EXISTS chunks_tri; DROP TABLE IF EXISTS file_manifest;",
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
