//! `ctx_index` / `ctx_search`: build and query an FTS5 content index.

pub mod schema;

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use ignore::WalkBuilder;

pub use schema::Index;

use crate::tools::{IndexResponse, QueryResult, SearchHit, SearchResponse};

/// Lines per chunk for non-markdown files.
const CODE_WINDOW: usize = 100;

impl Index {
    /// Index a file or directory, respecting `.gitignore`. Re-indexing a path
    /// replaces its existing chunks (idempotent).
    pub fn index_path(&self, root: &Path, recursive: bool) -> Result<IndexResponse> {
        // A non-existent root (commonly a shell-escaped path that survived as a
        // literal, e.g. `AI\ Stuff`) makes the walk silently yield zero files. Fail
        // loudly instead of reporting a successful index of nothing (mirrors
        // discovery::discover).
        if !root.exists() {
            anyhow::bail!("index root does not exist: {}", root.display());
        }

        let mut files_indexed = 0usize;
        let mut chunks_added = 0usize;

        let mut files: Vec<std::path::PathBuf> = Vec::new();
        if root.is_file() {
            files.push(root.to_path_buf());
        } else {
            let mut builder = WalkBuilder::new(root);
            builder.standard_filters(true); // respects .gitignore, hidden, etc.
            if !recursive {
                builder.max_depth(Some(1));
            }
            for entry in builder.build() {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    files.push(entry.into_path());
                }
            }
        }

        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        for file in files {
            let content = match std::fs::read(&file) {
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(s) => s,
                    Err(_) => continue, // skip binary/non-utf8
                },
                Err(_) => continue,
            };
            let path_str = file.to_string_lossy().to_string();
            tx.execute("DELETE FROM chunks WHERE path = ?1", [&path_str])?;

            let chunks = chunk_file(&file, &content);
            for (i, chunk) in chunks.iter().enumerate() {
                if chunk.trim().is_empty() {
                    continue;
                }
                let chunk_id = format!("{path_str}#{i}");
                tx.execute(
                    "INSERT INTO chunks (path, chunk_id, content) VALUES (?1, ?2, ?3)",
                    rusqlite::params![path_str, chunk_id, chunk],
                )?;
                chunks_added += 1;
            }
            files_indexed += 1;
        }
        tx.commit()?;

        Ok(IndexResponse {
            files_indexed,
            chunks: chunks_added,
        })
    }

    /// Remove indexed chunks for source files that no longer exist under `root`, so
    /// deleted files stop showing up in `ctx_search`. Only code-file chunks are
    /// touched — session-continuity records (`path` prefixed `session://`) are left
    /// intact. Also cleans up chunks left under a different path scheme (e.g. an old
    /// relative-root index) for files now indexed absolutely. Returns chunks removed.
    ///
    /// Call ONLY with the repo root: a subpath `root` would wrongly prune everything
    /// outside it.
    pub fn prune_missing(&self, root: &Path) -> Result<usize> {
        // Current file path strings, exactly as `index_path` would store them.
        let mut current: HashSet<String> = HashSet::new();
        if root.exists() {
            let mut builder = WalkBuilder::new(root);
            builder.standard_filters(true);
            for entry in builder.build().flatten() {
                if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    current.insert(entry.into_path().to_string_lossy().to_string());
                }
            }
        }
        let mut conn = self.conn()?;
        let existing: Vec<String> = {
            let mut stmt =
                conn.prepare("SELECT DISTINCT path FROM chunks WHERE path NOT LIKE 'session://%'")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.flatten().collect()
        };
        let stale: Vec<String> = existing
            .into_iter()
            .filter(|p| !current.contains(p))
            .collect();
        if stale.is_empty() {
            return Ok(0);
        }
        let tx = conn.transaction()?;
        let mut removed = 0usize;
        for path in &stale {
            removed += tx.execute("DELETE FROM chunks WHERE path = ?1", [path])?;
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Insert arbitrary `(path, chunk_id, content)` records into the index,
    /// replacing any existing rows with the same `chunk_id` first (idempotent).
    /// Used by session continuity to make detailed events `ctx_search`-able.
    pub fn index_records(&self, records: &[(String, String, String)]) -> Result<usize> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let mut added = 0usize;
        for (path, chunk_id, content) in records {
            if content.trim().is_empty() {
                continue;
            }
            tx.execute("DELETE FROM chunks WHERE chunk_id = ?1", [chunk_id])?;
            tx.execute(
                "INSERT INTO chunks (path, chunk_id, content) VALUES (?1, ?2, ?3)",
                rusqlite::params![path, chunk_id, content],
            )?;
            added += 1;
        }
        tx.commit()?;
        Ok(added)
    }

    /// Run BM25-ranked FTS5 search for each query.
    pub fn search(&self, queries: &[String], limit_per_query: usize) -> Result<SearchResponse> {
        let conn = self.conn()?;
        let mut results = Vec::new();
        for query in queries {
            let match_expr = sanitize_query(query);
            let mut hits = Vec::new();
            if !match_expr.is_empty() {
                // bm25() is more-negative-is-better; negate so higher = better.
                let mut stmt = conn.prepare(
                    "SELECT path, snippet(chunks, 2, '[', ']', ' … ', 24) AS snip, -bm25(chunks) AS score
                     FROM chunks
                     WHERE chunks MATCH ?1
                     ORDER BY bm25(chunks)
                     LIMIT ?2",
                )?;
                let rows = stmt.query_map(
                    rusqlite::params![match_expr, limit_per_query as i64],
                    |row| {
                        Ok(SearchHit {
                            path: row.get(0)?,
                            snippet: row.get(1)?,
                            score: row.get(2)?,
                        })
                    },
                );
                if let Ok(mapped) = rows {
                    for h in mapped.flatten() {
                        hits.push(h);
                    }
                }
            }
            results.push(QueryResult {
                query: query.clone(),
                hits,
            });
        }
        Ok(SearchResponse { results })
    }
}

/// Split a file into chunks: markdown by headings, everything else by line windows.
fn chunk_file(path: &Path, content: &str) -> Vec<String> {
    let is_md = matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("md") | Some("markdown")
    );
    if is_md {
        chunk_markdown(content)
    } else {
        chunk_by_lines(content, CODE_WINDOW)
    }
}

fn chunk_markdown(content: &str) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in content.lines() {
        if line.starts_with('#') && !current.trim().is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push(content.to_string());
    }
    chunks
}

fn chunk_by_lines(content: &str, window: usize) -> Vec<String> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return vec![];
    }
    lines.chunks(window).map(|w| w.join("\n")).collect()
}

/// Turn an arbitrary query into a safe FTS5 MATCH expression: each whitespace
/// token becomes a quoted term, which avoids syntax errors from punctuation
/// while keeping implicit-AND semantics.
fn sanitize_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(|tok| {
            let cleaned: String = tok
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            cleaned
        })
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Open an index at the given data dir.
pub fn open(data_dir: &Path) -> Result<Index> {
    Index::open(data_dir).context("opening index")
}

/// Cheap staleness signature for the FTS index: every file under `root` mapped to
/// its mtime (ms since epoch), walked the same gitignore-respecting way
/// [`Index::index_path`] walks. Comparing it to a saved copy tells us whether the
/// index is stale. Stat-only, so far cheaper than a reindex.
pub fn file_manifest(root: &Path) -> BTreeMap<String, u64> {
    let mut manifest = BTreeMap::new();
    if !root.exists() {
        return manifest;
    }
    let mut builder = WalkBuilder::new(root);
    builder.standard_filters(true);
    for entry in builder.build().flatten() {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let mtime = std::fs::metadata(path)
            .ok()
            .and_then(|md| md.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        manifest.insert(rel, mtime);
    }
    manifest
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn corpus() -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("auth.rs"),
            "fn authenticate(user: &str) {\n    // verify password hash\n    login(user);\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("math.rs"),
            "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("notes.md"),
            "# Intro\nsome text\n# Database\nconnection pooling details\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn index_and_search_finds_right_file() {
        let data = tempdir().unwrap();
        let src = corpus();
        let idx = Index::open(data.path()).unwrap();
        let res = idx.index_path(src.path(), true).unwrap();
        assert!(res.files_indexed >= 3);
        assert!(res.chunks >= 3);

        let out = idx.search(&["authenticate".into()], 5).unwrap();
        assert_eq!(out.results.len(), 1);
        let hits = &out.results[0].hits;
        assert!(!hits.is_empty());
        assert!(hits[0].path.ends_with("auth.rs"));
    }

    #[test]
    fn multiple_queries_in_one_call() {
        let data = tempdir().unwrap();
        let src = corpus();
        let idx = Index::open(data.path()).unwrap();
        idx.index_path(src.path(), true).unwrap();
        let out = idx
            .search(&["authenticate".into(), "pooling".into()], 5)
            .unwrap();
        assert_eq!(out.results.len(), 2);
        assert!(out.results[0].hits[0].path.ends_with("auth.rs"));
        assert!(out.results[1].hits[0].path.ends_with("notes.md"));
    }

    #[test]
    fn bm25_ordering_is_sane() {
        let data = tempdir().unwrap();
        let dir = tempdir().unwrap();
        // strong: term appears many times; weak: once.
        fs::write(
            dir.path().join("strong.txt"),
            "widget widget widget widget widget",
        )
        .unwrap();
        fs::write(
            dir.path().join("weak.txt"),
            "this file mentions widget once among many other unrelated words here",
        )
        .unwrap();
        let idx = Index::open(data.path()).unwrap();
        idx.index_path(dir.path(), true).unwrap();
        let out = idx.search(&["widget".into()], 5).unwrap();
        let hits = &out.results[0].hits;
        assert!(hits.len() >= 2);
        assert!(hits[0].path.ends_with("strong.txt"));
        assert!(hits[0].score >= hits[1].score);
    }

    #[test]
    fn reindex_is_idempotent() {
        let data = tempdir().unwrap();
        let src = corpus();
        let idx = Index::open(data.path()).unwrap();
        idx.index_path(src.path(), true).unwrap();
        let first = idx.chunk_count().unwrap();
        idx.index_path(src.path(), true).unwrap();
        let second = idx.chunk_count().unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn index_nonexistent_root_errors() {
        // A path that doesn't exist (e.g. a shell-escaped `AI\ Stuff` that survived
        // as a literal) must error, not silently index zero files.
        let data = tempdir().unwrap();
        let idx = Index::open(data.path()).unwrap();
        let missing = data.path().join("AItestslash\\ Stuff/src");
        let res = idx.index_path(&missing, true);
        assert!(res.is_err(), "nonexistent root must error");
        let err = res.err().unwrap();
        assert!(err.to_string().contains("does not exist"), "got: {err}");
    }
}
