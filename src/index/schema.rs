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
        // `path` and `chunk_id` are stored but not tokenized; `content` is the
        // searchable column. Porter stemming improves recall on code/prose.
        conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS chunks USING fts5(
                path UNINDEXED,
                chunk_id UNINDEXED,
                content,
                tokenize = 'porter'
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
