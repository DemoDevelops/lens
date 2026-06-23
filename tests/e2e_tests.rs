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

    let bin = env!("CARGO_BIN_EXE_lens");
    let repo_path = repo.path().to_path_buf();
    let data_path = data.path().to_path_buf();

    let transport = TokioChildProcess::new(Command::new(bin).configure(|cmd| {
        cmd.current_dir(&repo_path)
            .env("LENS_DIR", &data_path)
            .env("LENS_MAX_INLINE", "8192");
    }))
    .unwrap();

    // serve() performs the MCP initialize handshake.
    let client = ().serve(transport).await.expect("handshake");

    // Tools are advertised.
    let tools = client.list_tools(Default::default()).await.unwrap();
    let names: Vec<String> = tools.tools.iter().map(|t| t.name.to_string()).collect();
    for expected in [
        "lens_run",
        "lens_index",
        "lens_search",
        "lens_map",
        "lens_symbol",
        "lens_links",
        "lens_path",
        "lens_recall",
        "lens_stats",
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

    // --- lens_run: large output offloaded; raw input never returned ---
    let exec = call(
        "lens_run",
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

    // --- lens_recall: recover the full offloaded output ---
    let retrieved = call("lens_recall", json!({ "ref": exec_ref })).await;
    assert!(retrieved["content"]
        .as_str()
        .unwrap()
        .contains(&"A".repeat(50000)));

    // --- lens_index + lens_search ---
    let indexed = call("lens_index", json!({ "path": "." })).await;
    assert!(indexed["files_indexed"].as_u64().unwrap() >= 1);
    let searched = call("lens_search", json!({ "queries": ["helper"] })).await;
    let hits = &searched["results"][0]["hits"];
    assert!(hits
        .as_array()
        .unwrap()
        .iter()
        .any(|h| h["path"].as_str().unwrap().ends_with("lib.rs")));

    // --- lens_map + lens_symbol ---
    let discovered = call("lens_map", json!({ "path": "." })).await;
    assert!(discovered["nodes"].as_u64().unwrap() >= 3);
    let queried = call("lens_symbol", json!({ "name": "helper" })).await;
    let found_nodes = queried["nodes"].as_array().unwrap();
    assert!(found_nodes.iter().any(|n| n["name"] == json!("helper")));

    // lens_path between two connected symbols
    let pathed = call("lens_path", json!({ "from": "main", "to": "helper" })).await;
    assert_eq!(pathed["found"], json!(true));

    // --- lens_stats: non-zero savings after the large darkroom run ---
    let stats = call("lens_stats", json!({})).await;
    assert!(stats["darkroom_calls"].as_i64().unwrap() >= 1);
    assert!(stats["estimated_tokens_saved"].as_i64().unwrap() > 0);
    assert!(stats["graph_nodes"].as_i64().unwrap() >= 3);

    client.cancel().await.ok();
}

/// Lazy auto-build: on a fresh repo with no prior lens_index / lens_map, the
/// first lens_search and lens_symbol build the index/graph themselves and return
/// results — so lens works on any repo without an explicit init step.
#[tokio::test]
async fn lazy_autobuild_on_first_query() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    std::fs::write(
        repo.path().join("lib.rs"),
        "fn helper() -> i32 { 1 }\nfn main() { let _ = helper(); }\n",
    )
    .unwrap();

    let bin = env!("CARGO_BIN_EXE_lens");
    let repo_path = repo.path().to_path_buf();
    let data_path = data.path().to_path_buf();

    let transport = TokioChildProcess::new(Command::new(bin).configure(|cmd| {
        cmd.current_dir(&repo_path)
            .env("LENS_DIR", &data_path)
            .env("LENS_MAX_INLINE", "8192");
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

    // No lens_index first: lens_search must auto-index, then find the symbol.
    let searched = call("lens_search", json!({ "queries": ["helper"] })).await;
    let hits = &searched["results"][0]["hits"];
    assert!(
        hits.as_array()
            .unwrap()
            .iter()
            .any(|h| h["path"].as_str().unwrap().ends_with("lib.rs")),
        "lens_search should auto-index and find helper in lib.rs"
    );

    // No lens_map first: lens_symbol must auto-build the graph, then find it.
    let queried = call("lens_symbol", json!({ "name": "helper" })).await;
    assert!(
        queried["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["name"] == json!("helper")),
        "lens_symbol should auto-build the graph and find helper"
    );

    // The graph was persisted and the stats reflect the auto-build.
    assert!(
        data.path().join("graph.json").exists(),
        "graph.json should be persisted by the lazy build"
    );
    let stats = call("lens_stats", json!({})).await;
    assert!(stats["graph_nodes"].as_i64().unwrap() >= 3);
    assert!(stats["index_chunks"].as_i64().unwrap() >= 1);

    client.cancel().await.ok();
}

/// `lens_run_file` must credit the analyzed file's bytes as savings — they
/// never entered context — even when the script prints a small, un-offloaded result.
#[tokio::test]
async fn lens_run_file_credits_the_file_bytes() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    // A 40 KB file; analyzing it via the darkroom must not cost ~40 KB of context.
    std::fs::write(repo.path().join("big.log"), "x".repeat(40_000)).unwrap();

    let bin = env!("CARGO_BIN_EXE_lens");
    let repo_path = repo.path().to_path_buf();
    let data_path = data.path().to_path_buf();
    let transport = TokioChildProcess::new(Command::new(bin).configure(|cmd| {
        cmd.current_dir(&repo_path)
            .env("LENS_DIR", &data_path)
            .env("LENS_MAX_INLINE", "8192");
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
        "lens_run_file",
        json!({
            "path": "big.log",
            "language": "python",
            "code": "import sys; print(len(open(sys.argv[1]).read()))"
        }),
    )
    .await;
    assert_eq!(
        res["truncated"],
        json!(false),
        "small output, nothing offloaded"
    );

    // Savings must reflect the ~40 KB file that stayed out of context (≈10k tokens),
    // not the handful of bytes actually printed.
    let stats = call("lens_stats", json!({})).await;
    let saved = stats["estimated_tokens_saved"].as_i64().unwrap();
    assert!(
        saved >= 9000,
        "file bytes credited as savings; got {saved} tokens"
    );

    client.cancel().await.ok();
}
