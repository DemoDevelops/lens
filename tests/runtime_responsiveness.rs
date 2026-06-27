//! T4 gate: a tool call stuck on a locked index must not freeze the server. While one
//! `lens_search` is blocked on the index write lock (and will return an is_error
//! fallback after the busy ceiling), a second quick call (`lens_stats`, which does not
//! touch index.db) must still complete promptly.

use rmcp::model::CallToolRequestParams;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;

#[tokio::test(flavor = "multi_thread")]
async fn call_returns_under_contention() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("lib.rs"), "fn helper() {}\n").unwrap();

    let bin = env!("CARGO_BIN_EXE_lens");
    let repo_path = repo.path().to_path_buf();
    let data_path = data.path().to_path_buf();
    let transport = TokioChildProcess::new(Command::new(bin).configure(|cmd| {
        cmd.current_dir(&repo_path)
            .env("LENS_DIR", &data_path)
            .env("LENS_WALK_DEBOUNCE_MS", "0")
            // The stuck search waits ~1.5s on the lock, then returns an is_error fallback.
            .env("LENS_BUSY_MS", "1500");
    }))
    .unwrap();
    let client = Arc::new(().serve(transport).await.expect("handshake"));

    // Warm the index with no contention so it succeeds.
    let mut warm = CallToolRequestParams::new("lens_search");
    warm.arguments = json!({ "queries": ["helper"] }).as_object().cloned();
    client.call_tool(warm).await.expect("warm search should succeed");

    // Hold the index write lock, then make the index stale so the next search must
    // reindex and block on the lock.
    let hold = rusqlite::Connection::open(data.path().join("index.db")).unwrap();
    hold.execute_batch("BEGIN IMMEDIATE;").unwrap();
    std::fs::write(repo.path().join("added.rs"), "fn added() {}\n").unwrap();

    // Fire the search that will block on the held lock.
    let cs = client.clone();
    let search_task = tokio::spawn(async move {
        let t = Instant::now();
        let mut p = CallToolRequestParams::new("lens_search");
        p.arguments = json!({ "queries": ["helper"] }).as_object().cloned();
        let r = cs.call_tool(p).await;
        (r, t.elapsed())
    });

    // Give the search time to get stuck on the lock, then issue a quick call.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t = Instant::now();
    let mut sp = CallToolRequestParams::new("lens_stats");
    sp.arguments = json!({}).as_object().cloned();
    let stats_res = client.call_tool(sp).await;
    let stats_dur = t.elapsed();

    let (search_res, search_dur) = search_task.await.unwrap();

    // The quick call completed while the search was still blocked: the server stayed
    // responsive (it did not freeze on the sync busy-sleep). Measured ~7ms here vs a
    // ~1.9s blocked search, so this guards against a regression to serial request
    // dispatch or a single-threaded runtime, where the quick call would wait out the
    // stuck one. (The multi-thread `#[tokio::main]` runtime already provides this; an
    // explicit spawn_blocking of the cold-start work was therefore not needed.)
    stats_res.expect("the quick lens_stats call must complete while a search is blocked");
    assert!(
        stats_dur < Duration::from_millis(900),
        "lens_stats was blocked by the stuck search ({stats_dur:?}); the runtime froze"
    );

    // The stuck search came back as a readable is_error fallback within bounded time.
    let search = search_res.expect("search must return a tool result, not a protocol error");
    assert_eq!(
        search.is_error,
        Some(true),
        "the blocked search must return an is_error fallback"
    );
    assert!(
        search_dur >= Duration::from_millis(800),
        "the search should have waited on the lock before failing ({search_dur:?})"
    );

    drop(hold);
}
