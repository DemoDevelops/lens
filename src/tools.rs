//! Tool input/output structs shared across modules. Each implements serde
//! `Serialize`/`Deserialize` and `schemars::JsonSchema` so rmcp can derive the
//! MCP tool schema and (de)serialize requests/responses.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::discovery::query::{ClosureNode, TransitiveClosure};

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
    /// Optional file to analyze (relative to repo root, or absolute): injected as
    /// the script's first CLI argument (python sys.argv[1] / node process.argv[2] /
    /// bash $1), so the code can open/analyze it while only its printed output
    /// returns — the file's contents never enter context.
    #[serde(default)]
    pub path: Option<String>,
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
    /// 1-based line to start returning from (default 1, the beginning). Lets a
    /// large ref be paged through instead of recalled all at once.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Max lines to return starting at `offset` (default: the rest of the content).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Return only lines containing this substring (case-sensitive). Applied
    /// before `offset`/`limit`, so the two compose: narrow to matching lines,
    /// then page through them.
    #[serde(default)]
    pub grep: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RetrieveResponse {
    /// The full stored blob, or the narrowed slice when `offset`/`limit`/`grep`
    /// were given.
    pub content: String,
    /// Present when the blob snapshots a source file that has since changed or
    /// been deleted: a one-line warning naming the file. Absent while the file
    /// still matches the snapshot (or the blob has no source file).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale: Option<String>,
    /// True when `offset`/`limit`/`grep` were given, so `content` may be a
    /// narrower slice of the full stored blob rather than all of it.
    pub sliced: bool,
}

// ---------------------------------------------------------------------------
// lens_skeleton (file structure view)
// ---------------------------------------------------------------------------

/// Default for [`SkeletonRequest::with_lines`]: line-number prefixes on by default
/// (mined defect: callers citing skeleton output without a line number, since the
/// old default was off). Still overridable with an explicit `with_lines: false`.
fn default_with_lines() -> Option<bool> {
    Some(true)
}

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
    /// 1-indexed source line) so callers can cite exact locations. Default true;
    /// pass `false` to omit the prefixes.
    #[serde(default = "default_with_lines")]
    pub with_lines: Option<bool>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SkeletonResponse {
    /// The file's structure: signatures, types, and nesting with executable bodies
    /// elided to `…`. Budgeted to the server's inline limit: when the full skeleton
    /// would exceed it, this is a truncated head and the full text is at
    /// `skeleton_ref` (see `truncated`).
    pub skeleton: String,
    /// Detected language (e.g. "rust", "python").
    pub language: String,
    /// Ref to fetch the full file via `lens_recall` (any elided body is one call away).
    pub retrieve_ref: String,
    /// True when `skeleton` above was truncated to fit the response budget; the
    /// full (untruncated) skeleton text is at `skeleton_ref`.
    #[serde(default)]
    pub truncated: bool,
    /// Present only when `truncated`: a ref to fetch the full skeleton text via
    /// `lens_recall`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skeleton_ref: Option<String>,
}

// ---------------------------------------------------------------------------
// lens_search (full-text; the index itself is auto-ensured per query)
// ---------------------------------------------------------------------------

fn default_limit_per_query() -> usize {
    5
}

/// Summary of an index build. No MCP tool takes an index request anymore
/// (`ensure_index` auto-builds per query); this is the engine-level response
/// shape `index::Index::index_path` still returns.
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
    /// 1-based line of the snippet's matched window (the chunk's start line when no
    /// query term surfaces in the chunk), so a caller can jump straight to the match
    /// instead of re-searching the file.
    pub line: usize,
    /// Definition names the hit's chunk carries that the snippet does not already
    /// show (source order, capped), so a "which function does X" query can read
    /// candidate answers off the hit even when the snippet window doesn't reach
    /// them. Empty for prose/markdown chunks and for single-term queries (which
    /// already name their target).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub symbols: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct QueryResult {
    pub query: String,
    pub hits: Vec<SearchHit>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SearchResponse {
    pub results: Vec<QueryResult>,
    /// One line per nested-repo federation outcome worth surfacing: an autobuild
    /// that ran (`"nested repo X: built (N files)"`), one skipped for size
    /// (`"...: skipped: too large (...)"`), one skipped because the kill-switch is
    /// off (`"...: skipped: autobuild off"`), or a build failure. Empty (and
    /// omitted from JSON) when there are no nested repos or every nested repo
    /// already had a built index -- the common case stays a silent no-op exactly
    /// as before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

// ---------------------------------------------------------------------------
// Structural graph (lens_symbol / lens_graph; the graph is auto-ensured per query)
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

/// Summary of a graph build. No MCP tool takes a discover request anymore
/// (`ensure_graph` auto-builds per query); this is the engine-level response
/// shape `discovery::discover` still returns.
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
    /// Provenance: "prod", "test" (#[cfg(test)] / #[test] code), or "bench"
    /// (benchmark trees). Present on every node when the result contains any
    /// test/bench node, so callers can exclude them (e.g. "production callers
    /// of X") without reading the files; absent everywhere when the whole
    /// result is production code. All-or-none per response keeps TOON row
    /// compaction keys homogeneous.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
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

#[derive(Debug, Clone, Serialize, JsonSchema)]
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
    /// Resolution notes for an ambiguous exact-name query: present only when the
    /// query matched more than one exact-name candidate, naming the chosen node
    /// and how many others were passed over. Omitted when unambiguous, so
    /// existing outputs stay byte-identical. Mirrors `PathResponse::resolved`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub resolved: Vec<ResolvedNote>,
    /// Count of matching root symbols BEFORE the `limit` cut (their pulled-in
    /// neighbors are not counted). `None` when nothing was cut (every match was
    /// returned), so existing outputs stay byte-identical.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_matches: Option<usize>,
    /// Present only when `lens_graph`'s neighborhood form had to shrink `depth`
    /// and/or truncate the node/edge lists to fit the response budget: a short
    /// description of what was cut. The full requested-depth subgraph is still
    /// recoverable via `retrieve_ref`. Omitted (and absent from other tools'
    /// output) when no trimming was needed, so unaffected outputs stay
    /// byte-identical.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trim_note: Option<String>,
    /// How `lens_symbol` resolved the query: `"name"` (substring match) or
    /// `"meaning"` (zero substring matches, blend-ranked lexical fallback).
    /// Only `lens_symbol` sets it; omitted from every other `GraphView`
    /// producer's output, so those stay byte-identical.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_via: Option<String>,
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
pub struct GraphRequest {
    /// Node id or symbol name to start from.
    pub node: String,
    /// Optional destination node id or symbol name. Present: return the shortest
    /// directed path from `node` to `to`. Absent: return the local subgraph
    /// around `node` (or, with `transitive: true`, the full directed closure).
    #[serde(default)]
    pub to: Option<String>,
    /// Hops outward for the neighborhood walk, or the hop bound for the
    /// transitive closure when `transitive: true` (default 1). Ignored when
    /// `to` is given.
    #[serde(default = "default_depth")]
    pub depth: usize,
    /// Which way the walk follows edges: "callers" (fan-in), "callees"
    /// (fan-out), or "both" (undirected, the default). Unknown or absent
    /// falls back to "both". Ignored when `to` is given. With
    /// `transitive: true`, "both" is rejected (a closure has no undirected
    /// sense) instead of silently falling back.
    #[serde(default)]
    pub direction: Option<String>,
    /// Return the COMPLETE directed closure within `depth` hops instead of a
    /// one-hop neighborhood: every node reachable strictly following
    /// `direction`, each carrying a `witness` (the call-site `file:line`
    /// proving the edge) plus a `complete: true` claim. Mutually exclusive
    /// with `to` (a closure has no destination).
    #[serde(default)]
    pub transitive: bool,
    /// With `transitive: true`, filter the reported node list to
    /// production-origin nodes only (`count_total`/`count_prod` always report
    /// both regardless of this flag). Ignored otherwise.
    #[serde(default)]
    pub prod_only: bool,
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

/// `lens_graph`'s response: the natural shape of whichever form ran. Untagged, so
/// each variant serializes as exactly its inner type's JSON with no wrapper —
/// the `to`-form stays byte-identical to the old `lens_path` output and the
/// no-`to` form byte-identical to the old `lens_links` output.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum GraphResponse {
    /// `to` given: the shortest directed path between the two symbols.
    Path(PathResponse),
    /// `to` absent, `transitive` false: the local subgraph around `node`.
    Neighbors(GraphView),
    /// `transitive: true`, `to` absent: the complete directed closure, with
    /// per-node witnesses and a completeness claim (T3).
    Closure(TransitiveClosure),
}

/// Hand-written schema because the derive renders an untagged enum as a bare
/// `anyOf` with no root `type`, which rmcp rejects (the MCP spec requires an
/// `outputSchema` rooted at `"type": "object"`). All three variants serialize
/// as objects, so rooting the union at `type: object` is truthful and keeps
/// the untagged (wrapper-free) serialization untouched.
impl JsonSchema for GraphResponse {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "GraphResponse".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "object",
            "anyOf": [
                generator.subschema_for::<PathResponse>(),
                generator.subschema_for::<GraphView>(),
                generator.subschema_for::<TransitiveClosure>(),
            ],
        })
    }
}

/// Manual schemas for the two `discovery::query` types `GraphResponse::Closure`
/// carries. Kept here (not derived in `discovery::query`) so T3 stays scoped to
/// the surface layer without touching the T2-owned engine file.
impl JsonSchema for ClosureNode {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ClosureNode".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "object",
            "properties": {
                "id": { "type": "string" },
                "name": { "type": "string" },
                "kind": { "type": "string" },
                "file": { "type": "string" },
                "line": { "type": "integer" },
                "hops": { "type": "integer" },
                "origin": { "type": ["string", "null"] },
                "witness": { "type": ["string", "null"] }
            },
            "required": ["id", "name", "kind", "file", "line", "hops"]
        })
    }
}

impl JsonSchema for TransitiveClosure {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "TransitiveClosure".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "object",
            "properties": {
                "root": { "type": "string" },
                "root_name": { "type": "string" },
                "root_file": { "type": "string" },
                "root_line": { "type": "integer" },
                "direction": { "type": "string" },
                "depth": { "type": "integer" },
                "complete": { "type": "boolean" },
                "count_total": { "type": "integer" },
                "count_prod": { "type": "integer" },
                "nodes": { "type": "array", "items": generator.subschema_for::<ClosureNode>() },
                "resolved": { "type": "array", "items": generator.subschema_for::<ResolvedNote>() }
            },
            "required": [
                "root", "root_name", "root_file", "root_line", "direction",
                "depth", "complete", "count_total", "count_prod", "nodes"
            ]
        })
    }
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
    /// `print($X)` (python), `$A.map($F)` (typescript). `$$$` / `$$$NAME` match
    /// zero or more sibling nodes (any arity, including empty) — e.g. `f($$$)`
    /// or `fn $NAME($$$) -> Result<$$$> $BODY`. A repeated single metavariable
    /// must match equal text; repeated variadic names do not. Requires
    /// `language`. Set exactly one of `query` or `pattern`.
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
