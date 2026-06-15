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
