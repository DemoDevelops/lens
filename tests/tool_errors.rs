//! Bug B gate: a recoverable tool failure must reach the model as readable
//! `is_error` content it can act on (and fall back from), not as a JSON-RPC `-32603`
//! protocol error it never sees.
//!
//! This drives the real binary over MCP (so it exercises the actual JSON-RPC
//! boundary, where the bug lives) and forces `lens_search` to fail by holding the
//! `index.db` write lock while a freshly-added file forces an auto-reindex. With
//! `LENS_BUSY_MS=0` the blocked reindex gives up at once. Pre-fix the handler returned
//! `Err(ErrorData)`, which rmcp serializes as a protocol error and `call_tool` surfaces
//! as `Err` (RED); post-fix it returns a `CallToolResult{is_error:true}` carrying the
//! cause and a fallback instruction (GREEN).

use rmcp::model::CallToolRequestParams;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use serde_json::json;
use tokio::process::Command;

#[tokio::test]
async fn failure_is_readable_content() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("lib.rs"), "fn helper() -> i32 { 1 }\n").unwrap();

    let bin = env!("CARGO_BIN_EXE_lens");
    let repo_path = repo.path().to_path_buf();
    let data_path = data.path().to_path_buf();

    let transport = TokioChildProcess::new(Command::new(bin).configure(|cmd| {
        cmd.current_dir(&repo_path)
            .env("LENS_DIR", &data_path)
            .env("LENS_MAX_INLINE", "8192")
            // Walk on every call so a newly added file is seen immediately, and give up
            // on a locked DB at once so the forced failure is fast and deterministic.
            .env("LENS_WALK_DEBOUNCE_MS", "0")
            .env("LENS_BUSY_MS", "0");
    }))
    .unwrap();
    let client = ().serve(transport).await.expect("handshake");

    // 1. First search builds the index (creates index.db, writes the manifest) with no
    //    lock contention, so it succeeds.
    let mut warm = CallToolRequestParams::new("lens_search");
    warm.arguments = json!({ "queries": ["helper"] }).as_object().cloned();
    let warm_res = client.call_tool(warm).await.expect("warm search should succeed");
    assert_ne!(
        warm_res.is_error,
        Some(true),
        "the warm-up search should succeed before the lock is held"
    );

    // 2. Hold the index.db write lock. BEGIN IMMEDIATE takes the write lock at once, so
    //    any other writer now blocks. (rusqlite is a crate dependency, available to
    //    integration tests.)
    let hold = rusqlite::Connection::open(data.path().join("index.db")).unwrap();
    hold.execute_batch("BEGIN IMMEDIATE;").unwrap();

    // 3. Add a file so the next search finds the index stale and must reindex (write).
    std::fs::write(repo.path().join("added.rs"), "fn newly_added() {}\n").unwrap();

    // 4. The next search auto-reindexes, which is now blocked. The failure must come
    //    back as a readable is_error tool result, NOT a JSON-RPC protocol error.
    let mut params = CallToolRequestParams::new("lens_search");
    params.arguments = json!({ "queries": ["helper"] }).as_object().cloned();
    let res = client.call_tool(params).await;
    let result = res.expect(
        "lens_search must return a tool result, not a JSON-RPC protocol error (the bug)",
    );
    assert_eq!(
        result.is_error,
        Some(true),
        "a tool failure must be flagged is_error so the model can react"
    );
    let blob = serde_json::to_string(&result).unwrap().to_lowercase();
    assert!(
        blob.contains("fall back"),
        "failure content must instruct a fallback: {blob}"
    );
    assert!(
        blob.contains("lens tool failed"),
        "failure content must carry the cause: {blob}"
    );

    drop(hold);
    client.cancel().await.ok();
}
