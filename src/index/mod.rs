//! `ctx_index` / `ctx_search`: build and query an FTS5 content index.

pub mod schema;

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
    lines
        .chunks(window)
        .map(|w| w.join("\n"))
        .collect()
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
}
