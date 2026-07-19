//! `lens_index` / `lens_search`: build and query a Tantivy content index.

pub mod schema;
mod tantivy_index;

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use regex::Regex;
use tree_sitter::{Node as TsNode, Parser};

pub use schema::Index;

use self::tantivy_index::TantivyStore;
use crate::discovery;
use crate::discovery::tags_adapter::{any_spec_for_extension, AnySpec};
use crate::tools::{IndexResponse, QueryResult, SearchHit, SearchResponse};

/// Lines per chunk for non-markdown files.
const CODE_WINDOW: usize = 100;

/// Target byte span for AST-boundary chunks (see [`chunk_by_ast`]): a code file is
/// split at tree-sitter node boundaries into chunks of roughly this size, so a chunk
/// holds a whole function/impl rather than a fixed line cut. Byte-based, distinct from
/// `CODE_WINDOW`'s lines and `SNIPPET_TOKENS`' whitespace tokens. ~4 KB is on the order
/// of 130 lines of code, keeping granularity comparable to the line-window fallback
/// while leaving any file this size or smaller as a single chunk.
const AST_CHUNK_BYTES: usize = 4096;

/// A re-index touching at least this many changed files is a bulk build, so the
/// Tantivy writer fans out across all cores; smaller edits use a single writer thread
/// (spinning up N segment threads for one file is pure overhead).
const BULK_FILE_THRESHOLD: usize = 32;

/// Denylist of binary/media file extensions (lowercase, no leading dot) skipped
/// before `fs::read`: video/audio/image/archive/font/compiled-binary formats that
/// are never useful FTS content and can be large enough to make a naive read
/// expensive. Matched case-insensitively against the file's extension.
const BINARY_EXT_DENYLIST: &[&str] = &[
    "mp4", "mov", "mkv", "avi", "mp3", "wav", "flac", "png", "jpg", "jpeg", "gif", "webp", "pdf",
    "zip", "tar", "gz", "7z", "dmg", "exe", "bin", "o", "so", "dylib", "wasm", "ttf", "otf",
    "woff", "woff2",
];

/// A file larger than this is skipped before `fs::read` rather than fully loaded
/// into memory just to be checked for UTF-8 validity. Start at 2 MB.
const MAX_INDEXABLE_FILE_BYTES: u64 = 2 * 1024 * 1024;

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
        // Canonicalize so `.`/`..` components and symlinks collapse to one spelling.
        // Without this, `lens_index(path=".")` stores a second `/./`-keyed copy of
        // every chunk and search returns each file twice (mirrors warmup's canonicalize).
        let root = &std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());

        // Single walk: collect current files with their mtimes, keyed by repo-relative
        // storage key (see `rel_key`) so a file gets the same key whether the whole
        // repo or a subpath was indexed.
        let mut current: HashMap<String, u64> = HashMap::new();
        if root.is_file() {
            current.insert(self.rel_key(root), mtime_ms(root));
        } else {
            let mut builder = WalkBuilder::new(root);
            builder.standard_filters(true); // respects .gitignore, hidden, etc.
            if !recursive {
                builder.max_depth(Some(1));
            }
            // A nested git repo (its own `.git`) is a boundary this walk must not
            // cross: its content is indexed separately into its own `.lens/fts`,
            // not folded into this index (mirrors discovery::discover's pruning).
            let boundary_root = root.to_path_buf();
            builder.filter_entry(move |entry| {
                !(entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                    && discovery::is_repo_boundary(entry.path(), &boundary_root))
            });
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
                current.insert(self.rel_key(&path), mtime);
            }
        }

        let mut conn = self.conn()?;

        // Load the stored mtime manifest for this root from the DB. An empty prefix
        // means `root` is the repo root itself, so load the whole manifest; otherwise
        // load only keys at or under the relative prefix.
        let stored: HashMap<String, u64> = {
            let prefix = self.rel_key(root);
            if prefix.is_empty() {
                let mut stmt = conn.prepare_cached("SELECT path, mtime FROM file_manifest")?;
                let rows =
                    stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
                rows.flatten().map(|(p, m)| (p, m as u64)).collect()
            } else {
                let mut stmt = conn.prepare_cached(
                    "SELECT path, mtime FROM file_manifest WHERE path = ?1 OR path LIKE ?1 || '/%'",
                )?;
                let rows = stmt
                    .query_map([&prefix], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
                rows.flatten().map(|(p, m)| (p, m as u64)).collect()
            }
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

        // Nothing to do: every file unchanged and none deleted. Skip the writer
        // entirely (acquiring it takes the exclusive index lock).
        if changed.is_empty() && deleted.is_empty() {
            return Ok(IndexResponse {
                files_indexed,
                chunks: 0,
                files_read: 0,
            });
        }

        // One writer for the whole batch. Tantivy builds independent segments across
        // its worker threads and merges on commit, so a bulk build uses every core;
        // there is no single-writer lock to batch around (unlike the old SQLite path).
        let store = self.store();
        let threads = if changed.len() >= BULK_FILE_THRESHOLD {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        } else {
            1
        };
        let mut writer = store.writer(threads)?;

        // Delete-by-`path` term drops every existing chunk of a removed file.
        for &path in &deleted {
            store.delete_path(&writer, path);
        }

        // Re-index changed or new files: delete the old chunks, add the new ones.
        // Manifest is updated only for files actually read, so an unreadable file is
        // retried next run (matches the pre-Tantivy behavior).
        let mut read_keys: Vec<&String> = Vec::new();
        for &path_str in &changed {
            let file = self.abs_path(path_str);
            // Cheap pre-read guard: a denylisted binary/media extension, or a file
            // too large to be worth loading whole, is skipped before ever touching
            // its bytes (a parent folder can contain multi-GB video/audio/PDF
            // files). Neither path is written into `read_keys`/the manifest below,
            // so it stays retryable next run, same as the UTF-8/read-error arms.
            if is_binary_ext(&file) {
                continue;
            }
            if std::fs::metadata(&file)
                .map(|m| m.len())
                .unwrap_or(0)
                > MAX_INDEXABLE_FILE_BYTES
            {
                continue;
            }
            let content = match std::fs::read(&file) {
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(s) => s,
                    Err(_) => continue, // skip binary/non-utf8
                },
                Err(_) => continue,
            };
            store.delete_path(&writer, path_str);
            for (i, chunk) in chunk_file(&file, &content).iter().enumerate() {
                if chunk.trim().is_empty() {
                    continue;
                }
                store.add_chunk(
                    &writer,
                    path_str,
                    &format!("{path_str}#{i}"),
                    &chunk_symbols(chunk),
                    chunk,
                )?;
                chunks_added += 1;
            }
            files_read += 1;
            read_keys.push(path_str);
        }

        // Commit makes the segments durable and searchable; reload the reader so the
        // next search sees them.
        writer.commit().context("committing tantivy index")?;
        drop(writer);
        store.reload()?;

        // Mirror the change into the SQLite mtime manifest (the incremental key).
        let tx = conn.transaction()?;
        for &path in &deleted {
            tx.execute("DELETE FROM file_manifest WHERE path = ?1", [path])?;
        }
        for &path_str in &read_keys {
            tx.execute(
                "INSERT OR REPLACE INTO file_manifest (path, mtime) VALUES (?1, ?2)",
                rusqlite::params![path_str, current[path_str] as i64],
            )?;
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
    /// intact. Returns the number of files pruned.
    ///
    /// Call ONLY with the repo root: a subpath `root` would wrongly prune everything
    /// outside it.
    pub fn prune_missing(&self, root: &Path) -> Result<usize> {
        // Current file storage keys, exactly as `index_path` would store them.
        let mut current: HashSet<String> = HashSet::new();
        if root.exists() {
            let mut builder = WalkBuilder::new(root);
            builder.standard_filters(true);
            for entry in builder.build().flatten() {
                if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    current.insert(self.rel_key(&entry.into_path()));
                }
            }
        }
        let store = self.store();
        let stale: Vec<String> = store
            .distinct_paths()?
            .into_iter()
            .filter(|p| !p.starts_with("session://") && !current.contains(p))
            .collect();
        if stale.is_empty() {
            return Ok(0);
        }
        let mut writer = store.writer(1)?;
        for path in &stale {
            store.delete_path(&writer, path);
        }
        writer.commit().context("committing tantivy prune")?;
        drop(writer);
        store.reload()?;
        Ok(stale.len())
    }

    /// Insert arbitrary `(path, chunk_id, content)` records into the index,
    /// replacing any existing rows with the same `chunk_id` first (idempotent).
    /// Used by session continuity to make detailed events `lens_search`-able.
    pub fn index_records(&self, records: &[(String, String, String)]) -> Result<usize> {
        let store = self.store();
        let mut writer = store.writer(1)?;
        let mut added = 0usize;
        for (path, chunk_id, content) in records {
            if content.trim().is_empty() {
                continue;
            }
            store.delete_chunk_id(&writer, chunk_id);
            // Session-continuity records carry no code symbols, so the symbols field
            // is empty (they rank on content only).
            store.add_chunk(&writer, path, chunk_id, "", content)?;
            added += 1;
        }
        if added == 0 {
            return Ok(0);
        }
        writer.commit().context("committing tantivy records")?;
        drop(writer);
        store.reload()?;
        Ok(added)
    }

    /// Run FTS search for each query. Alphanumeric queries take the BM25-ranked
    /// stemmed path; queries carrying structural punctuation (`std::fs`, `->`) route
    /// to the trigram path for literal-substring matching.
    ///
    /// Delegates to [`search_fused`](Index::search_fused) with an empty file-rank
    /// map, so every existing caller sees exactly today's output.
    pub fn search(&self, queries: &[String], limit_per_query: usize) -> Result<SearchResponse> {
        self.search_fused(queries, limit_per_query, &HashMap::new())
    }

    /// Like [`search`](Index::search), but fuses the text ranking with a per-file
    /// graph-importance rank via reciprocal-rank fusion (RRF; see [`ranked_search`]).
    /// `file_ranks` maps a stored file path (`rel_key` form) to its graph rank
    /// (0 = most central). An empty map — or `LENS_RRF=0` — makes the output
    /// byte-identical to [`search`](Index::search). Only the BM25/prose path fuses;
    /// structural/trigram queries are unaffected.
    pub fn search_fused(
        &self,
        queries: &[String],
        limit_per_query: usize,
        file_ranks: &HashMap<String, usize>,
    ) -> Result<SearchResponse> {
        let store = self.store();
        let mut results = Vec::new();
        for query in queries {
            let hits = if is_structural(query) {
                structural_search(store, query, limit_per_query)?
            } else {
                ranked_search(store, query, limit_per_query, file_ranks)?
            };
            results.push(QueryResult {
                query: query.clone(),
                hits,
            });
        }
        Ok(SearchResponse { results })
    }
}

/// Proximity boost weight: added to the BM25 score of a multi-term hit, scaled by
/// `1 / span` where `span` is the tightest token-position window covering every query
/// term in the chunk. Adjacent terms (span 1) get the full weight; terms scattered
/// far apart decay toward zero. Sized so an adjacent-terms chunk overtakes a
/// higher-TF but scattered chunk without disturbing the single-term / unrelated-query
/// orderings the BM25 gates depend on.
const PROX_WEIGHT: f64 = 4.0;

/// Over-fetch factor and cap for the proximity re-rank. The BM25 candidate pool is
/// fetched `requested_limit * OVERFETCH_K` deep (bounded by `OVERFETCH_CAP`),
/// re-ranked by the combined BM25 + proximity score, then truncated to the caller's
/// limit. This lets an adjacent-terms chunk that BM25 alone ranks just OUTSIDE the
/// top-L be lifted INTO the final top-L, so proximity is a recall win, not merely a
/// reorder of the already-returned set. A query with no proximity boost (single-term,
/// or terms that never co-occur) re-ranks to the identical BM25 order, so truncating
/// the deeper pool to L yields the same top-L as a plain limit-L fetch.
const OVERFETCH_K: usize = 8;
const OVERFETCH_CAP: usize = 200;

/// Multiplicative rank penalty for documentation chunks (`.md`/`.markdown`), applied
/// to the BM25 base before the proximity boost. BM25 length-normalization otherwise
/// floats a short markdown chunk (e.g. the README's own example queries) above the
/// real code; the penalty lets equally-relevant code win. Uniform, so a doc-only
/// result set keeps its order. 0.7 flips the observed README-over-code cases.
const DOC_RANK_PENALTY: f64 = 0.7;

/// Multiplicative rank boost for a chunk whose raw text contains a strong compound
/// identifier from the query (see [`strong_ident_terms`]), applied once per distinct
/// such term present, to the BM25 base after [`DOC_RANK_PENALTY`] and before
/// the proximity boost. Multiplicative so it is scale-free against BM25 magnitudes
/// (like the doc penalty): a query mixing a rare compound identifier with common prose
/// words then reranks the file naming that identifier above a high-TF prose chunk that
/// never mentions it. The 5x `symbols`-field weight alone misses this whenever the
/// identifier is not captured as a definition (a call site, a config key, a language
/// whose def keyword the symbol regex omits). Gated by `LENS_IDENT_RERANK` (default
/// on); a query carrying no strong identifier yields no term, so the pass is a no-op
/// and the order is exactly today's BM25 + proximity.
const IDENT_BOOST: f64 = 3.0;

/// Multiplicative rank boost for a chunk that *defines* a queried identifier (its text
/// carries `fn NAME` / `struct NAME` / `const NAME` etc.), applied once per distinct
/// such identifier. Where [`IDENT_BOOST`] fires on any chunk that merely mentions the
/// identifier, this discriminates the definition from call sites, comments, and tests,
/// lifting the def chunk to the top hit so [`SearchContext::Rich`] renders the right
/// unit. Gated by `LENS_DEF_BOOST` (default off): unset, `def_terms` is empty and the
/// pass is a no-op, so the order is exactly today's BM25 + proximity.
const DEF_BOOST: f64 = 4.0;

/// Identifier-like query tokens (`[A-Za-z_][A-Za-z0-9_]{2,}`) that could name a symbol
/// whose definition a chunk might carry. A pure split, so a query with no such token
/// yields an empty list and the definition boost is a no-op.
pub(crate) fn def_ident_terms(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| t.len() >= 3 && t.starts_with(|c: char| c.is_alphabetic() || c == '_'))
        .map(str::to_string)
        .collect()
}

/// Whether `content` defines the exact identifier `ident`: a definition keyword (`fn`,
/// `struct`, `const`, ...) immediately followed by `ident` as a whole word. Lexical and
/// language-general (the keyword set spans the grammars lens indexes); the identifier
/// boundary on both sides stops `fn foobar` from matching `foo`.
fn defines_symbol(content: &str, ident: &str) -> bool {
    const DEF_KW: &[&str] = &[
        "fn", "struct", "enum", "trait", "type", "const", "static", "mod", "impl", "class",
        "def", "func", "interface",
    ];
    let boundary = |c: char| !c.is_alphanumeric() && c != '_';
    for kw in DEF_KW {
        let needle = format!("{kw} {ident}");
        let mut from = 0;
        while let Some(rel) = content[from..].find(&needle) {
            let start = from + rel;
            let end = start + needle.len();
            let before_ok = content[..start].chars().next_back().is_none_or(boundary);
            let after_ok = content[end..].chars().next().is_none_or(boundary);
            if before_ok && after_ok {
                return true;
            }
            from = end;
        }
    }
    false
}

/// BM25-ranked search over the stemmed `symbols` + `content` fields (the default
/// path), with a deterministic term-proximity (min-window span) re-rank on top.
/// Over-fetches a deeper BM25 pool (see `OVERFETCH_K`), re-ranks by the combined
/// score, then truncates to `limit`, so proximity can lift an adjacent-terms chunk
/// INTO the final top-L rather than only reordering the top-L BM25 already returned.
fn ranked_search(
    store: &TantivyStore,
    query: &str,
    limit: usize,
    file_ranks: &HashMap<String, usize>,
) -> Result<Vec<SearchHit>> {
    // Over-fetch a deeper BM25 pool than the caller asked for, so the proximity
    // re-rank below can pull a tight-span chunk ranked beyond L into the final top-L.
    let fetch = limit.saturating_mul(OVERFETCH_K).min(OVERFETCH_CAP);
    let candidates = store.ranked_candidates(query, fetch)?;
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    // Distinct query terms for proximity. The index is stemmed, so an inflected query
    // term won't position-match an unstemmed surface form; we match on exact
    // lowercased tokens, which is conservative — it can only miss a boost, never add a
    // spurious one. Single-term queries have no span, so the pass is a no-op and the
    // order matches BM25 exactly.
    let terms = proximity_terms(query);
    // Identifier-rarity boost terms: strong compound identifiers in the query (see
    // `strong_ident_terms`). Read the escape hatch per call (default on when unset or
    // any value != "0"); off, or a query carrying no compound identifier, yields an
    // empty list so the per-candidate boost below is a no-op and the order is exactly
    // today's BM25 + proximity (single-term prefix stability preserved by construction).
    let ident_boost_on = std::env::var("LENS_IDENT_RERANK")
        .map(|v| v != "0")
        .unwrap_or(true);
    let ident_terms = if ident_boost_on {
        strong_ident_terms(query)
    } else {
        Vec::new()
    };
    // Definition-boost terms: identifier-like query tokens whose defining chunk should
    // outrank its mentions. Gated by `LENS_DEF_BOOST` (default off); unset yields an
    // empty list so the per-candidate boost below is a no-op and the order is today's.
    let def_boost_on = std::env::var("LENS_DEF_BOOST")
        .map(|v| v != "0")
        .unwrap_or(false);
    let def_terms = if def_boost_on {
        def_ident_terms(query)
    } else {
        Vec::new()
    };
    // Context rendering per hit (env-gated, default Snippet = today's output verbatim).
    let ctx_mode = search_context_mode();
    // Full chunks kept aside for `SearchContext::Rich`'s top-hit swap (empty otherwise).
    let mut content_by_key: HashMap<String, String> = HashMap::new();
    // (path, chunk_id, snippet, combined_score)
    let mut rows: Vec<(String, String, String, f64)> = Vec::with_capacity(candidates.len());
    for (path, chunk_id, content, mut score) in candidates {
        if is_doc_path(&path) {
            score *= DOC_RANK_PENALTY;
        }
        // One multiply per DISTINCT strong identifier the raw chunk contains
        // (case-insensitive substring), applied to the BM25 base like the doc penalty.
        if !ident_terms.is_empty() {
            let content_lc = content.to_lowercase();
            for t in &ident_terms {
                if content_lc.contains(t.as_str()) {
                    score *= IDENT_BOOST;
                }
            }
        }
        // Definition boost: a chunk that DEFINES a queried identifier outranks one that
        // merely mentions it, so the top hit is the definition (what `Rich` renders whole).
        if !def_terms.is_empty() {
            for t in &def_terms {
                if defines_symbol(&content, t) {
                    score *= DEF_BOOST;
                }
            }
        }
        if terms.len() >= 2 {
            if let Some(span) = min_cover_span(&content, &terms) {
                score += PROX_WEIGHT / span.max(1) as f64;
            }
        }
        let snippet = if ctx_mode == SearchContext::Chunk {
            content
        } else {
            let rendered = match ctx_mode {
                SearchContext::Unit => unit_snippet(&content, &terms, &path)
                    .unwrap_or_else(|| ranked_snippet(&content, &terms)),
                _ => ranked_snippet(&content, &terms),
            };
            // Rich keeps the full chunk aside, keyed by (path, chunk_id), so `render_final`
            // can swap it in for the top hit once the final order is known.
            if ctx_mode == SearchContext::Rich {
                content_by_key.insert(format!("{path}\u{1f}{chunk_id}"), content);
            }
            rendered
        };
        rows.push((path, chunk_id, snippet, score));
    }
    // Re-rank: higher combined score first, then a stable (path, chunk_id) tiebreak.
    // With no proximity boost this reproduces the BM25 order, so the truncation below
    // leaves the unboosted top-L identical to a plain limit-L fetch.
    rows.sort_by(|a, b| {
        b.3.partial_cmp(&a.3)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
            .then_with(|| a.1.cmp(&b.1))
    });
    // Reciprocal-rank fusion (RRF, k=60): blend the text rank just established above
    // with a per-file graph-importance rank from the caller, so a structurally-central
    // file the text rank buries just outside top-L is lifted INTO it.
    // `fused(d) = 1/(60 + text_rank) + 1/(60 + file_rank)`, the second term only when
    // the path is present in `file_ranks`. The switch is read PER CALL (default on,
    // mirroring `LENS_IDENT_RERANK`). With an empty map or the switch off the block is
    // skipped entirely, so the truncation below is byte-identical to pre-fusion output.
    let rrf_on = std::env::var("LENS_RRF").map(|v| v != "0").unwrap_or(true);
    if rrf_on && !file_ranks.is_empty() {
        const RRF_K: f64 = 60.0;
        // (fused_score, text_rank, path, chunk_id, snippet, text_score)
        let mut fused: Vec<(f64, usize, String, String, String, f64)> = rows
            .into_iter()
            .enumerate()
            .map(|(text_rank, (path, chunk_id, snippet, score))| {
                let mut f = 1.0 / (RRF_K + text_rank as f64);
                if let Some(file_rank) = file_ranks.get(&path) {
                    f += 1.0 / (RRF_K + *file_rank as f64);
                }
                (f, text_rank, path, chunk_id, snippet, score)
            })
            .collect();
        // Fused score desc; tie-break text_rank asc, then path asc.
        fused.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.2.cmp(&b.2))
        });
        // The reported score stays the text combined score (fusion reorders, it does
        // not restate relevance), so an unfused hit's payload is unchanged.
        let ordered: Vec<(String, String, String, f64)> = fused
            .into_iter()
            .map(|(_fused, _text_rank, path, chunk_id, snippet, score)| {
                (path, chunk_id, snippet, score)
            })
            .collect();
        return Ok(render_final(ordered, limit, ctx_mode, &content_by_key));
    }
    // Truncate the over-fetched, re-ranked pool back to the caller's limit.
    Ok(render_final(rows, limit, ctx_mode, &content_by_key))
}

/// Truncate the ordered `(path, chunk_id, snippet, score)` pool to `limit` and map to
/// [`SearchHit`]. In [`SearchContext::Rich`] the top hit's snippet is swapped for its
/// full stored chunk (looked up in `content_by_key`), so the most relevant result
/// carries its whole AST-bounded unit while the tail stays cheap snippets.
fn render_final(
    ordered: Vec<(String, String, String, f64)>,
    limit: usize,
    ctx_mode: SearchContext,
    content_by_key: &HashMap<String, String>,
) -> Vec<SearchHit> {
    let mut top: Vec<(String, String, String, f64)> = ordered.into_iter().take(limit).collect();
    if ctx_mode == SearchContext::Rich {
        if let Some(first) = top.first_mut() {
            if let Some(full) = content_by_key.get(&format!("{}\u{1f}{}", first.0, first.1)) {
                first.2 = full.clone();
            }
        }
    }
    top.into_iter()
        .map(|(path, _chunk_id, snippet, score)| SearchHit {
            path,
            snippet,
            score,
        })
        .collect()
}

/// Whitespace-token width of a ranked snippet, mirroring the old FTS5
/// `snippet(content, …, 24)` so the context returned to the model (and its byte
/// size) stays comparable.
const SNIPPET_TOKENS: usize = 24;

/// A deterministic snippet for a ranked hit: a ~[`SNIPPET_TOKENS`]-token window
/// around the first query-term match in `content`, with matched tokens bracketed
/// `[like]` and a ` … ` marker where text is elided (mirrors the old FTS5 snippet).
/// A pure function of `(content, terms)`, so it is byte-stable across different
/// `limit`s — the over-fetch prefix invariant depends on it.
fn ranked_snippet(content: &str, terms: &[String]) -> String {
    let toks: Vec<&str> = content.split_whitespace().collect();
    if toks.is_empty() {
        return String::new();
    }
    let is_match = |t: &str| {
        let low = t.to_lowercase();
        terms.iter().any(|q| low.contains(q.as_str()))
    };
    // Start the window a few tokens before the first match so the match sits in
    // context, not at the very edge.
    let first = toks.iter().position(|t| is_match(t)).unwrap_or(0);
    let start = first.saturating_sub(4);
    let end = (start + SNIPPET_TOKENS).min(toks.len());
    let mut out = String::new();
    if start > 0 {
        out.push_str("… ");
    }
    for (i, t) in toks[start..end].iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        if is_match(t) {
            out.push('[');
            out.push_str(t);
            out.push(']');
        } else {
            out.push_str(t);
        }
    }
    if end < toks.len() {
        out.push_str(" …");
    }
    out
}

/// Byte cap for the adaptive enclosing-unit returned by `LENS_SEARCH_CONTEXT=unit`:
/// the match's enclosing definition is returned whole when it fits, otherwise the
/// search falls back to [`ranked_snippet`]. ~2 KB returns ~90% of this repo's
/// functions whole while bounding worst-case context to a few hundred tokens.
const SEARCH_UNIT_CAP_BYTES: usize = 2048;

/// How `lens_search` renders each hit's context, selected by `LENS_SEARCH_CONTEXT`.
/// `Snippet` (default) is today's byte-identical ~24-token window; `Unit` returns the
/// match's enclosing definition (capped, snippet fallback); `Chunk` returns the whole
/// stored index chunk for every hit (the blunt ~4 KB-per-hit variant, kept for A/B);
/// `Rich` returns the whole chunk for the top hit only, snippets for the rest.
#[derive(Clone, Copy, PartialEq)]
enum SearchContext {
    Snippet,
    Unit,
    Chunk,
    Rich,
}

/// Read the search-context mode. Default `Snippet` keeps the current output verbatim,
/// so with the env unset every existing search result is byte-identical.
fn search_context_mode() -> SearchContext {
    match std::env::var("LENS_SEARCH_CONTEXT").as_deref() {
        Ok("unit") => SearchContext::Unit,
        Ok("chunk") => SearchContext::Chunk,
        Ok("rich") => SearchContext::Rich,
        _ => SearchContext::Snippet,
    }
}

/// Function- and type-definition node kinds across the tree-sitter grammars lens parses,
/// used by [`unit_snippet`] to find the enclosing definition of a match. Statement-level
/// kinds (Rust `let_declaration`, JS `variable_declaration`, ...) are deliberately absent
/// so the walk returns the whole enclosing function, not the single statement the match
/// sits on.
const DEF_KINDS: &[&str] = &[
    // rust
    "function_item",
    "struct_item",
    "enum_item",
    "trait_item",
    "impl_item",
    "mod_item",
    "union_item",
    "macro_definition",
    // python, c/c++
    "function_definition",
    "class_definition",
    // javascript / typescript
    "function_declaration",
    "generator_function_declaration",
    "method_definition",
    "class_declaration",
    "interface_declaration",
    "enum_declaration",
    // go, java
    "method_declaration",
    "type_declaration",
    "constructor_declaration",
];

/// The enclosing definition around the first query-term match in `content`, returned as a
/// verbatim source slice when it parses and fits [`SEARCH_UNIT_CAP_BYTES`]. Descends to the
/// token at the match, then walks up to the nearest [`DEF_KINDS`] node (fn/struct/impl/
/// class/method). Returns `None` (caller falls back to [`ranked_snippet`]) when: the file
/// has no grammar, the chunk doesn't parse, the match sits in no definition (e.g. a
/// top-level comment), or the enclosing definition is larger than the cap. It never returns
/// a sub-fragment of an oversize definition, so a name query inside a large function yields
/// a snippet fallback, not the bare identifier. Uses the same spec as [`chunk_by_ast`].
fn unit_snippet(content: &str, terms: &[String], path: &str) -> Option<String> {
    let ext = Path::new(path).extension().and_then(|e| e.to_str())?;
    let spec = any_spec_for_extension(ext)?;
    // Byte offset of the first matching term. `to_ascii_lowercase` is length-preserving
    // and the proximity terms are already lowercased ASCII, so the offset maps straight
    // back into `content`. No match (query stemmed past the surface form) starts at 0,
    // mirroring `ranked_snippet`'s `unwrap_or(0)`.
    let lc = content.to_ascii_lowercase();
    let offset = terms
        .iter()
        .filter_map(|t| lc.find(t.as_str()))
        .min()
        .unwrap_or(0);
    enclosing_def(content, &spec, offset, SEARCH_UNIT_CAP_BYTES)
}

/// Walk up from byte `offset` in `content` to the nearest enclosing [`DEF_KINDS`] node,
/// returning its verbatim source slice when it fits `cap`. Shared by [`unit_snippet`] (a
/// query-match offset) and [`symbol_def_source`] (a graph-resolved line). Returns `None`
/// when `content` has no grammar match at `offset`, the offset sits in no definition, or
/// the enclosing definition exceeds `cap` (never a fragment).
fn enclosing_def(content: &str, spec: &AnySpec, offset: usize, cap: usize) -> Option<String> {
    let mut parser = Parser::new();
    parser.set_language(&spec.language()).ok()?;
    let tree = parser.parse(content, None)?;
    let mut node = tree.root_node().descendant_for_byte_range(offset, offset)?;
    loop {
        if DEF_KINDS.contains(&node.kind()) {
            if node.end_byte() - node.start_byte() <= cap {
                return content
                    .get(node.start_byte()..node.end_byte())
                    .map(str::to_string);
            }
            return None;
        }
        node = node.parent()?;
    }
}

/// The enclosing definition at 1-based `line` of a source file, verbatim, when it parses
/// and fits `cap`. The graph-aware symbol fetch uses it: the graph resolves a symbol to
/// its `(file, line)`, and this returns that definition's whole source to seed the top
/// hit, so an exact-symbol query surfaces the definition even when FTS recall buries it.
pub(crate) fn symbol_def_source(src: &str, path: &str, line: usize, cap: usize) -> Option<String> {
    let ext = Path::new(path).extension().and_then(|e| e.to_str())?;
    let spec = any_spec_for_extension(ext)?;
    // 1-based line -> byte offset of its first char (sum the bytes of the lines before it).
    let mut offset = 0usize;
    for (i, l) in src.split_inclusive('\n').enumerate() {
        if i + 1 == line {
            break;
        }
        offset += l.len();
    }
    enclosing_def(src, &spec, offset, cap)
}

/// Lowercased alphanumeric tokens of `text`, in order. Splits on non-alphanumeric
/// (the stemmed tokenizer's word boundaries minus stemming), applied identically to
/// query and chunk so positions align.
fn proximity_tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
}

/// Distinct query terms (lowercased, order-preserving) for the proximity pass.
fn proximity_terms(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for t in proximity_tokens(query) {
        if !terms.contains(&t) {
            terms.push(t);
        }
    }
    terms
}

/// Strong compound-identifier query terms (lowercased, first-seen order) for the
/// identifier-rarity boost. A whitespace token is kept only if it carries an
/// underscore OR an internal camelCase hump (a lowercase char immediately followed by
/// an uppercase one) AND is at least 4 chars long, i.e. a compound symbol like
/// `parse_shard_header` or `doFetchBillingInfo`, never a bare prose word (`parse`,
/// `call`, `Billing`). Char-scan only (mirrors [`split_subwords`]), no regex, no new
/// dependency.
fn strong_ident_terms(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for tok in query.split_whitespace() {
        let chars: Vec<char> = tok.chars().collect();
        if chars.len() < 4 {
            continue;
        }
        let has_underscore = chars.contains(&'_');
        let has_camel_hump = chars
            .windows(2)
            .any(|w| w[0].is_lowercase() && w[1].is_uppercase());
        if !(has_underscore || has_camel_hump) {
            continue;
        }
        let lowered = tok.to_lowercase();
        if !terms.contains(&lowered) {
            terms.push(lowered);
        }
    }
    terms
}

/// Tightest token-position window covering at least one occurrence of every term
/// in `terms` within `content`, expressed as `max_pos - min_pos` (adjacent terms
/// give 1). `None` when some term never appears, so no proximity boost applies.
fn min_cover_span(content: &str, terms: &[String]) -> Option<usize> {
    let k = terms.len();
    // Occurrences of any query term, as (token_position, term_index), in position
    // order (enumerate is monotonic, so `occ` is already sorted).
    let occ: Vec<(usize, usize)> = proximity_tokens(content)
        .enumerate()
        .filter_map(|(pos, tok)| terms.iter().position(|t| *t == tok).map(|ti| (pos, ti)))
        .collect();
    if occ.is_empty() {
        return None;
    }
    // Sliding window: smallest range covering all k distinct terms.
    let mut counts = vec![0usize; k];
    let mut have = 0usize;
    let mut left = 0usize;
    let mut best: Option<usize> = None;
    for right in 0..occ.len() {
        if counts[occ[right].1] == 0 {
            have += 1;
        }
        counts[occ[right].1] += 1;
        while have == k {
            let span = occ[right].0 - occ[left].0;
            best = Some(best.map_or(span, |b| b.min(span)));
            counts[occ[left].1] -= 1;
            if counts[occ[left].1] == 0 {
                have -= 1;
            }
            left += 1;
        }
    }
    best
}

/// Literal-substring search over the trigram field for structural / operator
/// queries, scored by case-insensitive occurrence count so the chunk mentioning the
/// identifier most (typically its definition) leads. The candidate pool is filtered
/// to exact literal matches (dropping ngram false positives), then sorted by count.
fn structural_search(store: &TantivyStore, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
    let q = query.trim();
    if q.is_empty() {
        return Ok(Vec::new());
    }
    // Over-fetch like ranked_search, then rank by literal-occurrence count below.
    let fetch = limit.saturating_mul(OVERFETCH_K).min(OVERFETCH_CAP);
    let needle = q.to_lowercase();
    // (path, chunk_id, snippet, occurrence_count)
    let mut hits: Vec<(String, String, String, f64)> = store
        .structural_candidates(q, fetch)?
        .into_iter()
        .filter_map(|(path, chunk_id, content)| {
            let count = content.to_lowercase().matches(&needle).count();
            if count == 0 {
                return None; // drop trigram false positives (ngram is a superset)
            }
            Some((path, chunk_id, structural_snippet(&content, q), count as f64))
        })
        .collect();
    // Sort by occurrence count desc, then a stable (path, chunk_id) tiebreak.
    hits.sort_by(|a, b| {
        b.3.partial_cmp(&a.3)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
            .then_with(|| a.1.cmp(&b.1))
    });
    hits.truncate(limit);
    Ok(hits
        .into_iter()
        .map(|(path, _chunk_id, snippet, score)| SearchHit {
            path,
            snippet,
            score,
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

/// True when a query is a single whitespace-free token carrying structural
/// punctuation the stemmed tokenizer would strip (`std::fs`, `->`, `Level::parse`),
/// so it routes to the trigram path for literal matching. A multi-word query goes to
/// the stemmed/BM25 path even if it contains a hyphen, since it is prose, not a literal
/// symbol (otherwise "a read-only bash command" would be phrase-matched and miss).
fn is_structural(query: &str) -> bool {
    let q = query.trim();
    !q.is_empty()
        && !q.contains(char::is_whitespace)
        && q.chars().any(|c| !c.is_alphanumeric() && c != '_')
}

/// True for documentation chunks (markdown), which get [`DOC_RANK_PENALTY`] so
/// equally relevant code outranks them.
fn is_doc_path(path: &str) -> bool {
    path.ends_with(".md") || path.ends_with(".markdown")
}

/// Split a file into chunks: markdown by headings, code by AST node boundaries (via
/// [`chunk_by_ast`] for any grammar we can parse), and everything else by line windows.
///
/// The `LENS_AST_CHUNK` kill switch (default on; `=0` forces the old fixed line-window
/// path, mirroring `LENS_RRF`/`LENS_IDENT_RERANK`) is read once per file — cheap, since
/// `chunk_file` is called once per file, not per chunk — and is the recall gate's
/// trip-proof. An extension with no tree-sitter grammar always falls back to line
/// windows regardless of the switch.
fn chunk_file(path: &Path, content: &str) -> Vec<String> {
    let ext = path.extension().and_then(|e| e.to_str());
    let is_md = matches!(ext, Some("md") | Some("markdown"));
    if is_md {
        return chunk_markdown(content);
    }
    let ast_on = std::env::var("LENS_AST_CHUNK")
        .map(|v| v != "0")
        .unwrap_or(true);
    match ext.and_then(any_spec_for_extension) {
        Some(spec) if ast_on => chunk_by_ast(content, &spec, AST_CHUNK_BYTES),
        _ => chunk_by_lines(content, CODE_WINDOW),
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

/// Split code into chunks aligned to tree-sitter node boundaries. A top-level named
/// node within `limit` bytes becomes one chunk; a node exceeding `limit` is split into
/// its named children (recursively); adjacent under-limit siblings merge by byte span
/// until the next would push the span past `limit`.
///
/// Chunks are consecutive byte slices of `content` (each runs from the previous chunk's
/// end to the current boundary, the last to end-of-file), so they tile the file exactly:
/// inter-node gaps — whitespace, punctuation the grammar leaves between named nodes — are
/// absorbed into the adjoining chunk and no byte is dropped, hence `chunks.concat()`
/// recovers `content` verbatim. A non-empty input yields at least one chunk.
///
/// Falls back to [`chunk_by_lines`] when the grammar yields no usable tree: the parser
/// can't load the language, `parse` returns `None`, or the root node has no named
/// children (an empty or fully-unparsed file).
fn chunk_by_ast(content: &str, spec: &AnySpec, limit: usize) -> Vec<String> {
    let language = spec.language();
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return chunk_by_lines(content, CODE_WINDOW);
    }
    let tree = match parser.parse(content, None) {
        Some(t) => t,
        None => return chunk_by_lines(content, CODE_WINDOW),
    };
    let root = tree.root_node();
    if root.named_child_count() == 0 {
        return chunk_by_lines(content, CODE_WINDOW);
    }

    // Flatten to atomic (start_byte, end_byte) units in source order.
    let mut units: Vec<(usize, usize)> = Vec::new();
    collect_units(root, limit, &mut units);
    if units.is_empty() {
        return chunk_by_lines(content, CODE_WINDOW);
    }

    // Merge adjacent units greedily by byte span, materializing each chunk as the
    // consecutive slice `content[chunk_start..boundary]` so gaps are covered. The span
    // is measured from the current group's first unit start; when the next unit would
    // push it past `limit`, flush at the previous unit's end and start a new group.
    let mut chunks: Vec<String> = Vec::new();
    let mut chunk_start = 0usize;
    let mut group_start = units[0].0;
    let mut last_end = units[0].1;
    for &(start, end) in &units[1..] {
        if end - group_start > limit {
            chunks.push(content[chunk_start..last_end].to_string());
            chunk_start = last_end;
            group_start = start;
        }
        last_end = end;
    }
    // Final group extends to end-of-file so any trailing gap is covered.
    chunks.push(content[chunk_start..].to_string());
    chunks
}

/// Recursively flatten a node's named descendants into atomic `(start_byte, end_byte)`
/// units for [`chunk_by_ast`]: a named child within `limit` bytes is one unit; an
/// oversize child is split into ITS named children; an oversize node with no named
/// children is kept whole (nothing left to split). Units come out in source order and
/// are pairwise disjoint (a unit is never an ancestor of another).
fn collect_units(node: TsNode<'_>, limit: usize, out: &mut Vec<(usize, usize)>) {
    let mut cursor = node.walk();
    let children: Vec<TsNode> = node.named_children(&mut cursor).collect();
    drop(cursor);
    for child in children {
        let (start, end) = (child.start_byte(), child.end_byte());
        if end - start <= limit || child.named_child_count() == 0 {
            out.push((start, end));
        } else {
            collect_units(child, limit, out);
        }
    }
}

/// True when `path`'s extension matches [`BINARY_EXT_DENYLIST`] (case-insensitive).
fn is_binary_ext(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let lower = e.to_ascii_lowercase();
            BINARY_EXT_DENYLIST.contains(&lower.as_str())
        })
        .unwrap_or(false)
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
/// snake_case or single-case, which the stemmed tokenizer already splits on
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
    fn single_term_overfetch_top_l_is_prefix_stable() {
        // Over-fetch invariant: a single-term query carries no proximity boost, so the
        // re-rank collapses to the plain BM25 order. Truncating a deeper over-fetched
        // pool to L must therefore yield exactly the first L of any larger limit — the
        // top-L stays byte-identical to a plain limit-L fetch. 12 matching chunks
        // saturate the over-fetch (3*8 and 12*8 both exceed 12), so only the truncation
        // differs.
        let data = tempdir().unwrap();
        let dir = tempdir().unwrap();
        for i in 0..12 {
            fs::write(dir.path().join(format!("f{i:02}.rs")), vec!["widget"; i + 1].join(" ")).unwrap();
        }
        let idx = Index::open(data.path()).unwrap();
        idx.index_path(dir.path(), true).unwrap();

        let small = &idx.search(&["widget".into()], 3).unwrap().results[0].hits;
        let large = &idx.search(&["widget".into()], 12).unwrap().results[0].hits;
        assert_eq!(small.len(), 3, "small query returns exactly L");
        assert!(large.len() >= 3);
        for k in 0..3 {
            assert_eq!(small[k].path, large[k].path, "path differs at {k}");
            assert_eq!(small[k].snippet, large[k].snippet, "snippet differs at {k}");
            assert_eq!(small[k].score, large[k].score, "score differs at {k}");
        }
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

    #[test]
    fn bulk_build_stays_correct() {
        // A large from-scratch build (fanned out across writer threads) must stay
        // correct, and a later 1-file edit must still re-index correctly.
        let data = tempdir().unwrap();
        let src = tempdir().unwrap();
        let n = BULK_FILE_THRESHOLD + 40;
        for i in 0..n {
            fs::write(
                src.path().join(format!("f{i}.rs")),
                format!("fn func_{i}() {{ let marker_{i} = {i}; }}\n"),
            )
            .unwrap();
        }
        let idx = Index::open(data.path()).unwrap();
        let res = idx.index_path(src.path(), true).unwrap();
        assert!(
            res.files_read >= n,
            "bulk build should read every file"
        );
        let out = idx.search(&["func_7".into()], 5).unwrap();
        assert!(
            out.results[0].hits.iter().any(|h| h.path.ends_with("f7.rs")),
            "search must be correct after bulk build"
        );

        // A 1-file edit (below the bulk threshold, single writer thread) must still
        // re-index.
        let edited = src.path().join("f7.rs");
        fs::write(&edited, "fn changed_7() { let z = 1; }\n").unwrap();
        let newer = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        std::fs::File::options()
            .write(true)
            .open(&edited)
            .unwrap()
            .set_modified(newer)
            .unwrap();
        let res2 = idx.index_path(src.path(), true).unwrap();
        assert_eq!(res2.files_read, 1, "only the edited file re-read");
        let out2 = idx.search(&["changed_7".into()], 5).unwrap();
        assert!(
            out2.results[0].hits.iter().any(|h| h.path.ends_with("f7.rs")),
            "edited content searchable"
        );
    }

    // ── lens_search defect guards (each reproduces a fixed bug) ──────────────

    #[test]
    fn search_recalls_multiword_hyphenated_query() {
        // DEFECT 3: a natural multi-word query with a hyphenated term must recall.
        // The terms are OR-joined and "read-only" splits into read + only (never a
        // fused, never-indexed "readonly" nor an implicit AND), so the query recalls.
        let data = tempdir().unwrap();
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("wrap.rs"),
            "fn wrap() {\n    // rewrite a read-only bash command to offload its output losslessly\n}\n",
        )
        .unwrap();
        fs::write(dir.path().join("other.rs"), "fn unrelated() {}\n").unwrap();
        let idx = Index::open(data.path()).unwrap();
        idx.index_path(dir.path(), true).unwrap();
        let hits = &idx
            .search(&["wrap a read-only bash command to offload output".into()], 5)
            .unwrap()
            .results[0]
            .hits;
        assert!(!hits.is_empty(), "hyphenated multi-word query must recall");
        assert!(hits[0].path.ends_with("wrap.rs"));
    }

    #[test]
    fn structural_query_scores_nonzero_and_ranks_by_mentions() {
        // DEFECT 2: identifier queries (with ::) route to the trigram path. They rank
        // by occurrence count, so the file mentioning the identifier most leads with
        // score > 0.
        let data = tempdir().unwrap();
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("def.rs"),
            "impl Level {}\n// Level::parse Level::parse Level::parse\n",
        )
        .unwrap();
        fs::write(dir.path().join("use.rs"), "let x = Level::parse();\n").unwrap();
        let idx = Index::open(data.path()).unwrap();
        idx.index_path(dir.path(), true).unwrap();
        let hits = &idx.search(&["Level::parse".into()], 5).unwrap().results[0].hits;
        assert!(!hits.is_empty());
        assert!(hits[0].score > 0.0, "structural hit must score > 0");
        assert!(hits[0].path.ends_with("def.rs"), "most-mentions file leads");
    }

    #[test]
    fn doc_penalty_ranks_code_above_identical_markdown() {
        // DEFECT 4: identical content in a .md and a .rs differs only by the doc
        // penalty. The .md is named to win the path tiebreak, so only the penalty can
        // demote it below the code.
        let data = tempdir().unwrap();
        let dir = tempdir().unwrap();
        let body = "the routing level is parsed in this module\n";
        fs::write(dir.path().join("a_doc.md"), body).unwrap();
        fs::write(dir.path().join("z_code.rs"), body).unwrap();
        let idx = Index::open(data.path()).unwrap();
        idx.index_path(dir.path(), true).unwrap();
        let hits = &idx
            .search(&["routing level parsed".into()], 5)
            .unwrap()
            .results[0]
            .hits;
        assert!(hits.len() >= 2);
        assert!(
            hits[0].path.ends_with("z_code.rs"),
            "doc penalty must rank code above the identical .md, got {}",
            hits[0].path
        );
    }

    #[test]
    fn dot_path_index_has_no_duplicate_hits() {
        // DEFECT 1: indexing both the canonical path and its "/./" spelling must
        // collapse to one canonical entry, not return the file twice.
        let data = tempdir().unwrap();
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("auth.rs"), "fn authenticate() {}\n").unwrap();
        let idx = Index::open(data.path()).unwrap();
        idx.index_path(dir.path(), true).unwrap();
        idx.index_path(&dir.path().join("."), true).unwrap();
        let hits = &idx.search(&["authenticate".into()], 5).unwrap().results[0].hits;
        assert_eq!(hits.len(), 1, "two path spellings must collapse to one hit");
        assert!(
            !hits[0].path.contains("/./"),
            "no /./ in stored path: {}",
            hits[0].path
        );
    }

    // ── identifier-rarity boost (L39) ───────────────────────────────────────

    /// Two-file corpus for the identifier-boost tests: `billing.rs` mentions the
    /// compound identifier `doFetchBillingInfo` once as a CALL (so `chunk_symbols`
    /// never captures it and the 5x symbols weight stays out of play), while
    /// `usage.rs` repeats the prose query words `call`/`action`, so plain BM25 ranks
    /// the prose file above the def file. The boost is the only thing that flips them.
    fn ident_corpus() -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("billing.rs"),
            "fn handler() {\n    doFetchBillingInfo();\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("usage.rs"),
            "call action call action call action call action call action call action\n",
        )
        .unwrap();
        dir
    }

    /// Serializes the two `LENS_IDENT_RERANK`-sensitive tests so their process-global
    /// env mutation cannot interleave. Other tests are unaffected: their queries carry
    /// no strong compound identifier, so a transient env value changes nothing for them.
    static BOOST_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn ident_boost_lifts_def_over_prose() {
        let _guard = BOOST_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Default ON: the escape hatch unset. (remove_var is a no-op if already unset.)
        std::env::remove_var("LENS_IDENT_RERANK");
        let data = tempdir().unwrap();
        let src = ident_corpus();
        let idx = Index::open(data.path()).unwrap();
        idx.index_path(src.path(), true).unwrap();

        let hits = &idx
            .search(&["doFetchBillingInfo call action".into()], 5)
            .unwrap()
            .results[0]
            .hits;
        assert!(
            hits[0].path.ends_with("billing.rs"),
            "identifier boost must lift the def file to rank 1, got {}",
            hits[0].path
        );
    }

    #[test]
    fn ident_boost_off_restores_bm25_order() {
        let _guard = BOOST_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("LENS_IDENT_RERANK", "0");
        let data = tempdir().unwrap();
        let src = ident_corpus();
        let idx = Index::open(data.path()).unwrap();
        idx.index_path(src.path(), true).unwrap();

        let top = idx
            .search(&["doFetchBillingInfo call action".into()], 5)
            .unwrap()
            .results[0]
            .hits[0]
            .path
            .clone();
        // Restore the env BEFORE asserting so a failed assertion can't leak the "0"
        // value into a later test.
        std::env::remove_var("LENS_IDENT_RERANK");
        assert!(
            !top.ends_with("billing.rs"),
            "with the boost off, plain BM25 keeps the high-TF prose file on top, got {top}"
        );
    }

    // ── RRF graph-importance fusion (L43) ───────────────────────────────────

    /// Serializes the two `LENS_RRF`-sensitive tests so their process-global env
    /// mutation cannot interleave. Every other test searches with an empty file-rank
    /// map, so the fusion block is skipped regardless of `LENS_RRF` and a transient
    /// value changes nothing for them.
    static RRF_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn rrf_empty_map_or_off_is_byte_identical() {
        let _guard = RRF_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Default ON (switch unset).
        std::env::remove_var("LENS_RRF");
        let data = tempdir().unwrap();
        let src = corpus();
        let idx = Index::open(data.path()).unwrap();
        idx.index_path(src.path(), true).unwrap();

        let queries = vec!["authenticate".to_string(), "pooling".to_string(), "add".to_string()];
        let baseline = idx.search(&queries, 5).unwrap();

        // (i) An empty file-rank map (fusion on by default) is byte-identical to search.
        let empty = idx.search_fused(&queries, 5, &HashMap::new()).unwrap();
        assert_eq!(
            serde_json::to_string(&baseline).unwrap(),
            serde_json::to_string(&empty).unwrap(),
            "empty-map fused search must be byte-identical to plain search"
        );

        // (ii) A POPULATED map with the switch off is still byte-identical: it is the
        // env kill switch, not the emptiness of the map, that neutralizes fusion.
        let mut ranks: HashMap<String, usize> = HashMap::new();
        for r in &baseline.results {
            for (i, h) in r.hits.iter().enumerate() {
                ranks.insert(h.path.clone(), i);
            }
        }
        std::env::set_var("LENS_RRF", "0");
        let off = idx.search_fused(&queries, 5, &ranks).unwrap();
        // Restore BEFORE asserting so a failed assertion can't leak "0" into a later test.
        std::env::remove_var("LENS_RRF");
        assert_eq!(
            serde_json::to_string(&baseline).unwrap(),
            serde_json::to_string(&off).unwrap(),
            "LENS_RRF=0 must produce byte-identical output even with a populated map"
        );
    }

    // ── nested-repo boundary pruning + binary/size guard (T2) ──────────────

    /// T2: `index_path` must not read a nested git repo's own files into chunks;
    /// its own `.lens/fts` is queried separately (T4), not folded into this index.
    #[test]
    fn index_path_prunes_nested_git_repo() {
        let data = tempdir().unwrap();
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("top.rs"), "fn top_widget() {}\n").unwrap();

        let nested = dir.path().join("vendor");
        fs::create_dir_all(nested.join(".git")).unwrap();
        fs::write(nested.join("inner.rs"), "fn vendor_gadget() {}\n").unwrap();

        let idx = Index::open(data.path()).unwrap();
        idx.index_path(dir.path(), true).unwrap();

        let top_hits = &idx.search(&["top_widget".into()], 5).unwrap().results[0].hits;
        assert!(!top_hits.is_empty(), "top-level file must be indexed");

        let nested_hits = &idx.search(&["vendor_gadget".into()], 5).unwrap().results[0].hits;
        assert!(
            nested_hits.is_empty(),
            "nested repo's content must not be read into this index, got {nested_hits:?}"
        );
    }

    /// T2 (Fix B): a denylisted binary/media extension and a file over
    /// `MAX_INDEXABLE_FILE_BYTES` must both be skipped before `fs::read`, produce
    /// zero chunks, and stay absent from the manifest (retryable next run).
    #[test]
    fn index_path_skips_binary_ext_and_oversized_files() {
        let data = tempdir().unwrap();
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("kept.rs"), "fn kept() {}\n").unwrap();
        fs::write(dir.path().join("movie.mp4"), b"not really a video").unwrap();
        fs::write(
            dir.path().join("huge.rs"),
            vec![b'a'; (MAX_INDEXABLE_FILE_BYTES + 1) as usize],
        )
        .unwrap();

        let idx = Index::open(data.path()).unwrap();
        let res = idx.index_path(dir.path(), true).unwrap();

        assert_eq!(res.files_read, 1, "only kept.rs should be read");
        let kept_hits = &idx.search(&["kept".into()], 5).unwrap().results[0].hits;
        assert!(!kept_hits.is_empty(), "kept.rs must be searchable");

        assert_eq!(
            idx.chunk_count().unwrap(),
            kept_hits.len() as i64,
            "skipped files must produce zero chunks"
        );

        let manifest: HashSet<String> = {
            let conn = idx.conn().unwrap();
            let mut stmt = conn.prepare("SELECT path FROM file_manifest").unwrap();
            stmt.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .flatten()
                .collect()
        };
        assert!(
            !manifest.iter().any(|p| p.ends_with("movie.mp4")),
            "denylisted extension must be absent from the manifest, got {manifest:?}"
        );
        assert!(
            !manifest.iter().any(|p| p.ends_with("huge.rs")),
            "oversized file must be absent from the manifest, got {manifest:?}"
        );
    }

    #[test]
    fn rrf_lifts_graph_central_file_into_top_l() {
        let _guard = RRF_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("LENS_RRF"); // default on
        let data = tempdir().unwrap();
        let dir = tempdir().unwrap();
        // central.rs mentions the prose query once, so BM25 buries it; six distractor
        // files repeat the terms, so plain BM25 fills the whole top-5 with distractors.
        fs::write(
            dir.path().join("central.rs"),
            "fn open_pool() {\n    // database connection\n    connect();\n}\n",
        )
        .unwrap();
        for i in 0..6 {
            fs::write(
                dir.path().join(format!("distractor{i}.rs")),
                "database connection database connection database connection database connection\n",
            )
            .unwrap();
        }
        let idx = Index::open(data.path()).unwrap();
        idx.index_path(dir.path(), true).unwrap();

        let query = "database connection";
        // Recover central's stored path from a deep no-fusion fetch (it is in the
        // over-fetched pool, just past top-5).
        let deep = idx.search(&[query.to_string()], 50).unwrap();
        let central_path = deep.results[0]
            .hits
            .iter()
            .find(|h| h.path.ends_with("central.rs"))
            .expect("central.rs must be in the candidate pool")
            .path
            .clone();

        // Setup check: without fusion, central is outside top-5.
        let base = idx.search(&[query.to_string()], 5).unwrap();
        assert!(
            !base.results[0].hits.iter().any(|h| h.path.ends_with("central.rs")),
            "setup: central.rs must be buried outside top-5 without fusion, got {:?}",
            base.results[0].hits.iter().map(|h| h.path.as_str()).collect::<Vec<_>>()
        );

        // With fusion and central ranked graph-central (rank 0), it is lifted to the top.
        let mut file_ranks: HashMap<String, usize> = HashMap::new();
        file_ranks.insert(central_path, 0);
        let fused = idx.search_fused(&[query.to_string()], 5, &file_ranks).unwrap();
        assert!(
            fused.results[0].hits[0].path.ends_with("central.rs"),
            "RRF must lift the graph-central file to rank 1, got {}",
            fused.results[0].hits[0].path
        );
    }

    // ── AST-boundary chunking (L32) ─────────────────────────────────────────

    #[test]
    fn chunk_by_ast_aligns_to_fn_boundaries_and_covers_every_byte() {
        use std::fmt::Write as _;

        // A > 100-line file with two substantial top-level fns: `alpha` behind a
        // leading `#[attr]` and `beta` declared `pub`, so the two chunk-start forms
        // (`#[`, `pub fn `) are both exercised alongside plain `fn `. Each body carries
        // uniquely-named markers so coverage can be checked line by line.
        let mut src = String::from("#[allow(dead_code)]\nfn alpha() {\n");
        for i in 0..48 {
            writeln!(src, "    let alpha_marker_{i} = {i};").unwrap();
        }
        src.push_str("}\n\npub fn beta() {\n");
        for i in 0..48 {
            writeln!(src, "    let beta_marker_{i} = {i};").unwrap();
        }
        src.push_str("}\n");
        assert!(src.lines().count() > 100, "fixture must exceed 100 lines");

        let spec = any_spec_for_extension("rs").unwrap();
        // Limit between one fn's byte size (~half the file) and the two combined, so
        // each fn is kept whole yet the pair does not merge into a single chunk.
        let limit = src.len() * 3 / 5;
        let chunks = chunk_by_ast(&src, &spec, limit);

        // Every chunk, once left-trimmed, starts on a definition boundary — a fn, a
        // `pub fn`, or a doc-comment/attribute line that precedes one — never mid-body.
        for (i, c) in chunks.iter().enumerate() {
            let t = c.trim_start();
            assert!(
                t.starts_with("fn ")
                    || t.starts_with("pub fn ")
                    || t.starts_with("///")
                    || t.starts_with("#["),
                "chunk {i} must start on a definition boundary, got: {:?}",
                &t[..t.len().min(40)]
            );
        }

        // The two fns land in SEPARATE chunks.
        let ai = chunks
            .iter()
            .position(|c| c.contains("fn alpha("))
            .expect("a chunk must contain alpha");
        let bi = chunks
            .iter()
            .position(|c| c.contains("fn beta("))
            .expect("a chunk must contain beta");
        assert_ne!(ai, bi, "alpha and beta must fall in different chunks");

        // Full byte coverage: the chunks tile the file exactly (gap-free, no join
        // separator), so concatenation recovers every byte — hence every body line.
        assert_eq!(chunks.concat(), src, "chunks must recover the whole file verbatim");
        assert!(
            chunks.iter().any(|c| c.contains("alpha_marker_47")),
            "alpha's body must survive intact across the chunks"
        );
        assert!(
            chunks.iter().any(|c| c.contains("beta_marker_47")),
            "beta's body must survive intact across the chunks"
        );

        // An unknown extension has no grammar, so `chunk_file` falls back to the fixed
        // line-window chunker (byte-identical to calling it directly).
        assert_eq!(
            chunk_file(Path::new("x.unknownext"), &src),
            chunk_by_lines(&src, CODE_WINDOW),
            "an extension with no tree-sitter grammar must use line-window chunking"
        );
    }

    #[test]
    fn unit_snippet_returns_the_whole_enclosing_fn_not_its_neighbors() {
        // Three small top-level fns; the match sits in the middle one. The enclosing-unit
        // render (LENS_SEARCH_CONTEXT=unit) must hand back `bravo` whole and neither
        // neighbor, where a fixed line window would bleed across the boundaries.
        let src = "fn alpha() {\n    let a = 1;\n}\n\nfn bravo() {\n    let unique_marker = 2;\n}\n\nfn gamma() {\n    let c = 3;\n}\n";
        let terms = vec!["unique_marker".to_string()];
        let out = unit_snippet(src, &terms, "x.rs").expect("enclosing unit for a parseable rs chunk");
        assert!(
            out.contains("fn bravo(") && out.contains("unique_marker"),
            "must return the matched fn, got: {out:?}"
        );
        assert!(
            !out.contains("fn alpha(") && !out.contains("fn gamma("),
            "must exclude neighbor fns, got: {out:?}"
        );
    }

    #[test]
    fn unit_snippet_falls_back_when_enclosing_fn_exceeds_cap() {
        use std::fmt::Write as _;
        // A fn far larger than the cap: the enclosing definition cannot be returned whole,
        // so unit_snippet returns None (the caller emits a ranked snippet) rather than a
        // fragment. Regression guard for the bare-identifier bug where a query matching the
        // fn NAME inside an oversize fn returned just the name.
        let mut src = String::from("fn oversize_target() {\n");
        let mut i = 0;
        while src.len() <= SEARCH_UNIT_CAP_BYTES * 2 {
            writeln!(src, "    let filler_{i} = {i};").unwrap();
            i += 1;
        }
        src.push_str("}\n");
        assert_eq!(
            unit_snippet(&src, &["oversize_target".to_string()], "x.rs"),
            None,
            "name match in an oversize fn must fall back, not return the bare identifier"
        );
        assert_eq!(
            unit_snippet(&src, &["filler_7".to_string()], "x.rs"),
            None,
            "body match in an oversize fn must fall back to a ranked snippet"
        );
    }

    #[test]
    fn defines_symbol_matches_defs_not_mentions() {
        // The def-boost must fire on a real definition and not on a call site, a comment,
        // or a longer identifier that merely contains the query token.
        assert!(defines_symbol("pub fn pagerank(&self) -> Map {", "pagerank"));
        assert!(defines_symbol(
            "const SEARCH_UNIT_CAP_BYTES: usize = 2048;",
            "SEARCH_UNIT_CAP_BYTES"
        ));
        assert!(defines_symbol("    struct SearchContext {\n", "SearchContext"));
        assert!(!defines_symbol("    let r = pagerank(&tp);\n", "pagerank"));
        assert!(!defines_symbol("// see pagerank for details", "pagerank"));
        assert!(!defines_symbol("fn pagerankish() {}", "pagerank"));
    }

    #[test]
    fn def_ident_terms_keeps_identifier_tokens() {
        assert_eq!(def_ident_terms("chunk_by_ast"), vec!["chunk_by_ast".to_string()]);
        assert_eq!(
            def_ident_terms("the pagerank DAMP value"),
            vec![
                "the".to_string(),
                "pagerank".to_string(),
                "DAMP".to_string(),
                "value".to_string()
            ]
        );
        assert!(def_ident_terms("a b 12 x").is_empty());
    }

    #[test]
    fn symbol_def_source_returns_def_at_line() {
        let src = "fn alpha() {\n    let a = 1;\n}\n\nfn bravo() {\n    let b = 2;\n}\n";
        // bravo's definition starts on line 5.
        let out = symbol_def_source(src, "x.rs", 5, 2048).expect("def at line 5");
        assert!(out.contains("fn bravo(") && out.contains("let b = 2"));
        assert!(!out.contains("fn alpha("));
        // Cap smaller than the definition falls back to None (never a fragment).
        assert_eq!(symbol_def_source(src, "x.rs", 5, 4), None);
    }

}
