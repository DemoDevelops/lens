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

    // Exactly the 10-tool 0.10.0 surface is advertised — nothing missing, no
    // removed tool lingering.
    let tools = client.list_tools(Default::default()).await.unwrap();
    let mut names: Vec<String> = tools.tools.iter().map(|t| t.name.to_string()).collect();
    names.sort();
    let mut expected: Vec<String> = [
        "lens_run",
        "lens_search",
        "lens_symbol",
        "lens_graph",
        "lens_skeleton",
        "lens_overview",
        "lens_recall",
        "lens_grep_ast",
        "lens_memory_query",
        "lens_memory_record",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    expected.sort();
    assert_eq!(names, expected, "advertised tools must be exactly the 10");

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

    // --- lens_search (the index auto-builds; no explicit index tool) ---
    let searched = call("lens_search", json!({ "queries": ["helper"] })).await;
    let hits = &searched["results"][0]["hits"];
    assert!(hits
        .as_array()
        .unwrap()
        .iter()
        .any(|h| h["path"].as_str().unwrap().ends_with("lib.rs")));

    // --- lens_symbol (the graph auto-builds; no explicit map tool) ---
    let queried = call("lens_symbol", json!({ "name": "helper" })).await;
    let found_nodes = queried["nodes"].as_array().unwrap();
    assert!(found_nodes.iter().any(|n| n["name"] == json!("helper")));
    assert_eq!(queried["matched_via"], json!("name"));

    // lens_graph with `to`: the shortest path between two connected symbols
    // (the old lens_path shape, unwrapped).
    let pathed = call("lens_graph", json!({ "node": "main", "to": "helper" })).await;
    assert_eq!(pathed["found"], json!(true));

    // lens_graph without `to`: the neighborhood (the old lens_links shape),
    // carrying no lens_symbol-only `matched_via` field.
    let hood = call("lens_graph", json!({ "node": "helper" })).await;
    assert!(!hood["nodes"].as_array().unwrap().is_empty());
    assert!(hood.get("matched_via").is_none());

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

    // The graph was persisted by the lazy build.
    assert!(
        data.path().join("graph.json").exists(),
        "graph.json should be persisted by the lazy build"
    );

    client.cancel().await.ok();
}

/// `lens_run` with `path` (the folded `lens_run_file`) must credit the analyzed
/// file's bytes as savings — they never entered context — even when the script
/// prints a small, un-offloaded result. With the stats tool folded out of the MCP
/// surface, the proof reads the op ledger in the data dir directly.
#[tokio::test]
async fn lens_run_with_path_credits_the_file_bytes() {
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
        "lens_run",
        json!({
            "path": "big.log",
            "language": "python",
            "code": "import sys; print(len(open(sys.argv[1]).read()))"
        }),
    )
    .await;
    assert_eq!(res["stdout"].as_str().unwrap().trim(), "40000");
    assert_eq!(
        res["truncated"],
        json!(false),
        "small output, nothing offloaded"
    );

    client.cancel().await.ok();

    // The op record must credit the ~40 KB file as raw input that stayed out of
    // context, not just the handful of bytes actually printed.
    let ops = std::fs::read_to_string(data.path().join("ops.log")).expect("ops.log written");
    let rec: Value = ops
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|r| r["tool"] == json!("lens_run"))
        .expect("a lens_run op record");
    assert!(
        rec["raw_bytes_in"].as_u64().unwrap() >= 40_000,
        "file bytes must be credited as raw input: {rec}"
    );

    // And the persistent savings counter (what the old lens_stats read) must
    // carry the full uncapped file size — ≈10k tokens' worth of bytes.
    let store = lens::store::Store::open(data.path()).expect("open store");
    let raw = store.get_stat("raw_bytes_processed").unwrap_or(0);
    assert!(
        raw >= 40_000,
        "file bytes credited to the persistent savings counter; got {raw}"
    );
}

/// T3 (Bug B, permission stall): read-only tools must declare `readOnlyHint=true` in
/// list_tools so Claude Code can auto-approve them and an unattended agent never stalls
/// on a permission prompt. Tools with side effects (run code, write the index/graph)
/// must NOT be marked read-only.
#[tokio::test]
async fn read_only_tools_declare_annotation() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("lib.rs"), "fn helper() {}\n").unwrap();

    let bin = env!("CARGO_BIN_EXE_lens");
    let repo_path = repo.path().to_path_buf();
    let data_path = data.path().to_path_buf();
    let transport = TokioChildProcess::new(Command::new(bin).configure(|cmd| {
        cmd.current_dir(&repo_path).env("LENS_DIR", &data_path);
    }))
    .unwrap();
    let client = ().serve(transport).await.expect("handshake");

    let tools = client.list_tools(Default::default()).await.unwrap();

    // The read-only tools (no code execution, no durable writes).
    const READ_ONLY: [&str; 8] = [
        "lens_search",
        "lens_overview",
        "lens_recall",
        "lens_symbol",
        "lens_graph",
        "lens_skeleton",
        "lens_grep_ast",
        "lens_memory_query",
    ];
    for t in &tools.tools {
        let read_only = t.annotations.as_ref().and_then(|a| a.read_only_hint);
        if READ_ONLY.contains(&t.name.as_ref()) {
            assert_eq!(
                read_only,
                Some(true),
                "{} must declare readOnlyHint=true",
                t.name
            );
        } else {
            assert_ne!(
                read_only,
                Some(true),
                "{} has side effects and must not be marked read-only",
                t.name
            );
        }
    }

    client.cancel().await.ok();
}
