//! MCP server wiring: the `Forge` handler holds shared state and exposes every
//! ctxforge tool. Tool bodies delegate to the feature modules.

use std::path::PathBuf;

use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};

use crate::discovery::{self, graph::Graph, query as gquery};
use crate::index::Index;
use crate::obs::{self, OpLog};
use crate::sandbox;
use crate::store::Store;
use crate::tools::*;

/// Default inline stdout limit (bytes) before offloading to the store.
const DEFAULT_MAX_INLINE: usize = 8 * 1024;

#[derive(Clone)]
pub struct Forge {
    /// Working dir the sandbox and walkers operate in (the repo root).
    repo_dir: PathBuf,
    /// Persistent state dir (`.ctxforge/` or `$CTXFORGE_DIR`).
    data_dir: PathBuf,
    /// Inline output threshold for `ctx_execute`.
    max_inline: usize,
    store: Store,
    index: Index,
    /// Always-on operation log (side channel; never touches tool payloads).
    ops: OpLog,
}

impl Forge {
    /// Build the handler, resolving paths from the environment.
    pub fn new() -> anyhow::Result<Self> {
        let repo_dir = std::env::current_dir()?;
        let data_dir = match std::env::var_os("CTXFORGE_DIR") {
            Some(d) => PathBuf::from(d),
            None => repo_dir.join(".ctxforge"),
        };
        let max_inline = std::env::var("CTXFORGE_MAX_INLINE")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(DEFAULT_MAX_INLINE);
        Self::with_paths(repo_dir, data_dir, max_inline)
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
        })
    }
}

#[tool_router]
impl Forge {
    /// Run code in a sandboxed subprocess and capture only its stdout/stderr.
    /// The raw data the script reads never enters context. Large output is
    /// offloaded to the reversible store and replaced with a preview + ref.
    #[tool(
        description = "Run code (python|javascript|typescript|bash|ruby|go) in a sandbox; only the script's stdout/stderr returns to context, not the data it processed. Large output is offloaded and retrievable via ctx_retrieve."
    )]
    async fn ctx_execute(
        &self,
        Parameters(req): Parameters<ExecuteRequest>,
    ) -> Result<Json<ExecuteResponse>, ErrorData> {
        let op = self.ops.start(
            "ctx_execute",
            serde_json::json!({ "language": req.language, "code_bytes": req.code.len() }),
        );
        match sandbox::run(req, &self.repo_dir, &self.store, self.max_inline).await {
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
                op.finish(raw_in, returned, resp.retrieve_ref.clone(), outcome, note, explain);
                Ok(Json(resp))
            }
            Err(e) => {
                op.finish(0, 0, None, "error", e.clone(), None);
                Err(ErrorData::internal_error(e, None))
            }
        }
    }

    /// Analyze a file in the sandbox: the code receives the file path as its
    /// first CLI argument and only its printed output returns to context. Large
    /// output is offloaded to the reversible store and replaced with a preview + ref.
    #[tool(
        description = "Analyze a file in the sandbox: your `code` receives the file path as its first CLI argument (python sys.argv[1] / node process.argv[2] / bash $1); only what it prints returns to context, not the file contents. Large output is offloaded and retrievable via ctx_retrieve."
    )]
    async fn ctx_execute_file(
        &self,
        Parameters(req): Parameters<ExecuteFileRequest>,
    ) -> Result<Json<ExecuteResponse>, ErrorData> {
        let op = self.ops.start(
            "ctx_execute_file",
            serde_json::json!({ "language": req.language, "path": req.path, "code_bytes": req.code.len() }),
        );
        let p = self.resolve(&req.path);
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
        match sandbox::run_file(&p, exec, &self.repo_dir, &self.store, self.max_inline).await {
            Ok(resp) => {
                let raw_in = resp.stdout_bytes as u64 + file_size;
                // Mirror the credit into the persistent counter ctx_stats reads
                // (the sandbox already counted stdout; add the file bytes).
                if file_size > 0 {
                    let _ = self.store.bump_stat("raw_bytes_processed", file_size as i64);
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
                op.finish(raw_in, returned, resp.retrieve_ref.clone(), outcome, note, explain);
                Ok(Json(resp))
            }
            Err(e) => {
                op.finish(0, 0, None, "error", e.clone(), None);
                Err(ErrorData::internal_error(e, None))
            }
        }
    }

    /// Fetch a full blob previously offloaded to the reversible store.
    #[tool(
        description = "Retrieve the full content for a retrieve_ref returned by another tool (reverses any truncation/compression)."
    )]
    async fn ctx_retrieve(
        &self,
        Parameters(req): Parameters<RetrieveRequest>,
    ) -> Result<Json<RetrieveResponse>, ErrorData> {
        let op = self
            .ops
            .start("ctx_retrieve", serde_json::json!({ "ref": req.reference }));
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
                op.finish(0, 0, Some(req.reference.clone()), "error", "unknown ref", None);
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
        description = "Index a file or directory (respecting .gitignore) into an FTS5 index for fast snippet search via ctx_search."
    )]
    async fn ctx_index(
        &self,
        Parameters(req): Parameters<IndexRequest>,
    ) -> Result<Json<IndexResponse>, ErrorData> {
        let op = self.ops.start(
            "ctx_index",
            serde_json::json!({ "path": req.path, "recursive": req.recursive }),
        );
        let root = self.resolve(&req.path);
        match self.index.index_path(&root, req.recursive) {
            Ok(resp) => {
                if let Ok(total) = self.index.chunk_count() {
                    let _ = self.store.set_stat("index_chunks", total);
                }
                let returned = obs::json_len(&resp);
                let note = format!("indexed {} files, {} chunks", resp.files_indexed, resp.chunks);
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
    async fn ctx_search(
        &self,
        Parameters(req): Parameters<SearchRequest>,
    ) -> Result<Json<SearchResponse>, ErrorData> {
        let op = self.ops.start(
            "ctx_search",
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
        description = "Parse the repo with tree-sitter into a graph of symbols (functions, types, modules) and relationships (calls, imports, contains). Run once, then use graph_query/graph_neighbors/graph_path."
    )]
    async fn ctx_discover(
        &self,
        Parameters(req): Parameters<DiscoverRequest>,
    ) -> Result<Json<DiscoverResponse>, ErrorData> {
        let op = self.ops.start(
            "ctx_discover",
            serde_json::json!({ "path": req.path, "languages": req.languages }),
        );
        let root = self.resolve(&req.path);
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
        let resp = self.finish_discovery(op, outcome, false)?;
        Ok(Json(resp))
    }

    /// Find symbols by name and return their immediate connections.
    #[tool(
        description = "Find graph symbols by name substring (+ optional kind) and return them with immediate connections. Large results are compacted with a ctx_retrieve ref."
    )]
    async fn graph_query(
        &self,
        Parameters(req): Parameters<GraphQueryRequest>,
    ) -> Result<Json<GraphView>, ErrorData> {
        let op = self.ops.start(
            "graph_query",
            serde_json::json!({ "name": req.name, "kind": req.kind, "limit": req.limit }),
        );
        let graph = match self.load_graph() {
            Ok(g) => g,
            Err(e) => {
                op.finish(0, 0, None, "error", "graph build failed", None);
                return Err(e);
            }
        };
        let view = gquery::query(&graph, &req.name, req.kind.as_deref(), req.limit);
        let raw_payload = view_payload_len(&view);
        let compacted = self.maybe_compact(view);
        self.record_graph_op(op, raw_payload, &compacted);
        Ok(Json(compacted))
    }

    /// Return the local subgraph around a node.
    #[tool(
        description = "Return the local subgraph within `depth` hops of a node id (from graph_query results)."
    )]
    async fn graph_neighbors(
        &self,
        Parameters(req): Parameters<GraphNeighborsRequest>,
    ) -> Result<Json<GraphView>, ErrorData> {
        let op = self.ops.start(
            "graph_neighbors",
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
    async fn graph_path(
        &self,
        Parameters(req): Parameters<GraphPathRequest>,
    ) -> Result<Json<PathResponse>, ErrorData> {
        let op = self.ops.start(
            "graph_path",
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

    /// Report token-savings counters and index/graph sizes.
    #[tool(
        description = "Report sandbox usage, estimated tokens saved, and index/graph sizes for this repo's ctxforge state."
    )]
    async fn ctx_stats(
        &self,
        Parameters(_): Parameters<EmptyRequest>,
    ) -> Result<Json<StatsResponse>, ErrorData> {
        let op = self.ops.start("ctx_stats", serde_json::json!({}));
        let s = &self.store;
        let read = |k: &str| s.get_stat(k).unwrap_or(0);
        let raw = read("raw_bytes_processed");
        let returned = read("bytes_returned_to_context");
        let saved = ((raw - returned).max(0)) / 4;
        let resp = StatsResponse {
            sandbox_calls: read("sandbox_calls"),
            raw_bytes_processed: raw,
            bytes_returned_to_context: returned,
            estimated_tokens_saved: saved,
            index_chunks: read("index_chunks"),
            graph_nodes: read("graph_nodes"),
            graph_edges: read("graph_edges"),
        };
        let returned_bytes = obs::json_len(&resp);
        op.finish(returned_bytes, returned_bytes, None, "ok", "counters read", None);
        Ok(Json(resp))
    }
}

#[tool_handler]
impl ServerHandler for Forge {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        let mut info = rmcp::model::ServerInfo::default();
        info.capabilities = rmcp::model::ServerCapabilities::builder().enable_tools().build();
        // Imperative tool-selection guidance (adapted from context-mode's CLAUDE.md
        // prose; tool names mapped to ctxforge). The MCP `instructions` ship on every
        // session handshake regardless of routing, so this is the always-on layer.
        info.instructions = Some(
            "Raw tool output floods your context window and costs reasoning capacity for \
             the rest of the session. Keep raw data in the ctxforge sandbox and surface \
             only the derived answer.\n\
             THINK IN CODE: to analyze, count, filter, search, parse, or transform data, \
             write a script via ctx_execute(language, code) and print only the answer — do \
             NOT read raw data into context. One script replaces many tool calls.\n\
             TOOL SELECTION: (1) code structure (who-calls-what, imports, how A reaches B, \
             where a symbol lives) → ctx_discover once, then graph_query / graph_neighbors / \
             graph_path on a scoped subgraph instead of reading many files. (2) where is X \
             mentioned → ctx_index then ctx_search(queries). (3) derive an answer FROM data \
             or a file → ctx_execute / ctx_execute_file. (4) recover an offloaded result → \
             ctx_retrieve. (5) savings → ctx_stats.\n\
             RULES: DO NOT use Read to analyze a file — use ctx_execute_file (Read is correct \
             only when you will Edit it). DO NOT use Grep/Bash to count, filter, or aggregate \
             — use ctx_search, graph_query, or ctx_execute. DO NOT use WebFetch — fetch and \
             reduce the URL with ctx_execute. If a ctx_*/graph_* tool is reported not-found, \
             it is deferred: load it with ToolSearch and retry — do not fall back to raw \
             tools. Bash/Read stay correct for short fixed output or mutating state \
             (git, mkdir, rm, mv, navigation)."
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

    /// Path to the persisted structural graph file.
    fn graph_file(&self) -> PathBuf {
        self.data_dir.join("graph.json")
    }

    /// Load the structural graph, building it on first use if discovery hasn't run
    /// yet (so graph queries work on any repo without an explicit ctx_discover).
    fn load_graph(&self) -> Result<Graph, ErrorData> {
        self.ensure_graph()?;
        Graph::load(&self.graph_file())
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))
    }

    /// Persist a freshly-built graph and its node/edge stats, then finalize `op`
    /// with a summary. Shared by `ctx_discover` (explicit) and `ensure_graph`
    /// (lazy); `auto` only adjusts the op note.
    fn finish_discovery(
        &self,
        op: obs::OpHandle,
        outcome: discovery::DiscoverOutcome,
        auto: bool,
    ) -> Result<DiscoverResponse, ErrorData> {
        if let Err(e) = outcome.graph.save(&self.graph_file()) {
            op.finish(0, 0, None, "error", e.to_string(), None);
            return Err(ErrorData::internal_error(e.to_string(), None));
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

    /// Build the structural graph on first use if `graph.json` is missing, so graph
    /// queries work on any repo without an explicit `ctx_discover`. A present graph
    /// short-circuits; otherwise this walks the whole repo once and persists it.
    fn ensure_graph(&self) -> Result<(), ErrorData> {
        if self.graph_file().exists() {
            return Ok(());
        }
        let op = self
            .ops
            .start("ctx_discover", serde_json::json!({ "auto": true }));
        let outcome = match discovery::discover(&self.repo_dir, None) {
            Ok(o) => o,
            Err(e) => {
                op.finish(0, 0, None, "error", e.to_string(), None);
                return Err(ErrorData::internal_error(e.to_string(), None));
            }
        };
        self.finish_discovery(op, outcome, true)?;
        Ok(())
    }

    /// Build the FTS index on first use if the code hasn't been indexed yet, so
    /// `ctx_search` works on any repo without an explicit `ctx_index`. Gated on the
    /// `index_chunks` stat — written only by code indexing, never by session-
    /// continuity records — so session events don't mask an unindexed codebase.
    fn ensure_index(&self) -> Result<(), ErrorData> {
        if self.store.get_stat("index_chunks").unwrap_or(0) > 0 {
            return Ok(());
        }
        let op = self
            .ops
            .start("ctx_index", serde_json::json!({ "auto": true }));
        match self.index.index_path(&self.repo_dir, true) {
            Ok(resp) => {
                if let Ok(total) = self.index.chunk_count() {
                    let _ = self.store.set_stat("index_chunks", total);
                }
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
    /// (plain) JSON for `ctx_retrieve` and return a dictionary-compacted form
    /// instead of the raw node/edge lists.
    fn maybe_compact(&self, view: GraphView) -> GraphView {
        let original = serde_json::json!({ "nodes": view.nodes, "edges": view.edges });
        let size = original.to_string().len();
        if size <= self.max_inline {
            return view;
        }
        let reference = self.store.put(&original.to_string()).ok();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::compress;
    use crate::tools::NodeView;
    use tempfile::tempdir;

    fn forge(max_inline: usize) -> (Forge, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let data = dir.path().join(".ctxforge");
        let f = Forge::with_paths(dir.path().to_path_buf(), data, max_inline).unwrap();
        (f, dir)
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
}
