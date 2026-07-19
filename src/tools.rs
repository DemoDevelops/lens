//! Tool input/output structs shared across modules. Each implements serde
//! `Serialize`/`Deserialize` and `schemars::JsonSchema` so rmcp can derive the
//! MCP tool schema and (de)serialize requests/responses.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

fn default_timeout() -> u64 {
    30
}

// ---------------------------------------------------------------------------
// lens_run (darkroom)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExecuteRequest {
    /// Language to run: python | javascript | typescript | bash | ruby | go.
    pub language: String,
    /// Source code to execute.
    pub code: String,
    /// Wall-clock timeout in seconds (default 30). The process is killed on overrun.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Optional data piped to the script's stdin.
    #[serde(default)]
    pub stdin: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExecuteFileRequest {
    /// Path to the file to analyze (relative to repo root, or absolute).
    pub path: String,
    /// Language to run the analysis code in: python | javascript | typescript | bash | ruby | go.
    pub language: String,
    /// Analysis code. It receives the file path as its first CLI argument
    /// (python sys.argv[1] / node process.argv[2] / bash $1); only what it prints returns to context.
    pub code: String,
    /// Wall-clock timeout in seconds (default 30).
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ExecuteResponse {
    /// Captured stdout (truncated to a head+tail preview if it exceeded the inline limit).
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// Process exit code (-1 if the process was killed by signal/timeout).
    pub exit_code: i32,
    /// True if the process was killed because it exceeded `timeout_secs`.
    pub timed_out: bool,
    /// Full size of stdout in bytes (before any truncation).
    pub stdout_bytes: usize,
    /// True if `stdout` above is a truncated preview of a larger captured output.
    pub truncated: bool,
    /// If truncated, the ref to fetch the full stdout via `lens_recall`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retrieve_ref: Option<String>,
}

// ---------------------------------------------------------------------------
// lens_recall (reversible store)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RetrieveRequest {
    /// A `retrieve_ref` returned by another tool.
    #[serde(rename = "ref")]
    pub reference: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RetrieveResponse {
    /// The full stored blob.
    pub content: String,
    /// Present when the blob snapshots a source file that has since changed or
    /// been deleted: a one-line warning naming the file. Absent while the file
    /// still matches the snapshot (or the blob has no source file).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale: Option<String>,
}

// ---------------------------------------------------------------------------
// lens_skeleton (file structure view)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SkeletonRequest {
    /// Path to the source file to skeletonize (relative to repo root, or absolute).
    pub path: String,
    /// Definition names (functions, methods, etc.) whose bodies should be emitted
    /// in full instead of elided to `…`. Names that don't match anything in the
    /// file are silently ignored.
    #[serde(default)]
    pub include_bodies: Option<Vec<String>>,
    /// When true, prefix each definition's signature line with `L{n}: ` (its
    /// 1-indexed source line) so callers can cite exact locations. Default false.
    #[serde(default)]
    pub with_lines: Option<bool>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SkeletonResponse {
    /// The file's structure: signatures, types, and nesting with executable bodies
    /// elided to `…`.
    pub skeleton: String,
    /// Detected language (e.g. "rust", "python").
    pub language: String,
    /// Ref to fetch the full file via `lens_recall` (any elided body is one call away).
    pub retrieve_ref: String,
}

// ---------------------------------------------------------------------------
// lens_index / lens_search (full-text)
// ---------------------------------------------------------------------------

fn default_true() -> bool {
    true
}

fn default_limit_per_query() -> usize {
    5
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct IndexRequest {
    /// File or directory to index.
    pub path: String,
    /// Recurse into directories (default true). Ignored for single files.
    #[serde(default = "default_true")]
    pub recursive: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct IndexResponse {
    pub files_indexed: usize,
    pub chunks: usize,
    /// Number of files actually read and re-indexed this call (0 if all unchanged).
    pub files_read: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchRequest {
    /// One or more FTS queries, run in a single call to save round-trips.
    pub queries: Vec<String>,
    /// Max hits returned per query (default 5).
    #[serde(default = "default_limit_per_query")]
    pub limit_per_query: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SearchHit {
    pub path: String,
    pub snippet: String,
    pub score: f64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct QueryResult {
    pub query: String,
    pub hits: Vec<SearchHit>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SearchResponse {
    pub results: Vec<QueryResult>,
}

// ---------------------------------------------------------------------------
// lens_map + graph_* (structural graph)
// ---------------------------------------------------------------------------

fn default_dot() -> String {
    ".".to_string()
}

fn default_depth() -> usize {
    1
}

fn default_graph_limit() -> usize {
    20
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DiscoverRequest {
    /// Repo root to scan (default ".").
    #[serde(default = "default_dot")]
    pub path: String,
    /// Optional filter to a subset of languages (rust, python, javascript, typescript, go).
    #[serde(default)]
    pub languages: Option<Vec<String>>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DiscoverResponse {
    pub nodes: usize,
    pub edges: usize,
    pub files_parsed: usize,
    pub languages: Vec<String>,
    /// Per-file warnings (e.g. files skipped because they failed to parse).
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct NodeView {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: usize,
    pub language: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EdgeView {
    pub from: String,
    pub to: String,
    pub kind: String,
}

/// A note that a name/token resolved to one of several same-named candidates.
/// Emitted only when the resolution was ambiguous (`other_candidates > 0`), so
/// unambiguous outputs never carry it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ResolvedNote {
    /// The name/token that was resolved to a node.
    pub query: String,
    /// The chosen node's id: the highest graph-importance exact-name match.
    pub chosen: String,
    /// How many OTHER exact-name candidates were passed over (0 = unambiguous).
    pub other_candidates: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct GraphView {
    pub nodes: Vec<NodeView>,
    pub edges: Vec<EdgeView>,
    /// When the subgraph is large it is dictionary-compacted into this field
    /// (nodes/edges left empty); decode with the `_d`/`_v` scheme or just call
    /// `lens_recall` on `retrieve_ref` for the plain original.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compact: Option<serde_json::Value>,
    /// True if the subgraph was compacted; full JSON is at `retrieve_ref`.
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retrieve_ref: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GraphQueryRequest {
    /// Substring to match against symbol names (case-insensitive).
    pub name: String,
    /// Optional kind filter (function, struct, class, method, interface, mod, ...).
    #[serde(default)]
    pub kind: Option<String>,
    /// Max matching nodes to expand (default 20).
    #[serde(default = "default_graph_limit")]
    pub limit: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GraphFindRequest {
    /// Natural-language description of the symbol you're looking for.
    pub query: String,
    /// Max matching symbols to return (default 20).
    #[serde(default = "default_graph_limit")]
    pub limit: usize,
    /// Optional kind filter (function, struct, class, method, interface, mod, ...):
    /// candidates whose kind differs are excluded before ranking.
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GraphNeighborsRequest {
    /// Node id to expand around.
    pub node_id: String,
    /// Hops outward (default 1).
    #[serde(default = "default_depth")]
    pub depth: usize,
    /// Which way to walk edges: "callers" (fan-in), "callees" (fan-out), or "both"
    /// (undirected, the default). Unknown or absent falls back to "both".
    #[serde(default)]
    pub direction: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GraphPathRequest {
    /// Source node id or symbol name.
    pub from: String,
    /// Destination node id or symbol name.
    pub to: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct PathResponse {
    pub found: bool,
    /// The node sequence of the shortest path (empty if none).
    pub path: Vec<NodeView>,
    /// Per-hop edge kinds aligned with `path`: `edges[i]` connects `path[i]` and
    /// `path[i+1]`, so `edges.len() == path.len().saturating_sub(1)`. Each edge
    /// keeps its real `from`/`to` so a consumer can read the hop's direction.
    /// Omitted (and empty) when there is no multi-node path, so single-node and
    /// not-found outputs stay byte-identical.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<EdgeView>,
    /// Resolution notes for ambiguous `from`/`to` inputs: present only for a token
    /// that matched more than one exact-name candidate, naming the chosen node and
    /// how many others were passed over. Omitted entirely when both ends resolved
    /// unambiguously, so unambiguous outputs stay byte-identical.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub resolved: Vec<ResolvedNote>,
}

// ---------------------------------------------------------------------------
// lens_overview (token-budgeted repo map)
// ---------------------------------------------------------------------------

fn default_overview_budget() -> usize {
    2000
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OverviewRequest {
    /// Token budget for the overview (default 2000).
    #[serde(default = "default_overview_budget")]
    pub token_budget: usize,
    /// Optional focus: symbols whose names match this query, plus files touched
    /// this session, are boosted in the ranking so the map centers on a topic.
    /// Omit for the unfocused, structurally-ranked map.
    #[serde(default)]
    pub query: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct OverviewResponse {
    /// The importance-ranked, budget-limited symbol map (markdown).
    pub overview: String,
}

// ---------------------------------------------------------------------------
// lens_grep_ast (structural / tree-sitter search)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GrepAstRequest {
    /// File or directory to search (default ".").
    #[serde(default = "default_dot")]
    pub path: String,
    /// A raw tree-sitter query (S-expression). Node kinds are language-specific, e.g.
    /// `(call_expression function: (field_expression field: (field_identifier) @m))`.
    /// Set exactly one of `query` or `pattern`.
    #[serde(default)]
    pub query: Option<String>,
    /// A code pattern with `$UPPERCASE` metavariables, compiled to a tree-sitter
    /// query in-server: write the shape as real code, e.g. `$X.unwrap()` (rust),
    /// `print($X)` (python), `$A.map($F)` (typescript). A repeated metavariable
    /// must match equal text. Requires `language`. Set exactly one of `query` or
    /// `pattern`.
    #[serde(default)]
    pub pattern: Option<String>,
    /// Language the query targets: any graph-supported language (the 6 hand-written
    /// rust, python, javascript, typescript, go, swift, plus the tags-adapter set
    /// c, cpp, csharp, java, kotlin, scala, ruby, php, lua, bash; see SUPPORTED.md).
    /// When omitted, every file is matched against the query compiled for its own
    /// grammar, skipping files whose grammar can't compile it.
    #[serde(default)]
    pub language: Option<String>,
    /// Max matches to return (default 100).
    #[serde(default = "default_grep_ast_limit")]
    pub limit: usize,
}

fn default_grep_ast_limit() -> usize {
    100
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AstMatch {
    pub path: String,
    /// 1-based line of the captured node.
    pub line: usize,
    /// The captured node's text (capped).
    pub text: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct GrepAstResponse {
    pub matches: Vec<AstMatch>,
    /// True if the result hit the `limit` cap.
    pub truncated: bool,
}

/// Empty input for tools that take no parameters.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct EmptyRequest {}

// ---------------------------------------------------------------------------
// lens_memory_record / lens_memory_query (durable project memory)
// ---------------------------------------------------------------------------

fn default_memory_limit() -> usize {
    20
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MemoryRecordRequest {
    /// Durable memory category: decision | constraint | rejected-approach | rule.
    pub category: String,
    /// The text to remember.
    pub text: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MemoryRecordResponse {
    pub recorded: bool,
    pub category: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MemoryQueryRequest {
    /// Optional text to rank durable memory by (case-insensitive token overlap
    /// against category + text). Omit for the full list, newest last.
    #[serde(default)]
    pub query: Option<String>,
    /// Max items returned (default 20).
    #[serde(default = "default_memory_limit")]
    pub limit: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MemoryItem {
    pub category: String,
    pub text: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MemoryQueryResponse {
    pub items: Vec<MemoryItem>,
}

// ---------------------------------------------------------------------------
// lens_stats
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct StatsResponse {
    pub darkroom_calls: i64,
    pub raw_bytes_processed: i64,
    pub bytes_returned_to_context: i64,
    pub estimated_tokens_saved: i64,
    pub index_chunks: i64,
    pub graph_nodes: i64,
    pub graph_edges: i64,
}
