//! MCP server wiring: the `Forge` handler holds shared state and exposes every
//! lens tool. Tool bodies delegate to the feature modules.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};

use crate::darkroom;
use crate::discovery::{self, graph::Graph, query as gquery};
use crate::index::Index;
use crate::obs::{self, OpLog};
use crate::store::Store;
use crate::tools::*;

/// Default inline stdout limit (bytes) before offloading to the store.
const DEFAULT_MAX_INLINE: usize = 8 * 1024;

/// In-memory parsed-graph cache: the `source_manifest` mtime map the graph was
/// built from, paired with the graph itself. A cached entry is served only while
/// its manifest byte-equals a fresh walk, so any add/edit/remove forces a rebuild.
/// Adjacency is intentionally NOT cached: the two callers use different `keep`
/// filters (`neighbors` keeps all edges; `shortest_path` drops `contains`), so a
/// single cached adjacency could not serve both without changing results.
type GraphCache = Arc<RwLock<Option<(BTreeMap<String, u64>, Graph)>>>;

/// Per-file tree-sitter parse cache (path -> source/tree/extract), shared across
/// `Forge` clones. Drives the incremental rediscovery in `ensure_graph`: unchanged
/// files are reused, changed files re-parsed via tree-sitter's incremental parse.
/// Held behind its own lock (independent of `graph_cache`) and only ever touched by
/// the single-threaded `ensure_graph` rebuild path.
type ParseCache = Arc<RwLock<discovery::ParseCache>>;

/// Production default for the per-query staleness-walk debounce window, in ms. Overridable
/// with `LENS_WALK_DEBOUNCE_MS` (0 disables, restoring a walk on every call).
const DEFAULT_WALK_DEBOUNCE_MS: u64 = 1000;

/// Wall-clock debounce for the per-query staleness walk (`file_manifest` /
/// `source_manifest`, run by `ensure_index` / `load_graph`). Within `ttl` of the last
/// walk a caller skips the walk and treats the index/graph as fresh, so a burst of
/// queries does one walk instead of N. This bounds staleness: a file changed less than
/// `ttl` ago may not be reflected until the window passes; after it, the next call walks
/// and re-indexes as before. `ttl == 0` disables it (walk every call) -- `with_paths`
/// (and thus every test) uses 0 to keep the strict "an edit is reflected on the very next
/// call" behavior; `Forge::new` sets the production value from `LENS_WALK_DEBOUNCE_MS`.
/// The window is shared across `Forge` clones via `Arc`, so it is per-process, not
/// per-clone.
#[derive(Clone)]
pub struct WalkDebounce {
    ttl: std::time::Duration,
    last: Arc<std::sync::Mutex<Option<std::time::Instant>>>,
}

impl WalkDebounce {
    pub fn new(ttl: std::time::Duration) -> Self {
        Self {
            ttl,
            last: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// True if a walk happened within `ttl` (the caller may skip walking). Always false
    /// when `ttl` is zero.
    pub fn fresh(&self) -> bool {
        if self.ttl.is_zero() {
            return false;
        }
        self.last
            .lock()
            .map(|g| g.map(|t| t.elapsed() < self.ttl).unwrap_or(false))
            .unwrap_or(false)
    }

    /// Record that a walk just happened, (re)starting the debounce window.
    pub fn mark(&self) {
        if let Ok(mut g) = self.last.lock() {
            *g = Some(std::time::Instant::now());
        }
    }
}

#[derive(Clone)]
pub struct Forge {
    /// Working dir the darkroom and walkers operate in (the repo root).
    repo_dir: PathBuf,
    /// Persistent state dir (`.lens/` or `$LENS_DIR`).
    data_dir: PathBuf,
    /// Inline output threshold for `lens_run`.
    max_inline: usize,
    store: Store,
    index: Index,
    /// Always-on operation log (side channel; never touches tool payloads).
    ops: OpLog,
    /// Parsed-graph cache shared across every clone (`Forge` is `Arc`-cloned and
    /// its methods take `&self`). Filled lazily on a cache miss in `load_graph`,
    /// emptied on every rebuild in `finish_discovery`.
    graph_cache: GraphCache,
    /// Per-file tree-sitter parse cache driving incremental rediscovery in
    /// `ensure_graph`. Sibling of `graph_cache`, not a replacement: `graph_cache`
    /// skips the mtime-stable case entirely; this one makes the rebuild itself
    /// cheap by re-parsing only changed files.
    parse_cache: ParseCache,
    /// Debounce for the FTS index's per-query staleness walk (`ensure_index`).
    index_walk: WalkDebounce,
    /// Debounce for the code graph's per-query staleness walk (`load_graph`). Separate
    /// from `index_walk` so an index re-walk doesn't suppress a graph re-walk (and vice
    /// versa); each tracks its own freshness.
    graph_walk: WalkDebounce,
}

impl Forge {
    /// Build the handler, resolving paths from the environment.
    pub fn new() -> anyhow::Result<Self> {
        let repo_dir = std::env::current_dir()?;
        let data_dir = match std::env::var_os("LENS_DIR") {
            Some(d) => PathBuf::from(d),
            None => repo_dir.join(".lens"),
        };
        let max_inline = std::env::var("LENS_MAX_INLINE")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(DEFAULT_MAX_INLINE);
        let walk_ttl = std::env::var("LENS_WALK_DEBOUNCE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_WALK_DEBOUNCE_MS);
        let mut forge = Self::with_paths(repo_dir, data_dir, max_inline)?;
        let ttl = std::time::Duration::from_millis(walk_ttl);
        forge.index_walk = WalkDebounce::new(ttl);
        forge.graph_walk = WalkDebounce::new(ttl);
        Ok(forge)
    }

    /// Build a handler with explicit paths (used by tests).
    pub fn with_paths(
        repo_dir: PathBuf,
        data_dir: PathBuf,
        max_inline: usize,
    ) -> anyhow::Result<Self> {
        let store = Store::open(&data_dir)?;
        let index = Index::open(&data_dir)?;
        let ops = OpLog::open(&data_dir);
        Ok(Forge {
            repo_dir,
            data_dir,
            max_inline,
            store,
            index,
            ops,
            graph_cache: Arc::new(RwLock::new(None)),
            parse_cache: Arc::new(RwLock::new(discovery::ParseCache::new())),
            // Disabled by default: tests (and any with_paths caller) keep the strict
            // "an edit is reflected on the very next call" behavior. Production opts in
            // via Forge::new.
            index_walk: WalkDebounce::new(std::time::Duration::ZERO),
            graph_walk: WalkDebounce::new(std::time::Duration::ZERO),
        })
    }
}

#[tool_router]
impl Forge {
    /// Run code in a darkroom subprocess and capture only its stdout/stderr.
    /// The raw data the script reads never enters context. Large output is
    /// offloaded to the reversible store and replaced with a preview + ref.
    #[tool(
        description = "Run code (python|javascript|typescript|bash|ruby|go) in a darkroom; only the script's stdout/stderr returns to context, not the data it processed. Large output is offloaded and retrievable via lens_recall."
    )]
    async fn lens_run(
        &self,
        Parameters(req): Parameters<ExecuteRequest>,
    ) -> Result<Json<ExecuteResponse>, ErrorData> {
        let op = self.ops.start(
            "lens_run",
            serde_json::json!({ "language": req.language, "code_bytes": req.code.len() }),
        );
        match darkroom::run(req, &self.repo_dir, &self.store, self.max_inline).await {
            Ok(resp) => {
                let raw_in = resp.stdout_bytes as u64;
                let returned = (resp.stdout.len() + resp.stderr.len()) as u64;
                let outcome = if resp.timed_out { "timed_out" } else { "ok" };
                let note = if resp.timed_out {
                    "process killed on timeout"
                } else if resp.truncated {
                    "large stdout stored, head+tail returned"
                } else {
                    ""
                };
                let explain = self.ops.explain(|| {
                    let branch = if resp.truncated {
                        format!(
                            "stdout {} > inline cap {} → stored ref {}, returned head+tail",
                            resp.stdout_bytes,
                            self.max_inline,
                            resp.retrieve_ref.as_deref().unwrap_or("?")
                        )
                    } else {
                        format!("stdout {} ≤ inline cap {} → returned inline", resp.stdout_bytes, self.max_inline)
                    };
                    format!(
                        "{branch}; exit_code={} timed_out={}; returned {} bytes (stdout {} + stderr {})",
                        resp.exit_code,
                        resp.timed_out,
                        returned,
                        resp.stdout.len(),
                        resp.stderr.len()
                    )
                });
                op.finish(
                    raw_in,
                    returned,
                    resp.retrieve_ref.clone(),
                    outcome,
                    note,
                    explain,
                );
                Ok(Json(resp))
            }
            Err(e) => {
                op.finish(0, 0, None, "error", e.clone(), None);
                Err(ErrorData::internal_error(e, None))
            }
        }
    }

    /// Analyze a file in the darkroom: the code receives the file path as its
    /// first CLI argument and only its printed output returns to context. Large
    /// output is offloaded to the reversible store and replaced with a preview + ref.
    #[tool(
        description = "Analyze a file in the darkroom: your `code` receives the file path as its first CLI argument (python sys.argv[1] / node process.argv[2] / bash $1); only what it prints returns to context, not the file contents. Large output is offloaded and retrievable via lens_recall."
    )]
    async fn lens_run_file(
        &self,
        Parameters(req): Parameters<ExecuteFileRequest>,
    ) -> Result<Json<ExecuteResponse>, ErrorData> {
        let op = self.ops.start(
            "lens_run_file",
            serde_json::json!({ "language": req.language, "path": req.path, "code_bytes": req.code.len() }),
        );
        let p = self.resolve_unescaped(&req.path);
        // The file's bytes never enter context (the whole point of this tool over
        // Read), so they count as processed-but-saved: raw_in = file size + the
        // script's stdout. Without this a file analysis that prints a small answer
        // records raw == returned and zero savings.
        let file_size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        let exec = crate::tools::ExecuteRequest {
            language: req.language.clone(),
            code: req.code.clone(),
            timeout_secs: req.timeout_secs,
            stdin: None,
        };
        match darkroom::run_file(&p, exec, &self.repo_dir, &self.store, self.max_inline).await {
            Ok(resp) => {
                let raw_in = resp.stdout_bytes as u64 + file_size;
                // Mirror the credit into the persistent counter lens_stats reads
                // (the darkroom already counted stdout; add the file bytes).
                if file_size > 0 {
                    let _ = self
                        .store
                        .bump_stat("raw_bytes_processed", file_size as i64);
                }
                let returned = (resp.stdout.len() + resp.stderr.len()) as u64;
                let outcome = if resp.timed_out { "timed_out" } else { "ok" };
                let note = if resp.timed_out {
                    "process killed on timeout"
                } else if resp.truncated {
                    "large stdout stored, head+tail returned"
                } else {
                    ""
                };
                let explain = self.ops.explain(|| {
                    let branch = if resp.truncated {
                        format!(
                            "stdout {} > inline cap {} → stored ref {}, returned head+tail",
                            resp.stdout_bytes,
                            self.max_inline,
                            resp.retrieve_ref.as_deref().unwrap_or("?")
                        )
                    } else {
                        format!("stdout {} ≤ inline cap {} → returned inline", resp.stdout_bytes, self.max_inline)
                    };
                    format!(
                        "{branch}; exit_code={} timed_out={}; returned {} bytes (stdout {} + stderr {})",
                        resp.exit_code,
                        resp.timed_out,
                        returned,
                        resp.stdout.len(),
                        resp.stderr.len()
                    )
                });
                op.finish(
                    raw_in,
                    returned,
                    resp.retrieve_ref.clone(),
                    outcome,
                    note,
                    explain,
                );
                Ok(Json(resp))
            }
            Err(e) => {
                op.finish(0, 0, None, "error", e.clone(), None);
                Err(ErrorData::internal_error(e, None))
            }
        }
    }

    /// Skeletonize a source file: signatures + nesting, executable bodies elided,
    /// the full text stored so any body is one `lens_recall` away.
    #[tool(
        description = "Show a source file's structure cheaply: signatures, types, and nesting with executable bodies elided to `…`. Far fewer tokens than reading the whole file, and the full text is stored so any elided body is one lens_recall away (use the returned retrieve_ref). Use this when you need a file's API/shape; use Read when you must see or edit the bodies."
    )]
    async fn lens_skeleton(
        &self,
        Parameters(req): Parameters<SkeletonRequest>,
    ) -> Result<Json<SkeletonResponse>, ErrorData> {
        let op = self
            .ops
            .start("lens_skeleton", serde_json::json!({ "path": req.path }));
        let p = self.resolve_unescaped(&req.path);
        let content = match std::fs::read_to_string(&p) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("read {}: {e}", p.display());
                op.finish(0, 0, None, "error", msg.clone(), None);
                return Err(ErrorData::internal_error(msg, None));
            }
        };
        let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("");
        let Some(spec) = crate::discovery::extract::spec_for_extension(ext) else {
            let msg = format!(
                "no skeleton for {} (unsupported language '.{ext}'); use Read",
                p.display()
            );
            op.finish(0, 0, None, "error", msg.clone(), None);
            return Err(ErrorData::internal_error(msg, None));
        };
        let language = spec.name.to_string();
        let Some(skeleton) = crate::discovery::skeleton::skeletonize(&content, &spec) else {
            let msg = format!("could not parse {} for skeleton; use Read", p.display());
            op.finish(0, 0, None, "error", msg.clone(), None);
            return Err(ErrorData::internal_error(msg, None));
        };
        // Stash the full file so any elided body is recoverable; surface a cheap
        // short handle (Store::get resolves prefixes) instead of the 64-char hash.
        let reference = match self.store.put(&content) {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("store {}: {e}", p.display());
                op.finish(0, 0, None, "error", msg.clone(), None);
                return Err(ErrorData::internal_error(msg, None));
            }
        };
        let retrieve_ref = reference[..reference.len().min(12)].to_string();
        let raw_in = content.len() as u64;
        let returned = skeleton.len() as u64;
        // The file bytes were processed but kept out of context; credit the savings
        // counter lens_stats reads (mirrors lens_run_file's file-size credit).
        let _ = self.store.bump_stat("raw_bytes_processed", raw_in as i64);
        let explain = self.ops.explain(|| {
            format!(
                "skeletonized {} ({language}): {raw_in} -> {returned} bytes; full text at ref {retrieve_ref}",
                p.display()
            )
        });
        op.finish(
            raw_in,
            returned,
            Some(retrieve_ref.clone()),
            "ok",
            "",
            explain,
        );
        Ok(Json(SkeletonResponse {
            skeleton,
            language,
            retrieve_ref,
        }))
    }

    /// Fetch a full blob previously offloaded to the reversible store.
    #[tool(
        description = "Retrieve the full content for a retrieve_ref returned by another tool (reverses any truncation/compression)."
    )]
    async fn lens_recall(
        &self,
        Parameters(req): Parameters<RetrieveRequest>,
    ) -> Result<Json<RetrieveResponse>, ErrorData> {
        let op = self
            .ops
            .start("lens_recall", serde_json::json!({ "ref": req.reference }));
        match self.store.get(&req.reference) {
            Ok(Some(content)) => {
                // Retrieve is the inverse of offloading (expansion), so it saves
                // nothing: raw_in == returned keeps tokens_saved_est at 0.
                let bytes = content.len() as u64;
                let explain = self
                    .ops
                    .explain(|| format!("expanded ref {} to {} bytes", req.reference, bytes));
                op.finish(
                    bytes,
                    bytes,
                    Some(req.reference.clone()),
                    "ok",
                    "blob expanded from store",
                    explain,
                );
                Ok(Json(RetrieveResponse { content }))
            }
            Ok(None) => {
                op.finish(
                    0,
                    0,
                    Some(req.reference.clone()),
                    "error",
                    "unknown ref",
                    None,
                );
                Err(ErrorData::invalid_params(
                    format!("unknown ref '{}'", req.reference),
                    None,
                ))
            }
            Err(e) => {
                op.finish(0, 0, None, "error", e.to_string(), None);
                Err(ErrorData::internal_error(e.to_string(), None))
            }
        }
    }

    /// Index files into the FTS5 search index.
    #[tool(
        description = "Index a file or directory (respecting .gitignore) into an FTS5 index for fast snippet search via lens_search."
    )]
    async fn lens_index(
        &self,
        Parameters(req): Parameters<IndexRequest>,
    ) -> Result<Json<IndexResponse>, ErrorData> {
        let op = self.ops.start(
            "lens_index",
            serde_json::json!({ "path": req.path, "recursive": req.recursive }),
        );
        let root = self.resolve_unescaped(&req.path);
        match self.index.index_path(&root, req.recursive) {
            Ok(resp) => {
                if let Ok(total) = self.index.chunk_count() {
                    let _ = self.store.set_stat("index_chunks", total);
                }
                // A full-repo index refreshes the staleness manifest so a later
                // lens_search (ensure_index) serves from cache instead of reindexing.
                // Subpath/single-file indexes don't represent the whole repo, so they
                // leave the manifest alone (ensure_index will refresh as needed).
                if self.same_repo_root(&root) {
                    write_manifest(
                        &self.index_manifest_file(),
                        &crate::index::file_manifest(&self.repo_dir),
                    );
                }
                let returned = obs::json_len(&resp);
                let note = format!(
                    "indexed {} files, {} chunks",
                    resp.files_indexed, resp.chunks
                );
                let explain = self.ops.explain(|| note.clone());
                op.finish(returned, returned, None, "ok", note, explain);
                Ok(Json(resp))
            }
            Err(e) => {
                op.finish(0, 0, None, "error", e.to_string(), None);
                Err(ErrorData::internal_error(e.to_string(), None))
            }
        }
    }

    /// Search the FTS5 index with one or more queries.
    #[tool(
        description = "Search the FTS5 index with one or more queries (BM25-ranked); returns top snippets with path and score per query."
    )]
    async fn lens_search(
        &self,
        Parameters(req): Parameters<SearchRequest>,
    ) -> Result<Json<SearchResponse>, ErrorData> {
        let op = self.ops.start(
            "lens_search",
            serde_json::json!({ "queries": req.queries.len(), "limit_per_query": req.limit_per_query }),
        );
        if let Err(e) = self.ensure_index() {
            op.finish(0, 0, None, "error", "auto-index failed", None);
            return Err(e);
        }
        match self.index.search(&req.queries, req.limit_per_query) {
            Ok(resp) => {
                let returned = obs::json_len(&resp);
                let hits: usize = resp.results.iter().map(|r| r.hits.len()).sum();
                let note = format!("{} queries, {} hits", resp.results.len(), hits);
                let explain = self.ops.explain(|| note.clone());
                op.finish(returned, returned, None, "ok", note, explain);
                Ok(Json(resp))
            }
            Err(e) => {
                op.finish(0, 0, None, "error", e.to_string(), None);
                Err(ErrorData::internal_error(e.to_string(), None))
            }
        }
    }

    /// Build the structural code graph for the repo.
    #[tool(
        description = "Parse the repo with tree-sitter into a graph of symbols (functions, types, modules) and relationships (calls, imports, contains). Run once, then use lens_symbol/lens_links/lens_path."
    )]
    async fn lens_map(
        &self,
        Parameters(req): Parameters<DiscoverRequest>,
    ) -> Result<Json<DiscoverResponse>, ErrorData> {
        let op = self.ops.start(
            "lens_map",
            serde_json::json!({ "path": req.path, "languages": req.languages }),
        );
        let root = self.discover_root(&req.path);
        let langs = req.languages.as_deref();
        let outcome = match discovery::discover(&root, langs) {
            Ok(o) => o,
            Err(e) => {
                op.finish(0, 0, None, "error", e.to_string(), None);
                return Err(ErrorData::internal_error(e.to_string(), None));
            }
        };
        // The graph is persisted to graph.json, not returned to context; only the
        // summary counts come back. Shared with the lazy `ensure_graph` path.
        let resp = self.finish_discovery(op, outcome, false, &root)?;
        Ok(Json(resp))
    }

    /// Find symbols by name and return their immediate connections.
    #[tool(
        description = "Find graph symbols by name substring (+ optional kind) and return them with immediate connections. Large results are compacted with a lens_recall ref."
    )]
    async fn lens_symbol(
        &self,
        Parameters(req): Parameters<GraphQueryRequest>,
    ) -> Result<Json<GraphView>, ErrorData> {
        let op = self.ops.start(
            "lens_symbol",
            serde_json::json!({ "name": req.name, "kind": req.kind, "limit": req.limit }),
        );
        let graph = match self.load_graph() {
            Ok(g) => g,
            Err(e) => {
                op.finish(0, 0, None, "error", "graph build failed", None);
                return Err(e);
            }
        };
        // Session-proximity boost: symbols defined in files the user recently
        // touched sort first. Best-effort — an empty list leaves ranking unchanged.
        let recent = self.recent_touched_files();
        let view = gquery::query(&graph, &req.name, req.kind.as_deref(), req.limit, &recent);
        let raw_payload = view_payload_len(&view);
        let compacted = self.maybe_compact(view);
        self.record_graph_op(op, raw_payload, &compacted);
        Ok(Json(compacted))
    }

    /// Find symbols by natural-language meaning, ranked lexically (no embeddings).
    #[tool(
        description = "Find symbols by natural-language query, ranked lexically (no embeddings): tokenizes the query and scores symbol names by word overlap (exact > prefix > substring, with a bonus for multi-word hits). Returns the best matches with their immediate connections. Use when you know what a symbol does but not its exact name."
    )]
    async fn lens_find(
        &self,
        Parameters(req): Parameters<GraphFindRequest>,
    ) -> Result<Json<GraphView>, ErrorData> {
        let op = self.ops.start(
            "lens_find",
            serde_json::json!({ "query": req.query, "limit": req.limit }),
        );
        let graph = match self.load_graph() {
            Ok(g) => g,
            Err(e) => {
                op.finish(0, 0, None, "error", "graph build failed", None);
                return Err(e);
            }
        };
        let view = gquery::find(&graph, &req.query, req.limit);
        let raw_payload = view_payload_len(&view);
        let compacted = self.maybe_compact(view);
        self.record_graph_op(op, raw_payload, &compacted);
        Ok(Json(compacted))
    }

    /// Return the local subgraph around a node.
    #[tool(
        description = "Return the local subgraph within `depth` hops of a node id (from lens_symbol results)."
    )]
    async fn lens_links(
        &self,
        Parameters(req): Parameters<GraphNeighborsRequest>,
    ) -> Result<Json<GraphView>, ErrorData> {
        let op = self.ops.start(
            "lens_links",
            serde_json::json!({ "node_id": req.node_id, "depth": req.depth }),
        );
        let graph = match self.load_graph() {
            Ok(g) => g,
            Err(e) => {
                op.finish(0, 0, None, "error", "graph build failed", None);
                return Err(e);
            }
        };
        let view = gquery::neighbors(&graph, &req.node_id, req.depth);
        let raw_payload = view_payload_len(&view);
        let compacted = self.maybe_compact(view);
        self.record_graph_op(op, raw_payload, &compacted);
        Ok(Json(compacted))
    }

    /// Shortest path between two symbols.
    #[tool(
        description = "Find the shortest path between two symbols (by node id or name) via BFS over graph edges."
    )]
    async fn lens_path(
        &self,
        Parameters(req): Parameters<GraphPathRequest>,
    ) -> Result<Json<PathResponse>, ErrorData> {
        let op = self.ops.start(
            "lens_path",
            serde_json::json!({ "from": req.from, "to": req.to }),
        );
        let graph = match self.load_graph() {
            Ok(g) => g,
            Err(e) => {
                op.finish(0, 0, None, "error", "graph build failed", None);
                return Err(e);
            }
        };
        let resp = gquery::path(&graph, &req.from, &req.to);
        let returned = obs::json_len(&resp);
        let note = format!("found={}, hops={}", resp.found, resp.path.len());
        let explain = self.ops.explain(|| note.clone());
        op.finish(returned, returned, None, "ok", note, explain);
        Ok(Json(resp))
    }

    /// A token-budgeted map of the repo's most important symbols.
    #[tool(
        description = "Get a token-budgeted overview of the repo: the most structurally important symbols (PageRank-ranked) with their callers/callees, as much as fits a token budget (default 2000). A high-signal map of a codebase at fixed cost instead of reading files."
    )]
    async fn lens_overview(
        &self,
        Parameters(req): Parameters<OverviewRequest>,
    ) -> Result<Json<OverviewResponse>, ErrorData> {
        let op = self.ops.start(
            "lens_overview",
            serde_json::json!({ "token_budget": req.token_budget }),
        );
        let graph = match self.load_graph() {
            Ok(g) => g,
            Err(e) => {
                op.finish(0, 0, None, "error", "graph build failed", None);
                return Err(e);
            }
        };
        let overview = gquery::overview(&graph, req.token_budget);
        let resp = OverviewResponse { overview };
        let returned = obs::json_len(&resp);
        let note = format!("{} bytes", resp.overview.len());
        let explain = self.ops.explain(|| note.clone());
        op.finish(returned, returned, None, "ok", note, explain);
        Ok(Json(resp))
    }

    /// Structural (tree-sitter) search: run an AST query, get path:line matches.
    #[tool(
        description = "Structural code search via a tree-sitter query (S-expression): matches syntax, not text, so it finds e.g. real `.unwrap()` calls or functions returning Result without the false positives grep hits in comments/strings. Returns path:line matches."
    )]
    async fn lens_grep_ast(
        &self,
        Parameters(req): Parameters<GrepAstRequest>,
    ) -> Result<Json<GrepAstResponse>, ErrorData> {
        let op = self.ops.start(
            "lens_grep_ast",
            serde_json::json!({ "path": req.path, "language": req.language, "limit": req.limit }),
        );
        let root = self.resolve_unescaped(&req.path);
        match crate::discovery::structural::grep_ast(
            &root,
            &req.query,
            req.language.as_deref(),
            req.limit,
        ) {
            Ok(matches) => {
                let truncated = matches.len() >= req.limit;
                let resp = GrepAstResponse { matches, truncated };
                let returned = obs::json_len(&resp);
                let note = format!("{} matches", resp.matches.len());
                let explain = self.ops.explain(|| note.clone());
                op.finish(returned, returned, None, "ok", note, explain);
                Ok(Json(resp))
            }
            Err(e) => {
                op.finish(0, 0, None, "error", e.to_string(), None);
                Err(ErrorData::internal_error(e.to_string(), None))
            }
        }
    }

    /// Report token-savings counters and index/graph sizes.
    #[tool(
        description = "Report darkroom usage, estimated tokens saved, and index/graph sizes for this repo's lens state."
    )]
    async fn lens_stats(
        &self,
        Parameters(_): Parameters<EmptyRequest>,
    ) -> Result<Json<StatsResponse>, ErrorData> {
        let op = self.ops.start("lens_stats", serde_json::json!({}));
        let s = &self.store;
        let read = |k: &str| s.get_stat(k).unwrap_or(0);
        let raw = read("raw_bytes_processed");
        let returned = read("bytes_returned_to_context");
        let saved = ((raw - returned).max(0)) / 4;
        let resp = StatsResponse {
            darkroom_calls: read("darkroom_calls"),
            raw_bytes_processed: raw,
            bytes_returned_to_context: returned,
            estimated_tokens_saved: saved,
            index_chunks: read("index_chunks"),
            graph_nodes: read("graph_nodes"),
            graph_edges: read("graph_edges"),
        };
        let returned_bytes = obs::json_len(&resp);
        op.finish(
            returned_bytes,
            returned_bytes,
            None,
            "ok",
            "counters read",
            None,
        );
        Ok(Json(resp))
    }
}

#[tool_handler]
impl ServerHandler for Forge {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        let mut info = rmcp::model::ServerInfo::default();
        info.capabilities = rmcp::model::ServerCapabilities::builder()
            .enable_tools()
            .build();
        // Imperative tool-selection guidance. The MCP `instructions` ship on every
        // session handshake regardless of routing, so this is the always-on layer.
        info.instructions = Some(
            "lens keeps large tool output out of the model's context so it doesn't keep \
             costing tokens on every later turn. Work over data in code and return only \
             the result.\n\
             WRITE A SCRIPT INSTEAD OF READING THE DATA: to count, filter, search, parse, \
             reshape, or summarize anything, do it inside lens_run(language, code) and print \
             just the answer rather than pulling the raw input into context. One lens_run \
             usually stands in for a pile of Read/Grep/Bash calls.\n\
             PICKING A TOOL: (1) code structure (who calls what, imports, where a symbol is \
             defined, how A reaches B) → lens_map once, then lens_symbol / lens_links / \
             lens_path over the subgraph. (2) where X appears → lens_index, then \
             lens_search(queries). (3) an answer derived from data or a file → lens_run / \
             lens_run_file. (4) a file's structure/API without the bodies → lens_skeleton \
             (full text via lens_recall). (5) getting back something offloaded → lens_recall. \
             (6) savings so far → lens_stats.\n\
             WHEN PLAIN TOOLS ARE STILL RIGHT: use lens_run_file rather than Read to analyze a \
             file (Read is for when you'll Edit it); use lens_run rather than Grep/Bash when \
             you'll count or aggregate; fetch URLs through lens_run, not WebFetch. If a lens_* \
             tool reports not-found its schema isn't loaded — register it with ToolSearch and \
             retry. Plain Bash and Read stay correct for short output you just want to see, or \
             for changing state."
                .into(),
        );
        info
    }

    /// Stamp `anthropic/alwaysLoad` on every advertised tool so this server's tools
    /// are exempt from Claude Code's tool-search deferral (loaded into context at
    /// session start) with no host-side `.claude.json` edit. Overrides the
    /// `#[tool_handler]`-generated `list_tools` (the macro skips its own when we
    /// define one).
    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, ErrorData> {
        let mut tools = Self::tool_router().list_all();
        for tool in &mut tools {
            let mut meta = tool.meta.take().unwrap_or_default();
            meta.0.insert(
                "anthropic/alwaysLoad".to_string(),
                serde_json::Value::Bool(true),
            );
            tool.meta = Some(meta);
        }
        Ok(rmcp::model::ListToolsResult {
            tools,
            meta: None,
            next_cursor: None,
        })
    }
}

// Keep `data_dir` reachable for later phases (index/discovery) without dead-code warnings.
impl Forge {
    #[allow(dead_code)]
    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    /// Resolve a possibly-relative path against the repo working dir.
    fn resolve(&self, p: &str) -> PathBuf {
        let path = PathBuf::from(p);
        if path.is_absolute() {
            path
        } else {
            self.repo_dir.join(path)
        }
    }

    /// Resolve a model-supplied path, tolerant of the *shell-escaped* form the
    /// model often hands back for paths with spaces (e.g.
    /// `/Users/me/AI\ Stuff/repo`): strip the common `\<space>` escape, then
    /// resolve. Shared by `lens_map` and `lens_index` so the two never diverge
    /// in how they accept a path — that divergence is what let escaped `lens_index`
    /// calls silently index zero files while `lens_map` succeeded.
    fn resolve_unescaped(&self, p: &str) -> PathBuf {
        self.resolve(&p.replace("\\ ", " "))
    }

    /// Resolve the root for `lens_map`. The model's path is un-escaped by
    /// [`Self::resolve_unescaped`]; if it still doesn't exist, fall back to
    /// `repo_dir` (the repo root is what `lens_map` almost always means).
    fn discover_root(&self, p: &str) -> PathBuf {
        let candidate = self.resolve_unescaped(p);
        if candidate.exists() {
            candidate
        } else {
            self.repo_dir.clone()
        }
    }

    /// Path to the persisted structural graph file.
    fn graph_file(&self) -> PathBuf {
        self.data_dir.join("graph.json")
    }

    /// Staleness manifest for the graph (mtimes of supported source files).
    fn graph_manifest_file(&self) -> PathBuf {
        self.data_dir.join("graph.manifest.json")
    }

    /// Staleness manifest for the FTS index (mtimes of all indexed files).
    fn index_manifest_file(&self) -> PathBuf {
        self.data_dir.join("index.manifest.json")
    }

    /// True when `root` resolves to the repo root — i.e. a full-repo build whose
    /// graph is authoritative for the whole project (vs a narrower subpath build).
    fn same_repo_root(&self, root: &Path) -> bool {
        match (
            std::fs::canonicalize(root).ok(),
            std::fs::canonicalize(&self.repo_dir).ok(),
        ) {
            (Some(a), Some(b)) => a == b,
            _ => root == self.repo_dir.as_path(),
        }
    }

    /// Load the structural graph, building it on first use if discovery hasn't run
    /// yet (so graph queries work on any repo without an explicit lens_map).
    ///
    /// Repeated queries with no source change are served from the in-memory
    /// `graph_cache` (mtime walk only — no disk read, no deserialize). On a miss the
    /// disk path runs exactly as before (`ensure_graph` rebuilds-if-stale + persists,
    /// then load from `graph.json`), and the result repopulates the cache.
    ///
    /// Lock discipline: the cache is checked under a read lock that is dropped before
    /// any write lock is taken (never held across an acquire — so it cannot deadlock),
    /// and the write path re-checks under the write lock so a thundering herd does at
    /// most one rebuild. A cached entry is returned only when its manifest byte-equals
    /// the freshly walked `current`, and `finish_discovery` empties the cache on every
    /// rebuild, so a query after a rebuild never sees stale data.
    fn load_graph(&self) -> Result<Graph, ErrorData> {
        // Debounce: within the walk window of the last staleness check, skip the
        // gitignore walk and serve the cached graph (staleness bounded to the TTL).
        // Falls through to a full walk when the cache is empty or the debounce is off.
        if self.graph_walk.fresh() {
            let guard = match self.graph_cache.read() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some((_, graph)) = guard.as_ref() {
                return Ok(graph.clone());
            }
        }
        // The per-query staleness walk (stat-only): the change detector for the graph.
        let current = discovery::source_manifest(&self.repo_dir);
        self.graph_walk.mark();

        // Check under the read lock, then DROP it before doing anything else.
        if let Some(g) = self.cache_hit(&current) {
            return Ok(g);
        }

        // Miss: rebuild-if-stale on disk (unchanged behavior) and load.
        self.ensure_graph()?;
        let graph = Graph::load(&self.graph_file())
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        // Repopulate under the write lock, re-checking first so a concurrent miss
        // that already filled the cache for this manifest wins (no double store).
        self.cache_store(current, graph)
    }

    /// Read-locked cache probe: returns a clone of the cached graph iff its stored
    /// manifest equals `current`. The guard is released when this returns.
    fn cache_hit(&self, current: &BTreeMap<String, u64>) -> Option<Graph> {
        let guard = match self.graph_cache.read() {
            Ok(g) => g,
            // A poisoned lock means a prior panic while holding it; the stored tuple
            // is still internally consistent (we only ever write a matched pair), so
            // recover the guard rather than panic and wedge every later query.
            Err(p) => p.into_inner(),
        };
        match guard.as_ref() {
            Some((manifest, graph)) if manifest == current => Some(graph.clone()),
            _ => None,
        }
    }

    /// Write-locked cache store with a double-check: if another thread already
    /// populated the cache for `current` while we were rebuilding, serve theirs;
    /// otherwise store ours. Returns the graph to serve either way.
    fn cache_store(
        &self,
        current: BTreeMap<String, u64>,
        graph: Graph,
    ) -> Result<Graph, ErrorData> {
        let mut guard = match self.graph_cache.write() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some((manifest, cached)) = guard.as_ref() {
            if *manifest == current {
                return Ok(cached.clone());
            }
        }
        *guard = Some((current, graph.clone()));
        Ok(graph)
    }

    /// Empty the in-memory graph cache. Called after a rebuild persists a new graph
    /// so the next `load_graph` re-reads the now-authoritative disk graph instead of
    /// serving the pre-rebuild copy.
    fn invalidate_graph_cache(&self) {
        let mut guard = match self.graph_cache.write() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        *guard = None;
    }

    /// Paths the user recently touched this session, newest first (bounded), used
    /// to bias `lens_symbol` ranking toward what they're working on. Sourced from
    /// the session hooks' "file"-category events (Edit/Write/Read), whose payload
    /// carries the touched path. Best-effort: any error (no session db yet, schema
    /// drift) yields an empty list, leaving ranking unchanged.
    fn recent_touched_files(&self) -> Vec<String> {
        const MAX_RECENT: usize = 20;
        let db = self.data_dir.join("session.db");
        if !db.exists() {
            return Vec::new();
        }
        let read = || -> rusqlite::Result<Vec<String>> {
            let conn = rusqlite::Connection::open(&db)?;
            obs::configure_conn(&conn)?;
            let mut stmt = conn.prepare(
                "SELECT payload FROM session_events
                 WHERE category = 'file' ORDER BY id DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map([(MAX_RECENT * 4) as i64], |r| r.get::<_, String>(0))?;
            let mut seen: Vec<String> = Vec::new();
            for payload in rows.flatten() {
                let path = serde_json::from_str::<serde_json::Value>(&payload)
                    .ok()
                    .and_then(|v| v.get("path").and_then(|p| p.as_str()).map(String::from));
                if let Some(p) = path {
                    if !seen.contains(&p) {
                        seen.push(p);
                        if seen.len() >= MAX_RECENT {
                            break;
                        }
                    }
                }
            }
            Ok(seen)
        };
        read().unwrap_or_default()
    }

    /// Persist a freshly-built graph and its node/edge stats, then finalize `op`
    /// with a summary. Shared by `lens_map` (explicit, `root` may be a subpath)
    /// and `ensure_graph` (lazy, `root` == repo root); `auto` only adjusts the note.
    fn finish_discovery(
        &self,
        op: obs::OpHandle,
        outcome: discovery::DiscoverOutcome,
        auto: bool,
        root: &Path,
    ) -> Result<DiscoverResponse, ErrorData> {
        // Never persist an empty graph. A 0-file discover (a wrong/escaped path, a
        // `languages` filter that matches nothing, or a sourceless repo) would
        // otherwise overwrite a good graph.json with `{"nodes":[],"edges":[]}` and
        // silently break every later lens_symbol. Keep the existing graph instead.
        if outcome.response.files_parsed == 0 {
            let note =
                "discover parsed 0 files — kept the existing graph (check the path/languages)"
                    .to_string();
            let explain = self.ops.explain(|| note.clone());
            op.finish(0, 0, None, "error", note.clone(), explain);
            return Err(ErrorData::internal_error(note, None));
        }
        // Shrink guard: a narrower-scope discover (a subpath, not the repo root)
        // must never clobber a comprehensive graph with a smaller partial one — that
        // is what made the graph "bounce" across runs. A full-repo rebuild is
        // authoritative (deletions legitimately shrink it) and always proceeds.
        if !self.same_repo_root(root) {
            if let Ok(existing) = Graph::load(&self.graph_file()) {
                if !existing.nodes.is_empty() && outcome.response.nodes < existing.nodes.len() {
                    let note = format!(
                        "refused: discovering a subpath would shrink the graph {}→{} nodes; \
                         run lens_map with no path to rebuild the full repo",
                        existing.nodes.len(),
                        outcome.response.nodes
                    );
                    let explain = self.ops.explain(|| note.clone());
                    op.finish(0, 0, None, "error", note.clone(), explain);
                    return Err(ErrorData::internal_error(note, None));
                }
            }
        }
        if let Err(e) = outcome.graph.save(&self.graph_file()) {
            op.finish(0, 0, None, "error", e.to_string(), None);
            return Err(ErrorData::internal_error(e.to_string(), None));
        }
        // The persisted graph just changed, so the in-memory cache (which holds the
        // pre-rebuild graph) is now stale. Empty it AFTER the new graph is on disk so
        // the next load_graph miss reads the fresh graph, never the old one. This is
        // the guard that a query after a discovery rebuild returns fresh data.
        self.invalidate_graph_cache();
        // A full-repo build refreshes the staleness manifest so `ensure_graph` can
        // serve from cache until the next file change. Subpath builds don't represent
        // the whole repo, so they must not touch the manifest.
        if self.same_repo_root(root) {
            write_manifest(
                &self.graph_manifest_file(),
                &discovery::source_manifest(&self.repo_dir),
            );
        }
        let _ = self
            .store
            .set_stat("graph_nodes", outcome.response.nodes as i64);
        let _ = self
            .store
            .set_stat("graph_edges", outcome.response.edges as i64);
        let returned = obs::json_len(&outcome.response);
        let note = format!(
            "{}{} nodes, {} edges, {} files parsed",
            if auto { "auto-built: " } else { "" },
            outcome.response.nodes,
            outcome.response.edges,
            outcome.response.files_parsed
        );
        let explain = self.ops.explain(|| note.clone());
        op.finish(returned, returned, None, "ok", note, explain);
        Ok(outcome.response)
    }

    /// Rediscover the whole repo via the incremental parse cache, returning a
    /// `DiscoverOutcome` byte-identical to a full `discover`. Holds the parse-cache
    /// write lock for the duration (the rebuild is single-owner). Errors only on a
    /// poisoned lock or a non-existent root, in which case the caller falls back to
    /// a full rebuild.
    fn reparse_incremental(&self) -> Result<discovery::DiscoverOutcome, ErrorData> {
        let mut cache = self
            .parse_cache
            .write()
            .map_err(|_| ErrorData::internal_error("parse cache poisoned", None))?;
        let inc = discovery::discover_incremental(&self.repo_dir, None, &mut cache)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(discovery::DiscoverOutcome {
            graph: inc.graph,
            response: inc.response,
        })
    }

    /// Ensure `graph.json` is present AND current before a query. Rebuilds the
    /// whole-repo graph when it is missing, empty (a poisoned prior build), or
    /// **stale** — i.e. any source file was added, edited, or removed since the last
    /// build. Staleness is a cheap mtime-manifest comparison, so the graph keeps
    /// itself fresh as the user adds/removes files, with no explicit `lens_map`
    /// and no server restart. Works for every project (it is in the query path).
    fn ensure_graph(&self) -> Result<(), ErrorData> {
        let current = discovery::source_manifest(&self.repo_dir);
        if let Ok(g) = Graph::load(&self.graph_file()) {
            if !g.nodes.is_empty()
                && read_manifest(&self.graph_manifest_file()).as_ref() == Some(&current)
            {
                return Ok(()); // present and up to date
            }
        }
        let op = self
            .ops
            .start("lens_map", serde_json::json!({ "auto": true }));
        // Incremental rediscovery: re-parse only changed files, reuse cached extracts
        // for the rest, then assemble the graph by the SAME path `discover` uses — so
        // the result is byte-identical to a full rebuild. The repo-root + no-language
        // build here is exactly what the parse cache is keyed for. Any error (e.g. a
        // poisoned lock) falls back to a full from-scratch `discover`.
        let outcome = match self.reparse_incremental() {
            Ok(o) => o,
            Err(_) => match discovery::discover(&self.repo_dir, None) {
                Ok(o) => o,
                Err(e) => {
                    op.finish(0, 0, None, "error", e.to_string(), None);
                    return Err(ErrorData::internal_error(e.to_string(), None));
                }
            },
        };
        self.finish_discovery(op, outcome, true, &self.repo_dir)?;
        Ok(())
    }

    /// Ensure the FTS index is present AND current before a search. Reindexes the
    /// repo when it has never been indexed (gated on the `index_chunks` stat, which
    /// only code indexing writes — never session records) or when **stale**: any
    /// file added/edited/removed since the last build, via a cheap mtime manifest.
    /// Reindex is incremental: `index_path` reads only changed/new files, prunes
    /// chunks for deleted files internally (a separate prune walk is redundant), and
    /// leaves unchanged files untouched.
    fn ensure_index(&self) -> Result<(), ErrorData> {
        // Debounce: within the walk window of the last staleness check, skip the
        // gitignore walk and assume the index is fresh (staleness bounded to the TTL).
        if self.index_walk.fresh() {
            return Ok(());
        }
        let current = crate::index::file_manifest(&self.repo_dir);
        if self.store.get_stat("index_chunks").unwrap_or(0) > 0
            && read_manifest(&self.index_manifest_file()).as_ref() == Some(&current)
        {
            self.index_walk.mark();
            return Ok(());
        }
        let op = self
            .ops
            .start("lens_index", serde_json::json!({ "auto": true }));
        match self.index.index_path(&self.repo_dir, true) {
            Ok(resp) => {
                if let Ok(total) = self.index.chunk_count() {
                    let _ = self.store.set_stat("index_chunks", total);
                }
                write_manifest(&self.index_manifest_file(), &current);
                self.index_walk.mark();
                let returned = obs::json_len(&resp);
                let note = format!(
                    "auto-indexed {} files, {} chunks",
                    resp.files_indexed, resp.chunks
                );
                let explain = self.ops.explain(|| note.clone());
                op.finish(returned, returned, None, "ok", note, explain);
                Ok(())
            }
            Err(e) => {
                op.finish(0, 0, None, "error", e.to_string(), None);
                Err(ErrorData::internal_error(e.to_string(), None))
            }
        }
    }

    /// If a subgraph serializes larger than the inline limit, store the full
    /// (plain) JSON for `lens_recall` and return a dictionary-compacted form
    /// instead of the raw node/edge lists.
    fn maybe_compact(&self, view: GraphView) -> GraphView {
        let original = serde_json::json!({ "nodes": view.nodes, "edges": view.edges });
        // Serialize once and reuse the string for both the size gate and the store
        // put (it was serialized twice before).
        let serialized = original.to_string();
        if serialized.len() <= self.max_inline {
            return view;
        }
        let reference = self.store.put(&serialized).ok();
        let compact = crate::store::compress::compact_json(&original);
        GraphView {
            nodes: vec![],
            edges: vec![],
            compact: Some(compact),
            truncated: true,
            retrieve_ref: reference,
        }
    }

    /// Record a graph-query/neighbors op. When compaction fired, the raw input is
    /// the full subgraph payload and the offloaded ref is logged; otherwise the
    /// op simply returned the subgraph inline (no savings).
    fn record_graph_op(&self, op: obs::OpHandle, raw_payload: u64, view: &GraphView) {
        let returned = obs::json_len(view);
        let (raw_in, store_ref, note) = if view.truncated {
            (
                raw_payload,
                view.retrieve_ref.clone(),
                "subgraph compacted; full JSON stored",
            )
        } else {
            (returned, None, "")
        };
        let truncated = view.truncated;
        let explain = self.ops.explain(|| {
            format!(
                "subgraph payload {} bytes vs inline cap {} → {}",
                raw_payload,
                self.max_inline,
                if truncated {
                    "compacted + offloaded to store"
                } else {
                    "returned inline"
                }
            )
        });
        op.finish(raw_in, returned, store_ref, "ok", note, explain);
    }
}

/// Serialized size of a subgraph's `{nodes, edges}` payload before any compaction
/// (what it would have cost in context if returned raw).
fn view_payload_len(view: &GraphView) -> u64 {
    serde_json::json!({ "nodes": view.nodes, "edges": view.edges })
        .to_string()
        .len() as u64
}

/// Load a saved staleness manifest, or `None` if absent/unreadable (which forces
/// a rebuild — the safe default).
fn read_manifest(path: &Path) -> Option<BTreeMap<String, u64>> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

/// Persist a staleness manifest (best effort: a write failure just means the next
/// query rebuilds, which is harmless).
fn write_manifest(path: &Path, manifest: &BTreeMap<String, u64>) {
    if let Ok(json) = serde_json::to_string(manifest) {
        let _ = std::fs::write(path, json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::compress;
    use crate::tools::NodeView;
    use tempfile::tempdir;

    fn forge(max_inline: usize) -> (Forge, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let data = dir.path().join(".lens");
        let f = Forge::with_paths(dir.path().to_path_buf(), data, max_inline).unwrap();
        (f, dir)
    }

    /// A Forge over a temp repo containing one rust source file.
    fn forge_with_source() -> (Forge, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "fn helper() -> i32 { 1 }\nfn main() { let _ = helper(); }\n",
        )
        .unwrap();
        let data = dir.path().join(".lens");
        let f = Forge::with_paths(dir.path().to_path_buf(), data, 8192).unwrap();
        (f, dir)
    }

    #[tokio::test]
    async fn escaped_path_discover_falls_back_and_does_not_clobber() {
        let (f, dir) = forge_with_source();
        // Good graph via the explicit tool (default path ".").
        let good = f
            .lens_map(Parameters(DiscoverRequest {
                path: ".".into(),
                languages: None,
            }))
            .await
            .unwrap();
        let before = good.0.nodes;
        assert!(before > 0, "baseline graph should be non-empty");

        // A shell-escaped / nonexistent absolute path must fall back to repo_dir
        // (rebuilding the same graph) rather than walking nothing and clobbering it.
        let escaped = format!("{}\\ nonexistent", dir.path().display());
        let _ = f
            .lens_map(Parameters(DiscoverRequest {
                path: escaped,
                languages: None,
            }))
            .await;
        let after = Graph::load(&f.graph_file()).unwrap().nodes.len();
        assert_eq!(after, before, "graph must not be clobbered by a bad path");
    }

    #[tokio::test]
    async fn zero_file_discover_errors_and_keeps_graph() {
        let (f, _dir) = forge_with_source();
        let good = f
            .lens_map(Parameters(DiscoverRequest {
                path: ".".into(),
                languages: None,
            }))
            .await
            .unwrap();
        let before = good.0.nodes;
        assert!(before > 0);

        // An unsupported `languages` filter matches nothing → 0 files parsed. That
        // must error and leave the existing graph untouched (never persist empty).
        let res = f
            .lens_map(Parameters(DiscoverRequest {
                path: ".".into(),
                languages: Some(vec!["swift".into()]),
            }))
            .await;
        assert!(res.is_err(), "0-file discover should error, not succeed");
        let after = Graph::load(&f.graph_file()).unwrap().nodes.len();
        assert_eq!(
            after, before,
            "graph must be preserved on a 0-file discover"
        );
    }

    #[tokio::test]
    async fn empty_graph_self_heals_on_query() {
        let (f, _dir) = forge_with_source();
        // Poison the data dir with an empty graph (the bug's end state).
        std::fs::create_dir_all(f.graph_file().parent().unwrap()).unwrap();
        std::fs::write(f.graph_file(), r#"{"nodes":[],"edges":[]}"#).unwrap();
        assert!(Graph::load(&f.graph_file()).unwrap().nodes.is_empty());

        // lens_symbol triggers ensure_graph, which must rebuild because nodes == 0.
        let _ = f
            .lens_symbol(Parameters(GraphQueryRequest {
                name: "helper".into(),
                kind: None,
                limit: 20,
            }))
            .await
            .unwrap();
        assert!(
            !Graph::load(&f.graph_file()).unwrap().nodes.is_empty(),
            "an empty graph.json must self-heal on the next graph query"
        );
    }

    #[tokio::test]
    async fn graph_refreshes_when_a_file_is_added() {
        // Every-project freshness: after a full build, adding a source file must make
        // the next lens_symbol auto-rebuild (manifest goes stale) — no explicit
        // lens_map, no restart.
        let (f, dir) = forge_with_source();
        f.lens_map(Parameters(DiscoverRequest {
            path: ".".into(),
            languages: None,
        }))
        .await
        .unwrap();
        let before = Graph::load(&f.graph_file()).unwrap().nodes.len();

        std::fs::write(
            dir.path().join("extra.rs"),
            "fn brand_new_symbol() -> i32 { 7 }\n",
        )
        .unwrap();
        let view = f
            .lens_symbol(Parameters(GraphQueryRequest {
                name: "brand_new_symbol".into(),
                kind: None,
                limit: 20,
            }))
            .await
            .unwrap();
        assert!(
            view.0.nodes.iter().any(|n| n.name == "brand_new_symbol"),
            "added symbol must appear after auto-refresh"
        );
        assert!(
            Graph::load(&f.graph_file()).unwrap().nodes.len() > before,
            "graph should grow after a file is added"
        );
    }

    #[tokio::test]
    async fn subpath_discover_refused_when_it_would_shrink() {
        // The shrink guard: a narrower subpath discover must not clobber a bigger
        // full-repo graph (the "bouncing" bug). Full-repo builds are unaffected.
        let (f, dir) = forge_with_source();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/only.rs"), "fn lonely() {}\n").unwrap();
        std::fs::write(dir.path().join("more.rs"), "fn a(){}\nfn b(){}\nfn c(){}\n").unwrap();

        f.lens_map(Parameters(DiscoverRequest {
            path: ".".into(),
            languages: None,
        }))
        .await
        .unwrap();
        let full = Graph::load(&f.graph_file()).unwrap().nodes.len();

        let res = f
            .lens_map(Parameters(DiscoverRequest {
                path: "sub".into(),
                languages: None,
            }))
            .await;
        assert!(
            res.is_err(),
            "subpath discover that shrinks must be refused"
        );
        assert_eq!(
            Graph::load(&f.graph_file()).unwrap().nodes.len(),
            full,
            "graph must be preserved when a shrink is refused"
        );
    }

    #[tokio::test]
    async fn index_refreshes_when_a_file_is_added() {
        // Index freshness via lens_search → ensure_index: a newly added file becomes
        // searchable on the next search, no explicit lens_index.
        let (f, dir) = forge_with_source();
        f.lens_search(Parameters(SearchRequest {
            queries: vec!["helper".into()],
            limit_per_query: 5,
        }))
        .await
        .unwrap();

        std::fs::write(
            dir.path().join("notes.md"),
            "# Topic\nqwerty_unique_term appears here\n",
        )
        .unwrap();
        let r = f
            .lens_search(Parameters(SearchRequest {
                queries: vec!["qwerty_unique_term".into()],
                limit_per_query: 5,
            }))
            .await
            .unwrap();
        assert!(
            r.0.results[0]
                .hits
                .iter()
                .any(|h| h.path.contains("notes.md")),
            "new file must be searchable after auto-reindex"
        );
    }

    #[tokio::test]
    async fn index_prunes_deleted_files() {
        // A deleted file must stop appearing in lens_search after the next search
        // (ensure_index reindexes + prunes). Driven via lens_search so the index uses
        // the clean repo_dir path scheme throughout.
        let (f, dir) = forge_with_source();
        std::fs::write(dir.path().join("gone.rs"), "fn vanishing_term() {}\n").unwrap();

        let pre = f
            .lens_search(Parameters(SearchRequest {
                queries: vec!["vanishing_term".into()],
                limit_per_query: 5,
            }))
            .await
            .unwrap();
        assert!(
            pre.0.results[0]
                .hits
                .iter()
                .any(|h| h.path.contains("gone.rs")),
            "term should be searchable before deletion"
        );

        std::fs::remove_file(dir.path().join("gone.rs")).unwrap();
        let post = f
            .lens_search(Parameters(SearchRequest {
                queries: vec!["vanishing_term".into()],
                limit_per_query: 5,
            }))
            .await
            .unwrap();
        assert!(
            !post.0.results[0]
                .hits
                .iter()
                .any(|h| h.path.contains("gone.rs")),
            "deleted file must be pruned from the index"
        );
    }

    #[test]
    fn walk_debounce_fresh_and_disabled_semantics() {
        // ttl=0 is always-walk (never fresh); a positive ttl reports fresh only after a
        // mark and only within the window.
        let off = WalkDebounce::new(std::time::Duration::ZERO);
        off.mark();
        assert!(!off.fresh(), "ttl=0 must never report fresh");
        let on = WalkDebounce::new(std::time::Duration::from_secs(60));
        assert!(!on.fresh(), "no walk yet => not fresh");
        on.mark();
        assert!(on.fresh(), "within window after a walk => fresh");
    }

    #[tokio::test]
    async fn walk_debounce_bounds_staleness_then_refreshes() {
        // With a short walk-debounce window, a file added inside the window is not yet
        // reflected (the walk is skipped), but after the window the next search walks,
        // re-indexes, and reflects it. Proves bounded staleness, not lost updates.
        let (mut f, dir) = forge_with_source();
        f.index_walk = WalkDebounce::new(std::time::Duration::from_millis(150));

        // First search walks, indexes, and opens the debounce window.
        f.lens_search(Parameters(SearchRequest {
            queries: vec!["helper".into()],
            limit_per_query: 5,
        }))
        .await
        .unwrap();

        std::fs::write(
            dir.path().join("late.md"),
            "# T\nzzz_unique_token lives here\n",
        )
        .unwrap();

        // Inside the window: the walk is skipped, so the new file is not yet searchable.
        let within = f
            .lens_search(Parameters(SearchRequest {
                queries: vec!["zzz_unique_token".into()],
                limit_per_query: 5,
            }))
            .await
            .unwrap();
        assert!(
            within.0.results[0].hits.is_empty(),
            "inside the debounce window the new file is not yet indexed (bounded staleness)"
        );

        // After the window: the next search walks and reflects the change.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let after = f
            .lens_search(Parameters(SearchRequest {
                queries: vec!["zzz_unique_token".into()],
                limit_per_query: 5,
            }))
            .await
            .unwrap();
        assert!(
            after.0.results[0]
                .hits
                .iter()
                .any(|h| h.path.contains("late.md")),
            "after the window the change must be reflected"
        );
    }

    #[tokio::test]
    async fn skeleton_elides_bodies_and_recovers_full_file() {
        let base = tempdir().unwrap();
        let repo = base.path().to_path_buf();
        let file = repo.join("widget.rs");
        let full = "pub fn render(x: i32) -> String {\n    let y = compute(x);\n    format!(\"{y}\")\n}\n";
        std::fs::write(&file, full).unwrap();
        let f = Forge::with_paths(repo.clone(), base.path().join(".lens"), 8192).unwrap();

        let resp = f
            .lens_skeleton(Parameters(crate::tools::SkeletonRequest {
                path: file.display().to_string(),
            }))
            .await
            .unwrap()
            .0;
        // Signature survives, executable body is elided.
        assert!(
            resp.skeleton.contains("pub fn render"),
            "skeleton dropped the signature: {}",
            resp.skeleton
        );
        assert!(
            !resp.skeleton.contains("compute(x)"),
            "body leaked into skeleton: {}",
            resp.skeleton
        );
        assert_eq!(resp.language, "rust");
        // The cheap handle recovers the exact original file via lens_recall.
        assert!(resp.retrieve_ref.len() <= 12, "ref not short: {}", resp.retrieve_ref);
        let recalled = f
            .lens_recall(Parameters(crate::tools::RetrieveRequest {
                reference: resp.retrieve_ref.clone(),
            }))
            .await
            .unwrap()
            .0;
        assert_eq!(recalled.content, full, "recall did not return the full file");
    }

    #[tokio::test]
    async fn escaped_path_index_unescapes_and_indexes() {
        // The meridian case: a repo whose path contains a space, where the model
        // hands lens_index a shell-escaped absolute path (`AI\ Stuff`). Index must
        // un-escape it and actually index the files — not report 0, not error.
        let base = tempdir().unwrap();
        let spaced = base.path().join("AI Stuff");
        std::fs::create_dir_all(&spaced).unwrap();
        std::fs::write(spaced.join("lib.rs"), "fn helper() -> i32 { 1 }\n").unwrap();
        let f = Forge::with_paths(spaced.clone(), base.path().join(".lens"), 8192).unwrap();

        let escaped = spaced.display().to_string().replace(' ', "\\ ");
        let resp = f
            .lens_index(Parameters(IndexRequest {
                path: escaped,
                recursive: true,
            }))
            .await
            .unwrap();
        assert!(
            resp.0.files_indexed >= 1,
            "escaped path must un-escape and index files, got {}",
            resp.0.files_indexed
        );
        assert!(resp.0.chunks >= 1);
    }

    #[tokio::test]
    async fn escaped_path_execute_file_unescapes() {
        // Same root cause as discover/index: a shell-escaped path to a real file in
        // a spaced dir must resolve so the darkroom script actually reads the file.
        let base = tempdir().unwrap();
        let spaced = base.path().join("AI Stuff");
        std::fs::create_dir_all(&spaced).unwrap();
        let file = spaced.join("data.txt");
        std::fs::write(&file, "hello\n").unwrap();
        let f = Forge::with_paths(spaced.clone(), base.path().join(".lens"), 8192).unwrap();

        let escaped = file.display().to_string().replace(' ', "\\ ");
        let resp = f
            .lens_run_file(Parameters(crate::tools::ExecuteFileRequest {
                path: escaped,
                language: "bash".into(),
                code: "wc -c < \"$1\"".into(),
                timeout_secs: 30,
            }))
            .await
            .unwrap();
        // 6 bytes ("hello\n"): proves the script read the real file via the
        // un-escaped path (a broken path would leave stdout empty).
        assert_eq!(resp.0.stdout.trim(), "6", "stdout: {:?}", resp.0.stdout);
    }

    #[test]
    fn small_subgraph_returned_inline() {
        let (f, _d) = forge(8192);
        let view = GraphView {
            nodes: vec![NodeView {
                id: "x".into(),
                name: "a".into(),
                kind: "function".into(),
                file: "f.rs".into(),
                line: 1,
                language: "rust".into(),
            }],
            edges: vec![],
            compact: None,
            truncated: false,
            retrieve_ref: None,
        };
        let out = f.maybe_compact(view);
        assert!(!out.truncated);
        assert_eq!(out.nodes.len(), 1);
        assert!(out.compact.is_none());
    }

    #[test]
    fn large_subgraph_compacts_with_working_ref() {
        // Tiny inline limit forces compaction.
        let (f, _d) = forge(64);
        let nodes: Vec<NodeView> = (0..30)
            .map(|i| NodeView {
                id: format!("id{i}"),
                name: format!("symbol_number_{i}"),
                kind: "function".into(),
                file: "src/discovery/extract.rs".into(),
                line: i,
                language: "rust".into(),
            })
            .collect();
        let original = serde_json::json!({ "nodes": nodes, "edges": [] });
        let view = GraphView {
            nodes,
            edges: vec![],
            compact: None,
            truncated: false,
            retrieve_ref: None,
        };
        let out = f.maybe_compact(view);
        assert!(out.truncated);
        assert!(out.nodes.is_empty());
        let compact = out.compact.expect("compact form");
        // compact expands back to the original.
        assert_eq!(compress::expand_json(&compact), original);
        // ref retrieves the plain original JSON.
        let reference = out.retrieve_ref.expect("ref");
        let stored = f.store.get(&reference).unwrap().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(parsed, original);
    }

    /// T8 invariant: the in-memory graph cache must never serve stale data across a
    /// discovery rebuild, and an unchanged repeated query must be served from cache.
    ///
    /// 1. Query once -> cache populated.
    /// 2. Add a source file -> next query auto-rebuilds (ensure_graph -> finish_discovery),
    ///    which invalidates the cache; the query must reflect the NEW graph (new symbol
    ///    present AND the original symbol still present, proving a fresh full rebuild,
    ///    not a stale partial).
    /// 3. Repeated query with no change -> served from cache: proven by deleting
    ///    graph.json after the cache is warm and asserting the query still resolves
    ///    the symbol (source_manifest excludes the non-source graph.json, so the cache
    ///    key is unchanged -> a hit that never touches disk).
    #[tokio::test]
    async fn graph_cache_invalidates_on_rebuild_and_serves_hits() {
        let (f, dir) = forge_with_source();

        // (1) First query populates the cache.
        let first = f
            .lens_symbol(Parameters(GraphQueryRequest {
                name: "helper".into(),
                kind: None,
                limit: 20,
            }))
            .await
            .unwrap();
        assert!(first.0.nodes.iter().any(|n| n.name == "helper"));
        {
            let guard = f.graph_cache.read().unwrap();
            assert!(guard.is_some(), "cache must be populated after first query");
        }

        // (2) Add a file: the manifest goes stale, so the next query rebuilds and the
        // cache is invalidated mid-rebuild. The result must be FRESH, not the cached
        // pre-rebuild graph.
        std::fs::write(
            dir.path().join("added.rs"),
            "fn freshly_added_symbol() -> i32 { 42 }\n",
        )
        .unwrap();
        let after_add = f
            .lens_symbol(Parameters(GraphQueryRequest {
                name: "freshly_added_symbol".into(),
                kind: None,
                limit: 20,
            }))
            .await
            .unwrap();
        assert!(
            after_add
                .0
                .nodes
                .iter()
                .any(|n| n.name == "freshly_added_symbol"),
            "query after a rebuild must see the NEW symbol, not the stale cached graph"
        );
        // The original symbol must still be present: a fresh FULL rebuild, not a
        // partial that dropped what was there.
        let still_helper = f
            .lens_symbol(Parameters(GraphQueryRequest {
                name: "helper".into(),
                kind: None,
                limit: 20,
            }))
            .await
            .unwrap();
        assert!(
            still_helper.0.nodes.iter().any(|n| n.name == "helper"),
            "the original symbol must survive the rebuild"
        );

        // (3) No change since the last query -> must serve from cache. Delete the
        // on-disk graph; a cache hit (manifest unchanged) resolves the symbol with no
        // disk read. If it fell through to disk it would error / find nothing.
        assert!(f.graph_file().exists(), "graph.json should exist while warm");
        std::fs::remove_file(f.graph_file()).unwrap();
        let from_cache = f
            .lens_symbol(Parameters(GraphQueryRequest {
                name: "freshly_added_symbol".into(),
                kind: None,
                limit: 20,
            }))
            .await
            .unwrap();
        assert!(
            from_cache
                .0
                .nodes
                .iter()
                .any(|n| n.name == "freshly_added_symbol"),
            "an unchanged repeated query must be served from the in-memory cache \
             (it resolved the symbol even with graph.json deleted)"
        );
    }

    /// Measured (not a gate): cold load_graph (cache miss: walk + ensure_graph +
    /// deserialize graph.json) vs warm (cache hit: mtime walk only). Reports the
    /// speedup ratio so the T8 win is visible. Run with `--nocapture` to see it.
    #[tokio::test]
    async fn measure_cold_vs_warm_load_graph() {
        use std::time::Instant;
        let (f, _dir) = forge_with_source();

        // Build the graph and warm the cache once (so the cold timing below is a pure
        // miss: drop the cache, then time the rebuild-from-disk path).
        f.lens_symbol(Parameters(GraphQueryRequest {
            name: "helper".into(),
            kind: None,
            limit: 20,
        }))
        .await
        .unwrap();

        // Cold: invalidate so load_graph misses and reloads + deserializes from disk.
        let cold = {
            f.invalidate_graph_cache();
            let t = Instant::now();
            let _ = f.load_graph().unwrap();
            t.elapsed()
        };

        // Warm: cache is now populated; subsequent loads are mtime-walk + clone only.
        // Average a few to smooth scheduler noise.
        let runs = 50u32;
        let t = Instant::now();
        for _ in 0..runs {
            let _ = f.load_graph().unwrap();
        }
        let warm = t.elapsed() / runs;

        let ratio = cold.as_secs_f64() / warm.as_secs_f64().max(f64::MIN_POSITIVE);
        println!(
            "[T8 measured] cold load_graph (miss, reads+deserializes graph.json) = {:?}; \
             warm (hit, mtime walk only) = {:?}; cold/warm ratio = {:.1}x",
            cold, warm, ratio
        );
        // Sanity only (not a perf gate): warm must not be slower than cold.
        assert!(warm <= cold, "warm ({warm:?}) should not exceed cold ({cold:?})");
    }
}
