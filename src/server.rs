//! MCP server wiring: the `Forge` handler holds shared state and exposes every
//! lens tool. Tool bodies delegate to the feature modules.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{Content, IntoContents};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};

use crate::darkroom;
use crate::discovery::{self, graph::Graph, query as gquery};
use crate::index::{self, Index};
use crate::obs::{self, OpLog};
use crate::session::{self, store::SessionStore};
use crate::store::Store;
use crate::tools::*;

/// Default inline stdout limit (bytes) before offloading to the store.
const DEFAULT_MAX_INLINE: usize = 8 * 1024;

/// The lens tools that only read: no code execution, no index/graph writes. They get a
/// `readOnlyHint` annotation in `list_tools` so Claude Code can auto-approve them (an
/// unattended agent otherwise stalls on a permission prompt), and `lens setup`
/// pre-approves them in the permission allowlist. Shared with [`crate::setup`] so the
/// two never drift. The write tools live in [`WRITE_TOOLS`] and are intentionally
/// absent here so they never get a `readOnlyHint`.
pub const READ_ONLY_TOOLS: [&str; 8] = [
    "lens_search",
    "lens_overview",
    "lens_recall",
    "lens_symbol",
    "lens_graph",
    "lens_skeleton",
    "lens_grep_ast",
    "lens_memory_query",
];

/// The lens tools that execute code (in the darkroom subprocess) or write durable
/// state. No `readOnlyHint` — but `lens setup` still pre-approves them in the
/// permission allowlist: modes that honor allow rules while prompting for everything
/// else (notably plan mode) would otherwise stall a session on every lens_run
/// call. Shared with [`crate::setup`] so the two never drift.
pub const WRITE_TOOLS: [&str; 2] = ["lens_run", "lens_memory_record"];

/// Appended to the `lens_search`/`lens_overview` descriptions so the model knows the
/// recovery path before it ever hits a transient index/graph lock (Bug B).
const SEARCH_FALLBACK_HINT: &str = " If a call returns is_error (usually a transient \
    index lock), fall back to grep/Read for this query and continue; do not kill the \
    lens server.";

/// A tool failure delivered to the model as readable `is_error` content rather than a
/// JSON-RPC protocol error (Bug B). rmcp serializes an `Err(ErrorData)` returned from a
/// tool handler as a `-32603` protocol error, which the model never sees, so it cannot
/// fall back. A handler that returns `Err(ToolFailure)` instead produces a
/// `CallToolResult{is_error: true}` whose text carries the cause and what to do next:
/// rmcp's `IntoCallToolResult for Result<T, E>` flips `is_error` to true whenever the
/// error type's own `into_call_tool_result` returns `Ok` (which it does here, via the
/// blanket `IntoContents` impl). Success paths keep returning `Ok(Json(..))` unchanged.
#[derive(Debug)]
pub struct ToolFailure {
    message: String,
}

impl ToolFailure {
    /// A recoverable failure (a transient index/graph lock, or an auto-index/auto-build
    /// that errored): the cause plus an explicit instruction to fall back and not to
    /// kill the server, so the model recovers within the same turn instead of stalling.
    fn recoverable(cause: impl std::fmt::Display) -> Self {
        ToolFailure {
            message: format!(
                "lens tool failed: {cause}\n\
                 This is usually a transient index/graph lock, not a crash. Fall back to \
                 grep/Read for this query and continue. Do NOT kill the lens MCP server or \
                 other lens processes; the lock clears on its own."
            ),
        }
    }

    /// A failure whose cause is the caller's to fix (e.g. a bad argument), delivered as
    /// readable `is_error` content with no fallback instruction.
    fn plain(message: impl Into<String>) -> Self {
        ToolFailure {
            message: message.into(),
        }
    }
}

/// Any internal `ErrorData` (from the index/graph helpers) becomes a recoverable
/// failure, so `?` at a handler boundary delivers it as content the model can act on.
impl From<ErrorData> for ToolFailure {
    fn from(e: ErrorData) -> Self {
        ToolFailure::recoverable(e.message)
    }
}

impl IntoContents for ToolFailure {
    fn into_contents(self) -> Vec<Content> {
        vec![Content::text(self.message)]
    }
}

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

/// Resolve the repo root the server indexes and graphs against, independent of the
/// process cwd at MCP-server-spawn time. `setup::register_mcp` runs `claude mcp add`
/// without pinning a cwd, so the server previously inherited whatever directory the
/// spawning process happened to be in -- verified in practice to sometimes land on an
/// unrelated parent workspace (e.g. one that also contains other, unrelated repos).
///
/// Thin wrapper over [`resolve_repo_root_from`], which owns the actual resolution
/// order and doc; split out so that function is unit-testable without mutating
/// process-global env vars or cwd (see its doc for why that split exists).
fn resolve_repo_root() -> PathBuf {
    let lens_dir = std::env::var_os("LENS_DIR").map(PathBuf::from);
    let claude_project_dir = std::env::var_os("CLAUDE_PROJECT_DIR").map(PathBuf::from);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    resolve_repo_root_from(lens_dir.as_deref(), claude_project_dir.as_deref(), &cwd)
}

/// Resolution order for [`resolve_repo_root`], first match wins:
///
/// 1. `lens_dir` (`$LENS_DIR`) present AND shaped like `<repo_root>/.lens` (its last
///    component is literally `.lens`) -> the PARENT directory. Interpretation call:
///    the locked plan orders `$LENS_DIR` ahead of `$CLAUDE_PROJECT_DIR` but doesn't say
///    what to DO with its value once present. Taking the parent matches the one
///    existing convention for `$LENS_DIR`'s shape (`crate::obs::data_dir` and
///    `session::resolve_data_dir` both build it as `<repo_root>/.lens`), so an explicit
///    data-dir override that follows the convention is the strongest available signal
///    of the intended repo root.
///
///    The `.lens`-suffix gate itself is a REVISION of the first cut of this
///    interpretation (unconditional parent-of-`$LENS_DIR`), made after that version
///    broke `tests/e2e_tests.rs` (confirmed by reverting this change and re-running:
///    all 4 e2e tests pass on `main`, 3 fail with the unconditional version). Those
///    tests -- unowned by any task in this plan, so out of scope to edit -- spawn the
///    real server with `$LENS_DIR` pointed at an arbitrary tempdir used purely to
///    isolate index/graph state, entirely unrelated to the separately-pinned repo
///    directory (`Command::current_dir`). Taking that arbitrary dir's parent as
///    "the repo root" is wrong; gating on the `.lens` convention makes the two uses
///    (a real `<repo>/.lens` override vs. an arbitrary isolated data dir) distinguishable,
///    and falls through to the next signal for the latter instead of guessing.
/// 2. Else `claude_project_dir` (`$CLAUDE_PROJECT_DIR`) present AND names a directory
///    that exists on disk -> used directly. This is how Claude Code's own env
///    naturally flows through at runtime; `setup::register_mcp` needs no explicit
///    `--cwd`/`--env` registration for it.
/// 3. Else walk up from `cwd` toward the filesystem root; the nearest ancestor
///    (including `cwd` itself) that owns a `.git` entry wins. Checked via `.exists()`,
///    not `.is_dir()`: a git worktree's `.git` is a FILE (a gitdir pointer), not a
///    directory.
/// 4. Else (no `.git` found above `cwd`) -> `cwd` as-is (pre-fix behavior).
///
/// Takes its inputs as parameters (rather than reading the environment/cwd itself) so
/// the decision logic is exercisable with fabricated `tempfile::tempdir()` trees under
/// `cargo test`'s parallel runner: mutating the real `$LENS_DIR` in-process is known
/// unsafe in this codebase specifically (`session::resolve_data_dir` is read unguarded
/// by `session::hook`'s own unit tests), and mutating the real process cwd would be a
/// process-global race against every other concurrently running test, guarded or not.
fn resolve_repo_root_from(
    lens_dir: Option<&Path>,
    claude_project_dir: Option<&Path>,
    cwd: &Path,
) -> PathBuf {
    let conventional_lens_dir = lens_dir.filter(|d| d.ends_with(".lens"));
    if let Some(root) = conventional_lens_dir.and_then(Path::parent) {
        return root.to_path_buf();
    }
    if let Some(dir) = claude_project_dir {
        if dir.is_dir() {
            return dir.to_path_buf();
        }
    }
    for dir in cwd.ancestors() {
        if dir.join(".git").exists() {
            return dir.to_path_buf();
        }
    }
    cwd.to_path_buf()
}

// Kept as its own `#[cfg(test)] mod` (rather than folded into the big `mod tests`
// below) so this T4 change stays a self-contained diff next to the code it tests,
// touching nothing in the existing test module.
#[cfg(test)]
mod resolve_repo_root_tests {
    use super::*;

    /// (a) `$LENS_DIR` present -> its parent directory wins, even over a
    /// simultaneously-present `$CLAUDE_PROJECT_DIR`.
    #[test]
    fn lens_dir_parent_wins_over_everything_else() {
        let tmp = tempfile::tempdir().unwrap();
        let lens_dir = tmp.path().join(".lens");
        let got =
            resolve_repo_root_from(Some(&lens_dir), Some(tmp.path()), Path::new("/unused"));
        assert_eq!(got, tmp.path());
    }

    /// Regression: an `$LENS_DIR` that ISN'T shaped like `<repo_root>/.lens` (an
    /// arbitrary data-dir override, unrelated to repo location -- exactly how
    /// `tests/e2e_tests.rs` uses it to isolate index/graph state per test) must fall
    /// through to the next signal, not have its parent used as a bogus repo root. This
    /// is the exact case that broke those e2e tests under the first cut of this
    /// function (unconditional parent-of-`$LENS_DIR`); see the doc comment above.
    #[test]
    fn lens_dir_not_shaped_like_dot_lens_falls_through() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        // An arbitrary tempdir, deliberately NOT named `.lens` and NOT a child of `repo`
        // -- mirrors `tests/e2e_tests.rs`'s independent `repo`/`data` tempdir pair.
        let arbitrary_data_dir = tmp.path().join("some-random-tempdir-name");

        let got = resolve_repo_root_from(Some(&arbitrary_data_dir), None, &repo);
        assert_eq!(
            got, repo,
            "non-`.lens`-shaped LENS_DIR must not be treated as a repo-root signal"
        );
    }

    /// (b) `$CLAUDE_PROJECT_DIR` present and names a real directory -> used directly.
    #[test]
    fn claude_project_dir_used_when_it_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let got = resolve_repo_root_from(None, Some(tmp.path()), Path::new("/unused"));
        assert_eq!(got, tmp.path());
    }

    /// (c) `$CLAUDE_PROJECT_DIR` set but pointing at nothing, and separately unset,
    /// both fall through to the `.git` walk rather than being used as-is.
    #[test]
    fn claude_project_dir_falls_through_when_nonexistent_or_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        let missing = tmp.path().join("does-not-exist");
        let got = resolve_repo_root_from(None, Some(&missing), &repo);
        assert_eq!(got, repo, "nonexistent CLAUDE_PROJECT_DIR must not be used as-is");

        let got_unset = resolve_repo_root_from(None, None, &repo);
        assert_eq!(got_unset, repo, "unset CLAUDE_PROJECT_DIR falls through the same way");
    }

    /// (d) Neither env var present: walk up from a deep cwd to the nearest ancestor
    /// owning a `.git` ENTRY. Uses a `.git` FILE (worktree-style gitdir pointer, not a
    /// directory) to prove the check is `.exists()`, not `.is_dir()`.
    #[test]
    fn walks_up_to_nearest_git_ancestor_file_or_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join(".git"), "gitdir: ../elsewhere/.git/worktrees/x\n").unwrap();
        let deep_cwd = repo.join("src").join("nested").join("deep");
        std::fs::create_dir_all(&deep_cwd).unwrap();

        let got = resolve_repo_root_from(None, None, &deep_cwd);
        assert_eq!(got, repo);
    }

    /// (e) Neither env var present and no `.git` anywhere above cwd: fall back to cwd
    /// as-is (pre-fix behavior). Relies on the OS temp root's own ancestry not
    /// containing a stray `.git`, true of any normal dev/CI machine.
    #[test]
    fn falls_back_to_cwd_when_no_git_found() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("no").join("git").join("here");
        std::fs::create_dir_all(&cwd).unwrap();

        let got = resolve_repo_root_from(None, None, &cwd);
        assert_eq!(got, cwd);
    }

    /// Wiring smoke test: the zero-arg `resolve_repo_root()` (what `Forge::new` calls)
    /// actually reads the real `$CLAUDE_PROJECT_DIR` and delegates, not just the pure
    /// core above. Deliberately does NOT exercise `$LENS_DIR` here: mutating it
    /// in-process is known unsafe in this codebase (`session::resolve_data_dir` is read
    /// unguarded by `session::hook`'s own unit tests, per the convention documented on
    /// `rtk::gain`'s `sync_child` test), so that branch is proven by the pure-function
    /// test above only. Guarded anyway for hygiene against any future test that also
    /// touches `$CLAUDE_PROJECT_DIR`.
    static REPO_ROOT_WRAPPER_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn wrapper_reads_real_claude_project_dir() {
        let _guard = REPO_ROOT_WRAPPER_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var_os("CLAUDE_PROJECT_DIR");
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("CLAUDE_PROJECT_DIR", tmp.path());

        let got = resolve_repo_root();

        match prior {
            Some(v) => std::env::set_var("CLAUDE_PROJECT_DIR", v),
            None => std::env::remove_var("CLAUDE_PROJECT_DIR"),
        }
        assert_eq!(got, tmp.path());
    }
}

impl Forge {
    /// Build the handler, resolving paths from the environment.
    pub fn new() -> anyhow::Result<Self> {
        let repo_dir = resolve_repo_root();
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
        let index = Index::open(&data_dir)?.with_repo_root(&repo_dir);
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
    /// The raw data the script reads never enters context. With `path`, the file
    /// is injected as the script's first CLI argument (the old `lens_run_file`).
    /// Large output is offloaded to the reversible store and replaced with a
    /// preview + ref.
    #[tool(
        description = "Run code (python|javascript|typescript|bash|ruby|go) in a darkroom; only the script's stdout/stderr returns to context, not the data it processed. Pass `path` to analyze a file: it arrives as the script's first CLI argument (python sys.argv[1] / node process.argv[2] / bash $1), so the file's contents never enter context either. Large output is offloaded and retrievable via lens_recall. Compose against the live repo: inside your script, `import lens` (python) or `import('./lens.mjs')` (js) exposes search/symbol/callers/callees/path/skeleton/grep_ast/overview/recall against this repo's live index and graph; compose and print only the answer."
    )]
    async fn lens_run(
        &self,
        Parameters(req): Parameters<ExecuteRequest>,
    ) -> Result<Json<ExecuteResponse>, ToolFailure> {
        let op = self.ops.start(
            "lens_run",
            serde_json::json!({ "language": req.language, "path": req.path, "code_bytes": req.code.len() }),
        );
        // Piped stdin never enters context either (the whole point of `lens_run`
        // over pasting data inline); credit it like a volunteered op, capped at
        // the plausible-slice floor since stdin has no path/extension for the
        // classifier to reason about. A no-stdin run that reads files inline
        // stays uncredited by design (raw_in == returned).
        let stdin_credit = req
            .stdin
            .as_ref()
            .map_or(0u64, |s| (s.len() as u64).min(obs::credit::vol_floor()));
        // With `path`, the analyzed file's bytes never enter context either (the
        // whole point over Read), so they count as processed-but-saved: raw_in =
        // file size + the script's stdout. Without this a file analysis that
        // prints a small answer records raw == returned and zero savings.
        let file = req.path.as_ref().map(|p| self.resolve_unescaped(p));
        let file_size = file
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .map_or(0, |m| m.len());
        let result = match &file {
            Some(p) => darkroom::run_file(p, req, &self.repo_dir, &self.store, self.max_inline).await,
            None => darkroom::run(req, &self.repo_dir, &self.store, self.max_inline).await,
        };
        match result {
            Ok(resp) => {
                let raw_in = resp.stdout_bytes as u64 + stdin_credit + file_size;
                // Mirror the file credit into the persistent savings counter (the
                // darkroom already counted stdout; add the file bytes).
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
                Err(ToolFailure::recoverable(e))
            }
        }
    }

    /// Skeletonize a source file: signatures + nesting, executable bodies elided,
    /// the full text stored so any body is one `lens_recall` away.
    #[tool(
        description = "Show a source file's structure cheaply: signatures, types, and nesting with executable bodies elided to `…`. Far fewer tokens than reading the whole file, and the full text is stored so any elided body is one lens_recall away (use the returned retrieve_ref). Pass `include_bodies` with definition names to get those bodies back verbatim in the same response, without a second call. Line-number prefixes (`L{n}:`) are included by default for exact citations; pass `with_lines: false` to omit them. A skeleton too large for the response budget comes back truncated (`truncated: true`) with a `skeleton_ref` to fetch the full skeleton text via lens_recall. Do not Read a code file just to see its structure — use this first; use Read only when about to Edit."
    )]
    async fn lens_skeleton(
        &self,
        Parameters(req): Parameters<SkeletonRequest>,
    ) -> Result<Json<SkeletonResponse>, ToolFailure> {
        let op = self
            .ops
            .start("lens_skeleton", serde_json::json!({ "path": req.path }));
        let p = self.resolve_unescaped(&req.path);
        let content = match std::fs::read_to_string(&p) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("read {}: {e}", p.display());
                op.finish(0, 0, None, "error", msg.clone(), None);
                return Err(ToolFailure::recoverable(msg));
            }
        };
        let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("");
        let Some(spec) = crate::discovery::extract::spec_for_extension(ext) else {
            let msg = format!(
                "no skeleton for {} (unsupported language '.{ext}'); use Read",
                p.display()
            );
            op.finish(0, 0, None, "error", msg.clone(), None);
            return Err(ToolFailure::recoverable(msg));
        };
        let language = spec.name.to_string();
        let Some(skeleton) = crate::discovery::skeleton::skeletonize(
            &content,
            &spec,
            req.include_bodies.as_deref(),
            req.with_lines.unwrap_or(true),
        ) else {
            let msg = format!("could not parse {} for skeleton; use Read", p.display());
            op.finish(0, 0, None, "error", msg.clone(), None);
            return Err(ToolFailure::recoverable(msg));
        };
        // Stash the full file so any elided body is recoverable; surface a cheap
        // short handle (Store::get resolves prefixes) instead of the 64-char hash.
        let reference = match self.store.put(&content) {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("store {}: {e}", p.display());
                op.finish(0, 0, None, "error", msg.clone(), None);
                return Err(ToolFailure::recoverable(msg));
            }
        };
        // Remember which file this snapshot came from, so a later edit to it can
        // be surfaced: lens_recall flags the ref stale and the session hook posts
        // a supersession notice. Best-effort — losing the staleness signal must
        // not lose the skeleton (mirrors the bump_stat credit below).
        let _ = self.store.record_source(&reference, &p.to_string_lossy());
        let retrieve_ref = reference[..reference.len().min(12)].to_string();
        let raw_in = content.len() as u64;
        // Budget the skeleton itself, mirroring `maybe_compact`'s inline-cap mechanism
        // for graph views: a skeleton whose signatures alone overflow the response cap
        // (mined defect, 21 cases: large files blew the 25k client cap) gets a
        // budgeted head plus a ref to the full skeleton text, instead of being
        // returned raw and unbounded.
        let (skeleton, truncated, skeleton_ref) = if skeleton.len() > self.max_inline {
            let full_ref = self.store.put(&skeleton).ok();
            let short_ref = full_ref.map(|r| r[..r.len().min(12)].to_string());
            (truncate_skeleton(&skeleton, self.max_inline), true, short_ref)
        } else {
            (skeleton, false, None)
        };
        let returned = skeleton.len() as u64;
        // The file bytes were processed but kept out of context; credit the savings
        // counter the stats CLI reads (mirrors lens_run's path-form file-size credit).
        let _ = self.store.bump_stat("raw_bytes_processed", raw_in as i64);
        let explain = self.ops.explain(|| {
            format!(
                "skeletonized {} ({language}): {raw_in} -> {returned} bytes; full text at ref {retrieve_ref}{}",
                p.display(),
                if truncated { "; skeleton itself budgeted" } else { "" }
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
            truncated,
            skeleton_ref,
        }))
    }

    /// Fetch a full blob previously offloaded to the reversible store.
    #[tool(
        description = "Retrieve the full content for a retrieve_ref returned by another tool (reverses any truncation/compression). Optional `offset`/`limit` (1-based lines) and `grep` (substring filter, applied first) slice a large ref instead of returning it all at once. If the blob snapshots a file that has since changed or been deleted, the response carries a one-line `stale` warning naming the file."
    )]
    async fn lens_recall(
        &self,
        Parameters(req): Parameters<RetrieveRequest>,
    ) -> Result<Json<RetrieveResponse>, ToolFailure> {
        let op = self.ops.start(
            "lens_recall",
            serde_json::json!({ "ref": req.reference, "offset": req.offset, "limit": req.limit, "grep": req.grep }),
        );
        match self.store.get(&req.reference) {
            Ok(Some(content)) => {
                let stale = self.stale_note(&req.reference);
                let (content, sliced) =
                    slice_content(&content, req.offset, req.limit, req.grep.as_deref());
                // Retrieve is the inverse of offloading (expansion), so it saves
                // nothing: raw_in == returned keeps tokens_saved_est at 0.
                let bytes = content.len() as u64;
                let explain = self.ops.explain(|| {
                    format!(
                        "expanded ref {} to {} bytes{}",
                        req.reference,
                        bytes,
                        if sliced { " (sliced)" } else { "" }
                    )
                });
                op.finish(
                    bytes,
                    bytes,
                    Some(req.reference.clone()),
                    "ok",
                    "blob expanded from store",
                    explain,
                );
                Ok(Json(RetrieveResponse { content, stale, sliced }))
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
                Err(ToolFailure::plain(format!(
                    "unknown ref '{}'",
                    req.reference
                )))
            }
            Err(e) => {
                op.finish(0, 0, None, "error", e.to_string(), None);
                Err(ToolFailure::recoverable(e.to_string()))
            }
        }
    }

    /// Search the full-text index with one or more queries. The index itself is
    /// auto-built and kept fresh per query (`ensure_index`); there is no explicit
    /// index tool.
    #[tool(
        description = "Full-text search across all indexed content (BM25-ranked; the index is auto-built and kept fresh, no setup call needed): finds where a string, idea, or usage appears anywhere, including inside function bodies, comments, strings, and config; returns ranked snippets per query, each with path, match line, and the definition names the hit's chunk carries (`symbols` — often the answer to a which-function-does-X question). When the query names a symbol, its full definition is returned as the top hit (resolved via the graph), answering a symbol lookup in one call. The only tool that sees inside bodies and finds call-sites/usages. For a named symbol's connections use lens_symbol (it also resolves by meaning when you don't know the exact name)."
    )]
    async fn lens_search(
        &self,
        Parameters(req): Parameters<SearchRequest>,
    ) -> Result<Json<SearchResponse>, ToolFailure> {
        let op = self.ops.start(
            "lens_search",
            serde_json::json!({ "queries": req.queries.len(), "limit_per_query": req.limit_per_query }),
        );
        if let Err(e) = self.ensure_index() {
            op.finish(0, 0, None, "error", "auto-index failed", None);
            return Err(e.into());
        }
        let file_ranks = self.file_ranks();
        match self
            .index
            .search_fused(&req.queries, req.limit_per_query, &file_ranks)
        {
            Ok(mut resp) => {
                self.federate_nested_search(&mut resp, &req.queries, req.limit_per_query);
                let targets = self.symbol_fetch_targets(&req.queries);
                self.inject_symbol_defs(&mut resp, &targets, req.limit_per_query);
                let returned = obs::json_len(&resp);
                let hits: usize = resp.results.iter().map(|r| r.hits.len()).sum();
                let note = format!("{} queries, {} hits", resp.results.len(), hits);
                let explain = self.ops.explain(|| note.clone());
                op.finish(returned, returned, None, "ok", note, explain);
                Ok(Json(resp))
            }
            Err(e) => {
                op.finish(0, 0, None, "error", e.to_string(), None);
                Err(ToolFailure::recoverable(e.to_string()))
            }
        }
    }

    /// Find symbols by name and return their immediate connections. On zero
    /// substring matches, falls back to the blend-ranked lexical find (the old
    /// `lens_find` engine); `matched_via` reports which path produced the result.
    /// The graph is auto-built and kept fresh per query (`ensure_graph`).
    #[tool(
        description = "Look up a declared symbol by name substring (case-insensitive; + optional kind); returns each match's location plus its immediate connections (calls, contains, imports, ...), NOT its source body (to read the code, lens_search the name). Zero substring matches fall back to a blend-ranked match by meaning over symbol names (no embeddings; exact > prefix > word-boundary token, plus a multi-word bonus), so a natural-language description still resolves; `matched_via` in the response says which path won (\"name\" | \"meaning\"). When a result contains test/bench code, every node carries `origin` (prod/test/bench), so test-only callers can be excluded without opening files; no `origin` fields = all production code. `limit` bounds the number of matching root symbols, not the total nodes returned — each root's neighbors come back on top of it. No match on either path returns an empty result, not an error. An exact-name match among several candidates is reported via `resolved` (chosen node + how many others it beat). Large results are compacted with a lens_recall ref."
    )]
    async fn lens_symbol(
        &self,
        Parameters(req): Parameters<GraphQueryRequest>,
    ) -> Result<Json<GraphView>, ToolFailure> {
        let op = self.ops.start(
            "lens_symbol",
            serde_json::json!({ "name": req.name, "kind": req.kind, "limit": req.limit }),
        );
        let graph = match self.load_graph() {
            Ok(g) => g,
            Err(e) => {
                op.finish(0, 0, None, "error", "graph build failed", None);
                return Err(e.into());
            }
        };
        // Session-proximity boost: symbols defined in files the user recently
        // touched sort first. Best-effort — an empty list leaves ranking unchanged.
        let recent = self.recent_touched_files();
        let mut view = gquery::query(&graph, &req.name, req.kind.as_deref(), req.limit, &recent);
        // Zero root matches (nodes always include every matched root, so an empty
        // list is exactly "no substring match"): fall through to the L36
        // blend-ranked find path, so a meaning-shaped query still resolves.
        // `kind` stays applied on the fallback via `find_kind`, the same seam the
        // absorbed `lens_find` threaded it through; dropping it would let a
        // wrong-kind symbol satisfy a kind-constrained query.
        let matched_via = if view.nodes.is_empty() {
            view = gquery::find_kind(&graph, &req.name, req.limit, req.kind.as_deref());
            "meaning"
        } else {
            "name"
        };
        view.matched_via = Some(matched_via.to_string());
        let raw_payload = view_payload_len(&view);
        let compacted = self.maybe_compact(view);
        self.record_graph_op(op, raw_payload, &compacted);
        Ok(Json(compacted))
    }

    /// Graph connections for a symbol: with `to`, the shortest directed path
    /// between the two (the old `lens_path`); without, the local subgraph around
    /// `node` (the old `lens_links`), or — with `transitive: true` — the
    /// complete directed closure with per-node witnesses (T3). Natural-signature
    /// dispatch, no mode enum; each form returns its natural shape unchanged.
    /// The graph is auto-built and kept fresh per query (`ensure_graph`).
    #[tool(
        description = "Graph connections for a symbol (by node id or name; the graph is auto-built and kept fresh, no setup call needed). With `to`: the shortest directed path from `node` to `to` via BFS over calls/imports edges — no path (or an unresolvable end) returns `found: false`, not an error, and an ambiguous name is reported via `resolved` (chosen node + how many others it beat). Without `to`: the local subgraph within `depth` hops of `node`, walking `direction` \"callers\" (fan-in), \"callees\" (fan-out), or \"both\" (undirected, the default) — a `node` that resolves to nothing is an explicit error, never an empty graph. Set `transitive: true` (no `to`) for the COMPLETE directed closure instead of a one-hop-at-a-time neighborhood: every node reachable within `depth` hops strictly following `direction` (\"callers\" or \"callees\" only — \"both\" is rejected, a closure has no undirected sense), each carrying a `witness` (the call-site `file:line` proving the edge on its shortest path back to `node`, so the edge is provable, not just asserted) and the response asserting `complete: true` for the given `depth` plus `count_total`/`count_prod` — one call answers a multi-hop reachability question with proof instead of chaining atomic calls. `prod_only` filters the reported node list to production-origin nodes (the counts always report both). `transitive: true` together with `to` is rejected: a closure has no destination. When a result contains test/bench code, every node carries `origin` (prod/test/bench), so test-only callers can be excluded without opening files; no `origin` fields = all production code. Large neighborhoods are budget-trimmed and compacted, with the full subgraph recoverable via the lens_recall ref."
    )]
    async fn lens_graph(
        &self,
        Parameters(req): Parameters<GraphRequest>,
    ) -> Result<Json<GraphResponse>, ToolFailure> {
        let op = self.ops.start(
            "lens_graph",
            serde_json::json!({
                "node": req.node,
                "to": req.to,
                "depth": req.depth,
                "direction": req.direction,
                "transitive": req.transitive,
                "prod_only": req.prod_only,
            }),
        );
        let graph = match self.load_graph() {
            Ok(g) => g,
            Err(e) => {
                op.finish(0, 0, None, "error", "graph build failed", None);
                return Err(e.into());
            }
        };
        // `transitive` and `to` are mutually exclusive request shapes: a closure
        // claims the complete reach with no destination, `to` asks for the
        // shortest path between two specific nodes.
        if req.transitive && req.to.is_some() {
            let msg = "lens_graph: `transitive` and `to` are mutually exclusive -- a \
                       transitive closure has no destination, `to` asks for a shortest \
                       path between two specific nodes"
                .to_string();
            op.finish(0, 0, None, "error", msg.clone(), None);
            return Err(ToolFailure::plain(msg));
        }
        // `transitive: true`, no `to`: the complete directed closure with witnesses.
        if req.transitive {
            let direction = match req.direction.as_deref() {
                Some("callers") => crate::discovery::graph::Direction::Callers,
                Some("callees") => crate::discovery::graph::Direction::Callees,
                _ => crate::discovery::graph::Direction::Both,
            };
            return match gquery::transitive_closure(&graph, &req.node, direction, req.depth, req.prod_only)
            {
                Ok(closure) => {
                    let returned = obs::json_len(&closure);
                    let note = format!(
                        "complete={}, count_total={}, count_prod={}",
                        closure.complete, closure.count_total, closure.count_prod
                    );
                    let explain = self.ops.explain(|| note.clone());
                    op.finish(returned, returned, None, "ok", note, explain);
                    Ok(Json(GraphResponse::Closure(closure)))
                }
                Err(e) => {
                    op.finish(0, 0, None, "error", e.to_string(), None);
                    Err(ToolFailure::plain(e.to_string()))
                }
            };
        }
        // `to` present: shortest directed path, exactly the old `lens_path`.
        if let Some(to) = req.to.as_deref() {
            let resp = gquery::path(&graph, &req.node, to);
            let returned = obs::json_len(&resp);
            let note = format!("found={}, hops={}", resp.found, resp.path.len());
            let explain = self.ops.explain(|| note.clone());
            op.finish(returned, returned, None, "ok", note, explain);
            return Ok(Json(GraphResponse::Path(resp)));
        }
        // `to` absent: neighborhood walk, exactly the old `lens_links`.
        // Resolve a symbol NAME the same way the path form resolves its ends,
        // before falling back to treating `node` as a raw graph id. Neither
        // resolving is an explicit error, never a silent empty graph (the
        // measured defect: an unknown raw id used to yield an empty subgraph).
        let Some(id) = gquery::resolve(&graph, &req.node) else {
            let msg = format!(
                "no node found for '{}': not a known node id and no symbol matches that name",
                req.node
            );
            op.finish(0, 0, None, "error", msg.clone(), None);
            return Err(ToolFailure::plain(msg));
        };
        let requested_depth = req.depth;
        let mut depth = requested_depth;
        let mut view = gquery::neighbors_dir(&graph, &id, depth, req.direction.as_deref());
        let raw_payload = view_payload_len(&view);
        // The dictionary compaction `maybe_compact` applies below isn't a hard cap: a
        // subgraph with little name repetition can still overflow the response budget
        // after compacting (mined defect: a compacted depth-10 subgraph still over the
        // client's response cap). Shrink depth first, then breadth, until the
        // compacted form actually fits.
        let mut depth_trimmed = false;
        while depth > 1 && compacted_len(&view) > self.max_inline {
            depth -= 1;
            view = gquery::neighbors_dir(&graph, &id, depth, req.direction.as_deref());
            depth_trimmed = true;
        }
        let nodes_before_breadth_trim = view.nodes.len();
        while view.nodes.len() > 1 && compacted_len(&view) > self.max_inline {
            let keep = (view.nodes.len() / 2).max(1);
            view.nodes.truncate(keep);
            let kept: HashSet<&str> = view.nodes.iter().map(|n| n.id.as_str()).collect();
            view.edges
                .retain(|e| kept.contains(e.from.as_str()) && kept.contains(e.to.as_str()));
        }
        let nodes_after_breadth_trim = view.nodes.len();
        let breadth_trimmed = nodes_after_breadth_trim < nodes_before_breadth_trim;

        let mut compacted = self.maybe_compact(view);
        if depth_trimmed || breadth_trimmed {
            let mut parts = Vec::new();
            if depth_trimmed {
                parts.push(format!("depth {requested_depth} -> {depth}"));
            }
            if breadth_trimmed {
                parts.push(format!(
                    "nodes {nodes_before_breadth_trim} -> {nodes_after_breadth_trim}"
                ));
            }
            compacted.trim_note = Some(format!(
                "response budget trim: {}; full depth-{requested_depth} subgraph via retrieve_ref",
                parts.join(", ")
            ));
            compacted.truncated = true;
            // Point retrieve_ref at the FULL requested-depth subgraph (not whatever
            // `maybe_compact` stored for the already-trimmed view above), so "the rest"
            // is always recoverable regardless of how much depth/breadth was cut.
            let full = gquery::neighbors_dir(&graph, &id, requested_depth, req.direction.as_deref());
            let full_json =
                serde_json::json!({ "nodes": full.nodes, "edges": full.edges }).to_string();
            if let Ok(r) = self.store.put(&full_json) {
                compacted.retrieve_ref = Some(r);
            }
        }
        self.record_graph_op(op, raw_payload, &compacted);
        Ok(Json(GraphResponse::Neighbors(compacted)))
    }

    /// A token-budgeted map of the repo's most important symbols.
    #[tool(
        description = "Get a token-budgeted overview of the repo: the most structurally important symbols (PageRank-ranked) with their callers/callees, as much as fits a token budget (default 2000). A high-signal map of a codebase at fixed cost instead of reading files. Pass an optional query to focus the map on a topic: symbols whose names match it, and files you have touched this session, are boosted into the budget. For one file's structure use lens_skeleton; this is the whole-repo ranked map."
    )]
    async fn lens_overview(
        &self,
        Parameters(req): Parameters<OverviewRequest>,
    ) -> Result<Json<OverviewResponse>, ToolFailure> {
        let op = self.ops.start(
            "lens_overview",
            serde_json::json!({ "token_budget": req.token_budget }),
        );
        let graph = match self.load_graph() {
            Ok(g) => g,
            Err(e) => {
                op.finish(0, 0, None, "error", "graph build failed", None);
                return Err(e.into());
            }
        };
        let seed =
            gquery::overview_seed(&graph, &self.recent_touched_files(), req.query.as_deref());
        let overview = gquery::overview(&graph, req.token_budget, &seed);
        let resp = OverviewResponse { overview };
        let returned = obs::json_len(&resp);
        let note = format!("{} bytes", resp.overview.len());
        let explain = self.ops.explain(|| note.clone());
        op.finish(returned, returned, None, "ok", note, explain);
        Ok(Json(resp))
    }

    /// Structural (tree-sitter) search: run an AST query, get path:line matches.
    #[tool(
        description = "Structural code search via a tree-sitter query (S-expression): matches syntax, not text, so it finds e.g. real `.unwrap()` calls or functions returning Result without the false positives grep hits in comments/strings. Returns one deduplicated path:line match per distinct call/pattern site (a call matched more than once internally, e.g. once per extra argument, still surfaces once). `limit` caps the underlying scan of raw captures before dedup, so `truncated: true` can still return fewer than `limit` matches. No match returns an empty result, not an error. When a result contains test/bench code, every match carries `origin` (prod/test/bench: `#[cfg(test)]` spans and bench/fixture paths, the graph's own provenance rules); no `origin` fields = all production code. `prod_only: true` drops non-prod matches before they count toward `limit` — use it for any 'excluding tests' count. For plain-text/idea search use lens_search; this is for syntax-shape matches."
    )]
    async fn lens_grep_ast(
        &self,
        Parameters(req): Parameters<GrepAstRequest>,
    ) -> Result<Json<GrepAstResponse>, ToolFailure> {
        // Which input path drives this call: a raw S-expression or a $META pattern.
        let mode = match (&req.query, &req.pattern) {
            (Some(_), None) => "query",
            (None, Some(_)) => "pattern",
            _ => "invalid",
        };
        let op = self.ops.start(
            "lens_grep_ast",
            serde_json::json!({
                "path": req.path, "language": req.language, "limit": req.limit, "mode": mode,
                "prod_only": req.prod_only,
            }),
        );
        // Resolve to a tree-sitter query: raw queries pass through; a pattern is
        // compiled, and only its `@match` capture may surface as results.
        let resolved: Result<(String, Option<&'static str>), String> =
            match (&req.query, &req.pattern) {
                (Some(q), None) => Ok((q.clone(), None)),
                (None, Some(p)) => match req.language.as_deref() {
                    None => Err("pattern requires language".into()),
                    Some(lang) => {
                        match crate::discovery::tags_adapter::any_spec_for_language(lang) {
                            None => Err(format!("unsupported language '{lang}'")),
                            Some(spec) => crate::discovery::pattern::compile_pattern(p, &spec)
                                .map(|q| (q, Some(crate::discovery::pattern::MATCH_CAPTURE)))
                                .map_err(|e| e.to_string()),
                        }
                    }
                },
                (Some(_), Some(_)) => {
                    Err("set exactly one of `query` or `pattern` (both were given)".into())
                }
                (None, None) => {
                    Err("set exactly one of `query` or `pattern` (neither was given)".into())
                }
            };
        let (query, only_capture) = match resolved {
            Ok(v) => v,
            Err(msg) => {
                op.finish(0, 0, None, "error", msg.clone(), None);
                return Err(ToolFailure::plain(msg));
            }
        };
        let root = self.resolve_unescaped(&req.path);
        match crate::discovery::structural::grep_ast_filtered(
            &root,
            &query,
            req.language.as_deref(),
            req.limit,
            only_capture,
            req.prod_only,
        ) {
            Ok(matches) => {
                // grep_ast_filtered dedupes capture sites before counting toward
                // `limit`, so hitting `limit` means `limit` DISTINCT matches —
                // `truncated: true` can no longer be an artifact of duplicate
                // captures crowding out real sites.
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
                Err(ToolFailure::recoverable(e.to_string()))
            }
        }
    }

    /// Record a durable project-memory item (decision/constraint/rejected-approach/
    /// rule), carried across sessions unlike the live per-session event log.
    #[tool(
        description = "Record durable project memory (category: decision | constraint | rejected-approach | rule) that survives across sessions, unlike the live event log. Also indexed for lens_search under session://memory/<category>."
    )]
    async fn lens_memory_record(
        &self,
        Parameters(req): Parameters<MemoryRecordRequest>,
    ) -> Result<Json<MemoryRecordResponse>, ToolFailure> {
        let op = self.ops.start(
            "lens_memory_record",
            serde_json::json!({ "category": req.category }),
        );
        let session_store = match SessionStore::open(&self.data_dir) {
            Ok(s) => s,
            Err(e) => {
                op.finish(0, 0, None, "error", e.to_string(), None);
                return Err(ToolFailure::recoverable(e.to_string()));
            }
        };
        let project = self.repo_dir.to_string_lossy().to_string();
        match session::record_memory(&session_store, &self.index, &project, &req.category, &req.text)
        {
            Ok(()) => {
                let raw_in = req.text.len() as u64;
                let note = format!("recorded {} memory item", req.category);
                let explain = self.ops.explain(|| note.clone());
                op.finish(raw_in, 0, None, "ok", note, explain);
                Ok(Json(MemoryRecordResponse {
                    recorded: true,
                    category: req.category,
                }))
            }
            Err(e) => {
                op.finish(0, 0, None, "error", e.to_string(), None);
                Err(ToolFailure::plain(e.to_string()))
            }
        }
    }

    /// Read durable project memory, optionally ranked by relevance to a query.
    #[tool(
        description = "Read durable project memory (decisions, constraints, rejected approaches, rules) recorded across sessions. Give `query` to rank by token overlap; omit it for the full list, newest last."
    )]
    async fn lens_memory_query(
        &self,
        Parameters(req): Parameters<MemoryQueryRequest>,
    ) -> Result<Json<MemoryQueryResponse>, ToolFailure> {
        let op = self.ops.start(
            "lens_memory_query",
            serde_json::json!({ "query": req.query, "limit": req.limit }),
        );
        let session_store = match SessionStore::open(&self.data_dir) {
            Ok(s) => s,
            Err(e) => {
                op.finish(0, 0, None, "error", e.to_string(), None);
                return Err(ToolFailure::recoverable(e.to_string()));
            }
        };
        let project = self.repo_dir.to_string_lossy().to_string();
        match session::query_memory(&session_store, &project, req.query.as_deref(), req.limit) {
            Ok(items) => {
                let resp = MemoryQueryResponse {
                    items: items
                        .into_iter()
                        .map(|(category, text)| MemoryItem { category, text })
                        .collect(),
                };
                let returned = obs::json_len(&resp);
                let note = format!("{} items", resp.items.len());
                let explain = self.ops.explain(|| note.clone());
                op.finish(returned, returned, None, "ok", note, explain);
                Ok(Json(resp))
            }
            Err(e) => {
                op.finish(0, 0, None, "error", e.to_string(), None);
                Err(ToolFailure::recoverable(e.to_string()))
            }
        }
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
             usually stands in for a pile of Read/Grep/Bash calls. Inside the script, \
             `import lens` (python) or `import('./lens.mjs')` (js) exposes search/symbol/\
             callers/callees/path/skeleton/grep_ast/overview/recall against this repo's live \
             index and graph, so multi-step questions compose in one run.\n\
             PICKING A TOOL: (1) code structure (who calls what, imports, where a symbol is \
             defined, how A reaches B) → lens_symbol for a name, lens_graph for its \
             neighborhood (no `to`) or the shortest directed path (`to`); the graph builds \
             itself on first use. (2) where X appears → lens_search(queries); the index \
             builds itself on first use. (3) an answer derived from data or a file → \
             lens_run (pass `path` to analyze a file). (4) a file's structure/API without \
             the bodies → lens_skeleton (full text via lens_recall). (5) getting back \
             something offloaded → lens_recall.\n\
             WHEN PLAIN TOOLS ARE STILL RIGHT: use lens_run with `path` rather than Read to \
             analyze a file (Read is for when you'll Edit it); use lens_run rather than \
             Grep/Bash when you'll count or aggregate; fetch URLs through lens_run, not \
             WebFetch. If a lens_* tool reports not-found its schema isn't loaded — register \
             it with ToolSearch and retry. Plain Bash and Read stay correct for short output \
             you just want to see, or for changing state."
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
            if crate::client::detect_host() == crate::client::Host::Claude {
                let mut meta = tool.meta.take().unwrap_or_default();
                meta.0.insert(
                    "anthropic/alwaysLoad".to_string(),
                    serde_json::Value::Bool(true),
                );
                tool.meta = Some(meta);
            }
            // Read-only tools carry `readOnlyHint` so Claude Code can auto-approve them;
            // without it an unattended agent stalls on a permission prompt even for a
            // pure-read call like lens_overview (Bug B).
            if READ_ONLY_TOOLS.contains(&tool.name.as_ref()) {
                let ann = tool.annotations.take().unwrap_or_default().read_only(true);
                tool.annotations = Some(ann);
            }
            // Prime the search tools with the recovery path up front, so the model knows
            // what to do if a call comes back is_error (a transient lock).
            if tool.name.as_ref() == "lens_search" || tool.name.as_ref() == "lens_overview" {
                if let Some(desc) = tool.description.take() {
                    tool.description = Some(format!("{desc}{SEARCH_FALLBACK_HINT}").into());
                }
            }
            // schemars stamps Rust integer widths as JSON Schema `format` (`uint`,
            // `uint64`, `int32`, …). They aren't real formats; Ajv-based hosts
            // (opencode) log an "unknown format ... ignored" warning per occurrence.
            Self::sanitize_schema(&mut tool.input_schema);
            if let Some(output) = tool.output_schema.as_mut() {
                Self::sanitize_schema(output);
            }
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

    /// Drop schemars' Rust integer `format` markers (`uint`, `uint64`, `int32`, …)
    /// from a schema tree; they aren't JSON Schema formats.
    fn strip_rust_int_formats(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(serde_json::Value::String(f)) = map.get("format") {
                    if f.starts_with("uint") || f.starts_with("int") {
                        map.remove("format");
                    }
                }
                for v in map.values_mut() {
                    Self::strip_rust_int_formats(v);
                }
            }
            serde_json::Value::Array(items) => {
                for v in items {
                    Self::strip_rust_int_formats(v);
                }
            }
            _ => {}
        }
    }

    /// Sanitize one advertised tool schema (input or output) in place.
    fn sanitize_schema(schema: &mut std::sync::Arc<rmcp::model::JsonObject>) {
        let mut value = serde_json::Value::Object((**schema).clone());
        Self::strip_rust_int_formats(&mut value);
        if let serde_json::Value::Object(obj) = value {
            *schema = std::sync::Arc::new(obj);
        }
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
    /// resolve. Shared by `lens_run`, `lens_skeleton`, and `lens_grep_ast` so
    /// path-taking tools never diverge in how they accept a path — that
    /// divergence is what once let escaped calls silently resolve zero files.
    fn resolve_unescaped(&self, p: &str) -> PathBuf {
        self.resolve(&p.replace("\\ ", " "))
    }

    /// A one-line staleness warning for a recalled blob that snapshots a source
    /// file: `None` while the file's bytes still hash to the blob's ref (or when
    /// the blob has no recorded source), `Some` once the file diverged or is
    /// gone. Compares content hashes rather than mtimes so a touch stays fresh
    /// and a revert to the captured bytes clears the warning; the file read is
    /// cheap at recall frequency.
    fn stale_note(&self, reference: &str) -> Option<String> {
        let src = self.store.source(reference).ok().flatten()?;
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

    /// Path to the persisted structural graph file.
    fn graph_file(&self) -> PathBuf {
        self.data_dir.join("graph.json")
    }

    /// Per-file RRF rank map (stored path -> rank, 0 = most graph-central) for the
    /// `lens_search` fusion stage. Built ONLY from an already-persisted `graph.json`;
    /// For each query, the `(file, line)` of the exact-named symbol definition it most
    /// plausibly names, or `None`. Gated by `LENS_SYMBOL_FETCH` (default on; `=0` disables);
    /// off, or an absent graph, yields all-`None`. Among identifier tokens that exactly name
    /// a definition node, the highest-importance one wins.
    fn symbol_fetch_targets(&self, queries: &[String]) -> Vec<Option<(String, usize)>> {
        let on = std::env::var("LENS_SYMBOL_FETCH")
            .map(|v| v != "0")
            .unwrap_or(true);
        if !on {
            return vec![None; queries.len()];
        }
        let graph = match Graph::load(&self.graph_file()) {
            Ok(g) => g,
            Err(_) => return vec![None; queries.len()],
        };
        let importance = graph.importance();
        const DEF_KINDS: &[&str] = &[
            "function", "method", "struct", "enum", "trait", "interface", "class", "type", "mod",
        ];
        queries
            .iter()
            .map(|q| {
                // A prose-shaped query describes behavior; a bare token in it that
                // happens to name a symbol (`path`, `server`) must not pin that
                // unrelated definition to the top hit. See `index::is_prose_query`.
                if index::is_prose_query(q) {
                    return None;
                }
                let mut best: Option<(String, usize, f64)> = None;
                for t in index::def_ident_terms(q) {
                    for n in graph.find_by_name(&t, None) {
                        if !n.name.eq_ignore_ascii_case(&t) || !DEF_KINDS.contains(&n.kind.as_str())
                        {
                            continue;
                        }
                        let imp = importance.get(&n.id).copied().unwrap_or(0.0);
                        if best.as_ref().map(|(_, _, b)| imp > *b).unwrap_or(true) {
                            best = Some((n.file.clone(), n.line, imp));
                        }
                    }
                }
                best.map(|(f, l, _)| (f, l))
            })
            .collect()
    }

    /// Seed each query's results with its graph-resolved symbol definition as the top hit
    /// (read from source, capped), so an exact-symbol query returns the definition even
    /// when FTS recall buries it below the fetch pool. A no-op for queries with no target.
    fn inject_symbol_defs(
        &self,
        resp: &mut SearchResponse,
        targets: &[Option<(String, usize)>],
        limit: usize,
    ) {
        const SYMBOL_FETCH_CAP: usize = 4096;
        for (qi, tgt) in targets.iter().enumerate() {
            let Some((file, line)) = tgt else { continue };
            let Some(qr) = resp.results.get_mut(qi) else {
                continue;
            };
            let Ok(src) = std::fs::read_to_string(self.repo_dir.join(file)) else {
                continue;
            };
            let Some(def) = index::symbol_def_source(&src, file, *line, SYMBOL_FETCH_CAP) else {
                continue;
            };
            // Skip if the top hit already carries this definition (avoid a duplicate top).
            if qr.hits.first().is_some_and(|h| {
                h.path == *file && (h.snippet.contains(&def) || def.contains(&h.snippet))
            }) {
                continue;
            }
            let score = qr.hits.first().map(|h| h.score).unwrap_or(1.0) + 1.0;
            qr.hits.insert(
                0,
                SearchHit {
                    path: file.clone(),
                    snippet: def,
                    score,
                    line: *line,
                    symbols: Vec::new(),
                },
            );
            qr.hits.truncate(limit.max(1));
        }
    }

    /// an absent (or unreadable) graph yields an empty map, so fusion is a no-op and
    /// discovery is NEVER triggered from a search. Per-file score = sum of
    /// [`Graph::importance`] over the file's nodes; files are ranked by score desc,
    /// ties by path asc.
    fn file_ranks(&self) -> HashMap<String, usize> {
        let graph_file = self.graph_file();
        if !graph_file.exists() {
            return HashMap::new();
        }
        let graph = match Graph::load(&graph_file) {
            Ok(g) => g,
            Err(_) => return HashMap::new(),
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
            .map(|(rank, (path, _score))| (path.to_string(), rank))
            .collect()
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

    /// Fold each nested git repo's own FTS hits into `resp`, per query, so a search
    /// from a parent folder still surfaces content from sibling repos that carry their
    /// own `.lens/fts`. A nested repo does not share the parent's graph `file_ranks`
    /// (its paths are its own), so it searches with an empty rank map. Each nested
    /// hit's `path` is prefixed with the nested repo's parent-relative path so a reader
    /// can tell which repo it came from (matching how the graph merge prefixes node
    /// files).
    ///
    /// A nested repo with no `.lens/fts` yet is auto-built (index only — federation
    /// never reads the graph) instead of silently skipped, behind
    /// `LENS_NESTED_AUTOBUILD` (default on) and a file-count cap
    /// (`LENS_NESTED_AUTOBUILD_MAX_FILES`). Every outcome that isn't "already built"
    /// (built / skipped-too-large / skipped-autobuild-off / build failure) appends a
    /// one-line note to `resp.notes`, so an absent nested repo is never silent. A
    /// still-unreadable index after a build attempt is a no-op for that repo's
    /// federation, same as before.
    fn federate_nested_search(
        &self,
        resp: &mut SearchResponse,
        queries: &[String],
        limit_per_query: usize,
    ) {
        for nested_root in discovery::nested_repo_roots(&self.repo_dir) {
            let data_dir = nested_root.join(".lens");
            let prefix = nested_root
                .strip_prefix(&self.repo_dir)
                .unwrap_or(&nested_root)
                .to_string_lossy()
                .to_string();
            if !data_dir.join("fts").exists() {
                if !nested_autobuild_enabled() {
                    resp.notes
                        .push(format!("nested repo {prefix}: skipped: autobuild off"));
                    continue;
                }
                let file_count = crate::index::file_manifest(&nested_root).len();
                let cap = nested_autobuild_max_files();
                if file_count > cap {
                    resp.notes.push(format!(
                        "nested repo {prefix}: skipped: too large ({file_count} files > {cap})"
                    ));
                    continue;
                }
                match self.build_nested_index(&nested_root, &data_dir) {
                    Ok(files_indexed) => {
                        resp.notes.push(format!(
                            "nested repo {prefix}: built ({files_indexed} files)"
                        ));
                    }
                    Err(e) => {
                        resp.notes
                            .push(format!("nested repo {prefix}: skipped: build failed ({e})"));
                        continue;
                    }
                }
            }
            let nested_index = match Index::open(&data_dir) {
                Ok(i) => i.with_repo_root(&nested_root),
                Err(_) => continue,
            };
            let nested = match nested_index.search_fused(queries, limit_per_query, &HashMap::new()) {
                Ok(r) => r,
                Err(_) => continue,
            };
            for (i, qr) in nested.results.into_iter().enumerate() {
                let Some(target) = resp.results.get_mut(i) else {
                    continue;
                };
                for mut hit in qr.hits {
                    hit.path = format!("{prefix}/{}", hit.path);
                    target.hits.push(hit);
                }
            }
        }
        // Re-rank each query's merged hits by score and cap back to the caller's
        // limit. A no-nested-repo search leaves this a stable no-op: the parent hits
        // are already score-desc and already within the limit.
        for qr in &mut resp.results {
            qr.hits.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            qr.hits.truncate(limit_per_query);
        }
    }

    /// Build the FTS index only (never the graph — federation only ever reads the
    /// FTS index) for a nested repo whose `.lens/fts` does not exist yet, mirroring
    /// `ensure_index`'s build path but rooted at `nested_root`/`nested_data_dir` and
    /// writing the same manifest shape `ensure_index` writes, so a later session that
    /// opens the nested repo directly also sees it as fresh. Locked on the NESTED
    /// repo's own data dir (via `build_locked`'s explicit `data_dir` param), not the
    /// parent's. Returns the file count `index_path` saw, for the response note.
    fn build_nested_index(
        &self,
        nested_root: &Path,
        nested_data_dir: &Path,
    ) -> Result<usize, ErrorData> {
        let nested_index = Index::open(nested_data_dir)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?
            .with_repo_root(nested_root);
        let manifest_file = nested_data_dir.join("index.manifest.json");
        let is_fresh = || {
            let current = crate::index::file_manifest(nested_root);
            nested_index.chunk_count().unwrap_or(0) > 0
                && read_manifest(&manifest_file).as_ref() == Some(&current)
        };
        let mut files_indexed = 0usize;
        self.build_locked(nested_data_dir, is_fresh, || {
            let current = crate::index::file_manifest(nested_root);
            match nested_index.index_path(nested_root, true) {
                Ok(resp) => {
                    write_manifest(&manifest_file, &current);
                    files_indexed = resp.files_indexed;
                    Ok(())
                }
                Err(e) => Err(ErrorData::internal_error(e.to_string(), None)),
            }
        })?;
        Ok(files_indexed)
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
    /// with a summary. Called by `ensure_graph` (lazy, `root` == repo root); the
    /// subpath guards below are kept defensively for any future non-root caller.
    /// `auto` only adjusts the note.
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
                         rebuild from the repo root instead",
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
            nested_repo_roots: Vec::new(),
        })
    }

    /// Run `build` under the cross-process single-flight lock (`<data_dir>/build.pid`),
    /// but skip it entirely if `is_fresh()` becomes true first. The normal multi-session
    /// case is that one cold session builds and every other finds the artifact already
    /// fresh, so `build` runs at most once per staleness epoch across all processes.
    ///
    /// `data_dir` is an explicit parameter (rather than always `self.data_dir`) so a
    /// nested-repo build can lock on the NESTED repo's own data dir instead of the
    /// parent Forge's: an unrelated parent build and two different nested-repo builds
    /// must never serialize on the same lock file.
    ///
    /// Contract:
    /// - `build` is only ever called while we hold the exclusive lock AND have just
    ///   re-confirmed `!is_fresh()`, so at most one process builds concurrently.
    /// - The lock file is removed on `build`'s success, its error (`?`), or a panic,
    ///   via [`BuildLockGuard`]'s `Drop` — a crash mid-build can't leak the lock.
    /// - A holder that crashed (its recorded pid is dead) has its stale lock reclaimed
    ///   and acquisition retried; a live holder is waited out with capped backoff.
    fn build_locked(
        &self,
        data_dir: &Path,
        is_fresh: impl Fn() -> bool,
        build: impl FnOnce() -> Result<(), ErrorData>,
    ) -> Result<(), ErrorData> {
        let lock_path = data_dir.join(BUILD_LOCK_FILE);
        let deadline = std::time::Instant::now() + BUILD_LOCK_MAX_WAIT;
        let mut build = Some(build);
        loop {
            // Fast path: a concurrent winner may already have built it (cross-process
            // visible — the manifest is a plain file, the index reader reloads).
            if is_fresh() {
                return Ok(());
            }
            match BuildLockGuard::try_acquire(&lock_path) {
                Ok(Some(_guard)) => {
                    // We hold the lock. Re-check under it: a winner may have finished
                    // between the freshness probe above and this acquisition, in which
                    // case building again would be wasted work.
                    if is_fresh() {
                        return Ok(()); // `_guard` drops -> lock removed
                    }
                    let build = build.take().expect("build lock builds at most once");
                    // `_guard` drops on Ok AND Err (and on a panic) -> lock removed.
                    return build();
                }
                // Held by another builder: wait for it to clear (or reclaim it if the
                // holder is dead), then loop back to re-probe freshness — the winner
                // almost always built it, so the next `is_fresh()` returns early.
                Ok(None) => wait_for_lock_clear(&lock_path, deadline),
                Err(e) => return Err(ErrorData::internal_error(e.to_string(), None)),
            }
        }
    }

    /// Ensure `graph.json` is present AND current before a query. Rebuilds the
    /// whole-repo graph when it is missing, empty (a poisoned prior build), or
    /// **stale** — i.e. any source file was added, edited, or removed since the last
    /// build. Staleness is a cheap mtime-manifest comparison, so the graph keeps
    /// itself fresh as the user adds/removes files, with no explicit `lens_map`
    /// and no server restart. Works for every project (it is in the query path).
    fn ensure_graph(&self) -> Result<(), ErrorData> {
        // Freshness probe: the persisted graph is present, non-empty, and its manifest
        // matches the current source mtimes. Recomputed on each call so a waiter
        // re-checks the winner's just-written manifest before deciding to build.
        let is_fresh = || {
            let current = discovery::source_manifest(&self.repo_dir);
            matches!(
                Graph::load(&self.graph_file()),
                Ok(g) if !g.nodes.is_empty()
                    && read_manifest(&self.graph_manifest_file()).as_ref() == Some(&current)
            )
        };
        self.build_locked(&self.data_dir, is_fresh, || {
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
        })
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
        // Freshness probe: a live (reloaded) chunk count > 0 AND a matching manifest.
        // Gate on the live chunk count, not the cached `index_chunks` stat: a schema/
        // path migration can wipe the index while the stat (in a separate db) still
        // reads non-zero, which would wrongly skip the rebuild. Recomputed on each call
        // so a waiter re-checks the winner's committed index before deciding to build.
        let is_fresh = || {
            let current = crate::index::file_manifest(&self.repo_dir);
            self.index.chunk_count().unwrap_or(0) > 0
                && read_manifest(&self.index_manifest_file()).as_ref() == Some(&current)
        };
        self.build_locked(&self.data_dir, is_fresh, || {
            let current = crate::index::file_manifest(&self.repo_dir);
            let op = self
                .ops
                .start("lens_index", serde_json::json!({ "auto": true }));
            match self.index.index_path(&self.repo_dir, true) {
                Ok(resp) => {
                    if let Ok(total) = self.index.chunk_count() {
                        let _ = self.store.set_stat("index_chunks", total);
                    }
                    write_manifest(&self.index_manifest_file(), &current);
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
        })?;
        // Fresh now (built here, or a concurrent winner built it): start the debounce
        // window so the next burst of queries skips the walk, matching prior behavior.
        self.index_walk.mark();
        Ok(())
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
            resolved: view.resolved,
            total_matches: view.total_matches,
            trim_note: view.trim_note,
            matched_via: view.matched_via,
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

/// Serialized size of `view`'s dictionary-compacted form (the same transform
/// `maybe_compact` applies, computed here without storing anything). Dictionary
/// compaction can under-compress a subgraph with little name repetition, so
/// `lens_graph`'s neighborhood form uses this — not `view_payload_len`'s raw size —
/// as its real budget check when deciding whether depth/breadth need trimming further.
fn compacted_len(view: &GraphView) -> usize {
    let original = serde_json::json!({ "nodes": view.nodes, "edges": view.edges });
    crate::store::compress::compact_json(&original).to_string().len()
}

/// Truncate a `lens_skeleton` skeleton to at most `budget` bytes, backing off to a
/// UTF-8 char boundary and then the last newline at or before the cut point (so a
/// line is never split mid-way), and appending a note pointing at `skeleton_ref`.
/// Used when the skeleton text itself — not just the file it was built from —
/// overflows the response budget.
fn truncate_skeleton(s: &str, budget: usize) -> String {
    if s.len() <= budget {
        return s.to_string();
    }
    let mut cut = budget.min(s.len());
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let cut = s[..cut].rfind('\n').map(|i| i + 1).unwrap_or(cut);
    format!(
        "{}\n… [truncated {} of {} bytes; full skeleton at skeleton_ref via lens_recall]",
        &s[..cut],
        s.len() - cut,
        s.len()
    )
}

/// Apply `lens_recall`'s optional `grep`/`offset`/`limit` narrowing to a stored blob's
/// full content. `grep`, when present, filters to lines containing the substring
/// first; `offset`/`limit` (1-based) then page through the (possibly filtered) lines.
/// Returns the narrowed text and whether any narrowing param was given (the caller's
/// signal that `content` may be less than the full stored blob).
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

/// Nested-repo auto-build kill-switch: `LENS_NESTED_AUTOBUILD=0` disables it,
/// falling back to the old silent-skip-on-miss behavior. On by default.
fn nested_autobuild_enabled() -> bool {
    std::env::var("LENS_NESTED_AUTOBUILD").map_or(true, |v| v.trim() != "0")
}

/// Default cap on a nested repo's file count before auto-build is skipped as too
/// large (`LENS_NESTED_AUTOBUILD_MAX_FILES` overrides). Generous relative to this
/// repo's own scale so it only guards against a genuinely oversized nested tree.
const NESTED_AUTOBUILD_MAX_FILES_DEFAULT: usize = 5000;

/// Size cap on a nested repo's file count before auto-build is skipped as too
/// large. Falls back to [`NESTED_AUTOBUILD_MAX_FILES_DEFAULT`] when unset or
/// unparseable.
fn nested_autobuild_max_files() -> usize {
    std::env::var("LENS_NESTED_AUTOBUILD_MAX_FILES")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(NESTED_AUTOBUILD_MAX_FILES_DEFAULT)
}

/// Cross-process single-flight lock file for the index/graph build. Lives in the
/// data dir; its content is the holder's pid so a crashed holder's lock can be
/// detected (via `kill(pid, 0)`) and reclaimed.
const BUILD_LOCK_FILE: &str = "build.pid";
/// Poll backoff bounds while a waiter watches a live lock holder finish.
const BUILD_LOCK_POLL_MIN: std::time::Duration = std::time::Duration::from_millis(5);
const BUILD_LOCK_POLL_MAX: std::time::Duration = std::time::Duration::from_millis(100);
/// Safety cap: a *live* holder that keeps the lock past this is assumed wedged and
/// force-reclaimed. Generous vs. any real index/graph build so it never trips in
/// normal operation; it only guarantees a session can never hang forever on a
/// stuck peer.
const BUILD_LOCK_MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// RAII holder of the cross-process build lock (`<data_dir>/build.pid`). Created by
/// [`BuildLockGuard::try_acquire`]; its `Drop` removes the file, so the lock is
/// released on the build's success path, any early return / `?`, and a panic — a
/// crash mid-build can never leak the lock and stall every later session.
struct BuildLockGuard {
    path: PathBuf,
}

impl BuildLockGuard {
    /// Try to take the lock by creating the file with `O_CREAT | O_EXCL` (`create_new`
    /// maps to exactly that on unix). `Ok(Some)` = we own it (our pid is written into
    /// it); `Ok(None)` = another builder already holds it; `Err` = a real IO failure.
    fn try_acquire(path: &Path) -> std::io::Result<Option<Self>> {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut f) => {
                use std::io::Write;
                // Best-effort pid write: a reader that sees an empty/short file treats
                // the pid as unknown and waits, rather than reclaiming a just-created
                // lock whose owner has not finished stamping it yet.
                let _ = write!(f, "{}", std::process::id());
                let _ = f.flush();
                Ok(Some(BuildLockGuard {
                    path: path.to_path_buf(),
                }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl Drop for BuildLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The pid recorded in a lock file, or `None` if absent / empty / not yet stamped.
fn read_lock_pid(path: &Path) -> Option<i32> {
    std::fs::read_to_string(path).ok()?.trim().parse::<i32>().ok()
}

/// True if `pid` names a live process. `kill(pid, 0)` sends no signal, only probes:
/// rc 0 => alive; `ESRCH` => dead; `EPERM` (exists but unsignalable) still counts as
/// alive. A non-positive pid never names a real process (0/-1 address process groups),
/// so it is treated as dead and its lock is reclaimable.
#[cfg(unix)]
fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// No portable liveness probe off unix: assume alive so a waiter never clobbers a live
/// holder's lock (correctness over crash-recovery on non-unix platforms).
#[cfg(not(unix))]
fn pid_alive(_pid: i32) -> bool {
    true
}

/// Block until the build lock at `path` is released, i.e. its file no longer exists.
/// Polls the file's existence (cheap) with capped exponential backoff — the expensive
/// freshness re-probe is left to the caller, which loops back after this returns. If
/// the recorded pid is dead the stale lock is reclaimed (removed) so acquisition can
/// proceed. Bounded by `deadline`: a live holder that overruns the safety cap is
/// force-reclaimed so a wedged peer can never hang the session forever.
fn wait_for_lock_clear(path: &Path, deadline: std::time::Instant) {
    let mut backoff = BUILD_LOCK_POLL_MIN;
    loop {
        // Cleared: the holder finished and its RAII guard removed the file.
        if !path.exists() {
            return;
        }
        match read_lock_pid(path) {
            // Crashed holder: reclaim the stale lock. Re-read the pid immediately
            // before removing to narrow the classic reclaim race — a live builder that
            // just re-acquired (writing a different pid) must not have its lock deleted.
            Some(pid) if !pid_alive(pid) => {
                if read_lock_pid(path) == Some(pid) {
                    let _ = std::fs::remove_file(path);
                }
                return;
            }
            // Live (or not-yet-stamped) holder: back off and re-poll. Past the safety
            // cap, assume it wedged and force-reclaim so the caller can build.
            _ => {
                if std::time::Instant::now() >= deadline {
                    let _ = std::fs::remove_file(path);
                    return;
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(BUILD_LOCK_POLL_MAX);
            }
        }
    }
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

    /// Every registered MCP tool must appear in READ_ONLY_TOOLS or WRITE_TOOLS, so the
    /// `lens setup` permission allowlist can never silently miss a tool (a missed tool
    /// prompts on every call in plan mode).
    #[test]
    fn every_tool_is_classified_for_the_allowlist() {
        let registered: std::collections::BTreeSet<String> = Forge::tool_router()
            .list_all()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        let classified: std::collections::BTreeSet<String> = READ_ONLY_TOOLS
            .iter()
            .chain(WRITE_TOOLS.iter())
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            registered, classified,
            "every #[tool] must be listed in READ_ONLY_TOOLS or WRITE_TOOLS"
        );
    }

    /// The 0.10.0 locked surface: `list_tools` advertises EXACTLY these 10 tools —
    /// no removed tool lingers, no accidental addition rides along. `list_tools`
    /// derives its set from `tool_router().list_all()`, enumerated here the same
    /// way as the allowlist test above.
    #[test]
    fn list_tools_is_exactly_the_ten_tool_surface() {
        let registered: std::collections::BTreeSet<String> = Forge::tool_router()
            .list_all()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        let expected: std::collections::BTreeSet<String> = [
            "lens_search",
            "lens_symbol",
            "lens_graph",
            "lens_skeleton",
            "lens_overview",
            "lens_recall",
            "lens_run",
            "lens_grep_ast",
            "lens_memory_query",
            "lens_memory_record",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            registered, expected,
            "the MCP surface must be exactly the 10 locked tools"
        );
    }

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

    /// Regression: after the backend-version migration wipes the index, a session hook
    /// can write `session://` records (making `chunk_count() > 0`) before any real
    /// `index_path`. If the stale `index.manifest.json` survived the migration,
    /// `ensure_index`'s gate would see "non-empty + manifest matches current" and skip
    /// the rebuild, so `lens_search` serves session noise with no code. The migration
    /// invalidates that manifest so the first search still rebuilds.
    #[tokio::test]
    async fn resume_noise_after_migration_still_rebuilds() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "fn helper() -> i32 { 1 }\nfn main() { let _ = helper(); }\n",
        )
        .unwrap();
        let data = dir.path().join(".lens");
        std::fs::create_dir_all(&data).unwrap();
        // The pre-upgrade server's staleness snapshot matched the repo as-is; without the
        // fix the post-migration gate would treat the wiped index as already fresh.
        write_manifest(
            &data.join("index.manifest.json"),
            &crate::index::file_manifest(dir.path()),
        );

        // First open runs the backend migration (a fresh index.db has no marker), which
        // must delete the stale staleness manifest.
        let f = Forge::with_paths(dir.path().to_path_buf(), data.clone(), 8192).unwrap();
        assert!(
            !data.join("index.manifest.json").exists(),
            "migration must invalidate the stale FTS staleness manifest"
        );

        // Session-continuity noise arrives first, faking a non-empty index with no code.
        f.index
            .index_records(&[(
                "session://s/note".into(),
                "session://s#0".into(),
                "[note] resumed".into(),
            )])
            .unwrap();
        assert!(
            f.index.chunk_count().unwrap() > 0,
            "session noise should make the index non-empty"
        );

        // The first real search must rebuild and surface the code, not serve only noise.
        f.ensure_index().expect("ensure_index rebuilds");
        let out = f.index.search(&["helper".into()], 5).unwrap();
        assert!(
            out.results[0].hits.iter().any(|h| h.path.ends_with("lib.rs")),
            "post-migration search must find real code, not just session noise"
        );
    }

    fn grep_ast_req(
        query: Option<&str>,
        pattern: Option<&str>,
        language: Option<&str>,
    ) -> GrepAstRequest {
        GrepAstRequest {
            path: ".".into(),
            query: query.map(String::from),
            pattern: pattern.map(String::from),
            language: language.map(String::from),
            limit: 100,
            prod_only: false,
        }
    }

    /// `lens_grep_ast` takes exactly one of `query` / `pattern`, and `pattern`
    /// requires `language`; each violation is a clear ToolFailure.
    #[tokio::test]
    async fn grep_ast_param_errors_are_clear() {
        let (f, _dir) = forge_with_source();
        let Err(both) = f
            .lens_grep_ast(Parameters(grep_ast_req(
                Some("(identifier) @x"),
                Some("$X.unwrap()"),
                Some("rust"),
            )))
            .await
        else {
            panic!("query+pattern together must error")
        };
        assert!(both.message.contains("exactly one"), "{}", both.message);

        let Err(neither) = f
            .lens_grep_ast(Parameters(grep_ast_req(None, None, Some("rust"))))
            .await
        else {
            panic!("neither query nor pattern must error")
        };
        assert!(neither.message.contains("exactly one"), "{}", neither.message);

        let Err(no_lang) = f
            .lens_grep_ast(Parameters(grep_ast_req(None, Some("$X.unwrap()"), None)))
            .await
        else {
            panic!("pattern without language must error")
        };
        assert!(
            no_lang.message.contains("pattern requires language"),
            "{}",
            no_lang.message
        );
    }

    /// End-to-end pattern path: `helper()` (a concrete call, pinning the
    /// `helper` token per the pattern.rs guard) compiles, runs through the grep
    /// engine, and surfaces only the `@match` capture (one hit per call site).
    #[tokio::test]
    async fn grep_ast_pattern_path_matches_end_to_end() {
        let (f, _dir) = forge_with_source();
        let resp = f
            .lens_grep_ast(Parameters(grep_ast_req(None, Some("helper()"), Some("rust"))))
            .await
            .unwrap();
        // lib.rs has exactly one call site: `helper()`.
        assert_eq!(resp.0.matches.len(), 1, "{:?}", resp.0.matches);
        assert!(resp.0.matches[0].text.contains("helper"), "{:?}", resp.0.matches);
    }

    /// T8: a single-metavar-arg pattern against a real multi-arg call is exactly
    /// the shape that triggered unanchored-sibling duplication before the dedupe
    /// fix: tree-sitter matches the pattern's one arg position against each of
    /// the 3 real arguments in turn, producing 3 raw `@match` captures of the
    /// SAME call node. Dedup on (path, line, text) must collapse them to 1.
    #[tokio::test]
    async fn grep_ast_pattern_dedupes_unanchored_sibling_matches() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("call.py"), "x = object()\nx.append(1, 2, 3)\n").unwrap();
        let data = dir.path().join(".lens");
        let f = Forge::with_paths(dir.path().to_path_buf(), data, 8192).unwrap();
        let resp = f
            .lens_grep_ast(Parameters(grep_ast_req(
                None,
                Some("x.append($A)"),
                Some("python"),
            )))
            .await
            .unwrap();
        assert_eq!(resp.0.matches.len(), 1, "{:?}", resp.0.matches);
    }

    /// Variadic `$$$` matches a real multi-arg call once (arity is unconstrained;
    /// the server-side path/line/text dedupe still collapses any residual
    /// multi-captures to a single site).
    #[tokio::test]
    async fn grep_ast_variadic_pattern_matches_multi_arg_call_once() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("call.py"), "x = object()\nx.append(1, 2, 3)\nx.append()\n")
            .unwrap();
        let data = dir.path().join(".lens");
        let f = Forge::with_paths(dir.path().to_path_buf(), data, 8192).unwrap();
        let resp = f
            .lens_grep_ast(Parameters(grep_ast_req(
                None,
                Some("x.append($$$)"),
                Some("python"),
            )))
            .await
            .unwrap();
        assert_eq!(
            resp.0.matches.len(),
            2,
            "one hit per call site (3-arg and 0-arg): {:?}",
            resp.0.matches
        );
        assert!(
            resp.0.matches.iter().any(|m| m.text.contains("1, 2, 3")),
            "{:?}",
            resp.0.matches
        );
    }

    /// Unwrap `lens_graph`'s no-`to` form to its `GraphView` (the old `lens_links`
    /// shape); panics if the path variant came back for a neighborhood request.
    fn neighbors_of(resp: Json<crate::tools::GraphResponse>) -> GraphView {
        match resp.0 {
            crate::tools::GraphResponse::Neighbors(v) => v,
            crate::tools::GraphResponse::Path(p) => {
                panic!("no-`to` lens_graph must return the neighborhood shape, got path {p:?}")
            }
            crate::tools::GraphResponse::Closure(c) => {
                panic!("no-`to` lens_graph must return the neighborhood shape, got closure {c:?}")
            }
        }
    }

    /// Ported from the removed `lens_links`: `lens_graph` without `to` resolves a
    /// symbol NAME the same way the path form resolves its ends, not just a raw
    /// node id; an unresolvable input is an explicit error naming it, never a
    /// silent empty graph.
    #[tokio::test]
    async fn lens_graph_resolves_name_and_errors_on_unresolvable_input() {
        let (f, _dir) = forge_with_source();
        let ok = f
            .lens_graph(Parameters(GraphRequest {
                node: "helper".into(),
                to: None,
                depth: 1,
                direction: None,
                transitive: false,
                prod_only: false,
            }))
            .await
            .unwrap();
        assert!(
            !neighbors_of(ok).nodes.is_empty(),
            "name-form lookup must return a non-empty neighborhood"
        );

        let Err(err) = f
            .lens_graph(Parameters(GraphRequest {
                node: "totally_unresolvable_xyz_123".into(),
                to: None,
                depth: 1,
                direction: None,
                transitive: false,
                prod_only: false,
            }))
            .await
        else {
            panic!("an unresolvable node must error, not return an empty graph")
        };
        assert!(
            err.message.contains("totally_unresolvable_xyz_123"),
            "{}",
            err.message
        );
    }

    /// T3 parity, `to`-form: `lens_graph {node, to}` serializes byte-identically to
    /// the engine result the removed `lens_path` handler returned unchanged
    /// (`gquery::path`), the untagged enum adding no wrapper.
    #[tokio::test]
    async fn lens_graph_to_form_byte_equals_the_old_path_output() {
        let (f, _dir) = forge_with_source();
        let got = f
            .lens_graph(Parameters(GraphRequest {
                node: "main".into(),
                to: Some("helper".into()),
                depth: 1,
                direction: None,
                transitive: false,
                prod_only: false,
            }))
            .await
            .unwrap();
        // Same graph the handler used (cache hit after the call above).
        let graph = f.load_graph().unwrap();
        let expected = gquery::path(&graph, "main", "helper");
        assert!(expected.found, "fixture must contain a main -> helper path");
        assert_eq!(
            serde_json::to_string(&got.0).unwrap(),
            serde_json::to_string(&expected).unwrap(),
            "to-form JSON must byte-equal the old lens_path handler's output"
        );
    }

    /// T3 parity, no-`to` form: `lens_graph {node}` serializes byte-identically to
    /// the removed `lens_links` handler's output on the same input — the same
    /// resolve + directed-neighborhood engine, through the same `maybe_compact`,
    /// with no enum wrapper and no `matched_via` (that field is lens_symbol-only).
    #[tokio::test]
    async fn lens_graph_no_to_form_byte_equals_the_old_links_output() {
        let (f, _dir) = forge_with_source();
        let got = f
            .lens_graph(Parameters(GraphRequest {
                node: "helper".into(),
                to: None,
                depth: 1,
                direction: Some("callers".into()),
                transitive: false,
                prod_only: false,
            }))
            .await
            .unwrap();
        let graph = f.load_graph().unwrap();
        let id = gquery::resolve(&graph, "helper").expect("helper resolves");
        // The old handler was: resolve -> neighbors_dir -> (no trim needed on this
        // small fixture) -> maybe_compact (a no-op below the inline cap).
        let expected = f.maybe_compact(gquery::neighbors_dir(&graph, &id, 1, Some("callers")));
        let got_json = serde_json::to_string(&got.0).unwrap();
        assert_eq!(
            got_json,
            serde_json::to_string(&expected).unwrap(),
            "no-to-form JSON must byte-equal the old lens_links handler's output"
        );
        assert!(
            !got_json.contains("matched_via"),
            "matched_via is lens_symbol-only and must not appear here: {got_json}"
        );
    }

    /// `lens_symbol` fallback: a substring hit reports `matched_via: "name"`; a
    /// query with zero substring matches resolves via the blend-ranked find path
    /// and reports `matched_via: "meaning"`.
    #[tokio::test]
    async fn lens_symbol_falls_back_to_blend_find_and_reports_matched_via() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "struct TotalPriceRule;\nfn compute_total_price() -> i32 { 1 }\nfn main() { let _ = compute_total_price(); }\n",
        )
        .unwrap();
        let data = dir.path().join(".lens");
        let f = Forge::with_paths(dir.path().to_path_buf(), data, 8192).unwrap();

        // Substring hit: matched via "name".
        let by_name = f
            .lens_symbol(Parameters(GraphQueryRequest {
                name: "compute_total".into(),
                kind: None,
                limit: 20,
            }))
            .await
            .unwrap();
        assert!(by_name.0.nodes.iter().any(|n| n.name == "compute_total_price"));
        assert_eq!(by_name.0.matched_via.as_deref(), Some("name"));

        // "total price" is no substring of any symbol name (the space), so the
        // substring pass finds zero roots and the blend fallback must resolve it.
        let by_meaning = f
            .lens_symbol(Parameters(GraphQueryRequest {
                name: "total price".into(),
                kind: None,
                limit: 20,
            }))
            .await
            .unwrap();
        assert!(
            by_meaning
                .0
                .nodes
                .iter()
                .any(|n| n.name == "compute_total_price"),
            "blend fallback must resolve the meaning-shaped query, got {:?}",
            by_meaning.0.nodes
        );
        assert_eq!(by_meaning.0.matched_via.as_deref(), Some("meaning"));

        // The serialized field is present exactly when set — and spelled as pinned.
        let json = serde_json::to_string(&by_meaning.0).unwrap();
        assert!(json.contains(r#""matched_via":"meaning""#), "{json}");

        // The fallback must keep threading `kind` (the absorbed `lens_find`
        // contract): the same meaning-shaped query constrained to structs must
        // resolve the struct and never the same-tokens function.
        let by_kind = f
            .lens_symbol(Parameters(GraphQueryRequest {
                name: "total price".into(),
                kind: Some("struct".into()),
                limit: 20,
            }))
            .await
            .unwrap();
        assert_eq!(by_kind.0.matched_via.as_deref(), Some("meaning"));
        assert!(
            by_kind.0.nodes.iter().any(|n| n.name == "TotalPriceRule"),
            "kind-filtered fallback must resolve the struct, got {:?}",
            by_kind.0.nodes
        );
        assert!(
            !by_kind.0.nodes.iter().any(|n| n.name == "compute_total_price"),
            "wrong-kind symbol must not survive the kind filter: {:?}",
            by_kind.0.nodes
        );
    }

    /// Ported from the removed `lens_links` (T14): a `lens_graph` neighborhood whose
    /// requested depth (10) pulls in a subgraph that still overflows the response
    /// budget even after `maybe_compact`'s dictionary compaction (mined defect:
    /// uniquely-named nodes compress poorly) must be trimmed — depth and/or breadth
    /// — to actually fit, note what was cut, and keep the full requested-depth
    /// subgraph recoverable via `retrieve_ref`.
    #[tokio::test]
    async fn graph_depth_ten_fits_the_response_budget_via_trim() {
        let base = tempdir().unwrap();
        let repo = base.path().to_path_buf();
        // A long linear call chain with long, largely-unique names (little repetition
        // for `compact_json`'s dictionary to exploit), so a depth-10 neighborhood
        // still overflows a tiny response budget after compaction.
        let mut src = String::new();
        for i in 0..25 {
            src.push_str(&format!(
                "pub fn node_alpha_chain_member_{i:03}() {{\n    node_alpha_chain_member_{:03}();\n}}\n\n",
                i + 1
            ));
        }
        src.push_str("pub fn node_alpha_chain_member_025() {}\n");
        std::fs::write(repo.join("chain.rs"), &src).unwrap();

        // Tiny inline budget so even a modest subgraph must be trimmed to fit.
        let f = Forge::with_paths(repo.clone(), base.path().join(".lens"), 64).unwrap();
        let resp = neighbors_of(
            f.lens_graph(Parameters(GraphRequest {
                node: "node_alpha_chain_member_000".into(),
                to: None,
                depth: 10,
                direction: Some("callees".into()),
                transitive: false,
                prod_only: false,
            }))
            .await
            .unwrap(),
        );

        assert!(
            resp.truncated,
            "a depth-10 chain over a 64-byte budget must be trimmed"
        );
        assert!(
            resp.trim_note.is_some(),
            "a trimmed response must note what was cut"
        );
        let served = serde_json::to_string(&resp).unwrap().len();
        assert!(
            served < 4096,
            "the served response ({served} bytes) must be budgeted, not the raw depth-10 payload"
        );
        // "The rest" is still recoverable: the full requested-depth subgraph via ref.
        let full_ref = resp.retrieve_ref.clone().expect("ref to the full subgraph");
        let recalled = f
            .lens_recall(Parameters(crate::tools::RetrieveRequest {
                reference: full_ref,
                offset: None,
                limit: None,
                grep: None,
            }))
            .await
            .unwrap()
            .0;
        assert!(
            recalled.content.contains("node_alpha_chain_member_010"),
            "the full ref must cover nodes beyond whatever depth/breadth was trimmed"
        );
    }

    /// Piped `stdin` never enters context, so `lens_run` must credit it (floor-capped)
    /// toward `tokens_saved_est`, on top of the existing stdout-offload credit.
    #[tokio::test]
    async fn lens_run_credits_piped_stdin_floor_capped() {
        let (f, _dir) = forge(8192);
        let stdin = "x".repeat(40 * 1024);
        f.lens_run(Parameters(ExecuteRequest {
            language: "bash".into(),
            code: "echo hi".into(),
            timeout_secs: 30,
            stdin: Some(stdin),
            path: None,
        }))
        .await
        .unwrap();

        let rec = last_op_record(&f);
        assert_eq!(rec.tool, "lens_run");
        // raw_bytes_in = stdout_bytes + min(stdin_len, vol_floor()); "echo hi" produces
        // a tiny, untruncated stdout with no stderr, so bytes_returned == stdout_bytes
        // and cancels out of the savings formula, leaving exactly the floor-capped
        // stdin credit — proving the 40 KB stdin was capped, not credited in full.
        let vol_floor = obs::credit::vol_floor();
        assert_eq!(
            rec.raw_bytes_in,
            rec.bytes_returned + vol_floor.min(40 * 1024),
            "stdin credit must floor-cap at vol_floor(), not the full 40 KB"
        );
        let expected = ((rec.raw_bytes_in as i64 - rec.bytes_returned as i64).max(0)) / 4;
        assert_eq!(rec.tokens_saved_est, expected);
    }

    /// A no-stdin `lens_run` that reads files inline stays uncredited by design
    /// (raw_in == returned, since the darkroom never sees data lens didn't already
    /// hand it).
    #[tokio::test]
    async fn lens_run_no_stdin_records_zero_savings() {
        let (f, _dir) = forge(8192);
        f.lens_run(Parameters(ExecuteRequest {
            language: "bash".into(),
            code: "echo hi".into(),
            timeout_secs: 30,
            stdin: None,
            path: None,
        }))
        .await
        .unwrap();

        let rec = last_op_record(&f);
        assert_eq!(rec.tool, "lens_run");
        assert_eq!(rec.tokens_saved_est, 0);
    }

    /// Read back the most recently appended `OpRecord` from this Forge's `ops.log`.
    fn last_op_record(f: &Forge) -> obs::OpRecord {
        let raw = std::fs::read_to_string(f.data_dir.join("ops.log")).unwrap();
        let last = raw.lines().last().expect("at least one op recorded");
        serde_json::from_str(last).unwrap()
    }

    /// Adapted from the removed `lens_map`'s zero-file test: the guard now
    /// protects the auto-ensure path. Deleting every source file makes the next
    /// rebuild parse 0 files; `finish_discovery` must error and keep the existing
    /// graph rather than persisting an empty one.
    #[tokio::test]
    async fn zero_file_rebuild_errors_and_keeps_graph() {
        let (f, dir) = forge_with_source();
        // First query auto-builds the graph.
        f.lens_symbol(Parameters(GraphQueryRequest {
            name: "helper".into(),
            kind: None,
            limit: 20,
        }))
        .await
        .unwrap();
        let before = Graph::load(&f.graph_file()).unwrap().nodes.len();
        assert!(before > 0);

        // All source gone → the stale-manifest rebuild parses 0 files. That must
        // error and leave the existing graph untouched (never persist empty).
        std::fs::remove_file(dir.path().join("lib.rs")).unwrap();
        let res = f
            .lens_symbol(Parameters(GraphQueryRequest {
                name: "helper".into(),
                kind: None,
                limit: 20,
            }))
            .await;
        assert!(res.is_err(), "0-file rebuild should error, not succeed");
        let after = Graph::load(&f.graph_file()).unwrap().nodes.len();
        assert_eq!(after, before, "graph must be preserved on a 0-file rebuild");
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
        // build call, no restart.
        let (f, dir) = forge_with_source();
        f.lens_symbol(Parameters(GraphQueryRequest {
            name: "helper".into(),
            kind: None,
            limit: 20,
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

    // ── T4: nested-repo search federation ──────────────────────────────────

    /// `lens_search` from a parent folder federates each nested repo's own
    /// `.lens/fts`, so hits from both nested repos surface alongside the parent's own,
    /// path-prefixed by repo, and the merged set still respects `limit_per_query`.
    #[tokio::test]
    async fn nested_search_federates_and_respects_limit() {
        let parent = tempdir().unwrap();
        let data = parent.path().join(".lens");
        // Parent's own content carrying the shared token.
        std::fs::write(parent.path().join("notes.md"), "widget in the parent repo\n").unwrap();
        let f = Forge::with_paths(parent.path().to_path_buf(), data, 8192).unwrap();

        // Two nested git repos, each pre-built into its own `.lens/fts`.
        for name in ["alpha", "beta"] {
            let nested = parent.path().join(name);
            std::fs::create_dir_all(nested.join(".git")).unwrap();
            std::fs::write(nested.join("notes.md"), format!("widget in {name}\n")).unwrap();
            let idx = Index::open(&nested.join(".lens"))
                .unwrap()
                .with_repo_root(&nested);
            idx.index_path(&nested, true).unwrap();
        }

        // Generous limit: every repo's single hit for the shared token survives.
        let out = f
            .lens_search(Parameters(SearchRequest {
                queries: vec!["widget".into()],
                limit_per_query: 5,
            }))
            .await
            .unwrap();
        let hits = &out.0.results[0].hits;
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert!(
            paths.contains(&"notes.md"),
            "parent's own hit must be present, got {paths:?}"
        );
        assert!(
            paths.contains(&"alpha/notes.md"),
            "nested repo alpha's hit must be present + prefixed, got {paths:?}"
        );
        assert!(
            paths.contains(&"beta/notes.md"),
            "nested repo beta's hit must be present + prefixed, got {paths:?}"
        );
        assert!(hits.len() <= 5, "must not exceed limit_per_query");

        // A tight limit still holds after the merge: federation truncates to the cap.
        let capped = f
            .lens_search(Parameters(SearchRequest {
                queries: vec!["widget".into()],
                limit_per_query: 1,
            }))
            .await
            .unwrap();
        assert_eq!(
            capped.0.results[0].hits.len(),
            1,
            "merged + capped hits must respect limit_per_query = 1"
        );
    }

    // ── T5: combined reproduction of the reported parent-folder hang ───────

    /// Reproduces the reported hang: opening lens on a parent folder containing (a) a
    /// nested git repo with a PRE-BUILT `.lens` (graph + fts), (b) a nested git repo
    /// with `.git` but no `.lens` yet, and (c) a large stray file over the index's
    /// size guard sitting directly in the parent (simulating a stray video/binary).
    /// Driving the same call sequence a fresh session makes -- `lens_symbol`
    /// (triggers `ensure_graph`) then `lens_search` (triggers `ensure_index`) -- must
    /// complete with no panics, surface the pre-built nested repo's content, and
    /// never read the stray file (or either nested repo) into the parent's own index.
    #[tokio::test]
    async fn nested_repo_parent_open_does_not_hang() {
        let parent = tempdir().unwrap();
        let data = parent.path().join(".lens");
        std::fs::write(parent.path().join("top.rs"), "fn top_widget() {}\n").unwrap();

        // (a) nested repo with its own PRE-BUILT graph + fts, as if `lens_map`/
        // `lens_index` had already run there (T4's lazy-build-in-own-folder path).
        let built = parent.path().join("built");
        std::fs::create_dir_all(built.join(".git")).unwrap();
        std::fs::write(
            built.join("inner.rs"),
            "fn built_widget_symbol() { built_helper(); }\nfn built_helper() {}\n",
        )
        .unwrap();
        let outcome = discovery::discover(&built, None).unwrap();
        outcome
            .graph
            .save(&built.join(".lens").join("graph.json"))
            .unwrap();
        let built_idx = Index::open(&built.join(".lens"))
            .unwrap()
            .with_repo_root(&built);
        built_idx.index_path(&built, true).unwrap();

        // (b) nested repo with `.git` but no `.lens` yet: never lazily built by a
        // whole-repo query, only reachable via an explicit path-scoped call.
        let bare = parent.path().join("bare");
        std::fs::create_dir_all(bare.join(".git")).unwrap();
        std::fs::write(bare.join("inner.rs"), "fn bare_widget_symbol() {}\n").unwrap();

        // (c) a large stray file directly in the parent, simulating a stray
        // video/binary that must never be fully read before being skipped.
        std::fs::write(
            parent.path().join("stray.mp4"),
            vec![b'x'; 3 * 1024 * 1024],
        )
        .unwrap();

        let f = Forge::with_paths(parent.path().to_path_buf(), data, 8192).unwrap();

        // A fresh session's first graph query: ensure_graph must build + merge the
        // pre-built nested graph, not hang trying to parse every sibling repo.
        let symbol = f
            .lens_symbol(Parameters(GraphQueryRequest {
                name: "built_widget_symbol".into(),
                kind: None,
                limit: 20,
            }))
            .await
            .unwrap();
        assert!(
            symbol
                .0
                .nodes
                .iter()
                .any(|n: &NodeView| n.name == "built_widget_symbol"),
            "pre-built nested repo's graph content must surface via lens_symbol, got {:?}",
            symbol.0.nodes
        );

        // A fresh session's first search: ensure_index must build + federate, not
        // hang on the stray file or either nested repo.
        let search = f
            .lens_search(Parameters(SearchRequest {
                queries: vec!["built_widget_symbol".into()],
                limit_per_query: 5,
            }))
            .await
            .unwrap();
        let hits = &search.0.results[0].hits;
        assert!(
            hits.iter().any(|h| h.path == "built/inner.rs"),
            "pre-built nested repo's fts content must surface via lens_search, got {hits:?}"
        );

        // The stray file and both nested repos must never have been read into the
        // PARENT's own index: only the parent's own top.rs is there.
        let manifest: Vec<String> = {
            let conn = f.index.conn().unwrap();
            let mut stmt = conn.prepare("SELECT path FROM file_manifest").unwrap();
            stmt.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .flatten()
                .collect()
        };
        assert!(
            manifest.iter().any(|p| p.ends_with("top.rs")),
            "parent's own top.rs must be indexed, got {manifest:?}"
        );
        assert!(
            !manifest.iter().any(|p| p.contains("stray.mp4")),
            "the large stray file must never be read into the parent's own index, got {manifest:?}"
        );
        assert!(
            !manifest.iter().any(|p| p.contains("built/") || p.contains("bare/")),
            "nested repos' files must never be read into the parent's own index, got {manifest:?}"
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
                include_bodies: None,
                with_lines: None,
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
        // T14: `with_lines` defaults to true (no explicit request value given above),
        // so the signature is prefixed with its source line without asking for it.
        assert!(
            resp.skeleton.contains("L1:"),
            "with_lines must default to true: {}",
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
                offset: None,
                limit: None,
                grep: None,
            }))
            .await
            .unwrap()
            .0;
        assert_eq!(recalled.content, full, "recall did not return the full file");
        // Well under the 8192-byte budget: not truncated, no skeleton_ref.
        assert!(!resp.truncated);
        assert!(resp.skeleton_ref.is_none());
    }

    /// T14: a skeleton whose signatures ALONE overflow the response budget (mined
    /// defect, 21 cases: `lens_skeleton` blowing the 25k client cap on large files)
    /// must come back as a budgeted head plus a `skeleton_ref`, and that ref, when
    /// recalled, must return the full (untruncated) skeleton text.
    #[tokio::test]
    async fn skeleton_over_budget_returns_truncated_head_and_full_ref() {
        let base = tempdir().unwrap();
        let repo = base.path().to_path_buf();
        let file = repo.join("many.rs");
        let mut src = String::new();
        for i in 0..80 {
            src.push_str(&format!(
                "pub fn func_number_{i:03}(x: i32) -> i32 {{\n    x + {i}\n}}\n\n"
            ));
        }
        std::fs::write(&file, &src).unwrap();
        // Tiny inline budget forces the skeleton itself (not just the source file)
        // to overflow, even though the skeleton is far smaller than the raw file.
        let f = Forge::with_paths(repo.clone(), base.path().join(".lens"), 512).unwrap();

        let resp = f
            .lens_skeleton(Parameters(crate::tools::SkeletonRequest {
                path: file.display().to_string(),
                include_bodies: None,
                with_lines: None,
            }))
            .await
            .unwrap()
            .0;

        assert!(
            resp.truncated,
            "an 80-fn skeleton must overflow a 512-byte budget"
        );
        assert!(
            !resp.skeleton.contains("func_number_079"),
            "the truncated head must not carry the tail of the skeleton: {}",
            resp.skeleton
        );
        let skeleton_ref = resp
            .skeleton_ref
            .clone()
            .expect("a truncated skeleton must carry a ref to the full text");

        let recalled = f
            .lens_recall(Parameters(crate::tools::RetrieveRequest {
                reference: skeleton_ref,
                offset: None,
                limit: None,
                grep: None,
            }))
            .await
            .unwrap()
            .0;
        assert!(
            recalled.content.len() > resp.skeleton.len(),
            "the full skeleton must be larger than the truncated head"
        );
        assert!(
            recalled.content.contains("func_number_079"),
            "the full skeleton must include what the truncated head elided"
        );
    }

    /// Recall a ref and unwrap the response (staleness tests hit this repeatedly).
    async fn recall(f: &Forge, reference: &str) -> crate::tools::RetrieveResponse {
        f.lens_recall(Parameters(crate::tools::RetrieveRequest {
            reference: reference.to_string(),
            offset: None,
            limit: None,
            grep: None,
        }))
        .await
        .unwrap()
        .0
    }

    #[tokio::test]
    async fn recall_flags_stale_after_source_edit() {
        let base = tempdir().unwrap();
        let repo = base.path().to_path_buf();
        let file = repo.join("widget.rs");
        let v1 = "pub fn one() -> i32 { 1 }\n";
        std::fs::write(&file, v1).unwrap();
        let f = Forge::with_paths(repo.clone(), base.path().join(".lens"), 8192).unwrap();
        let skel = f
            .lens_skeleton(Parameters(crate::tools::SkeletonRequest {
                path: file.display().to_string(),
                include_bodies: None,
                with_lines: None,
            }))
            .await
            .unwrap()
            .0;

        // Fresh: the file still matches the snapshot, so no warning.
        let fresh = recall(&f, &skel.retrieve_ref).await;
        assert_eq!(fresh.content, v1);
        assert!(
            fresh.stale.is_none(),
            "unchanged file flagged stale: {:?}",
            fresh.stale
        );

        // A rewrite with identical bytes is not a change (mtime alone must not trip it).
        std::fs::write(&file, v1).unwrap();
        assert!(recall(&f, &skel.retrieve_ref).await.stale.is_none());

        // Content change: recall still returns the captured bytes, plus a warning.
        std::fs::write(&file, "pub fn one() -> i32 { 2 }\n").unwrap();
        let stale = recall(&f, &skel.retrieve_ref).await;
        assert_eq!(
            stale.content, v1,
            "recall must keep returning the captured snapshot"
        );
        let msg = stale.stale.expect("edited file must be flagged stale");
        assert!(
            msg.contains("widget.rs"),
            "warning should name the file: {msg}"
        );

        // Deleted source: the snapshot is historical, and the warning says so.
        std::fs::remove_file(&file).unwrap();
        let gone = recall(&f, &skel.retrieve_ref).await;
        assert_eq!(gone.content, v1);
        assert!(gone.stale.is_some(), "deleted file must be flagged");
    }

    #[tokio::test]
    async fn recall_without_provenance_has_no_stale_field() {
        let (f, _dir) = forge(8192);
        // A plain offloaded blob (darkroom output) has no source file to go stale.
        let reference = f.store.put("just a blob").unwrap();
        let resp = recall(&f, &reference).await;
        assert_eq!(resp.content, "just a blob");
        assert!(resp.stale.is_none());
        assert!(!resp.sliced, "no offset/limit/grep given, so nothing was sliced");
    }

    /// T14: `lens_recall`'s optional `offset`/`limit`/`grep` slice a large ref instead
    /// of returning it all at once (mined defect, 6 cases: a 141KB ref was
    /// all-or-nothing).
    #[tokio::test]
    async fn recall_offset_limit_grep_slice_a_large_ref() {
        let (f, _dir) = forge(8192);
        let mut lines: Vec<String> = (1..=50).map(|i| format!("line {i:02} content")).collect();
        lines[9] = "line 10 MARKER content".to_string();
        lines[24] = "line 25 MARKER content".to_string();
        let content = lines.join("\n");
        let reference = f.store.put(&content).unwrap();

        async fn sliced(
            f: &Forge,
            reference: &str,
            offset: Option<usize>,
            limit: Option<usize>,
            grep: Option<&str>,
        ) -> crate::tools::RetrieveResponse {
            f.lens_recall(Parameters(crate::tools::RetrieveRequest {
                reference: reference.to_string(),
                offset,
                limit,
                grep: grep.map(|s| s.to_string()),
            }))
            .await
            .unwrap()
            .0
        }

        // offset/limit page through 1-based lines.
        let page = sliced(&f, &reference, Some(5), Some(3), None).await;
        assert_eq!(page.content, lines[4..7].join("\n"));
        assert!(page.sliced);

        // grep narrows to matching lines only (both params composable, tested via
        // grep alone here since it is the seam most likely to regress).
        let grepped = sliced(&f, &reference, None, None, Some("MARKER")).await;
        assert_eq!(grepped.content, format!("{}\n{}", lines[9], lines[24]));
        assert!(grepped.sliced);

        // grep + limit compose: narrow to matches, then page through them.
        let grepped_paged = sliced(&f, &reference, Some(2), Some(1), Some("MARKER")).await;
        assert_eq!(grepped_paged.content, lines[24]);
        assert!(grepped_paged.sliced);

        // No slicing params: unsliced, full content, byte-identical to a plain recall.
        let full = recall(&f, &reference).await;
        assert_eq!(full.content, content);
        assert!(!full.sliced);
    }

    /// The folded `lens_run{path}` keeps the removed `lens_run_file`'s argv
    /// behavior: the (possibly shell-escaped) path resolves, lands as the script's
    /// first CLI argument (`$1`), and the analyzed file's bytes are credited to
    /// the op record exactly as the old tool credited them — the MCP-layer mirror
    /// of darkroom's own `run_file_passes_path_as_argv`.
    #[tokio::test]
    async fn lens_run_with_path_passes_argv_and_credits_file_bytes() {
        let base = tempdir().unwrap();
        let spaced = base.path().join("AI Stuff");
        std::fs::create_dir_all(&spaced).unwrap();
        let file = spaced.join("data.txt");
        std::fs::write(&file, "hello\n").unwrap();
        let f = Forge::with_paths(spaced.clone(), base.path().join(".lens"), 8192).unwrap();

        let escaped = file.display().to_string().replace(' ', "\\ ");
        let resp = f
            .lens_run(Parameters(ExecuteRequest {
                language: "bash".into(),
                code: "wc -c < \"$1\"".into(),
                timeout_secs: 30,
                stdin: None,
                path: Some(escaped),
            }))
            .await
            .unwrap();
        // 6 bytes ("hello\n"): proves the script read the real file via the
        // un-escaped path injected as argv (a broken path would leave stdout empty).
        assert_eq!(resp.0.stdout.trim(), "6", "stdout: {:?}", resp.0.stdout);

        // The file's 6 bytes never entered context, so the op credits them on top
        // of stdout — the old lens_run_file's raw_in = stdout_bytes + file_size.
        let rec = last_op_record(&f);
        assert_eq!(rec.tool, "lens_run");
        assert_eq!(
            rec.raw_bytes_in,
            resp.0.stdout_bytes as u64 + 6,
            "path-form lens_run must credit the analyzed file's bytes"
        );
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
                origin: None,
            }],
            edges: vec![],
            compact: None,
            truncated: false,
            retrieve_ref: None,
            resolved: vec![],
            total_matches: None,
            trim_note: None,
            matched_via: None,
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
                origin: None,
            })
            .collect();
        let original = serde_json::json!({ "nodes": nodes, "edges": [] });
        let view = GraphView {
            nodes,
            edges: vec![],
            compact: None,
            truncated: false,
            retrieve_ref: None,
            resolved: vec![],
            total_matches: None,
            trim_note: None,
            matched_via: None,
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
