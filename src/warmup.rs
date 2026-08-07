//! `lens warmup` — build the structural graph + FTS index for a repo up front,
//! instead of waiting for the MCP server's lazy first-call build.
//!
//! Writes to the SAME data dir the server reads (`$LENS_DIR`, else
//! `<cwd>/.lens`), so a server that's already running picks the graph up on its
//! next `lens_symbol` with no restart (the graph is a plain file it re-reads; the
//! index is WAL SQLite both processes can share).
//!
//! A separate process whose stdout is its own response channel — never the MCP
//! JSON-RPC stream.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ignore::WalkBuilder;

use crate::discovery;
use crate::index::Index;
use crate::obs;
use crate::store::Store;

/// `lens warmup [path]` — discover + index `path` (default `.`) into its data dir.
pub fn run_cli(args: &[String]) -> Result<()> {
    let mut root = PathBuf::from(".");
    for arg in args {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("usage: lens warmup [path]");
                println!();
                println!("Build the code graph + search index for <path> (default: cwd) into");
                println!("its .lens data dir, so lens_symbol / lens_search work immediately.");
                println!("Re-run any time to refresh after the code changes.");
                return Ok(());
            }
            other => root = PathBuf::from(other),
        }
    }
    let data_dir = obs::data_dir();
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("creating {}", data_dir.display()))?;
    warmup(&root, &data_dir)
}

/// Build the graph (→ `graph.json`) and FTS index for `root`, persisting both into
/// `data_dir` and recording the count stats. Prints a human-readable summary.
pub fn warmup(root: &Path, data_dir: &Path) -> Result<()> {
    // Canonicalize so the walk and the `with_repo_root(root)` base below share one
    // spelling — stored keys come out repo-root-relative, matching the MCP server
    // (which relativizes against the same canonical repo_dir).
    let root_buf = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let root = root_buf.as_path();
    // Same scope classification the MCP server applies to auto-builds: refuse to
    // walk a giant non-project tree (a bare `lens warmup` from `$HOME`).
    if !discovery::indexable_root(root) {
        anyhow::bail!(
            "{} is not a code project (no project marker like .git/Cargo.toml, and \
             over 10k files) — refusing to index it. Pass a project directory, or set \
             LENS_SCOPE_GUARD=0 to force.",
            root.display()
        );
    }
    let store = Store::open(data_dir).context("opening store")?;

    // --- Structural graph (tree-sitter → graph.json) ---
    let outcome = discovery::discover(root, None).context("building code graph")?;
    // Never overwrite a good graph.json with an empty one (no supported source under
    // `root`). Bail so the caller sees the problem instead of a silent empty graph.
    if outcome.response.files_parsed == 0 {
        anyhow::bail!(
            "discover parsed 0 files under {} — refusing to write an empty graph (check the path)",
            root.display()
        );
    }
    let graph_file = data_dir.join("graph.json");
    outcome
        .graph
        .save(&graph_file)
        .with_context(|| format!("writing {}", graph_file.display()))?;
    // Refresh the staleness manifest so the server's lazy `ensure_graph` serves from
    // cache instead of rebuilding what we just built.
    write_manifest(
        &data_dir.join("graph.manifest.json"),
        &discovery::source_manifest(root),
    );
    let g = &outcome.response;
    let _ = store.set_stat("graph_nodes", g.nodes as i64);
    let _ = store.set_stat("graph_edges", g.edges as i64);

    // --- FTS content index ---
    let index = Index::open(data_dir)
        .context("opening index")?
        .with_repo_root(root);
    let idx = index
        .index_path(root, true)
        .context("indexing repo contents")?;
    // Drop chunks for files deleted since the last build, then record the manifest.
    let _ = index.prune_missing(root);
    if let Ok(total) = index.chunk_count() {
        let _ = store.set_stat("index_chunks", total);
    }
    write_manifest(
        &data_dir.join("index.manifest.json"),
        &crate::index::file_manifest(root),
    );

    let langs = if g.languages.is_empty() {
        "none".to_string()
    } else {
        g.languages.join(", ")
    };
    println!("warmed up {}", root.display());
    println!(
        "  graph : {} nodes, {} edges  ({} files parsed; {langs})",
        g.nodes, g.edges, g.files_parsed
    );
    println!(
        "  index : {} files, {} chunks",
        idx.files_indexed, idx.chunks
    );
    println!("  out   : {}", data_dir.display());
    if !g.warnings.is_empty() {
        println!(
            "  note  : {} file(s) skipped (unparseable)",
            g.warnings.len()
        );
    }
    Ok(())
}

/// Combined staleness signature of a repo: every file's mtime. Any add/edit/delete
/// changes it, which is what triggers a watch rebuild.
fn signature(root: &Path) -> BTreeMap<String, u64> {
    crate::index::file_manifest(root)
}

/// `lens watch [path]` — keep the graph + index fresh in real time as files
/// change, WITHOUT restarting the MCP server. A standalone process that writes to
/// the same data dir the server reads: the server re-reads `graph.json` on its next
/// query and shares the WAL index, so changes appear with no reconnect.
///
/// Polls a cheap mtime manifest (robust on macOS, where FSEvents misses rapid
/// editor saves) and rebuilds once changes have stayed quiet for `debounce`.
pub fn watch(root: &Path, data_dir: &Path, debounce: Duration, poll: Duration) -> Result<()> {
    let root_buf = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let root = root_buf.as_path();
    std::fs::create_dir_all(data_dir).ok();
    println!("lens watch: {}", root.display());
    println!("  data dir : {}", data_dir.display());
    println!(
        "  debounce : {}s   poll : {}s",
        debounce.as_secs(),
        poll.as_secs()
    );
    println!(
        "  rebuilds graph + index on change; the MCP server is never touched (Ctrl-C to stop)"
    );

    if let Err(e) = warmup(root, data_dir) {
        eprintln!("  initial build skipped: {e}");
    }
    let mut last_built = signature(root);
    let mut last_seen = last_built.clone();
    let mut last_change: Option<Instant> = None;

    loop {
        std::thread::sleep(poll);
        let sig = signature(root);
        if sig != last_seen {
            // Something changed since the last poll — (re)arm the quiet timer.
            last_seen = sig;
            last_change = Some(Instant::now());
            continue;
        }
        // No change this tick. Rebuild once a pending change has been quiet long
        // enough and actually differs from what we last built.
        if let Some(t) = last_change {
            if last_seen == last_built {
                last_change = None; // reverted to the built state
            } else if t.elapsed() >= debounce {
                match warmup(root, data_dir) {
                    Ok(()) => {}
                    Err(e) => eprintln!("  rebuild failed: {e}"),
                }
                last_built = signature(root); // re-scan post-build (mtimes stable)
                last_seen = last_built.clone();
                last_change = None;
            }
        }
    }
}

/// CLI entry for `lens watch [path] [--debounce SECS] [--poll SECS]`.
pub fn run_watch_cli(args: &[String]) -> Result<()> {
    let mut root = PathBuf::from(".");
    let mut debounce = Duration::from_secs(3);
    let mut poll = Duration::from_secs(1);
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("usage: lens watch [path] [--debounce SECS] [--poll SECS]");
                println!();
                println!("Keep the code graph + search index fresh as files change, writing into");
                println!("the repo's .lens data dir. A running MCP server picks the changes up");
                println!("on its next query — no restart needed. Defaults: path '.', debounce 3s,");
                println!("poll 1s.");
                return Ok(());
            }
            "--debounce" => {
                if let Some(v) = iter.next() {
                    if let Ok(s) = v.parse::<u64>() {
                        debounce = Duration::from_secs(s);
                    }
                }
            }
            "--poll" => {
                if let Some(v) = iter.next() {
                    if let Ok(s) = v.parse::<u64>() {
                        poll = Duration::from_secs(s.max(1));
                    }
                }
            }
            other => root = PathBuf::from(other),
        }
    }
    // Write where the server reads: $LENS_DIR, else <repo>/.lens.
    let data_dir = match std::env::var_os("LENS_DIR") {
        Some(d) => PathBuf::from(d),
        None => std::fs::canonicalize(&root)
            .unwrap_or_else(|_| root.clone())
            .join(".lens"),
    };
    watch(&root, &data_dir, debounce, poll)
}

/// Persist a staleness manifest (best effort; matches the format the server reads).
fn write_manifest(path: &Path, manifest: &BTreeMap<String, u64>) {
    if let Ok(json) = serde_json::to_string(manifest) {
        let _ = std::fs::write(path, json);
    }
}

// --- Detached background builder (`lens __build`) ---------------------------------

/// Progress file the detached builder keeps in the data dir for the life of its run:
/// one line, space separated, `<builder-pid> <files-done> <files-total> <phase>`.
///
/// It is the server's whole readiness signal. Present means a build is in flight (and,
/// in the `index` phase, that search results are partial); absent means done. So the
/// steady-state cost of the check is one failed open — never a walk, a `chunk_count`,
/// or a `graph.json` parse. The pid is what makes a leftover self-healing: a builder
/// killed with SIGKILL can't clean up after itself, and a reader that finds a dead pid
/// removes the file instead of reporting a build that will never finish.
pub const BUILD_PROGRESS_FILE: &str = "build.progress";

/// Phase marker for "the FTS index is still filling in", i.e. search results are
/// partial. Anything else means the index is complete and only the graph is left.
const PHASE_INDEX: &str = "index";
const PHASE_GRAPH: &str = "graph";

/// `<data_dir>/build.progress`.
pub fn progress_path(data_dir: &Path) -> PathBuf {
    data_dir.join(BUILD_PROGRESS_FILE)
}

/// What a background builder has published about its run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildProgressState {
    /// The builder's pid, so a reader can tell a running build from a leftover.
    pub pid: i32,
    pub done: usize,
    pub total: usize,
    /// True while the FTS index is still being written, i.e. a search served now
    /// returns partial results.
    pub indexing: bool,
}

/// Read the published progress for `data_dir`, or `None` when no build is in flight.
/// Liveness is the caller's to check (see `live_background_build` in `server.rs`).
pub fn read_progress(data_dir: &Path) -> Option<BuildProgressState> {
    parse_progress(&std::fs::read_to_string(progress_path(data_dir)).ok()?)
}

fn parse_progress(raw: &str) -> Option<BuildProgressState> {
    let mut fields = raw.split_whitespace();
    Some(BuildProgressState {
        pid: fields.next()?.parse().ok()?,
        done: fields.next()?.parse().ok()?,
        total: fields.next()?.parse().ok()?,
        indexing: fields.next().unwrap_or(PHASE_INDEX) == PHASE_INDEX,
    })
}

/// Record that `pid` is building this data dir, before it has published anything of
/// its own — so the response that triggered the spawn can already say the index is
/// filling in. `create_new`, so it can never clobber counts a builder has published.
pub fn seed_progress(data_dir: &Path, pid: u32) {
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(progress_path(data_dir))
    {
        use std::io::Write;
        let _ = write!(file, "{pid} 0 0 {PHASE_INDEX}");
    }
}

/// The builder's end of the progress file. `Drop` removes it on the success path, on
/// any `?`, and on a panic, exactly like `BuildLockGuard`; the heartbeat watchdog's
/// hard exit is the one path that has to remove it by hand.
struct BuildProgress {
    path: PathBuf,
    total: usize,
    done: AtomicUsize,
    indexing: std::sync::atomic::AtomicBool,
}

impl BuildProgress {
    fn start(data_dir: &Path, total: usize) -> Self {
        let progress = BuildProgress {
            path: progress_path(data_dir),
            total,
            done: AtomicUsize::new(0),
            indexing: std::sync::atomic::AtomicBool::new(true),
        };
        progress.publish();
        progress
    }

    /// Republish after another `files` files are indexed. Called once per committed
    /// batch (a few hundred files), so the write rate is a handful per second at
    /// worst — no sampler thread needed to keep it cheap.
    fn advance(&self, files: usize) {
        self.done.fetch_add(files, Ordering::Relaxed);
        self.publish();
    }

    /// The index is complete; only the graph is left. Search results stop being
    /// partial here, so the server stops noting them.
    fn enter_graph_phase(&self) {
        self.indexing.store(false, Ordering::Relaxed);
        self.publish();
    }

    fn publish(&self) {
        let phase = if self.indexing.load(Ordering::Relaxed) {
            PHASE_INDEX
        } else {
            PHASE_GRAPH
        };
        let _ = std::fs::write(
            &self.path,
            format!(
                "{} {} {} {phase}",
                std::process::id(),
                self.done.load(Ordering::Relaxed),
                self.total
            ),
        );
    }
}

impl Drop for BuildProgress {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// `lens __build <root> <data_dir>` — the detached, throttled builder the MCP server
/// hands a cold repo to. Hidden: it is spawned, never typed, and never listed in
/// `print_usage`.
///
/// Startup order is load-bearing:
///   1. background scheduling priority, before any worker thread exists, so every
///      thread this process later spawns inherits it;
///   2. `LENS_BUILD_PROFILE=background` in our own environment, so
///      [`crate::index::build_threads`] caps the Tantivy writer. Set here rather than
///      relied on from the spawning server, so the throttle can't be lost by a caller
///      that forgets the variable;
///   3. the global rayon pool, capped by that same helper, before anything that uses
///      rayon (`discovery::discover`) runs. Capping rayon alone still measured 139%
///      CPU because Tantivy's writer threads sit outside the pool — both pools plus
///      OS priority are what make this genuinely background.
pub fn run_build_cli(args: &[String]) -> Result<()> {
    let (Some(root), Some(data_dir)) = (args.first(), args.get(1)) else {
        anyhow::bail!("usage: lens __build <root> <data_dir>");
    };
    set_background_priority();
    if std::env::var_os("LENS_BUILD_PROFILE").is_none() {
        std::env::set_var("LENS_BUILD_PROFILE", "background");
    }
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(crate::index::build_threads())
        .build_global();

    let (root, data_dir) = (PathBuf::from(root), PathBuf::from(data_dir));
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("creating {}", data_dir.display()))?;

    // Single-flight across processes: if a live builder already owns this data dir,
    // our whole job is already being done. A dead holder's lock is reclaimed inside.
    let Some(_lock) = crate::server::try_acquire_build_lock(&data_dir)? else {
        return Ok(());
    };
    spawn_exit_watchdog(data_dir.clone());
    warmup_background(&root, &data_dir)
}

/// Drop this process to background scheduling priority.
///
/// On macOS both levers are pulled: `PRIO_DARWIN_BG` moves the work onto efficiency
/// cores and throttles its I/O (what actually keeps the machine responsive), and a
/// classic `nice` of 19 cuts its CPU share and is what `ps -o nice` reports —
/// `PRIO_DARWIN_BG` alone leaves the nice value at 0, so on its own the throttle is
/// invisible to anyone inspecting the process.
#[cfg(target_os = "macos")]
fn set_background_priority() {
    unsafe {
        libc::setpriority(libc::PRIO_DARWIN_PROCESS, 0, libc::PRIO_DARWIN_BG);
        libc::setpriority(libc::PRIO_PROCESS, 0, 19);
    }
}

#[cfg(target_os = "linux")]
fn set_background_priority() {
    unsafe {
        libc::nice(19);
    }
}

/// No portable equivalent elsewhere; the thread caps still apply.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn set_background_priority() {}

/// How often the builder checks whether any server session is still listening.
const HEARTBEAT_POLL: Duration = Duration::from_secs(3);
/// A heartbeat older than this counts as gone. Servers re-touch theirs every 30s, so
/// this is two missed beats.
const HEARTBEAT_TTL: Duration = Duration::from_secs(60);
/// Grace from builder start before the no-session rule can fire. The server plants its
/// heartbeat before it serves anything, so by the time a builder exists the file is
/// already there; this is only insurance against a slow start, and it is what bounds
/// how long a builder can outlive a client that vanished the instant it spawned one.
const HEARTBEAT_GRACE: Duration = Duration::from_secs(15);

/// Self-exit once no server session is listening any more.
///
/// A detached builder is by definition an orphan, and an orphan that outlives every
/// reader is exactly the CPU-burning process users end up killing from Activity
/// Monitor. This is the leash. Runs on its own thread so the build never has to poll
/// for it.
///
/// Exiting hard skips `BuildProgress`'s and `BuildLockGuard`'s `Drop`, so the progress
/// file is removed by hand here; the lock is deliberately left behind, because a
/// dead-pid lock is already reclaimed by the next builder through the same path a
/// SIGKILL exercises.
fn spawn_exit_watchdog(data_dir: PathBuf) {
    std::thread::spawn(move || {
        // Same layout `main.rs` writes: `<data_dir>/heartbeats/<pid>.pid`, re-touched
        // by every live server session.
        let dir = data_dir.join("heartbeats");
        let started = Instant::now();
        let mut ever_seen = false;
        loop {
            std::thread::sleep(HEARTBEAT_POLL);
            match live_session(&dir) {
                // Somebody is still listening.
                Some(true) => {
                    ever_seen = true;
                    continue;
                }
                Some(false) => ever_seen = true,
                // No heartbeat dir at all: a hand-run `lens __build` with no server
                // anywhere. Nothing to be orphaned from until one shows up.
                None if !ever_seen => continue,
                None => {}
            }
            if started.elapsed() < HEARTBEAT_GRACE {
                continue;
            }
            let _ = std::fs::remove_file(progress_path(&data_dir));
            std::process::exit(0);
        }
    });
}

/// Whether any server session is listening on this data dir, or `None` when there is
/// no heartbeat directory at all.
///
/// A session counts as live only if the pid its file is named for is still running AND
/// the file is fresh. The mtime alone can't bound how fast the builder reacts: beats
/// land every 30s, so a client killed right after one leaves a file that reads fresh
/// for another 60s. The pid is exact and immediate, and requiring both means a
/// recycled pid inheriting a dead session's file can't hold the leash either.
fn live_session(dir: &Path) -> Option<bool> {
    let live = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|entry| {
            entry
                .metadata()
                .and_then(|m| m.modified())
                .map(|m| m.elapsed().map(|age| age < HEARTBEAT_TTL).unwrap_or(true))
                .unwrap_or(false)
        })
        .any(|entry| {
            entry
                .path()
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<i32>().ok())
                .is_some_and(crate::server::pid_alive)
        });
    Some(live)
}

/// Cap on the files one committed index batch covers. Small enough that a partial
/// index becomes searchable early and a killed build resumes near where it stopped;
/// large enough that the per-batch Tantivy commit stays noise against the read+parse.
const INDEX_BATCH_FILES: usize = 400;

/// A directory with more loose files than this is indexed whole rather than split into
/// one batch per file: splitting there would trade a handful of commits for thousands.
const INDEX_BATCH_LOOSE_MAX: usize = 64;

/// The background builder's build: the same graph and FTS index `warmup` produces, but
/// index first, in committed batches that publish progress, and skipping a phase that
/// is already current.
///
/// The order flips relative to `warmup` because the index is the only plane with a
/// queryable partial state — `discovery::discover` assembles the graph in memory and
/// `graph.json` is written once, at the end — and it is what `lens_search`, the usual
/// first call, needs. Doing it first is what makes "results are partial" mean anything
/// during the minutes a large repo takes at the background profile.
///
/// Skipping current phases is what makes a build killed mid-flight resume instead of
/// starting over: `index_path` re-reads only changed files, and a graph whose manifest
/// still matches is left alone.
fn warmup_background(root: &Path, data_dir: &Path) -> Result<()> {
    let root_buf = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let root = root_buf.as_path();
    // Same scope classification every other auto-build applies. The server checks it
    // before spawning us, but a builder that trusted its arguments would be one
    // `lens __build $HOME` away from the walk the guard exists to prevent.
    if !discovery::indexable_root(root) {
        anyhow::bail!(
            "{} is not a code project (no project marker like .git/Cargo.toml, and \
             over 10k files) — refusing to index it.",
            root.display()
        );
    }
    let store = Store::open(data_dir).context("opening store")?;
    let index = Index::open(data_dir)
        .context("opening index")?
        .with_repo_root(root);

    // Both manifests are captured BEFORE the work they describe: a file edited while
    // a minutes-long build runs must read as stale afterwards, not as already built.
    let index_manifest = crate::index::file_manifest(root);
    let source_manifest = discovery::source_manifest(root);
    let files = indexable_files(root);
    let progress = BuildProgress::start(data_dir, files.len());

    for (unit, count) in index_units(root, &files) {
        if let Err(e) = index.index_path(&unit, true) {
            eprintln!("lens __build: indexing {} failed: {e}", unit.display());
        }
        progress.advance(count);
    }
    // Authoritative sweep. The batches above are an optimization for partial
    // visibility; this is what guarantees the result equals one whole-repo
    // `index_path`. Anything the partition missed is picked up here, and everything it
    // covered is a no-op the unchanged-file fast path skips without taking the writer.
    index
        .index_path(root, true)
        .context("indexing repo contents")?;
    let _ = index.prune_missing(root);
    if let Ok(total) = index.chunk_count() {
        let _ = store.set_stat("index_chunks", total);
    }
    write_manifest(&data_dir.join("index.manifest.json"), &index_manifest);

    progress.enter_graph_phase();
    if !graph_is_current(data_dir, &source_manifest) {
        let outcome = discovery::discover(root, None).context("building code graph")?;
        // Never overwrite a good graph with an empty one (no supported source under
        // `root`), matching `warmup` and `finish_discovery`.
        if outcome.response.files_parsed > 0 {
            let graph_file = data_dir.join("graph.json");
            outcome
                .graph
                .save(&graph_file)
                .with_context(|| format!("writing {}", graph_file.display()))?;
            write_manifest(&data_dir.join("graph.manifest.json"), &source_manifest);
            let _ = store.set_stat("graph_nodes", outcome.response.nodes as i64);
            let _ = store.set_stat("graph_edges", outcome.response.edges as i64);
        }
    }
    Ok(())
}

/// Whether the persisted graph is present, non-empty, and built from `manifest` — the
/// same gate the server's `ensure_graph` applies before rebuilding.
fn graph_is_current(data_dir: &Path, manifest: &BTreeMap<String, u64>) -> bool {
    let saved: Option<BTreeMap<String, u64>> =
        std::fs::read_to_string(data_dir.join("graph.manifest.json"))
            .ok()
            .and_then(|json| serde_json::from_str(&json).ok());
    if saved.as_ref() != Some(manifest) {
        return false;
    }
    discovery::graph::Graph::load(&data_dir.join("graph.json"))
        .map(|g| !g.nodes.is_empty())
        .unwrap_or(false)
}

/// Every file [`Index::index_path`] would index under `root`, walked the same
/// gitignore-respecting, nested-repo-pruning way it walks. Both the progress
/// denominator and the input to [`index_units`], so a batch can never name a path the
/// real indexer would skip.
fn indexable_files(root: &Path) -> Vec<PathBuf> {
    let mut builder = WalkBuilder::new(root);
    builder.standard_filters(true);
    let boundary_root = root.to_path_buf();
    builder.filter_entry(move |entry| {
        !(entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
            && discovery::is_repo_boundary(entry.path(), &boundary_root))
    });
    builder
        .build()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.into_path())
        .collect()
}

/// Split `root` into `(path, file count)` units to index one at a time, each a whole
/// recursive directory or a single file. Both shapes are safe to hand
/// [`Index::index_path`]: it loads the stored manifest for exactly the subtree it is
/// given, so a unit only ever prunes deletions inside itself. A *non-recursive*
/// directory would not be — it would read the whole subtree's manifest but see only
/// the top level, and delete everything below it — which is why loose files are
/// emitted one by one instead.
///
/// A directory at or under [`INDEX_BATCH_FILES`] is one unit; a bigger one is split
/// into its loose files plus its subdirectories, unless it holds so many loose files
/// that splitting would trade a handful of commits for thousands.
fn index_units(root: &Path, files: &[PathBuf]) -> Vec<(PathBuf, usize)> {
    let mut counts: HashMap<&Path, usize> = HashMap::new();
    let mut loose: HashMap<&Path, Vec<&Path>> = HashMap::new();
    let mut subdirs: HashMap<&Path, BTreeSet<&Path>> = HashMap::new();
    for file in files {
        let Some(parent) = file.parent() else { continue };
        loose.entry(parent).or_default().push(file);
        let mut dir = parent;
        loop {
            *counts.entry(dir).or_default() += 1;
            if dir == root {
                break;
            }
            let Some(up) = dir.parent() else { break };
            subdirs.entry(up).or_default().insert(dir);
            dir = up;
        }
    }

    let mut units = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let total = counts.get(dir).copied().unwrap_or(0);
        if total == 0 {
            continue;
        }
        let here = loose.get(dir).map(|f| f.len()).unwrap_or(0);
        if total <= INDEX_BATCH_FILES || here > INDEX_BATCH_LOOSE_MAX {
            units.push((dir.to_path_buf(), total));
            continue;
        }
        units.extend(
            loose
                .get(dir)
                .into_iter()
                .flatten()
                .map(|f| (f.to_path_buf(), 1)),
        );
        stack.extend(subdirs.get(dir).into_iter().flatten().copied());
    }
    units
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn warmup_writes_manifests_and_prunes_deletions() {
        let repo = tempdir().unwrap();
        let data = tempdir().unwrap();
        fs::write(repo.path().join("a.rs"), "fn alpha() {}\n").unwrap();
        fs::write(repo.path().join("b.rs"), "fn beta_unique() {}\n").unwrap();
        warmup(repo.path(), data.path()).unwrap();

        // Manifests written so the server's lazy path sees a fresh cache.
        assert!(data.path().join("graph.manifest.json").exists());
        assert!(data.path().join("index.manifest.json").exists());

        let idx = Index::open(data.path()).unwrap();
        assert!(
            idx.search(&["beta_unique".into()], 5).unwrap().results[0]
                .hits
                .iter()
                .any(|h| h.path.ends_with("b.rs")),
            "beta_unique searchable before deletion"
        );

        // Delete b.rs, warm up again → graph + index must reflect the deletion.
        fs::remove_file(repo.path().join("b.rs")).unwrap();
        warmup(repo.path(), data.path()).unwrap();

        let idx2 = Index::open(data.path()).unwrap();
        assert!(
            !idx2.search(&["beta_unique".into()], 5).unwrap().results[0]
                .hits
                .iter()
                .any(|h| h.path.ends_with("b.rs")),
            "deleted file must be pruned from the index"
        );
        let graph = discovery::graph::Graph::load(&data.path().join("graph.json")).unwrap();
        assert!(
            !graph.nodes.iter().any(|n| n.name == "beta_unique"),
            "deleted symbol must be gone from the graph"
        );
    }

    #[test]
    fn warmup_builds_queryable_graph_and_searchable_index() {
        let repo = tempdir().unwrap();
        let data = tempdir().unwrap();
        fs::write(
            repo.path().join("lib.rs"),
            "fn helper() -> i32 { 1 }\nfn main() { let _ = helper(); }\n",
        )
        .unwrap();

        warmup(repo.path(), data.path()).unwrap();

        // Graph persisted and queryable for a symbol.
        let graph_file = data.path().join("graph.json");
        assert!(graph_file.exists(), "graph.json written");
        let graph = discovery::graph::Graph::load(&graph_file).unwrap();
        let view = discovery::query::query(&graph, "helper", None, 10, &[]);
        assert!(
            view.nodes.iter().any(|n| n.name == "helper"),
            "graph has helper"
        );

        // Index populated and searchable.
        let idx = Index::open(data.path()).unwrap();
        assert!(idx.chunk_count().unwrap() > 0, "index has chunks");
        let res = idx.search(&["helper".into()], 5).unwrap();
        assert!(
            res.results[0]
                .hits
                .iter()
                .any(|h| h.path.ends_with("lib.rs")),
            "search finds lib.rs"
        );

        // Count stats recorded for lens_stats / dashboard.
        let store = Store::open(data.path()).unwrap();
        assert!(store.get_stat("graph_nodes").unwrap() >= 3);
        assert!(store.get_stat("index_chunks").unwrap() >= 1);
    }

    /// The progress line is the whole contract between builder and server, including
    /// the phase field an older builder wouldn't have written.
    #[test]
    fn progress_line_round_trips_including_a_missing_phase() {
        let state = parse_progress("4242 1720 14502 index").unwrap();
        assert_eq!(
            state,
            BuildProgressState {
                pid: 4242,
                done: 1720,
                total: 14502,
                indexing: true
            }
        );
        assert!(!parse_progress("1 2 3 graph").unwrap().indexing);
        assert!(
            parse_progress("1 2 3").unwrap().indexing,
            "no phase field means the index is still filling in"
        );
        assert_eq!(parse_progress(""), None);
        assert_eq!(parse_progress("not a pid 0 0"), None);
    }

    /// Seeding never clobbers a builder that has already published real counts, so the
    /// server's post-spawn write can't race the child's first one backwards.
    #[test]
    fn seed_progress_never_overwrites_published_counts() {
        let data = tempdir().unwrap();
        fs::write(progress_path(data.path()), "77 900 1000 index").unwrap();
        seed_progress(data.path(), 99);
        assert_eq!(read_progress(data.path()).unwrap().done, 900);
    }

    /// The batch partition must be a partition: every indexable file covered exactly
    /// once, so nothing is skipped and nothing is indexed twice. A directory of loose
    /// files stays one unit however big it is (splitting it would mean one commit per
    /// file), while a wide tree splits down to per-directory units.
    #[test]
    fn index_units_cover_every_file_exactly_once() {
        let repo = tempdir().unwrap();
        let root = repo.path();
        fs::write(root.join("Cargo.toml"), "[package]\nname='x'\n").unwrap();
        fs::create_dir_all(root.join("flat")).unwrap();
        for i in 0..(INDEX_BATCH_FILES + 50) {
            fs::write(root.join("flat").join(format!("f{i}.rs")), "fn a() {}\n").unwrap();
        }
        for d in 0..3 {
            let dir = root.join("wide").join(format!("d{d}"));
            fs::create_dir_all(&dir).unwrap();
            for i in 0..10 {
                fs::write(dir.join(format!("g{i}.rs")), "fn b() {}\n").unwrap();
            }
        }

        let files = indexable_files(root);
        let units = index_units(root, &files);
        assert!(units.len() > 1, "a big tree must split: {units:?}");

        // Exhaustive and disjoint: assign each file to the units containing it.
        for file in &files {
            let owners = units
                .iter()
                .filter(|(unit, _)| file == unit || file.starts_with(unit))
                .count();
            assert_eq!(owners, 1, "{} covered by {owners} units", file.display());
        }
        // Counts are what the progress denominator is spent against, so they must
        // total the file set exactly.
        assert_eq!(
            units.iter().map(|(_, n)| n).sum::<usize>(),
            files.len(),
            "unit counts must sum to the total"
        );
        assert!(
            units
                .iter()
                .any(|(unit, n)| unit.ends_with("flat") && *n == INDEX_BATCH_FILES + 50),
            "a directory of loose files stays one unit: {units:?}"
        );
    }

    /// The background build is the same artifact as a foreground one: same searchable
    /// index, same graph, same manifests. It also skips a phase that is already
    /// current, which is what makes a killed build resume instead of starting over.
    #[test]
    fn background_build_matches_a_foreground_one_and_skips_current_phases() {
        let repo = tempdir().unwrap();
        fs::write(repo.path().join("Cargo.toml"), "[package]\nname='x'\n").unwrap();
        fs::create_dir_all(repo.path().join("src")).unwrap();
        fs::write(
            repo.path().join("src").join("lib.rs"),
            "fn helper() -> i32 { 1 }\nfn background_unique() { helper(); }\n",
        )
        .unwrap();

        let bg = tempdir().unwrap();
        warmup_background(repo.path(), bg.path()).unwrap();
        let fg = tempdir().unwrap();
        warmup(repo.path(), fg.path()).unwrap();

        for data in [bg.path(), fg.path()] {
            assert!(data.join("index.manifest.json").exists());
            assert!(data.join("graph.manifest.json").exists());
            let idx = Index::open(data).unwrap();
            assert!(
                idx.search(&["background_unique".into()], 5).unwrap().results[0]
                    .hits
                    .iter()
                    .any(|h| h.path.ends_with("lib.rs")),
                "searchable after the build in {}",
                data.display()
            );
            let graph = discovery::graph::Graph::load(&data.join("graph.json")).unwrap();
            assert!(graph.nodes.iter().any(|n| n.name == "background_unique"));
        }
        assert_eq!(
            Index::open(bg.path()).unwrap().chunk_count().unwrap(),
            Index::open(fg.path()).unwrap().chunk_count().unwrap(),
            "batched indexing must land the same chunks as one whole-repo pass"
        );

        // The progress file is removed on completion: that absence is the server's
        // entire readiness signal.
        assert!(!progress_path(bg.path()).exists());

        // Everything is current now, so the graph phase is skipped: delete the graph's
        // inputs' only other trace and confirm the manifest gate, not a rebuild, is
        // what decides.
        let before = fs::metadata(bg.path().join("graph.json"))
            .unwrap()
            .modified()
            .unwrap();
        assert!(graph_is_current(
            bg.path(),
            &discovery::source_manifest(
                &std::fs::canonicalize(repo.path()).unwrap_or_else(|_| repo.path().to_path_buf())
            ),
        ));
        warmup_background(repo.path(), bg.path()).unwrap();
        assert_eq!(
            fs::metadata(bg.path().join("graph.json"))
                .unwrap()
                .modified()
                .unwrap(),
            before,
            "a current graph must not be rewritten"
        );
    }
}
