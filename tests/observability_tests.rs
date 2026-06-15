//! Observability integration tests (plan §6): one well-formed ops.log record per
//! tool invocation with correct byte/token fields; stdout stays pure JSON-RPC;
//! concurrency stress (shared stores, WAL) with zero corruption + roundtrip; and
//! explain mode never changes a tool result payload.

use std::time::Duration;

use rmcp::model::CallToolRequestParams;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use ctxforge::obs::stats::{aggregate, read_records};
use ctxforge::obs::OpLog;
use ctxforge::sandbox;
use ctxforge::store::Store;
use ctxforge::tools::ExecuteRequest;

/// Read and parse every ops.log record under a data dir.
fn ops_lines(data_dir: &std::path::Path) -> Vec<Value> {
    let raw = std::fs::read_to_string(data_dir.join("ops.log")).unwrap_or_default();
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).expect("each ops.log line must be valid JSON"))
        .collect()
}

// ---------------------------------------------------------------------------
// §6: every tool invocation writes exactly one well-formed record
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_tool_call_logs_one_correct_record() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::write(
        repo.path().join("lib.rs"),
        "fn helper() -> i32 { 1 }\nfn main() { let _ = helper(); }\n",
    )
    .unwrap();
    std::fs::write(repo.path().join("big.txt"), "z".repeat(200_000)).unwrap();

    let bin = env!("CARGO_BIN_EXE_ctxforge");
    let repo_path = repo.path().to_path_buf();
    let data_path = data.path().to_path_buf();
    let transport = TokioChildProcess::new(Command::new(bin).configure(|cmd| {
        cmd.current_dir(&repo_path)
            .env("CTXFORGE_DIR", &data_path)
            .env("CTXFORGE_MAX_INLINE", "8192");
    }))
    .unwrap();
    let client = ().serve(transport).await.expect("handshake");

    let call = |name: &'static str, args: Value| {
        let client = &client;
        async move {
            let mut params = CallToolRequestParams::new(name);
            params.arguments = args.as_object().cloned();
            client
                .call_tool(params)
                .await
                .unwrap()
                .structured_content
                .expect("structured content")
        }
    };

    // A known large-output op, then the rest of the surface.
    let exec = call(
        "ctx_execute",
        json!({ "language": "python", "code": "data = open('big.txt').read(); print('A' * 50000)" }),
    )
    .await;
    let exec_ref = exec["retrieve_ref"].as_str().unwrap().to_string();
    let exec_stdout_bytes = exec["stdout_bytes"].as_u64().unwrap();
    let exec_returned =
        exec["stdout"].as_str().unwrap().len() + exec["stderr"].as_str().unwrap().len();

    call("ctx_retrieve", json!({ "ref": exec_ref })).await;
    call("ctx_index", json!({ "path": "." })).await;
    call("ctx_search", json!({ "queries": ["helper"] })).await;
    call("ctx_discover", json!({ "path": "." })).await;
    call("graph_query", json!({ "name": "helper" })).await;
    call("graph_path", json!({ "from": "main", "to": "helper" })).await;
    call("ctx_stats", json!({})).await;
    client.cancel().await.ok();

    // 8 successful tool calls -> exactly 8 records, all well-formed.
    let records = ops_lines(data.path());
    assert_eq!(records.len(), 8, "one record per tool call");
    for r in &records {
        assert!(r["ts"].as_str().unwrap().ends_with('Z'));
        assert!(r["tool"].is_string());
        assert!(r["pid"].is_u64());
        assert!(r["agent_id"].is_string());
        assert!(r["duration_ms"].is_u64());
        assert_eq!(r["outcome"], json!("ok"));
    }

    // The known ctx_execute op: byte/token fields are exactly right.
    let e = records.iter().find(|r| r["tool"] == json!("ctx_execute")).unwrap();
    assert_eq!(e["raw_bytes_in"].as_u64().unwrap(), exec_stdout_bytes);
    assert_eq!(e["bytes_returned"].as_u64().unwrap(), exec_returned as u64);
    assert_eq!(e["store_ref"].as_str().unwrap(), exec_ref);
    let expected_saved = (exec_stdout_bytes as i64 - exec_returned as i64).max(0) / 4;
    assert_eq!(e["tokens_saved_est"].as_i64().unwrap(), expected_saved);
    assert!(expected_saved > 0, "the large op must show real savings");
}

// ---------------------------------------------------------------------------
// §6: MCP server stdout stays pure JSON-RPC (even with explain on)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn server_stdout_is_pure_json_rpc() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_ctxforge");

    // Explain ON so observability is maximally chatty — if anything leaked to
    // stdout instead of the log files, we'd catch it here.
    let mut child = Command::new(bin)
        .current_dir(repo.path())
        .env("CTXFORGE_DIR", data.path())
        .env("CTXFORGE_EXPLAIN", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();

    // Newline-delimited JSON-RPC: initialize, initialized, a tool call that
    // offloads to the store (exercises the write paths + explain).
    let msgs = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"purity-test","version":"0"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"ctx_execute","arguments":{"language":"python","code":"print('A'*50000)"}}}),
    ];
    for m in &msgs {
        stdin.write_all(format!("{m}\n").as_bytes()).await.unwrap();
    }
    stdin.flush().await.unwrap();

    // Collect stdout lines until we've seen the tool-call response (id 2) or time out.
    let mut reader = BufReader::new(stdout).lines();
    let mut lines: Vec<String> = Vec::new();
    let mut saw_tool_response = false;
    let deadline = tokio::time::timeout(Duration::from_secs(15), async {
        while let Ok(Some(line)) = reader.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let parsed: Value = serde_json::from_str(&line)
                .unwrap_or_else(|_| panic!("non-JSON-RPC line leaked to stdout: {line:?}"));
            assert_eq!(parsed["jsonrpc"], json!("2.0"), "every stdout line is JSON-RPC");
            if parsed["id"] == json!(2) {
                saw_tool_response = true;
                break;
            }
            lines.push(line);
        }
    })
    .await;
    let _ = deadline; // timing out is fine; the purity assertions above are the point
    let _ = stdin.shutdown().await;
    let _ = child.kill().await;

    assert!(saw_tool_response, "should have received the tool-call response");
    // Instrumentation actually ran (proving it had the chance to leak) but went to files.
    assert!(data.path().join("ops.log").exists(), "ops.log was written");
    assert!(data.path().join("explain.log").exists(), "explain.log was written");
    assert!(!ops_lines(data.path()).is_empty());
}

// ---------------------------------------------------------------------------
// §4/§6: concurrency stress — shared stores, WAL, zero corruption, roundtrip
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_workers_no_corruption_all_roundtrip() {
    let repo = tempfile::tempdir().unwrap();
    let data = repo.path().join(".ctxforge");
    let store = Store::open(&data).unwrap();
    let ops = OpLog::open(&data);

    const N: usize = 24;
    let mut handles = Vec::new();
    for i in 0..N {
        let store = store.clone();
        let ops = ops.clone();
        let repo_dir = repo.path().to_path_buf();
        handles.push(tokio::spawn(async move {
            // Unique large output per worker so every blob (and ref) is distinct.
            let marker = format!("L{i:03}");
            let code = format!("print('{marker}' * 20000)");
            let req = ExecuteRequest {
                language: "python".into(),
                code,
                timeout_secs: 30,
                stdin: None,
            };
            let resp = sandbox::run(req, &repo_dir, &store, 8192).await.expect("no lock error");
            // Record an op too, so ops.log takes concurrent appends.
            let returned = (resp.stdout.len() + resp.stderr.len()) as u64;
            ops.start("ctx_execute", json!({ "worker": i }))
                .finish(
                    resp.stdout_bytes as u64,
                    returned,
                    resp.retrieve_ref.clone(),
                    "ok",
                    "",
                    None,
                );
            (marker, resp)
        }));
    }

    let mut refs = Vec::new();
    for h in handles {
        let (marker, resp) = h.await.unwrap();
        assert!(resp.truncated, "large output should offload");
        let reference = resp.retrieve_ref.clone().expect("ref");
        // Exact losslessness: the store reproduces the worker's full output.
        let full = store.get(&reference).unwrap().expect("blob present");
        let expected = format!("{}\n", marker.repeat(20000));
        assert_eq!(full, expected, "store roundtrip must be byte-exact");
        refs.push(reference);
    }

    // WAL is actually engaged on the shared store DB.
    assert_eq!(store.journal_mode().unwrap().to_lowercase(), "wal");

    // Every worker's op appended a complete, parseable line — no interleave corruption.
    let records = read_records(&data, None);
    assert_eq!(records.len(), N, "all ops logged, none torn");
    let totals = aggregate(&records);
    assert_eq!(totals.ops as usize, N);
    assert!(totals.tokens_saved_est > 0);
    // lock_wait_ms is surfaced (may be zero under WAL; the field is always present).
    println!("concurrency lock_wait_ms total: {}", totals.lock_wait_ms);
}

// ---------------------------------------------------------------------------
// §6: CTXFORGE_EXPLAIN=1 never changes a tool result payload
// ---------------------------------------------------------------------------

async fn run_exec(data_dir: &std::path::Path, explain: bool) -> Value {
    let repo = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_ctxforge");
    let repo_path = repo.path().to_path_buf();
    let data_path = data_dir.to_path_buf();
    let transport = TokioChildProcess::new(Command::new(bin).configure(move |cmd| {
        cmd.current_dir(&repo_path)
            .env("CTXFORGE_DIR", &data_path)
            .env("CTXFORGE_MAX_INLINE", "8192");
        if explain {
            cmd.env("CTXFORGE_EXPLAIN", "1");
        }
    }))
    .unwrap();
    let client = ().serve(transport).await.expect("handshake");
    let mut params = CallToolRequestParams::new("ctx_execute");
    params.arguments = json!({ "language": "python", "code": "print('A'*50000)" })
        .as_object()
        .cloned();
    let out = client
        .call_tool(params)
        .await
        .unwrap()
        .structured_content
        .expect("structured content");
    client.cancel().await.ok();
    out
}

#[tokio::test]
async fn explain_mode_does_not_change_result_payload() {
    let data_off = tempfile::tempdir().unwrap();
    let data_on = tempfile::tempdir().unwrap();

    let off = run_exec(data_off.path(), false).await;
    let on = run_exec(data_on.path(), true).await;

    // Byte-identical result payloads (deterministic content -> same ref).
    assert_eq!(
        serde_json::to_string(&off).unwrap(),
        serde_json::to_string(&on).unwrap(),
        "explain must not alter the tool result"
    );

    // explain.log only exists when explain is on.
    assert!(!data_off.path().join("explain.log").exists());
    assert!(data_on.path().join("explain.log").exists());
    // ops.log written in both regardless.
    assert!(data_off.path().join("ops.log").exists());
    assert!(data_on.path().join("ops.log").exists());
}
