//! `lens_index` / `lens_search`: build and query an FTS5 content index.

pub mod schema;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use regex::Regex;
use rusqlite::Connection;

pub use schema::Index;

use crate::tools::{IndexResponse, QueryResult, SearchHit, SearchResponse};

/// Lines per chunk for non-markdown files.
const CODE_WINDOW: usize = 100;

impl Index {
    /// Index a file or directory, respecting `.gitignore`. Re-indexing a path
    /// replaces its existing chunks (idempotent). Incremental: only files whose
    /// mtime changed (or are new) are read and re-inserted; deleted files have
    /// their chunks pruned; unchanged files are skipped entirely.
    ///
    /// Returns the number of files actually read this call in `files_read`.
    pub fn index_path(&self, root: &Path, recursive: bool) -> Result<IndexResponse> {
        // A non-existent root (commonly a shell-escaped path that survived as a
        // literal, e.g. `AI\ Stuff`) makes the walk silently yield zero files. Fail
        // loudly instead of reporting a successful index of nothing (mirrors
        // discovery::discover).
        if !root.exists() {
            anyhow::bail!("index root does not exist: {}", root.display());
        }

        // Single walk: collect current files with their mtimes.
        let mut current: HashMap<String, u64> = HashMap::new();
        if root.is_file() {
            let path_str = root.to_string_lossy().to_string();
            let mtime = mtime_ms(root);
            current.insert(path_str, mtime);
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
                if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    continue;
                }
                let path = entry.into_path();
                let mtime = mtime_ms(&path);
                current.insert(path.to_string_lossy().into_owned(), mtime);
            }
        }

        let mut conn = self.conn()?;

        // Load the stored mtime manifest for this root from the DB.
        let stored: HashMap<String, u64> = {
            let prefix = root.to_string_lossy().into_owned();
            let mut stmt = conn.prepare_cached(
                "SELECT path, mtime FROM file_manifest WHERE path = ?1 OR path LIKE ?1 || '/%'",
            )?;
            let rows = stmt.query_map([&prefix], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            rows.flatten()
                .map(|(p, m)| (p, m as u64))
                .collect()
        };

        // Classify: changed_or_new (mtime differs or absent), deleted (in stored but not current).
        let changed: Vec<&String> = current
            .keys()
            .filter(|p| stored.get(*p).copied() != Some(*current.get(*p).unwrap()))
            .collect();
        let deleted: Vec<&String> = stored
            .keys()
            .filter(|p| !current.contains_key(*p))
            .collect();

        let files_indexed = current.len();
        let mut chunks_added = 0usize;
        let mut files_read = 0usize;

        let tx = conn.transaction()?;

        // Delete chunks for removed files.
        for path in &deleted {
            tx.execute("DELETE FROM chunks WHERE path = ?1", [path])?;
            tx.execute("DELETE FROM chunks_tri WHERE path = ?1", [path])?;
            tx.execute("DELETE FROM file_manifest WHERE path = ?1", [path])?;
        }

        // Re-index changed or new files.
        for path_str in &changed {
            let file = std::path::Path::new(path_str.as_str());
            let content = match std::fs::read(file) {
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(s) => s,
                    Err(_) => continue, // skip binary/non-utf8
                },
                Err(_) => continue,
            };

            tx.execute("DELETE FROM chunks WHERE path = ?1", [path_str])?;
            tx.execute("DELETE FROM chunks_tri WHERE path = ?1", [path_str])?;

            let chunks = chunk_file(file, &content);
            for (i, chunk) in chunks.iter().enumerate() {
                if chunk.trim().is_empty() {
                    continue;
                }
                let chunk_id = format!("{path_str}#{i}");
                let symbols = chunk_symbols(chunk);
                tx.prepare_cached(
                    "INSERT INTO chunks (path, chunk_id, symbols, content) VALUES (?1, ?2, ?3, ?4)",
                )?.execute(rusqlite::params![path_str, chunk_id, symbols, chunk])?;
                tx.prepare_cached(
                    "INSERT INTO chunks_tri (path, chunk_id, content) VALUES (?1, ?2, ?3)",
                )?.execute(rusqlite::params![path_str, chunk_id, chunk])?;
                chunks_added += 1;
            }

            let mtime = current[*path_str] as i64;
            tx.prepare_cached(
                "INSERT OR REPLACE INTO file_manifest (path, mtime) VALUES (?1, ?2)",
            )?.execute(rusqlite::params![path_str, mtime])?;

            files_read += 1;
        }

        tx.commit()?;

        Ok(IndexResponse {
            files_indexed,
            chunks: chunks_added,
            files_read,
        })
    }

    /// Remove indexed chunks for source files that no longer exist under `root`, so
    /// deleted files stop showing up in `lens_search`. Only code-file chunks are
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
            tx.execute("DELETE FROM chunks_tri WHERE path = ?1", [path])?;
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Insert arbitrary `(path, chunk_id, content)` records into the index,
    /// replacing any existing rows with the same `chunk_id` first (idempotent).
    /// Used by session continuity to make detailed events `lens_search`-able.
    pub fn index_records(&self, records: &[(String, String, String)]) -> Result<usize> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let mut added = 0usize;
        for (path, chunk_id, content) in records {
            if content.trim().is_empty() {
                continue;
            }
            tx.execute("DELETE FROM chunks WHERE chunk_id = ?1", [chunk_id])?;
            tx.execute("DELETE FROM chunks_tri WHERE chunk_id = ?1", [chunk_id])?;
            // Session-continuity records carry no code symbols, so the symbols
            // column is empty (they rank on content only).
            tx.execute(
                "INSERT INTO chunks (path, chunk_id, symbols, content) VALUES (?1, ?2, '', ?3)",
                rusqlite::params![path, chunk_id, content],
            )?;
            tx.execute(
                "INSERT INTO chunks_tri (path, chunk_id, content) VALUES (?1, ?2, ?3)",
                rusqlite::params![path, chunk_id, content],
            )?;
            added += 1;
        }
        tx.commit()?;
        Ok(added)
    }

    /// Run FTS5 search for each query. Alphanumeric queries take the BM25F-ranked
    /// porter path; queries carrying structural punctuation (`std::fs`, `->`) route
    /// to the trigram path for literal-substring matching.
    pub fn search(&self, queries: &[String], limit_per_query: usize) -> Result<SearchResponse> {
        let conn = self.conn()?;
        let mut results = Vec::new();
        for query in queries {
            let hits = if is_structural(query) {
                structural_search(&conn, query, limit_per_query)?
            } else {
                ranked_search(&conn, query, limit_per_query)?
            };
            results.push(QueryResult {
                query: query.clone(),
                hits,
            });
        }
        Ok(SearchResponse { results })
    }
}

/// BM25F-ranked search over the porter `chunks` table (the default path).
fn ranked_search(conn: &Connection, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
    let match_expr = sanitize_query(query);
    if match_expr.is_empty() {
        return Ok(Vec::new());
    }
    // bm25() is more-negative-is-better; negate so higher = better. Column weights
    // are (path, chunk_id, symbols, content): symbols is weighted 5x so a query
    // naming a symbol ranks its defining file above files that only mention the
    // term. snippet targets content (column 3).
    let mut stmt = conn.prepare(
        "SELECT path, snippet(chunks, 3, '[', ']', ' … ', 24) AS snip,
                -bm25(chunks, 0.0, 0.0, 5.0, 1.0) AS score
         FROM chunks
         WHERE chunks MATCH ?1
         ORDER BY bm25(chunks, 0.0, 0.0, 5.0, 1.0), path, chunk_id
         LIMIT ?2",
    )?;
    let mut hits = Vec::new();
    let rows = stmt.query_map(rusqlite::params![match_expr, limit as i64], |row| {
        Ok(SearchHit {
            path: row.get(0)?,
            snippet: row.get(1)?,
            score: row.get(2)?,
        })
    });
    if let Ok(mapped) = rows {
        for h in mapped.flatten() {
            hits.push(h);
        }
    }
    Ok(hits)
}

/// Literal-substring search over the trigram `chunks_tri` table for structural /
/// operator queries. Queries of at least 3 chars use the trigram index; shorter
/// operators (`->`) fall back to a `LIKE` scan, which a trigram index can't serve.
fn structural_search(conn: &Connection, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
    let q = query.trim();
    if q.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<(String, String)> = if q.chars().count() >= 3 {
        let phrase = format!("\"{}\"", q.replace('"', "\"\""));
        let mut stmt =
            conn.prepare("SELECT path, content FROM chunks_tri WHERE chunks_tri MATCH ?1 ORDER BY path, chunk_id LIMIT ?2")?;
        let mapped = stmt.query_map(rusqlite::params![phrase, limit as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        mapped.flatten().collect()
    } else {
        let like = format!("%{q}%");
        let mut stmt =
            conn.prepare("SELECT path, content FROM chunks_tri WHERE content LIKE ?1 ORDER BY path, chunk_id LIMIT ?2")?;
        let mapped = stmt.query_map(rusqlite::params![like, limit as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        mapped.flatten().collect()
    };
    Ok(rows
        .into_iter()
        .map(|(path, content)| SearchHit {
            path,
            snippet: structural_snippet(&content, q),
            score: 0.0,
        })
        .collect())
}

/// The first line of `content` containing `needle`, trimmed and capped — the
/// snippet for a structural hit (the match is a substring, not an FTS token).
fn structural_snippet(content: &str, needle: &str) -> String {
    content
        .lines()
        .find(|l| l.contains(needle))
        .unwrap_or("")
        .trim()
        .chars()
        .take(120)
        .collect()
}

/// True when a query carries structural punctuation the porter tokenizer would
/// strip (`:`, `.`, `>`, `-`, ...), so it should route to the trigram path.
fn is_structural(query: &str) -> bool {
    query
        .chars()
        .any(|c| !c.is_alphanumeric() && !c.is_whitespace() && c != '_')
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

/// File mtime in milliseconds since the Unix epoch; 0 on any error.
fn mtime_ms(path: &Path) -> u64 {
    std::fs::metadata(path)
        .ok()
        .and_then(|md| md.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Symbol names defined in a code chunk: the identifier following a definition
/// keyword (`fn`, `struct`, `def`, `class`, ...). Joined into the FTS `symbols`
/// column so a query that names a symbol is field-weighted toward the file that
/// defines it. Regex-based and language-agnostic; markdown/prose chunks yield an
/// empty string and rank on content alone.
fn chunk_symbols(content: &str) -> String {
    static SYMBOL_RE: OnceLock<Regex> = OnceLock::new();
    let re = SYMBOL_RE.get_or_init(|| {
        Regex::new(
            r"\b(?:fn|func|function|def|struct|enum|trait|interface|class|type|const|let|var|impl|mod)\s+([A-Za-z_][A-Za-z0-9_]*)",
        )
        .expect("symbol regex")
    });
    let mut names: Vec<&str> = re
        .captures_iter(content)
        .filter_map(|c| c.get(1).map(|m| m.as_str()))
        .collect();
    names.sort_unstable();
    names.dedup();

    // Append camelCase/PascalCase subwords of each captured identifier so a query
    // that is a subword of a compound identifier (`Subscription` inside
    // `ConfirmSubscriptionScreen`) matches. `names` is the deduped capture set;
    // `out` preserves capture order with subwords following, then a final dedup.
    let mut out: Vec<String> = names.iter().map(|n| n.to_string()).collect();
    for name in &names {
        out.extend(split_subwords(name));
    }
    let mut seen = HashSet::new();
    out.retain(|s| seen.insert(s.clone()));
    out.join(" ")
}

/// Split a camelCase/PascalCase identifier into its subwords for FTS expansion.
///
/// Returns an empty vec for identifiers with no camel/acronym signal (pure
/// snake_case or single-case, which the porter tokenizer already splits on
/// underscores). Boundaries: lower/digit→Upper, an acronym run ending where the
/// last uppercase begins a lowercase word (`HTTPServer` → `HTTP`, `Server`), and
/// letter↔digit. Fragments of length <= 1 are dropped. Lookaround-free (a
/// forward `next` char is inspected inline), allocation-light, no new dependency.
fn split_subwords(ident: &str) -> Vec<String> {
    let chars: Vec<char> = ident.chars().collect();

    // Gate: only expand on a camel/acronym signal — a lower→Upper transition or an
    // Upper-Upper-lower run. Pure snake_case / single-case yields nothing.
    let has_signal = chars.windows(2).any(|w| w[0].is_lowercase() && w[1].is_uppercase())
        || chars.windows(3).any(|w| {
            w[0].is_uppercase() && w[1].is_uppercase() && w[2].is_lowercase()
        });
    if !has_signal {
        return Vec::new();
    }

    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if i > 0 {
            let prev = chars[i - 1];
            let next = chars.get(i + 1).copied();
            let lower_or_digit_to_upper =
                (prev.is_lowercase() || prev.is_ascii_digit()) && c.is_uppercase();
            let acronym_end = prev.is_uppercase()
                && c.is_uppercase()
                && next.map(|n| n.is_lowercase()).unwrap_or(false);
            let alpha_digit_boundary = prev.is_alphabetic() && c.is_ascii_digit()
                || prev.is_ascii_digit() && c.is_alphabetic();
            if (lower_or_digit_to_upper || acronym_end || alpha_digit_boundary)
                && !cur.is_empty()
            {
                parts.push(std::mem::take(&mut cur));
            }
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts.retain(|p| p.chars().count() > 1);
    parts
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
    fn split_subwords_locked_contract() {
        assert_eq!(
            split_subwords("ConfirmSubscriptionScreen"),
            ["Confirm", "Subscription", "Screen"]
        );
        assert_eq!(split_subwords("HTTPServer"), ["HTTP", "Server"]);
        assert_eq!(split_subwords("HTMLParser"), ["HTML", "Parser"]);
        assert_eq!(split_subwords("IOError"), ["IO", "Error"]);
        assert_eq!(split_subwords("getUserID"), ["get", "User", "ID"]);
        assert_eq!(split_subwords("OAuth2Token"), ["Auth", "Token"]);
        assert!(split_subwords("parse_json_value").is_empty());
        assert!(split_subwords("MAX_SIZE").is_empty());
        // Hard rule: never fabricate a token (no "HTTPS" out of "HTTPServer").
        assert!(!split_subwords("HTTPServer").iter().any(|s| s == "HTTPS"));
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

    #[test]
    fn incremental_reindex_reads_only_changed() {
        let data = tempdir().unwrap();
        let src = corpus();
        let idx = Index::open(data.path()).unwrap();

        // First full index.
        idx.index_path(src.path(), true).unwrap();

        // Confirm auth.rs content is findable.
        let before = idx.search(&["authenticate".into()], 5).unwrap();
        assert!(!before.results[0].hits.is_empty(), "authenticate must be found before edit");

        // Modify auth.rs and advance its mtime to a strictly newer value so the
        // test is not flaky on filesystems with coarse mtime resolution.
        let auth_path = src.path().join("auth.rs");
        fs::write(&auth_path, "fn login_replaced(user: &str) { /* new content */ }\n").unwrap();
        let new_mtime = std::time::SystemTime::now()
            + std::time::Duration::from_secs(2);
        std::fs::File::options()
            .write(true)
            .open(&auth_path)
            .unwrap()
            .set_modified(new_mtime)
            .unwrap();

        // Incremental reindex: only auth.rs should be read.
        let res = idx.index_path(src.path(), true).unwrap();
        assert_eq!(res.files_read, 1, "only the changed file must be re-read");

        // Old content gone, new content present.
        let after_old = idx.search(&["authenticate".into()], 5).unwrap();
        assert!(
            after_old.results[0].hits.is_empty(),
            "old content must not be found after reindex"
        );
        let after_new = idx.search(&["login_replaced".into()], 5).unwrap();
        assert!(
            !after_new.results[0].hits.is_empty(),
            "new content must be searchable after reindex"
        );

        // Unchanged files still searchable.
        let math = idx.search(&["fn add".into()], 5).unwrap();
        assert!(!math.results[0].hits.is_empty(), "unchanged math.rs must still be searchable");
    }
}
