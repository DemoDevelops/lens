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
    // Conditional for opencode (T12): the meta is only inserted when host==claude.
    if std::env::var("LENS_HOST").unwrap_or_default() != "opencode" {
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

/// T3: `lens_graph` with `transitive: true` returns the COMPLETE directed
/// closure instead of a one-hop-at-a-time neighborhood: every reached node
/// carries a real witness call-site, and the response asserts `complete: true`.
#[tokio::test]
async fn lens_graph_transitive_returns_closure_with_witnesses() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    // top -> mid -> target: a two-hop directed caller chain.
    std::fs::write(
        repo.path().join("lib.rs"),
        "fn target() {}\nfn mid() { target(); }\nfn top() { mid(); }\n",
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

    let closure = call(
        "lens_graph",
        json!({ "node": "target", "direction": "callers", "transitive": true, "depth": 2 }),
    )
    .await;

    assert_eq!(closure["complete"], json!(true), "closure must claim completeness");
    assert_eq!(closure["count_total"], json!(2), "mid and top both reach target");
    let nodes = closure["nodes"].as_array().unwrap();
    for name in ["mid", "top"] {
        let node = nodes
            .iter()
            .find(|n| n["name"] == json!(name))
            .unwrap_or_else(|| panic!("{name} missing from closure"));
        assert!(
            node["witness"].as_str().is_some(),
            "{name} must carry a witness call-site"
        );
    }

    client.cancel().await.ok();
}

/// T3: `lens q callers <name> --transitive --depth N --prod-only` runs as a
/// bare subprocess (`lens q` is the read-only CLI family, no MCP handshake)
/// and exits 0 with the closure JSON, `count_total`/`count_prod` first.
#[test]
fn qcli_callers_transitive_exits_zero_with_count_prod() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::write(
        repo.path().join("lib.rs"),
        "fn target() {}\nfn mid() { target(); }\nfn top() { mid(); }\n",
    )
    .unwrap();

    let bin = env!("CARGO_BIN_EXE_lens");
    let warm = std::process::Command::new(bin)
        .current_dir(repo.path())
        .env("LENS_DIR", data.path())
        .arg("warmup")
        .output()
        .expect("spawn lens warmup");
    assert!(warm.status.success(), "warmup failed: {warm:?}");

    let out = std::process::Command::new(bin)
        .current_dir(repo.path())
        .env("LENS_DIR", data.path())
        .args(["q", "callers", "target", "--transitive", "--depth", "2", "--prod-only"])
        .output()
        .expect("spawn lens q callers --transitive");
    assert!(
        out.status.success(),
        "lens q callers --transitive exited nonzero: {out:?}"
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let v: Value = serde_json::from_str(stdout.trim()).expect("single-line JSON on stdout");
    assert!(
        v.get("count_prod").is_some(),
        "closure output must carry count_prod: {stdout}"
    );
    assert_eq!(v["complete"], json!(true));
}

/// T3: `lens.callers(..., transitive=True)` works end-to-end from a Python
/// darkroom script through the real `lens.py` prelude (subprocess to `lens q`),
/// not just through the qcli/MCP layers directly.
#[tokio::test]
async fn darkroom_python_callers_transitive_via_prelude() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::write(
        repo.path().join("lib.rs"),
        "fn target() {}\nfn mid() { target(); }\nfn top() { mid(); }\n",
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

    // Force the graph to build + persist before the script shells out to
    // `lens q` (which is read-only and never triggers a build itself).
    let _ = call("lens_symbol", json!({ "name": "target" })).await;

    let exec = call(
        "lens_run",
        json!({
            "language": "python",
            "code": "import lens, json\n\
                     result = lens.callers('target', transitive=True, depth=2)\n\
                     print(json.dumps({'complete': result['complete'], 'count_total': result['count_total']}))"
        }),
    )
    .await;

    let stdout = exec["stdout"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(stdout.trim()).expect("script prints valid JSON");
    assert_eq!(parsed["complete"], json!(true));
    assert_eq!(parsed["count_total"], json!(2));

    client.cancel().await.ok();
}

/// T6: a nested git repo with no pre-built `.lens` is auto-built on the first
/// federated `lens_search` instead of silently skipped: the response's new
/// `notes` field records the build, and re-querying afterward proves the built
/// index is real (the nested repo's own content is actually searchable), not
/// just a note with no effect.
#[tokio::test]
async fn nested_repo_auto_builds_on_federation_miss() {
    let parent = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::write(parent.path().join("top.rs"), "fn top_widget() {}\n").unwrap();

    // A nested git repo (its own `.git`), no `.lens` built yet.
    let nested = parent.path().join("nested");
    std::fs::create_dir_all(nested.join(".git")).unwrap();
    std::fs::write(
        nested.join("inner.rs"),
        "fn nested_federation_marker() {}\n",
    )
    .unwrap();

    let bin = env!("CARGO_BIN_EXE_lens");
    let repo_path = parent.path().to_path_buf();
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

    // First federated search: the nested repo has no `.lens/fts` yet, so it must
    // auto-build (index only) instead of silently skipping, and the response
    // must note the build.
    let first = call(
        "lens_search",
        json!({ "queries": ["nested_federation_marker"] }),
    )
    .await;
    let notes: Vec<String> = first["notes"]
        .as_array()
        .expect("notes field present")
        .iter()
        .map(|n| n.as_str().unwrap().to_string())
        .collect();
    assert!(
        notes.iter().any(|n| n.contains("built")),
        "first federated search must note the nested autobuild, got {notes:?}"
    );

    // Second query: the built index must be REAL, not just a note -- the
    // nested repo's own content must actually be searchable.
    let second = call(
        "lens_search",
        json!({ "queries": ["nested_federation_marker"] }),
    )
    .await;
    let hits = &second["results"][0]["hits"];
    assert!(
        hits.as_array()
            .unwrap()
            .iter()
            .any(|h| h["path"].as_str().unwrap().ends_with("nested/inner.rs")),
        "second query must return hits from the auto-built nested repo, got {hits:?}"
    );

    client.cancel().await.ok();
}

/// T6: `LENS_NESTED_AUTOBUILD=0` disables the auto-build, keeping the old
/// silent-skip behavior except the response still notes WHY nothing was built.
#[tokio::test]
async fn nested_autobuild_off_skips_with_note() {
    let parent = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::write(parent.path().join("top.rs"), "fn top_widget() {}\n").unwrap();

    let nested = parent.path().join("nested");
    std::fs::create_dir_all(nested.join(".git")).unwrap();
    std::fs::write(
        nested.join("inner.rs"),
        "fn nested_federation_marker() {}\n",
    )
    .unwrap();

    let bin = env!("CARGO_BIN_EXE_lens");
    let repo_path = parent.path().to_path_buf();
    let data_path = data.path().to_path_buf();
    let transport = TokioChildProcess::new(Command::new(bin).configure(|cmd| {
        cmd.current_dir(&repo_path)
            .env("LENS_DIR", &data_path)
            .env("LENS_MAX_INLINE", "8192")
            .env("LENS_NESTED_AUTOBUILD", "0");
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

    let out = call(
        "lens_search",
        json!({ "queries": ["nested_federation_marker"] }),
    )
    .await;
    let notes: Vec<String> = out["notes"]
        .as_array()
        .expect("notes field present")
        .iter()
        .map(|n| n.as_str().unwrap().to_string())
        .collect();
    assert!(
        notes.iter().any(|n| n.contains("skipped: autobuild off")),
        "LENS_NESTED_AUTOBUILD=0 must note the skip, got {notes:?}"
    );
    let hits = &out["results"][0]["hits"];
    assert!(
        !hits
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["path"].as_str().unwrap().ends_with("nested/inner.rs")),
        "with autobuild off, the nested repo's content must not surface, got {hits:?}"
    );

    client.cancel().await.ok();
}

/// T6: a nested repo over the file-count cap is skipped (with a note) instead
/// of auto-built, even with the kill-switch on.
#[tokio::test]
async fn nested_autobuild_max_files_skips_oversized_nested_repo() {
    let parent = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::write(parent.path().join("top.rs"), "fn top_widget() {}\n").unwrap();

    let nested = parent.path().join("nested");
    std::fs::create_dir_all(nested.join(".git")).unwrap();
    std::fs::write(nested.join("a.rs"), "fn a_widget() {}\n").unwrap();
    std::fs::write(nested.join("b.rs"), "fn b_widget() {}\n").unwrap();

    let bin = env!("CARGO_BIN_EXE_lens");
    let repo_path = parent.path().to_path_buf();
    let data_path = data.path().to_path_buf();
    let transport = TokioChildProcess::new(Command::new(bin).configure(|cmd| {
        cmd.current_dir(&repo_path)
            .env("LENS_DIR", &data_path)
            .env("LENS_MAX_INLINE", "8192")
            .env("LENS_NESTED_AUTOBUILD_MAX_FILES", "1");
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

    let out = call("lens_search", json!({ "queries": ["a_widget"] })).await;
    let notes: Vec<String> = out["notes"]
        .as_array()
        .expect("notes field present")
        .iter()
        .map(|n| n.as_str().unwrap().to_string())
        .collect();
    assert!(
        notes.iter().any(|n| n.contains("skipped: too large")),
        "over the file-count cap must skip with a note, got {notes:?}"
    );

    client.cancel().await.ok();
}

/// Under the opencode host the Anthropic-specific `anthropic/alwaysLoad` meta
/// must NOT be stamped (Ajv-based hosts warn on unknown meta/formats).
#[tokio::test]
async fn opencode_host_omits_always_load_meta() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("lib.rs"), "fn helper() {}\n").unwrap();

    let bin = env!("CARGO_BIN_EXE_lens");
    let repo_path = repo.path().to_path_buf();
    let data_path = data.path().to_path_buf();
    let transport = TokioChildProcess::new(Command::new(bin).configure(|cmd| {
        cmd.current_dir(&repo_path)
            .env("LENS_HOST", "opencode")
            .env("LENS_DIR", &data_path);
    }))
    .unwrap();
    let client = ().serve(transport).await.expect("handshake");

    let tools = client.list_tools(Default::default()).await.unwrap();
    assert!(!tools.tools.is_empty());
    for t in &tools.tools {
        let stamped = t
            .meta
            .as_ref()
            .is_some_and(|m| m.0.contains_key("anthropic/alwaysLoad"));
        assert!(
            !stamped,
            "{} must not carry anthropic/alwaysLoad under opencode",
            t.name
        );
    }

    client.cancel().await.ok();
}
