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

pub use schema::Index;

use self::tantivy_index::TantivyStore;
use crate::discovery::{self, graph::Graph};
use crate::tools::{IndexResponse, QueryResult, SearchHit, SearchResponse};

/// Lines per chunk for non-markdown files.
const CODE_WINDOW: usize = 100;

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
        self.search_fused(queries, limit_per_query, &HashMap::new(), None)
    }

    /// Like [`search`](Index::search), but fuses the text ranking with a per-file
    /// graph-importance rank via reciprocal-rank fusion (RRF; see [`ranked_search`]).
    /// `file_ranks` maps a stored file path (`rel_key` form) to its graph rank
    /// (0 = most central). An empty map — or `LENS_RRF=0` — makes the output
    /// byte-identical to [`search`](Index::search). Only the BM25/prose path fuses;
    /// structural/trigram queries are unaffected. When `graph` is `Some` and
    /// `LENS_EXPAND=1`, each query's hits are then expanded into their 1-hop graph
    /// neighborhood (see [`expand_hits`]); a `None` graph or the default-off flag
    /// leaves them untouched.
    pub fn search_fused(
        &self,
        queries: &[String],
        limit_per_query: usize,
        file_ranks: &HashMap<String, usize>,
        graph: Option<&Graph>,
    ) -> Result<SearchResponse> {
        let store = self.store();
        let mut results = Vec::new();
        for query in queries {
            let hits = if is_structural(query) {
                structural_search(store, query, limit_per_query)?
            } else {
                ranked_search(store, query, limit_per_query, file_ranks)?
            };
            // L51: when a graph is supplied, expand each query's lexical hits into
            // their 1-hop neighborhood. budget = limit_per_query (one neighbor "slot"
            // per requested hit). expand_hits is gated by LENS_EXPAND (default off)
            // and returns hits untouched when off or the graph is empty, so both a
            // None graph and the off flag stay byte-identical.
            let hits = match graph {
                Some(g) => expand_hits(hits, g, limit_per_query),
                None => hits,
            };
            results.push(QueryResult {
                query: query.clone(),
                hits,
            });
        }
        Ok(SearchResponse { results })
    }
}

/// L51 edge-type weights for lexical-hit expansion: how much a 1-hop neighbor
/// inherits from the hit that anchors it, by relationship kind. `calls` is the
/// strongest signal (a direct dependency), `references` weaker (the symbol is
/// used), `imports` weakest (a module-level pull). `contains` (hierarchy) and any
/// other kind score `0.0`, which doubles as the adjacency `keep` filter: only
/// positive-weight kinds expand. A small fixed table; revisit only if the L51
/// recall A/B (T5) justifies it.
fn edge_weight(kind: &str) -> f64 {
    match kind {
        "calls" => 1.0,
        "references" => 0.8,
        "imports" => 0.6,
        _ => 0.0,
    }
}

/// L51 confidence blend weights. A neighbor's confidence is an even split between
/// the anchoring hit's normalized lexical strength (its score over the strongest
/// hit this query, so neighbors of a weak match are discounted) and the neighbor's
/// normalized query-personalized PageRank (how central it is to the query's matched
/// entry points). Both terms lie in `[0, 1]`, so confidence does too. Deliberately
/// simple; the A/B decides whether the split moves.
const LEX_BLEND: f64 = 0.5;
const PI_BLEND: f64 = 0.5;

/// Total order for the L51-expanded result set: score descending, then a
/// deterministic tie-break (path asc, line asc, snippet asc). The appended
/// neighbors are unique by `(path, line)`, so this is a total order over them and
/// no `HashMap` iteration order can leak into the returned ordering.
fn expand_rank_cmp(a: &SearchHit, b: &SearchHit) -> Ordering {
    b.score
        .partial_cmp(&a.score)
        .unwrap_or(Ordering::Equal)
        .then_with(|| a.path.cmp(&b.path))
        .then_with(|| a.line.cmp(&b.line))
        .then_with(|| a.snippet.cmp(&b.snippet))
}

/// Whether L51 hit expansion is enabled: any `LENS_EXPAND` value other than
/// `"0"` is on; unset defaults to off. Single source of truth for the flag —
/// `expand_hits` gates expansion itself on it, and `ranked_search` /
/// `structural_search` gate whether `line` is populated at all on it, so a
/// `LENS_EXPAND=0` (default) search never carries a `line` field and stays
/// byte-identical to master's `SearchHit { path, snippet, score }` shape.
fn expand_enabled() -> bool {
    std::env::var("LENS_EXPAND").map(|v| v != "0").unwrap_or(false)
}

/// L51: expand each lexical search hit into its confidence-filtered 1-hop graph
/// neighborhood, merging the structural neighbors into the ranked result set.
///
/// Gated by `LENS_EXPAND` (read PER CALL, default OFF). When the flag is off, when
/// the graph has no nodes, or when no hit aligns to a graph node, the input `Vec`
/// is returned untouched (byte-identical), so every caller that does not opt in,
/// and every empty-graph caller such as [`Index::search`], is provably unaffected.
///
/// Otherwise, for each hit carrying a `line`, its nearest graph node is the anchor
/// ([`Graph::node_nearest`]). Neighbors are the 1-hop undirected [`Graph::adjacency`]
/// over `calls|imports|references` edges (`contains` hierarchy excluded). A neighbor
/// scores `hit.score * edge_weight(kind) * confidence`, where confidence blends the
/// anchor's lexical strength with one query-seeded [`Graph::personalized_importance`]
/// pass over the anchor nodes. That PageRank propagates over calls+imports only, so
/// a neighbor reached solely by a `references` edge leans on its edge weight rather
/// than its PR. Each factor is `<= 1` and `hit.score` sets the scale, so a neighbor
/// never outranks its own anchor, though a strong hit's neighbor can outrank an
/// unrelated weak hit.
///
/// Neighbors are deduped against the originals and one another by `(path, line)`
/// (highest score wins), the top `budget` survivors are appended, and the whole vec
/// is re-sorted by [`expand_rank_cmp`]. Snippets are a lightweight `"{kind} {name}"`
/// signature, so expansion does no per-neighbor file I/O.
fn expand_hits(hits: Vec<SearchHit>, graph: &Graph, budget: usize) -> Vec<SearchHit> {
    // Flag guard FIRST so the OFF path is byte-identical for every caller.
    let on = expand_enabled();
    if !on || graph.nodes.is_empty() {
        return hits;
    }

    // Align each lexical hit that has a line to its nearest graph node. seed maps an
    // anchor node id to its lexical score, the restart vector for personalized PR.
    let mut anchors = Vec::new();
    let mut seed: HashMap<String, f64> = HashMap::new();
    let mut max_hit_score = 0.0_f64;
    for hit in &hits {
        let Some(line) = hit.line else { continue };
        let Some(node) = graph.node_nearest(&hit.path, line) else {
            continue;
        };
        anchors.push((node, hit.score));
        let slot = seed.entry(node.id.clone()).or_insert(0.0);
        *slot = slot.max(hit.score);
        max_hit_score = max_hit_score.max(hit.score);
    }
    if anchors.is_empty() {
        return hits;
    }

    // Neighbor enumeration reuses adjacency, restricted to the positive-weight
    // (relevance-bearing) edge kinds. adjacency drops the edge kind, so build a
    // companion undirected (from, to) -> best weight map from the same kept edges
    // for the per-neighbor edge_weight lookup.
    let adj = graph.adjacency(|k| edge_weight(k) > 0.0);
    let mut edge_w: HashMap<(&str, &str), f64> = HashMap::new();
    for e in &graph.edges {
        let w = edge_weight(&e.kind);
        if w <= 0.0 {
            continue;
        }
        for (a, b) in [
            (e.from.as_str(), e.to.as_str()),
            (e.to.as_str(), e.from.as_str()),
        ] {
            let slot = edge_w.entry((a, b)).or_insert(0.0);
            if w > *slot {
                *slot = w;
            }
        }
    }

    // One query-personalized PageRank pass seeded on the anchor nodes.
    let pi = graph.personalized_importance(&seed);
    let pi_max = pi.values().copied().fold(0.0_f64, f64::max);

    // Best score per candidate neighbor node id (a neighbor reachable from several
    // anchors keeps its strongest score). HashMap order here does not leak into the
    // result: the materialized neighbors get a total sort below.
    let mut best: HashMap<&str, f64> = HashMap::new();
    for (anchor, hit_score) in &anchors {
        let hit_score = *hit_score;
        let Some(neigh_ids) = adj.get(&anchor.id) else {
            continue;
        };
        let lex_norm = if max_hit_score > 0.0 {
            hit_score / max_hit_score
        } else {
            0.0
        };
        for nid in neigh_ids {
            let kind_w = edge_w
                .get(&(anchor.id.as_str(), nid.as_str()))
                .copied()
                .unwrap_or(0.0);
            if kind_w <= 0.0 {
                continue;
            }
            let pi_norm = if pi_max > 0.0 {
                pi.get(nid).copied().unwrap_or(0.0) / pi_max
            } else {
                0.0
            };
            let confidence = LEX_BLEND * lex_norm + PI_BLEND * pi_norm;
            let score = hit_score * kind_w * confidence;
            if score <= 0.0 {
                continue;
            }
            let slot = best.entry(nid.as_str()).or_insert(0.0);
            if score > *slot {
                *slot = score;
            }
        }
    }

    // Materialize neighbor hits with a lightweight signature snippet (no file I/O).
    let mut neighbors: Vec<SearchHit> = best
        .into_iter()
        .filter_map(|(nid, score)| {
            let n = graph.node(nid)?;
            Some(SearchHit {
                path: n.file.clone(),
                snippet: format!("{} {}", n.kind, n.name),
                score,
                line: Some(n.line),
            })
        })
        .collect();

    // Dedup against the originals and among neighbors by (path, line): sort by score
    // desc first so the strongest survivor of a collision wins, then cap by budget.
    // seen is seeded with the originals so an existing hit is never re-added.
    let mut seen: HashSet<(String, Option<usize>)> =
        hits.iter().map(|h| (h.path.clone(), h.line)).collect();
    neighbors.sort_by(expand_rank_cmp);
    neighbors.retain(|h| seen.insert((h.path.clone(), h.line)));
    neighbors.truncate(budget);
    if neighbors.is_empty() {
        return hits; // nothing new merged, so preserve the original order exactly.
    }

    let mut out = hits;
    out.extend(neighbors);
    out.sort_by(expand_rank_cmp);
    out
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
    // L51: read once per call (not per-hit, to avoid an env read in the candidate
    // loop) so `line` is populated only when expansion can use it; LENS_EXPAND=0
    // (default) keeps every hit's `line` None, matching master's shape exactly.
    let expand_on = expand_enabled();
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
        if terms.len() >= 2 {
            if let Some(span) = min_cover_span(&content, &terms) {
                score += PROX_WEIGHT / span.max(1) as f64;
            }
        }
        let snippet = ranked_snippet(&content, &terms);
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
        return Ok(fused
            .into_iter()
            .take(limit)
            .map(|(_fused, _text_rank, path, chunk_id, snippet, score)| {
                let line = if expand_on {
                    chunk_start_line(&path, &chunk_id)
                } else {
                    None
                };
                SearchHit {
                    path,
                    snippet,
                    score,
                    line,
                }
            })
            .collect());
    }
    // Truncate the over-fetched, re-ranked pool back to the caller's limit.
    Ok(rows
        .into_iter()
        .take(limit)
        .map(|(path, chunk_id, snippet, score)| {
            let line = if expand_on {
                chunk_start_line(&path, &chunk_id)
            } else {
                None
            };
            SearchHit {
                path,
                snippet,
                score,
                line,
            }
        })
        .collect())
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
    // L51: read once per call (not per-hit), mirroring ranked_search — see there for
    // why `line` must be gated on this flag rather than populated unconditionally.
    let expand_on = expand_enabled();
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
        .map(|(path, chunk_id, snippet, score)| {
            let line = if expand_on {
                chunk_start_line(&path, &chunk_id)
            } else {
                None
            };
            SearchHit {
                path,
                snippet,
                score,
                line,
            }
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

/// The chunk's 1-based start line, when derivable. `index_path` names each
/// code-file chunk `"{path}#{0-based window index}"` (see `chunk_file` /
/// `chunk_by_lines`), and every window is exactly `CODE_WINDOW` source lines,
/// so the index maps directly back to a start line. Returns `None` for
/// session-continuity records (`path` prefixed `session://`, mirroring
/// `Index::prune_missing`'s check) and for markdown chunks, which are
/// heading-delimited rather than a fixed line window.
fn chunk_start_line(path: &str, chunk_id: &str) -> Option<usize> {
    if path.starts_with("session://") || is_doc_path(path) {
        return None;
    }
    let idx: usize = chunk_id.strip_prefix(path)?.strip_prefix('#')?.parse().ok()?;
    Some(idx * CODE_WINDOW + 1)
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
        let empty = idx.search_fused(&queries, 5, &HashMap::new(), None).unwrap();
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
        let off = idx.search_fused(&queries, 5, &ranks, None).unwrap();
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
        let fused = idx.search_fused(&[query.to_string()], 5, &file_ranks, None).unwrap();
        assert!(
            fused.results[0].hits[0].path.ends_with("central.rs"),
            "RRF must lift the graph-central file to rank 1, got {}",
            fused.results[0].hits[0].path
        );
    }

    // ── L51 hit expansion (expand_hits) ─────────────────────────────────────

    /// Serializes the `LENS_EXPAND`-sensitive tests (mirrors `RRF_TEST_LOCK`): they
    /// mutate a process-global env var. No other test passes a graph to
    /// `search_fused`, so expansion never runs elsewhere and a transient value is
    /// invisible to them.
    static EXPAND_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A tiny graph: `caller` (caller.rs:1) `calls` `callee` (target.rs:1); two
    /// unconnected nodes `a`/`b` exist so importance is not degenerate.
    fn expand_fixture_graph() -> Graph {
        use discovery::graph::Node;
        let mut g = Graph::new();
        let caller = Node::new("caller.rs", "function", "caller", 1, "rust");
        let callee = Node::new("target.rs", "function", "callee", 1, "rust");
        let caller_id = caller.id.clone();
        let callee_id = callee.id.clone();
        g.add_node(caller);
        g.add_node(callee);
        g.add_node(Node::new("a.rs", "function", "a", 1, "rust"));
        g.add_node(Node::new("b.rs", "function", "b", 1, "rust"));
        g.add_edge(&caller_id, &callee_id, "calls");
        g
    }

    /// Flag OFF (default): `expand_hits` returns its input byte-identical, even when
    /// the graph is non-empty and hits align to nodes.
    #[test]
    fn expand_hits_off_is_identity() {
        let _lock = EXPAND_TEST_LOCK.lock().unwrap();
        std::env::remove_var("LENS_EXPAND");
        let graph = expand_fixture_graph();
        let hits = vec![
            SearchHit {
                path: "caller.rs".to_string(),
                snippet: "fn caller".to_string(),
                score: 3.0,
                line: Some(1),
            },
            SearchHit {
                path: "a.rs".to_string(),
                snippet: "fn a".to_string(),
                score: 1.0,
                line: Some(1),
            },
        ];
        let before = serde_json::to_string(&hits).unwrap();
        let out = expand_hits(hits, &graph, 5);
        assert_eq!(
            serde_json::to_string(&out).unwrap(),
            before,
            "LENS_EXPAND off must return the input hits unchanged"
        );
    }

    /// Flag ON: a lexical hit on `caller.rs` surfaces its 1-hop `calls` neighbor
    /// `target.rs`, while the original hit is preserved.
    #[test]
    fn expand_hits_on_adds_one_hop_neighbor() {
        let _lock = EXPAND_TEST_LOCK.lock().unwrap();
        let graph = expand_fixture_graph();
        let hits = vec![SearchHit {
            path: "caller.rs".to_string(),
            snippet: "fn caller".to_string(),
            score: 3.0,
            line: Some(1),
        }];
        std::env::set_var("LENS_EXPAND", "1");
        let out = expand_hits(hits, &graph, 5);
        std::env::remove_var("LENS_EXPAND"); // restore before asserting

        assert!(
            out.iter()
                .any(|h| h.path == "target.rs" && h.line == Some(1)),
            "LENS_EXPAND=1 must surface the 1-hop `calls` neighbor target.rs, got {:?}",
            out.iter()
                .map(|h| (h.path.clone(), h.line))
                .collect::<Vec<_>>()
        );
        assert!(
            out.iter().any(|h| h.path == "caller.rs"),
            "the original lexical hit must remain in the expanded set"
        );
    }

    /// Guard mirroring [`rrf_empty_map_or_off_is_byte_identical`], but at the
    /// `search_fused`/`SearchResponse` level (T2's `expand_hits_off_is_identity`
    /// already covers the bare `Vec<SearchHit>` level): a PRESENT, non-empty graph
    /// whose `auth.rs:1` node aligns to a real corpus hit and `calls` a distinct
    /// neighbor must still leave `LENS_EXPAND`-off output byte-identical to a
    /// graph-absent search (the production-risk case where `graph.json` exists but
    /// the flag stays off). The anchor node's `file` is the corpus file's
    /// canonicalized absolute path, matching what hits actually carry (verified: with
    /// this same fixture, `LENS_EXPAND=1` DOES surface the neighbor and diverge from
    /// baseline), so this guard is a real regression check, not a vacuous pass.
    #[test]
    fn expand_off_is_byte_identical() {
        let _lock = EXPAND_TEST_LOCK.lock().unwrap();
        // Default OFF (switch unset).
        std::env::remove_var("LENS_EXPAND");
        let data = tempdir().unwrap();
        let src = corpus();
        let idx = Index::open(data.path()).unwrap();
        idx.index_path(src.path(), true).unwrap();

        let queries = vec!["authenticate".to_string(), "pooling".to_string(), "add".to_string()];
        let file_ranks: HashMap<String, usize> = HashMap::new();
        let baseline = idx.search_fused(&queries, 5, &file_ranks, None).unwrap();

        // A graph whose `auth.rs:1` node ANCHORS the real "authenticate" hit and
        // `calls` a distinct `login.rs:1` neighbor: if expansion ran despite the flag
        // being off, the neighbor would appear and the assertions below would catch
        // it. Hits store the canonicalized absolute file path (index_path
        // canonicalizes its root before walking), so the node's `file` must match
        // that form, not a bare "auth.rs" relative name.
        let root = std::fs::canonicalize(src.path()).unwrap();
        let auth_path = root.join("auth.rs").to_string_lossy().into_owned();
        let login_path = root.join("login.rs").to_string_lossy().into_owned();

        use discovery::graph::Node;
        let mut graph = Graph::new();
        let anchor = Node::new(&auth_path, "function", "authenticate", 1, "rust");
        let neighbor = Node::new(&login_path, "function", "login", 1, "rust");
        let anchor_id = anchor.id.clone();
        let neighbor_id = neighbor.id.clone();
        graph.add_node(anchor);
        graph.add_node(neighbor);
        graph.add_edge(&anchor_id, &neighbor_id, "calls");

        // (i) A present, non-empty graph with the switch unset (default off) is still
        // byte-identical to the graph-absent baseline.
        let unset = idx
            .search_fused(&queries, 5, &file_ranks, Some(&graph))
            .unwrap();
        assert_eq!(
            serde_json::to_string(&baseline).unwrap(),
            serde_json::to_string(&unset).unwrap(),
            "a present non-empty graph with LENS_EXPAND unset must be byte-identical to a graph-absent search"
        );

        // (ii) An EXPLICIT switch-off ("0") with the same non-empty graph is still
        // byte-identical: it is the env kill switch, not merely the graph's absence,
        // that keeps expansion off.
        std::env::set_var("LENS_EXPAND", "0");
        let off = idx
            .search_fused(&queries, 5, &file_ranks, Some(&graph))
            .unwrap();
        // Restore BEFORE asserting so a failed assertion can't leak "0" into a later test.
        std::env::remove_var("LENS_EXPAND");
        let serialized_off = serde_json::to_string(&off).unwrap();
        assert_eq!(
            serde_json::to_string(&baseline).unwrap(),
            serialized_off,
            "LENS_EXPAND=0 must produce byte-identical output even with a present non-empty graph"
        );

        // Strengthened guard: this is the assertion that actually enforces "OFF ==
        // master shape" — the byte-identical checks above only prove `baseline` /
        // `unset` / `off` agree WITH EACH OTHER, not that any of them matches the
        // pre-L51 shape. Assert directly that LENS_EXPAND=0 output carries no "line"
        // key at all, and that every hit has exactly master's
        // `SearchHit { path, snippet, score }` keys, nothing more.
        assert!(
            !serialized_off.contains("\"line\""),
            "LENS_EXPAND=0 output must contain no \"line\" key, got {serialized_off}"
        );
        let off_value: serde_json::Value = serde_json::from_str(&serialized_off).unwrap();
        for result in off_value["results"].as_array().unwrap() {
            for hit in result["hits"].as_array().unwrap() {
                let mut keys: Vec<&str> = hit.as_object().unwrap().keys().map(|k| k.as_str()).collect();
                keys.sort();
                assert_eq!(
                    keys,
                    vec!["path", "score", "snippet"],
                    "LENS_EXPAND=0 hit must have exactly master's {{path, snippet, score}} shape, got {hit}"
                );
            }
        }
    }
}
