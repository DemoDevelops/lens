//! End-to-end test: spawn the compiled binary, complete a real MCP handshake
//! over stdio, and exercise every tool through the rmcp client.

use rmcp::model::CallToolRequestParams;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use serde_json::{json, Value};
use tokio::process::Command;

#[tokio::test]
async fn full_mcp_session() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    // A repo with a code file (for index/discover) and a big data file.
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

    // serve() performs the MCP initialize handshake.
    let client = ().serve(transport).await.expect("handshake");

    // Tools are advertised.
    let tools = client.list_tools(Default::default()).await.unwrap();
    let names: Vec<String> = tools.tools.iter().map(|t| t.name.to_string()).collect();
    for expected in [
        "ctx_execute",
        "ctx_index",
        "ctx_search",
        "ctx_discover",
        "graph_query",
        "graph_neighbors",
        "graph_path",
        "ctx_retrieve",
        "ctx_stats",
    ] {
        assert!(names.contains(&expected.to_string()), "missing {expected}");
    }

    // Every tool is stamped `anthropic/alwaysLoad` so Claude Code never defers them.
    for t in &tools.tools {
        let meta = t
            .meta
            .as_ref()
            .unwrap_or_else(|| panic!("{} missing _meta", t.name));
        assert_eq!(
            meta.0.get("anthropic/alwaysLoad"),
            Some(&json!(true)),
            "{} should be marked alwaysLoad",
            t.name
        );
    }

    let call = |name: &'static str, args: Value| {
        let client = &client;
        async move {
            let mut params = CallToolRequestParams::new(name);
            params.arguments = args.as_object().cloned();
            let res = client.call_tool(params).await.unwrap();
            res.structured_content
                .expect("structured content for tool result")
        }
    };

    // --- ctx_execute: large output offloaded; raw input never returned ---
    let exec = call(
        "ctx_execute",
        json!({
            "language": "python",
            "code": "data = open('big.txt').read(); print('A' * 50000)"
        }),
    )
    .await;
    assert_eq!(exec["truncated"], json!(true));
    assert!(exec["stdout"].as_str().unwrap().len() < 50000);
    assert!(!exec["stdout"].as_str().unwrap().contains(&"z".repeat(50)));
    let exec_ref = exec["retrieve_ref"].as_str().unwrap().to_string();

    // --- ctx_retrieve: recover the full offloaded output ---
    let retrieved = call("ctx_retrieve", json!({ "ref": exec_ref })).await;
    assert!(retrieved["content"].as_str().unwrap().contains(&"A".repeat(50000)));

    // --- ctx_index + ctx_search ---
    let indexed = call("ctx_index", json!({ "path": "." })).await;
    assert!(indexed["files_indexed"].as_u64().unwrap() >= 1);
    let searched = call("ctx_search", json!({ "queries": ["helper"] })).await;
    let hits = &searched["results"][0]["hits"];
    assert!(hits.as_array().unwrap().iter().any(|h| h["path"]
        .as_str()
        .unwrap()
        .ends_with("lib.rs")));

    // --- ctx_discover + graph_query ---
    let discovered = call("ctx_discover", json!({ "path": "." })).await;
    assert!(discovered["nodes"].as_u64().unwrap() >= 3);
    let queried = call("graph_query", json!({ "name": "helper" })).await;
    let found_nodes = queried["nodes"].as_array().unwrap();
    assert!(found_nodes.iter().any(|n| n["name"] == json!("helper")));

    // graph_path between two connected symbols
    let pathed = call("graph_path", json!({ "from": "main", "to": "helper" })).await;
    assert_eq!(pathed["found"], json!(true));

    // --- ctx_stats: non-zero savings after the large sandbox run ---
    let stats = call("ctx_stats", json!({})).await;
    assert!(stats["sandbox_calls"].as_i64().unwrap() >= 1);
    assert!(stats["estimated_tokens_saved"].as_i64().unwrap() > 0);
    assert!(stats["graph_nodes"].as_i64().unwrap() >= 3);

    client.cancel().await.ok();
}

/// Lazy auto-build: on a fresh repo with no prior ctx_index / ctx_discover, the
/// first ctx_search and graph_query build the index/graph themselves and return
/// results — so ctxforge works on any repo without an explicit init step.
#[tokio::test]
async fn lazy_autobuild_on_first_query() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    std::fs::write(
        repo.path().join("lib.rs"),
        "fn helper() -> i32 { 1 }\nfn main() { let _ = helper(); }\n",
    )
    .unwrap();

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
            let res = client.call_tool(params).await.unwrap();
            res.structured_content
                .expect("structured content for tool result")
        }
    };

    // No ctx_index first: ctx_search must auto-index, then find the symbol.
    let searched = call("ctx_search", json!({ "queries": ["helper"] })).await;
    let hits = &searched["results"][0]["hits"];
    assert!(
        hits.as_array()
            .unwrap()
            .iter()
            .any(|h| h["path"].as_str().unwrap().ends_with("lib.rs")),
        "ctx_search should auto-index and find helper in lib.rs"
    );

    // No ctx_discover first: graph_query must auto-build the graph, then find it.
    let queried = call("graph_query", json!({ "name": "helper" })).await;
    assert!(
        queried["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["name"] == json!("helper")),
        "graph_query should auto-build the graph and find helper"
    );

    // The graph was persisted and the stats reflect the auto-build.
    assert!(
        data.path().join("graph.json").exists(),
        "graph.json should be persisted by the lazy build"
    );
    let stats = call("ctx_stats", json!({})).await;
    assert!(stats["graph_nodes"].as_i64().unwrap() >= 3);
    assert!(stats["index_chunks"].as_i64().unwrap() >= 1);

    client.cancel().await.ok();
}

/// `ctx_execute_file` must credit the analyzed file's bytes as savings — they
/// never entered context — even when the script prints a small, un-offloaded result.
#[tokio::test]
async fn ctx_execute_file_credits_the_file_bytes() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    // A 40 KB file; analyzing it via the sandbox must not cost ~40 KB of context.
    std::fs::write(repo.path().join("big.log"), "x".repeat(40_000)).unwrap();

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

    // Analyze the 40 KB file but print only a tiny summary (no offload).
    let res = call(
        "ctx_execute_file",
        json!({
            "path": "big.log",
            "language": "python",
            "code": "import sys; print(len(open(sys.argv[1]).read()))"
        }),
    )
    .await;
    assert_eq!(res["truncated"], json!(false), "small output, nothing offloaded");

    // Savings must reflect the ~40 KB file that stayed out of context (≈10k tokens),
    // not the handful of bytes actually printed.
    let stats = call("ctx_stats", json!({})).await;
    let saved = stats["estimated_tokens_saved"].as_i64().unwrap();
    assert!(saved >= 9000, "file bytes credited as savings; got {saved} tokens");

    client.cancel().await.ok();
}
