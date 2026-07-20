//! Concurrency gate for the FTS index writer (Bug A: "database is locked").
//!
//! `index_path` used to hold the `index.db` write lock for an entire repo walk
//! (one transaction opened before the walk, committed after it). A second writer
//! (the MCP server's warmup/auto-index, or a concurrent `lens index`) then spun
//! on the busy handler until its ceiling and got `SQLITE_BUSY`. Batching the
//! commits (release the lock every ~150 files) lets concurrent writers interleave.
//!
//! `concurrent_writers_no_lock` sets `LENS_BUSY_MS` in-process; the cross-process
//! build-lock tests below never read that env in-parent (they configure it, if at
//! all, on the child), so they don't race it.
//!
//! The remaining tests cover the cross-process build lock (T5): N cold sessions
//! opening one shared `.lens` must not stampede-build. They drive the real MCP
//! server binary (one OS process per session, so `std::process::id()` and the
//! `kill(pid, 0)` liveness probe are exercised for real) and count actual builds by
//! reading the op ledger — `ensure_index`/`ensure_graph` append exactly one
//! `lens_index`/`lens_map` record per real build and none for a fresh early-return.

use lens::index::Index;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use serde_json::{json, Value};
use std::path::Path;
use std::thread;
use tokio::process::Command;

/// Files per writer: a few COMMIT_BATCH-sized batches. Enough that two writers
/// started together overlap their write phases (pre-fix, the second writer's deferred
/// write tries to upgrade past the first's lock and SQLite returns SQLITE_BUSY
/// *without* the busy handler, so it fails instantly (RED). Small enough that
/// post-fix the brief, lock-free read phase of each batch lets the other writer
/// interleave well within the 200 ms ceiling rather than starving over many rounds.
const FILES_PER_WRITER: usize = 400;

/// Lines per file: a single short function (one chunk), keeping per-file work low so
/// a 150-file batch commits quickly.
const LINES_PER_FILE: usize = 4;

fn write_corpus(dir: &std::path::Path, prefix: &str) {
    for i in 0..FILES_PER_WRITER {
        let mut body = String::with_capacity(LINES_PER_FILE * 48);
        for l in 0..LINES_PER_FILE {
            // A distinct symbol per line so chunk_symbols does real work too.
            body.push_str(&format!(
                "fn {prefix}_fn_{i}_{l}() {{ let {prefix}_v_{i}_{l} = {l}; }}\n"
            ));
        }
        std::fs::write(dir.join(format!("{prefix}_{i}.rs")), body).unwrap();
    }
}

#[test]
fn concurrent_writers_no_lock() {
    // A short busy ceiling: a writer that cannot acquire the lock within 200 ms of
    // retries gives up with SQLITE_BUSY. Must be set before any connection opens.
    std::env::set_var("LENS_BUSY_MS", "200");

    let data = tempfile::tempdir().unwrap();
    let corpus_a = tempfile::tempdir().unwrap();
    let corpus_b = tempfile::tempdir().unwrap();
    write_corpus(corpus_a.path(), "a");
    write_corpus(corpus_b.path(), "b");

    // Both writers share one index.db (same data dir). `Index` is just a path, so
    // each clone opens its own connection: the real two-writer scenario.
    let idx = Index::open(data.path()).unwrap();
    let (idx_a, idx_b) = (idx.clone(), idx.clone());
    let (root_a, root_b) = (
        corpus_a.path().to_path_buf(),
        corpus_b.path().to_path_buf(),
    );

    let ha = thread::spawn(move || idx_a.index_path(&root_a, true));
    let hb = thread::spawn(move || idx_b.index_path(&root_b, true));
    let ra = ha.join().expect("writer A panicked");
    let rb = hb.join().expect("writer B panicked");

    std::env::remove_var("LENS_BUSY_MS");

    assert!(
        ra.is_ok(),
        "writer A got a lock error (the bug): {:?}",
        ra.err()
    );
    assert!(
        rb.is_ok(),
        "writer B got a lock error (the bug): {:?}",
        rb.err()
    );

    // Both corpora must be fully indexed — batched commits must not drop work.
    let total = idx.chunk_count().unwrap();
    let expected = (2 * FILES_PER_WRITER) as i64; // >= 1 chunk per file, both corpora
    assert!(
        total >= expected,
        "expected >= {expected} chunks across both corpora, got {total}"
    );
}

// ── Cross-process build lock (T5) ───────────────────────────────────────────
//
// Files per race corpus: large enough that a build takes long enough for
// concurrently-fired sessions to genuinely overlap on `ensure_index`/`ensure_graph`
// (pre-lock, each cold session would build → N builds), small enough to stay quick.
const RACE_FILES: usize = 200;

/// A repo whose every file defines a distinct `fn item_sym_<i>` — so `item_sym_0`
/// is both a searchable token (FTS) and a graph node (structural symbol), letting
/// one corpus drive both the index race (`lens_search`) and the graph race
/// (`lens_symbol`).
fn write_race_corpus(dir: &Path) {
    for i in 0..RACE_FILES {
        let body = format!("fn item_sym_{i}() -> i32 {{ let racecorpus_v_{i} = {i}; racecorpus_v_{i} }}\n");
        std::fs::write(dir.join(format!("f_{i}.rs")), body).unwrap();
    }
}

/// Bring up one real MCP server subprocess over stdio (its own OS process, so it has
/// a distinct pid), sharing `data` as `LENS_DIR` and rooted at `repo`, handshake done.
async fn spawn_server(repo: &Path, data: &Path) -> RunningService<RoleClient, ()> {
    let bin = env!("CARGO_BIN_EXE_lens");
    let repo = repo.to_path_buf();
    let data = data.to_path_buf();
    let transport = TokioChildProcess::new(Command::new(bin).configure(move |cmd| {
        cmd.current_dir(&repo).env("LENS_DIR", &data);
    }))
    .unwrap();
    ().serve(transport).await.expect("handshake")
}

/// Call a tool and return its structured content.
async fn call_tool(client: &RunningService<RoleClient, ()>, name: &'static str, args: Value) -> Value {
    let mut params = CallToolRequestParams::new(name);
    params.arguments = args.as_object().cloned();
    client
        .call_tool(params)
        .await
        .unwrap()
        .structured_content
        .expect("structured content")
}

/// Count op-ledger records for `tool` in `<data>/ops.log`. Each real build appends
/// exactly one (`lens_index` for the FTS index, `lens_map` for the graph); a fresh
/// early-return appends none. This is the build-operation counter the predicate asks
/// for, read straight off disk without touching the public API.
fn build_op_count(data: &Path, tool: &str) -> usize {
    let log = std::fs::read_to_string(data.join("ops.log")).unwrap_or_default();
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|r| r["tool"] == json!(tool))
        .count()
}

/// Spawn a short-lived child and reap it, returning a pid that is now dead — exactly
/// what a crashed lock holder looks like to `kill(pid, 0)`.
fn reaped_dead_pid() -> i32 {
    let mut child = std::process::Command::new("true")
        .spawn()
        .expect("spawn a short-lived child");
    let pid = child.id() as i32;
    child.wait().expect("reap the child");
    pid
}

/// Three cold sessions open one shared `.lens` and search at once. Without a
/// cross-process lock each would pass its own freshness check and build, giving three
/// FTS builds. The lock must collapse that to exactly one build while every session
/// still gets a ready index and real hits.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_cold_sessions_build_the_index_once() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_race_corpus(repo.path());

    // Bring all three servers up first (handshakes done), THEN fire the searches
    // together so they contend on `ensure_index` rather than serializing behind
    // staggered startup.
    let c0 = spawn_server(repo.path(), data.path()).await;
    let c1 = spawn_server(repo.path(), data.path()).await;
    let c2 = spawn_server(repo.path(), data.path()).await;

    let args = || json!({ "queries": ["item_sym_0"] });
    let (r0, r1, r2) = tokio::join!(
        call_tool(&c0, "lens_search", args()),
        call_tool(&c1, "lens_search", args()),
        call_tool(&c2, "lens_search", args()),
    );
    c0.cancel().await.ok();
    c1.cancel().await.ok();
    c2.cancel().await.ok();

    // Every session succeeded against a ready index (none errored, none returned before
    // the build finished): the token is present in the results.
    for (n, r) in [&r0, &r1, &r2].iter().enumerate() {
        let hits = r["results"][0]["hits"].as_array().unwrap();
        assert!(
            hits.iter()
                .any(|h| h["path"].as_str().unwrap().ends_with("f_0.rs")),
            "session {n} should find item_sym_0 in a ready index: {r}"
        );
    }

    // Exactly one real FTS build across all three cold sessions.
    let builds = build_op_count(data.path(), "lens_index");
    assert_eq!(builds, 1, "cold index stampede must collapse to one build");
}

/// The graph counterpart: three cold sessions calling `lens_symbol` at once must
/// produce exactly one graph build (`ensure_graph` shares the same lock).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_cold_sessions_build_the_graph_once() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_race_corpus(repo.path());

    let c0 = spawn_server(repo.path(), data.path()).await;
    let c1 = spawn_server(repo.path(), data.path()).await;
    let c2 = spawn_server(repo.path(), data.path()).await;

    let args = || json!({ "name": "item_sym_0" });
    let (r0, r1, r2) = tokio::join!(
        call_tool(&c0, "lens_symbol", args()),
        call_tool(&c1, "lens_symbol", args()),
        call_tool(&c2, "lens_symbol", args()),
    );
    c0.cancel().await.ok();
    c1.cancel().await.ok();
    c2.cancel().await.ok();

    for (n, r) in [&r0, &r1, &r2].iter().enumerate() {
        let nodes = r["nodes"].as_array().unwrap();
        assert!(
            nodes.iter().any(|node| node["name"] == json!("item_sym_0")),
            "session {n} should find item_sym_0 in a ready graph: {r}"
        );
    }

    let builds = build_op_count(data.path(), "lens_map");
    assert_eq!(builds, 1, "cold graph stampede must collapse to one build");
}

/// A lock left behind by a crashed holder (its recorded pid is dead) must not wedge
/// the next session forever: the dead-pid check reclaims the stale lock, the build
/// proceeds, and the lock is released (no leaked `build.pid`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_lock_with_dead_pid_is_reclaimed() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_race_corpus(repo.path());

    // Plant a stale lock: a build.pid naming a process that has already exited.
    let lock = data.path().join("build.pid");
    std::fs::write(&lock, reaped_dead_pid().to_string()).unwrap();

    let client = spawn_server(repo.path(), data.path()).await;
    let r = call_tool(&client, "lens_search", json!({ "queries": ["item_sym_0"] })).await;
    client.cancel().await.ok();

    // The session did not hang on the stale lock — it reclaimed it and built.
    let hits = r["results"][0]["hits"].as_array().unwrap();
    assert!(
        hits.iter()
            .any(|h| h["path"].as_str().unwrap().ends_with("f_0.rs")),
        "search should reclaim the stale lock, build, and return hits: {r}"
    );
    assert_eq!(
        build_op_count(data.path(), "lens_index"),
        1,
        "the reclaiming session builds exactly once"
    );
    // The RAII guard released the reclaimed lock on completion.
    assert!(!lock.exists(), "build.pid must not leak after the build");
}
