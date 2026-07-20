//! `lens q <verb>` - a read-only CLI query family over an already-built `.lens/`.
//!
//! This is Layer 2's callback seam: a darkroom script (bash/ruby/go directly, or
//! the injected `lens.py`/`lens.mjs` wrappers) can query this repo's own index and
//! graph by shelling out to `lens q …` instead of round-tripping through MCP.
//!
//! Every verb:
//!   * emits EXACTLY ONE line of JSON to stdout - this process's stdout is its own
//!     response channel (distinct from the MCP server's JSON-RPC stdout). All
//!     diagnostics go to stderr.
//!   * is READ-ONLY against the data dir: it NEVER triggers an index or graph
//!     rebuild, even when stale. It resolves the data dir exactly like the server
//!     ([`obs::data_dir`]: `$LENS_DIR`, else `<cwd>/.lens`).
//!   * carries `"stale": <bool>` in every success payload (except `recall`, which
//!     mirrors `lens_recall`'s per-blob `stale: Option<String>`), computed by the
//!     same debounced-manifest comparison `ensure_index`/`ensure_graph` use, WITHOUT
//!     their reindex arm.
//!
//! Missing index/graph at the data dir → exit code 2 and a single-line JSON error
//! `{"error":"no index; run lens warmup"}` on stdout.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Result;
use serde_json::{json, Value};

use crate::discovery::graph::{Direction, Graph};
use crate::discovery::query as gquery;
use crate::discovery::{self, extract, pattern, skeleton, structural, tags_adapter};
use crate::index::{self, Index};
use crate::obs;
use crate::store::Store;
use crate::tools::RetrieveResponse;

/// Exit code for a missing index/graph (the one error written to stdout as JSON).
const EXIT_NO_INDEX: i32 = 2;
/// Exit code for every other failure (usage, not-found, IO) - diagnostics to stderr.
const EXIT_ERROR: i32 = 1;

/// Verb outcome. `NoIndex` maps to exit 2 + the stdout JSON error; `Bad` maps to
/// exit 1 + a stderr diagnostic. Kept distinct from `anyhow::Error` so `run_cli`
/// can pick the right exit code without string-matching.
#[derive(Debug)]
enum QError {
    NoIndex,
    Bad(String),
}

impl QError {
    fn bad(msg: impl Into<String>) -> Self {
        QError::Bad(msg.into())
    }
}

/// `lens q <verb> [args]`. `args` is everything AFTER `q` (verb first).
pub fn run_cli(args: &[String]) -> Result<()> {
    let Some(verb) = args.first().map(|s| s.as_str()) else {
        eprintln!("{}", USAGE);
        std::process::exit(EXIT_ERROR);
    };
    if matches!(verb, "-h" | "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    let ctx = QCli::from_env();
    match run_verb(&ctx, verb, &args[1..]) {
        Ok(value) => {
            // The ONLY thing on stdout: the single-line JSON payload.
            println!("{value}");
            Ok(())
        }
        Err(QError::NoIndex) => {
            println!("{}", json!({ "error": "no index; run lens warmup" }));
            std::process::exit(EXIT_NO_INDEX);
        }
        Err(QError::Bad(msg)) => {
            eprintln!("lens q {verb}: {msg}");
            std::process::exit(EXIT_ERROR);
        }
    }
}

const USAGE: &str = "usage: lens q <verb> [args] - read-only queries over an existing .lens/\n\
\n\
search  <query...> [--limit N]                      ranked hits, each with its FULL chunk\n\
symbol  <name> [--kind K] [--limit N]               declared symbols by name substring\n\
callers <name> [--depth N] [--transitive] [--prod-only]  directed fan-in subgraph (or full closure with witnesses)\n\
callees <name> [--depth N] [--transitive] [--prod-only]  directed fan-out subgraph (or full closure with witnesses)\n\
path    <from> <to>                                 shortest directed path\n\
skeleton <file> [--bodies a,b]                      full skeleton text + per-def lines\n\
grep-ast [--path P] [--pattern PAT | --query Q] [--lang L] [--limit N]\n\
overview [--budget N] [--query Q]                   importance-ranked repo map\n\
recall  <ref> [--grep S] [--offset N] [--limit N]   full stored blob";

/// Route a verb to its implementation.
fn run_verb(ctx: &QCli, verb: &str, rest: &[String]) -> Result<Value, QError> {
    match verb {
        "search" => search(ctx, rest),
        "symbol" => symbol(ctx, rest),
        "callers" => neighbors(ctx, rest, "callers"),
        "callees" => neighbors(ctx, rest, "callees"),
        "path" => path(ctx, rest),
        "skeleton" => skeleton_verb(ctx, rest),
        "grep-ast" => grep_ast(ctx, rest),
        "overview" => overview(ctx, rest),
        "recall" => recall(ctx, rest),
        other => Err(QError::bad(format!("unknown verb '{other}'\n{USAGE}"))),
    }
}

// ---------------------------------------------------------------------------
// Context: paths + read-only opens + staleness
// ---------------------------------------------------------------------------

/// The read-only query context: the repo working dir (for the staleness walk and
/// relative-path resolution) and the data dir (where the persisted graph/index/store
/// live). Mirrors how [`crate::server::Forge`] pairs `repo_dir` with `data_dir`.
struct QCli {
    repo_dir: PathBuf,
    data_dir: PathBuf,
}

impl QCli {
    /// Resolve exactly like the server: `repo_dir` = cwd, `data_dir` = `$LENS_DIR`
    /// else `<cwd>/.lens` (via [`obs::data_dir`]).
    fn from_env() -> Self {
        QCli {
            repo_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            data_dir: obs::data_dir(),
        }
    }

    /// Explicit-paths constructor for unit tests (no cwd/env dependency).
    #[cfg(test)]
    fn with_paths(repo_dir: PathBuf, data_dir: PathBuf) -> Self {
        QCli { repo_dir, data_dir }
    }

    fn graph_file(&self) -> PathBuf {
        self.data_dir.join("graph.json")
    }

    /// Resolve a possibly-relative path against the repo working dir (mirrors
    /// `Forge::resolve`).
    fn resolve(&self, p: &str) -> PathBuf {
        let path = PathBuf::from(p);
        if path.is_absolute() {
            path
        } else {
            self.repo_dir.join(path)
        }
    }

    /// Load the persisted graph read-only. `NoIndex` when `graph.json` is absent,
    /// unreadable, or empty (a never-built / poisoned graph) - NEVER a rebuild.
    fn require_graph(&self) -> Result<Graph, QError> {
        if !self.graph_file().exists() {
            return Err(QError::NoIndex);
        }
        let graph = Graph::load(&self.graph_file()).map_err(|_| QError::NoIndex)?;
        if graph.nodes.is_empty() {
            return Err(QError::NoIndex);
        }
        Ok(graph)
    }

    /// Open the FTS index read-only. `NoIndex` when the index db is absent or holds
    /// no code chunks. Gating on the db file's existence first keeps a missing data
    /// dir from being CREATED by `Index::open`.
    fn require_index(&self) -> Result<Index, QError> {
        if !self.data_dir.join("index.db").exists() {
            return Err(QError::NoIndex);
        }
        let index = Index::open(&self.data_dir)
            .map_err(|e| QError::bad(e.to_string()))?
            .with_repo_root(&self.repo_dir);
        if index.chunk_count().unwrap_or(0) <= 0 {
            return Err(QError::NoIndex);
        }
        Ok(index)
    }

    /// Open the reversible store read-only. `NoIndex` when `store.db` is absent
    /// (again gating on existence so a missing data dir is never created).
    fn require_store(&self) -> Result<Store, QError> {
        if !self.data_dir.join("store.db").exists() {
            return Err(QError::NoIndex);
        }
        Store::open(&self.data_dir).map_err(|e| QError::bad(e.to_string()))
    }

    /// The "this is a warmed lens dir" gate for the file-based verbs (`skeleton`,
    /// `grep-ast`) that read source directly and don't traverse the graph/index:
    /// require a persisted graph or index to be present so a bare directory still
    /// returns the `no index` contract.
    fn require_warmed(&self) -> Result<(), QError> {
        if self.graph_file().exists() || self.data_dir.join("index.db").exists() {
            Ok(())
        } else {
            Err(QError::NoIndex)
        }
    }

    /// Per-file graph-importance rank map for `lens_search`'s RRF fusion, rebuilt
    /// from the persisted `graph.json` (empty when absent, so search still runs and
    /// discovery is never triggered). A faithful copy of `Forge::file_ranks`, which
    /// is a private method this process can't call.
    fn file_ranks(&self) -> HashMap<String, usize> {
        let Ok(graph) = Graph::load(&self.graph_file()) else {
            return HashMap::new();
        };
        let importance = graph.importance();
        let mut per_file: HashMap<&str, f64> = HashMap::new();
        for node in &graph.nodes {
            if let Some(score) = importance.get(&node.id) {
                *per_file.entry(node.file.as_str()).or_insert(0.0) += *score;
            }
        }
        let mut files: Vec<(&str, f64)> = per_file.into_iter().collect();
        files.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(b.0))
        });
        files
            .into_iter()
            .enumerate()
            .map(|(rank, (path, _))| (path.to_string(), rank))
            .collect()
    }

    /// True when the persisted graph is stale vs the working tree: the same
    /// mtime-manifest comparison `ensure_graph` gates on, WITHOUT its reindex arm.
    fn graph_stale(&self) -> bool {
        let current = discovery::source_manifest(&self.repo_dir);
        read_manifest(&self.data_dir.join("graph.manifest.json")).as_ref() != Some(&current)
    }

    /// True when the persisted FTS index is stale vs the working tree: the same
    /// mtime-manifest comparison `ensure_index` gates on, WITHOUT its reindex arm.
    fn index_stale(&self) -> bool {
        let current = index::file_manifest(&self.repo_dir);
        read_manifest(&self.data_dir.join("index.manifest.json")).as_ref() != Some(&current)
    }
}

// ---------------------------------------------------------------------------
// Verbs
// ---------------------------------------------------------------------------

/// `search <query...> [--limit N]` - the exact `lens_search` ranking (`file_ranks`
/// RRF + `search_fused`), but each hit carries its FULL stored chunk instead of the
/// capped snippet. Uncapped by policy: this feeds in-script composition.
fn search(ctx: &QCli, args: &[String]) -> Result<Value, QError> {
    let (positionals, flags) = parse_flags(args);
    let query = positionals.join(" ");
    if query.trim().is_empty() {
        return Err(QError::bad("usage: lens q search <query...> [--limit N]"));
    }
    let limit = usize_flag(&flags, "limit").unwrap_or(20);
    let index = ctx.require_index()?;
    let file_ranks = ctx.file_ranks();
    // Full chunk per hit: run the SAME engine `lens_search` runs, under
    // SearchContext::Chunk. The context mode only changes each hit's rendered text
    // (snippet -> whole stored chunk), never the ranking, so the hit order is
    // byte-identical to lens_search. `with_chunk_context` scopes the env flag under
    // a process-global lock so the chunk-mode window can't leak into a concurrent
    // Snippet-mode search elsewhere in the same process.
    let resp = with_chunk_context(|| index.search_fused(&[query], limit, &file_ranks))
        .map_err(|e| QError::bad(e.to_string()))?;
    let hits: Vec<Value> = resp
        .results
        .into_iter()
        .flat_map(|r| r.hits)
        .map(|h| {
            json!({
                "path": h.path,
                "line": h.line,
                "score": h.score,
                "symbols": h.symbols,
                "chunk": h.snippet, // in Chunk mode this IS the full stored chunk
            })
        })
        .collect();
    Ok(json!({ "hits": hits, "stale": ctx.index_stale() }))
}

/// `symbol <name> [--kind K] [--limit N]` - declared symbols by name substring plus
/// their immediate connections. Substring-only, mirroring today's `lens_symbol`; the
/// blend-ranked no-match fallback is deliberately NOT added here (that lands on the
/// MCP handler in a later task).
fn symbol(ctx: &QCli, args: &[String]) -> Result<Value, QError> {
    let (positionals, flags) = parse_flags(args);
    let Some(name) = positionals.first() else {
        return Err(QError::bad("usage: lens q symbol <name> [--kind K] [--limit N]"));
    };
    let kind = flags.get("kind").map(String::as_str);
    let limit = usize_flag(&flags, "limit").unwrap_or(20);
    let graph = ctx.require_graph()?;
    // No session-proximity signal in a one-shot CLI query (the server sources it
    // from live session hooks); ranking is the pure structural ranking.
    let view = gquery::query(&graph, name, kind, limit, &[]);
    with_stale(serde_json::to_value(&view), ctx.graph_stale())
}

/// `callers|callees <name> [--depth N] [--transitive] [--prod-only]` - the directed
/// neighborhood subgraph, or (with `--transitive`) the complete directed closure with
/// witnesses (T3). Resolves `<name>` to a node id the way `lens_links` does
/// (`gquery::resolve`), then walks the requested direction (`gquery::neighbors_dir`);
/// `--transitive` instead calls `gquery::transitive_closure`, which resolves the root
/// itself.
fn neighbors(ctx: &QCli, args: &[String], dir: &str) -> Result<Value, QError> {
    // `--transitive`/`--prod-only` are presence-only flags (no value), unlike every
    // other `--flag` in this file's `parse_flags` convention (which always consumes
    // the next token as a value) - strip them out before the shared parser sees them.
    let transitive = args.iter().any(|a| a == "--transitive");
    let prod_only = args.iter().any(|a| a == "--prod-only");
    let rest: Vec<String> = args
        .iter()
        .filter(|a| a.as_str() != "--transitive" && a.as_str() != "--prod-only")
        .cloned()
        .collect();
    let (positionals, flags) = parse_flags(&rest);
    let Some(name) = positionals.first() else {
        return Err(QError::bad(format!(
            "usage: lens q {dir} <name> [--depth N] [--transitive] [--prod-only]"
        )));
    };
    let depth = usize_flag(&flags, "depth").unwrap_or(1);
    let graph = ctx.require_graph()?;
    if transitive {
        let direction = if dir == "callers" {
            Direction::Callers
        } else {
            Direction::Callees
        };
        let closure = gquery::transitive_closure(&graph, name, direction, depth, prod_only)
            .map_err(|e| QError::bad(e.to_string()))?;
        let serialized = serde_json::to_value(&closure).map_err(|e| QError::bad(e.to_string()))?;
        let Value::Object(fields) = serialized else {
            return Err(QError::bad("expected a JSON object payload"));
        };
        // Count-first convention: the total/prod reach up front, so a scan of the
        // compact line answers "how many" before "which ones".
        let mut ordered = serde_json::Map::new();
        for key in ["count_total", "count_prod"] {
            if let Some(v) = fields.get(key) {
                ordered.insert(key.to_string(), v.clone());
            }
        }
        for (k, v) in fields {
            ordered.entry(k).or_insert(v);
        }
        ordered.insert("stale".to_string(), json!(ctx.graph_stale()));
        return Ok(Value::Object(ordered));
    }
    let Some(id) = gquery::resolve(&graph, name) else {
        return Err(QError::bad(format!(
            "no node found for '{name}': not a known node id and no symbol matches that name"
        )));
    };
    let view = gquery::neighbors_dir(&graph, &id, depth, Some(dir));
    with_stale(serde_json::to_value(&view), ctx.graph_stale())
}

/// `path <from> <to>` - the shortest directed path (old `lens_path`/`PathResponse`
/// shape: `found`/`path`/`edges`/`resolved`) plus `stale`.
fn path(ctx: &QCli, args: &[String]) -> Result<Value, QError> {
    let (positionals, _) = parse_flags(args);
    if positionals.len() < 2 {
        return Err(QError::bad("usage: lens q path <from> <to>"));
    }
    let graph = ctx.require_graph()?;
    let resp = gquery::path(&graph, &positionals[0], &positionals[1]);
    with_stale(serde_json::to_value(&resp), ctx.graph_stale())
}

/// `skeleton <file> [--bodies a,b]` - the full skeleton text with per-def line
/// numbers, no budget/truncation (unlike the MCP tool). `--bodies` names definitions
/// to emit in full (comma-separated), mirroring `include_bodies`.
fn skeleton_verb(ctx: &QCli, args: &[String]) -> Result<Value, QError> {
    let (positionals, flags) = parse_flags(args);
    let Some(file) = positionals.first() else {
        return Err(QError::bad("usage: lens q skeleton <file> [--bodies a,b]"));
    };
    ctx.require_warmed()?;
    let p = ctx.resolve(file);
    let content = std::fs::read_to_string(&p)
        .map_err(|e| QError::bad(format!("read {}: {e}", p.display())))?;
    let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("");
    let Some(spec) = extract::spec_for_extension(ext) else {
        return Err(QError::bad(format!(
            "no skeleton for {} (unsupported language '.{ext}'); use Read",
            p.display()
        )));
    };
    let bodies: Option<Vec<String>> = flags
        .get("bodies")
        .map(|s| s.split(',').map(str::to_string).collect());
    let Some(text) = skeleton::skeletonize(&content, &spec, bodies.as_deref(), true) else {
        return Err(QError::bad(format!(
            "could not parse {} for skeleton; use Read",
            p.display()
        )));
    };
    Ok(json!({ "skeleton": text, "language": spec.name, "stale": ctx.graph_stale() }))
}

/// `grep-ast [--path P] [--pattern PAT | --query Q] [--lang L] [--limit N]` - the
/// `lens_grep_ast` engine (`structural::grep_ast_filtered`), resolving `--pattern`
/// through the same `$META` compiler the handler uses.
fn grep_ast(ctx: &QCli, args: &[String]) -> Result<Value, QError> {
    let (_, flags) = parse_flags(args);
    ctx.require_warmed()?;
    let root = ctx.resolve(flags.get("path").map(String::as_str).unwrap_or("."));
    let lang = flags.get("lang").map(String::as_str);
    let limit = usize_flag(&flags, "limit").unwrap_or(100);
    // Resolve to a tree-sitter query exactly like the MCP handler: a raw query passes
    // through; a $META pattern is compiled, and only its `@match` capture may surface.
    let (query, only_capture): (String, Option<&str>) = match (flags.get("query"), flags.get("pattern"))
    {
        (Some(q), None) => (q.clone(), None),
        (None, Some(p)) => {
            let lang = lang.ok_or_else(|| QError::bad("--pattern requires --lang"))?;
            let spec = tags_adapter::any_spec_for_language(lang)
                .ok_or_else(|| QError::bad(format!("unsupported language '{lang}'")))?;
            let compiled = pattern::compile_pattern(p, &spec).map_err(|e| QError::bad(e.to_string()))?;
            (compiled, Some(pattern::MATCH_CAPTURE))
        }
        (Some(_), Some(_)) => {
            return Err(QError::bad("set exactly one of --query or --pattern (both were given)"))
        }
        (None, None) => {
            return Err(QError::bad("set exactly one of --query or --pattern (neither was given)"))
        }
    };
    let matches = structural::grep_ast_filtered(&root, &query, lang, limit, only_capture)
        .map_err(|e| QError::bad(e.to_string()))?;
    let arr: Vec<Value> = matches
        .iter()
        .map(|m| json!({ "path": m.path, "line": m.line, "text": m.text }))
        .collect();
    Ok(json!({ "matches": arr, "stale": ctx.graph_stale() }))
}

/// `overview [--budget N] [--query Q]` - the importance-ranked repo map. Accepts an
/// arbitrarily large `--budget` (no forced cap).
fn overview(ctx: &QCli, args: &[String]) -> Result<Value, QError> {
    let (_, flags) = parse_flags(args);
    let budget = usize_flag(&flags, "budget").unwrap_or(2000);
    let query = flags.get("query").map(String::as_str);
    let graph = ctx.require_graph()?;
    // No session-proximity signal in a one-shot CLI query (see `symbol`).
    let seed = gquery::overview_seed(&graph, &[], query);
    let text = gquery::overview(&graph, budget, &seed);
    Ok(json!({ "overview": text, "stale": ctx.graph_stale() }))
}

/// `recall <ref> [--grep S] [--offset N] [--limit N]` - the full stored blob honoring
/// grep(first)/offset/limit, exactly like `lens_recall`. Store-only. Its `stale` is
/// the per-blob `Option<String>` file-snapshot warning (NOT the bool the other verbs
/// carry) - the one intentional exception.
fn recall(ctx: &QCli, args: &[String]) -> Result<Value, QError> {
    let (positionals, flags) = parse_flags(args);
    let Some(reference) = positionals.first() else {
        return Err(QError::bad(
            "usage: lens q recall <ref> [--grep S] [--offset N] [--limit N]",
        ));
    };
    let store = ctx.require_store()?;
    let content = store
        .get(reference)
        .map_err(|e| QError::bad(e.to_string()))?
        .ok_or_else(|| QError::bad(format!("unknown ref '{reference}'")))?;
    let stale = blob_stale_note(&store, reference);
    let (content, sliced) = slice_content(
        &content,
        usize_flag(&flags, "offset"),
        usize_flag(&flags, "limit"),
        flags.get("grep").map(String::as_str),
    );
    let resp = RetrieveResponse { content, stale, sliced };
    serde_json::to_value(&resp).map_err(|e| QError::bad(e.to_string()))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Serialize a reusable tool struct's `Value` and splice in a top-level `"stale"`
/// bool, so a verb reuses the exact MCP shape (GraphView / PathResponse) and only
/// adds the freshness flag.
fn with_stale(value: Result<Value, serde_json::Error>, stale: bool) -> Result<Value, QError> {
    let mut value = value.map_err(|e| QError::bad(e.to_string()))?;
    let Value::Object(map) = &mut value else {
        return Err(QError::bad("expected a JSON object payload"));
    };
    map.insert("stale".to_string(), json!(stale));
    Ok(value)
}

/// Serializes concurrent uses of the process-global `LENS_SEARCH_CONTEXT` env flag so
/// the `search` verb's chunk-mode window is never observed by another search running
/// in a different mode in the same process.
static SEARCH_CTX_LOCK: Mutex<()> = Mutex::new(());

/// Run `f` with `LENS_SEARCH_CONTEXT=chunk`, restoring the prior value afterward, all
/// under [`SEARCH_CTX_LOCK`]. The env flag is how `Index::search_fused` selects the
/// full-chunk render for every hit; scoping + restoring it keeps this read-only and
/// side-effect-free to the rest of the process.
fn with_chunk_context<T>(f: impl FnOnce() -> T) -> T {
    let _guard = SEARCH_CTX_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let prev = std::env::var_os("LENS_SEARCH_CONTEXT");
    std::env::set_var("LENS_SEARCH_CONTEXT", "chunk");
    let out = f();
    match prev {
        Some(v) => std::env::set_var("LENS_SEARCH_CONTEXT", v),
        None => std::env::remove_var("LENS_SEARCH_CONTEXT"),
    }
    out
}

/// One-line staleness warning for a recalled blob that snapshots a source file:
/// `None` while the file still hashes to the blob (or the blob has no source),
/// `Some` once it diverged or vanished. A faithful copy of `Forge::stale_note`
/// (a private method), comparing content hashes so a touch stays fresh.
fn blob_stale_note(store: &Store, reference: &str) -> Option<String> {
    let src = store.source(reference).ok().flatten()?;
    match std::fs::read(&src.path) {
        Ok(bytes) => {
            if blake3::hash(&bytes).to_hex().to_string() == src.hash {
                None
            } else {
                Some(format!(
                    "{} has changed since this snapshot was captured; the content here is \
                     the captured version, not the current file. Re-run lens_skeleton (or \
                     Read) for the current contents.",
                    src.path
                ))
            }
        }
        Err(_) => Some(format!(
            "{} no longer exists (or is unreadable); the content here is a historical \
             snapshot.",
            src.path
        )),
    }
}

/// Apply `lens_recall`'s optional `grep`/`offset`/`limit` narrowing to a blob's full
/// content: `grep` filters to lines containing the substring first, then `offset`/
/// `limit` (1-based) page through the survivors. Returns the narrowed text and whether
/// any narrowing param was given. A faithful copy of `server::slice_content` (a private
/// free fn).
fn slice_content(
    content: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    grep: Option<&str>,
) -> (String, bool) {
    if offset.is_none() && limit.is_none() && grep.is_none() {
        return (content.to_string(), false);
    }
    let lines: Vec<&str> = content.lines().collect();
    let filtered: Vec<&str> = match grep {
        Some(pat) => lines.into_iter().filter(|l| l.contains(pat)).collect(),
        None => lines,
    };
    let start = offset.unwrap_or(1).max(1) - 1;
    if start >= filtered.len() {
        return (String::new(), true);
    }
    let end = match limit {
        Some(n) => (start + n).min(filtered.len()),
        None => filtered.len(),
    };
    (filtered[start..end].join("\n"), true)
}

/// Load a saved staleness manifest, or `None` if absent/unreadable (which reads as
/// "stale" - the safe default). Mirrors `server::read_manifest`.
fn read_manifest(path: &Path) -> Option<std::collections::BTreeMap<String, u64>> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

/// Lightweight manual arg split (same convention as `warmup`/`setup`): every `--flag`
/// consumes the following token as its value; everything else is a positional. No new
/// arg-parsing dependency.
fn parse_flags(args: &[String]) -> (Vec<String>, HashMap<String, String>) {
    let mut positionals = Vec::new();
    let mut flags = HashMap::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if let Some(key) = arg.strip_prefix("--") {
            if let Some(value) = it.next() {
                flags.insert(key.to_string(), value.clone());
            }
        } else {
            positionals.push(arg.clone());
        }
    }
    (positionals, flags)
}

/// Parse a numeric flag value, `None` when absent or unparseable.
fn usize_flag(flags: &HashMap<String, String>, key: &str) -> Option<usize> {
    flags.get(key).and_then(|v| v.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Build a small multi-file repo, warm its graph + FTS index into a `.lens` under
    /// it (writes the staleness manifests too), and return a `QCli` pointed at both.
    /// The fixture-setup helper the golden tests share.
    fn fixture() -> (tempfile::TempDir, QCli) {
        let repo = tempdir().unwrap();
        // route_inner -> helper_alpha; dispatch -> route_inner (caller/callee chain).
        fs::write(
            repo.path().join("a.rs"),
            "pub fn route_inner() -> i32 { helper_alpha() }\n\
             pub fn dispatch() { route_inner(); }\n\
             pub fn helper_alpha() -> i32 { 42 }\n",
        )
        .unwrap();
        // A function whose body is long enough that a 24-token snippet can't span it,
        // so a search hit's `chunk` (full chunk) must carry BOTH markers.
        let mut busy = String::from("pub struct Widget { pub id: u32 }\n");
        busy.push_str("pub fn busy_marker() {\n    let needle_start = 1;\n");
        for i in 0..40 {
            busy.push_str(&format!("    let pad_{i} = {i};\n"));
        }
        busy.push_str("    let needle_end = 99;\n    let _ = (needle_start, needle_end);\n}\n");
        fs::write(repo.path().join("b.rs"), busy).unwrap();

        let data = repo.path().join(".lens");
        crate::warmup::warmup(repo.path(), &data).unwrap();
        let ctx = QCli::with_paths(repo.path().to_path_buf(), data);
        (repo, ctx)
    }

    #[test]
    fn search_returns_full_chunk_and_fresh_stale() {
        let (_repo, ctx) = fixture();
        let out = search(&ctx, &["needle_start".to_string()]).unwrap();
        assert_eq!(out["stale"], json!(false), "just-warmed index is fresh");
        let hits = out["hits"].as_array().expect("hits array");
        let hit = hits
            .iter()
            .find(|h| h["path"].as_str().is_some_and(|p| p.ends_with("b.rs")))
            .expect("a hit in b.rs");
        let chunk = hit["chunk"].as_str().unwrap();
        assert!(
            chunk.contains("needle_start") && chunk.contains("needle_end"),
            "chunk must be the FULL chunk (both markers), not a capped snippet: {chunk}"
        );
        assert!(hit["line"].as_u64().is_some(), "hit carries a line");
        assert!(hit["score"].as_f64().is_some(), "hit carries a score");
    }

    #[test]
    fn symbol_matches_by_substring() {
        let (_repo, ctx) = fixture();
        let out = symbol(&ctx, &["helper".to_string()]).unwrap();
        assert_eq!(out["stale"], json!(false));
        let names: Vec<&str> = out["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|n| n["name"].as_str())
            .collect();
        assert!(names.contains(&"helper_alpha"), "substring 'helper' matches helper_alpha");
    }

    #[test]
    fn callers_returns_the_calling_function() {
        let (_repo, ctx) = fixture();
        let out = neighbors(&ctx, &["route_inner".to_string()], "callers").unwrap();
        assert_eq!(out["stale"], json!(false));
        let names: Vec<&str> = out["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|n| n["name"].as_str())
            .collect();
        assert!(names.contains(&"dispatch"), "dispatch calls route_inner");
        assert!(
            !names.contains(&"helper_alpha"),
            "helper_alpha is a callee, not a caller"
        );
    }

    #[test]
    fn callees_returns_the_called_function() {
        let (_repo, ctx) = fixture();
        let out = neighbors(&ctx, &["route_inner".to_string()], "callees").unwrap();
        let names: Vec<&str> = out["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|n| n["name"].as_str())
            .collect();
        assert!(names.contains(&"helper_alpha"), "route_inner calls helper_alpha");
        assert!(!names.contains(&"dispatch"), "dispatch is a caller, not a callee");
    }

    #[test]
    fn path_finds_the_directed_chain() {
        let (_repo, ctx) = fixture();
        let out = path(&ctx, &["dispatch".to_string(), "helper_alpha".to_string()]).unwrap();
        assert_eq!(out["stale"], json!(false));
        assert_eq!(out["found"], json!(true), "dispatch reaches helper_alpha");
        let hops: Vec<&str> = out["path"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|n| n["name"].as_str())
            .collect();
        assert_eq!(hops.first(), Some(&"dispatch"));
        assert_eq!(hops.last(), Some(&"helper_alpha"));
    }

    #[test]
    fn skeleton_emits_full_text_with_line_prefixes() {
        let (repo, ctx) = fixture();
        let file = repo.path().join("b.rs").to_string_lossy().into_owned();
        let out = skeleton_verb(&ctx, &[file]).unwrap();
        assert_eq!(out["language"], json!("rust"));
        assert_eq!(out["stale"], json!(false));
        let sk = out["skeleton"].as_str().unwrap();
        assert!(sk.contains("busy_marker"), "skeleton lists the function");
        assert!(sk.contains("L1:") || sk.contains("L2:"), "per-def L{{n}}: line prefixes present: {sk}");
    }

    #[test]
    fn grep_ast_query_matches_structurally() {
        let (repo, ctx) = fixture();
        let file = repo.path().join("b.rs").to_string_lossy().into_owned();
        // A raw tree-sitter query for every let-binding in b.rs.
        let out = grep_ast(
            &ctx,
            &[
                "--path".to_string(),
                file,
                "--lang".to_string(),
                "rust".to_string(),
                "--query".to_string(),
                "(let_declaration) @m".to_string(),
            ],
        )
        .unwrap();
        assert_eq!(out["stale"], json!(false));
        let matches = out["matches"].as_array().unwrap();
        assert!(!matches.is_empty(), "let-bindings match structurally");
        assert!(matches[0]["line"].as_u64().is_some());
    }

    #[test]
    fn grep_ast_pattern_compiles_via_meta() {
        let (repo, ctx) = fixture();
        let file = repo.path().join("b.rs").to_string_lossy().into_owned();
        let out = grep_ast(
            &ctx,
            &[
                "--path".to_string(),
                file,
                "--lang".to_string(),
                "rust".to_string(),
                "--pattern".to_string(),
                "(needle_start, $Y)".to_string(),
            ],
        )
        .unwrap();
        let matches = out["matches"].as_array().unwrap();
        // b.rs has exactly one 2-tuple: `(needle_start, needle_end)`.
        assert_eq!(matches.len(), 1, "the $META pattern matches exactly the one tuple");
        assert!(matches[0]["text"].as_str().unwrap().contains("needle_start"));
    }

    #[test]
    fn overview_lists_important_symbols() {
        let (_repo, ctx) = fixture();
        let out = overview(&ctx, &["--budget".to_string(), "100000".to_string()]).unwrap();
        assert_eq!(out["stale"], json!(false));
        let map = out["overview"].as_str().unwrap();
        assert!(map.contains("route_inner"), "overview names the graph's symbols");
    }

    #[test]
    fn recall_grep_returns_only_matching_lines() {
        let repo = tempdir().unwrap();
        let data = repo.path().join(".lens");
        // Create the store the way an offloaded call would, then recall a ref.
        let store = Store::open(&data).unwrap();
        let reference = store
            .put("alpha line\nfoo the first\nbeta line\nfoo the second\n")
            .unwrap();
        let ctx = QCli::with_paths(repo.path().to_path_buf(), data);

        let out = recall(&ctx, &[reference.clone(), "--grep".to_string(), "foo".to_string()]).unwrap();
        assert_eq!(out["sliced"], json!(true));
        let content = out["content"].as_str().unwrap();
        assert_eq!(
            content, "foo the first\nfoo the second",
            "grep keeps only the matching lines, in order"
        );
        assert!(out.get("stale").is_none(), "an in-memory blob has no source file, so no stale note");
    }

    #[test]
    fn recall_unknown_ref_is_an_error() {
        let repo = tempdir().unwrap();
        let data = repo.path().join(".lens");
        let _ = Store::open(&data).unwrap(); // create store.db so it's not a NoIndex
        let ctx = QCli::with_paths(repo.path().to_path_buf(), data);
        let err = recall(&ctx, &["deadbeef".to_string()]).unwrap_err();
        assert!(matches!(err, QError::Bad(_)), "unknown ref is a Bad error, not NoIndex");
    }

    #[test]
    fn missing_index_is_no_index() {
        // A bare directory (no .lens): every graph/index verb reports NoIndex.
        let repo = tempdir().unwrap();
        let ctx = QCli::with_paths(repo.path().to_path_buf(), repo.path().join(".lens"));
        assert!(matches!(symbol(&ctx, &["x".to_string()]), Err(QError::NoIndex)));
        assert!(matches!(
            neighbors(&ctx, &["x".to_string()], "callers"),
            Err(QError::NoIndex)
        ));
        assert!(matches!(search(&ctx, &["x".to_string()]), Err(QError::NoIndex)));
        assert!(matches!(
            recall(&ctx, &["deadbeef".to_string()]),
            Err(QError::NoIndex)
        ));
    }

    #[test]
    fn stale_flips_when_a_source_file_changes() {
        let (repo, ctx) = fixture();
        assert!(!ctx.graph_stale(), "fresh right after warmup");
        // Add a new source file WITHOUT re-warming: the manifest walk now diverges.
        fs::write(repo.path().join("c.rs"), "pub fn newly_added() {}\n").unwrap();
        assert!(ctx.graph_stale(), "a new source file makes the persisted graph stale");
        // And the verb still answers read-only (no rebuild), reporting stale=true.
        let out = symbol(&ctx, &["route_inner".to_string()]).unwrap();
        assert_eq!(out["stale"], json!(true));
    }
}
