//! Integration tests for the LENS_ROUTING layer (plan §4).
//!
//! These drive the REAL compiled binary the way Claude Code does:
//!   * `lens hook claude PreToolUse/SessionStart` over stdin → assert the
//!     exact hook JSON per routing level (and that `=off` is a byte-identical
//!     no-op — the safety contract);
//!   * `lens wrap` → offload large stdout, then `verify --roundtrip` (PASS)
//!     and `stats` (the op surfaces with real savings);
//!   * `lens_run` with `path` end-to-end through the rmcp client.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_lens")
}

/// Seed a populated `index.db` in `data_dir` so `routing::index_present` returns
/// true. The armed-Grep deny gate (grep-first and grep-scope) only fires against
/// a populated index — the Tantivy-era `file_manifest` manifest table — so a test
/// that drives the real binary and asserts an armed deny must first make its
/// tempdir look like a real indexed repo (the in-crate `seed_index` fixture is
/// `#[cfg(test)]` and unreachable from this integration crate).
fn seed_index(data_dir: &Path) {
    let conn = rusqlite::Connection::open(data_dir.join("index.db")).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS file_manifest(path TEXT PRIMARY KEY, mtime INTEGER NOT NULL);
         INSERT OR REPLACE INTO file_manifest(path, mtime) VALUES ('src/f0.rs', 123);",
    )
    .unwrap();
}

/// Run `lens hook claude <event>` with `payload` on stdin under a clean,
/// explicit routing env. Returns (trimmed stdout, parsed JSON or Null).
fn run_hook(
    event: &str,
    payload: &Value,
    envs: &[(&str, &str)],
    data_dir: &Path,
) -> (String, Value) {
    let mut cmd = Command::new(bin());
    cmd.args(["hook", "claude", event])
        .env("LENS_DIR", data_dir)
        // Determinism: never inherit routing env from the test runner.
        .env_remove("LENS_ROUTING")
        .env_remove("LENS_ROUTING_MCP")
        // ...including the reroute-rail deny flags and their tunables.
        // Kill-switch polarity means every rail flag is ON by default and `=0`
        // disables it, so an unset flag here is LIVE, not off — the rail tests
        // below pin whichever flags each call needs explicitly (see `rail_envs`).
        .env_remove("LENS_GREP_SCOPE_DENY")
        .env_remove("LENS_GREP_SYMBOL_DENY")
        .env_remove("LENS_READ_SKELETON_DENY")
        .env_remove("LENS_BASH_AGG_DENY")
        .env_remove("LENS_EDIT_LINKS_DENY")
        .env_remove("LENS_GREP_AST_DENY")
        .env_remove("LENS_READ_OVERVIEW_DENY")
        .env_remove("LENS_BASH_GREP_DENY")
        .env_remove("LENS_READ_RUNFILE_DENY")
        .env_remove("LENS_EDIT_LINKS_MIN_CALLERS")
        .env_remove("LENS_READ_OVERVIEW_THRESHOLD")
        // RTK coexistence (plan T4): force the defer-Bash-to-RTK gate OFF so these
        // Bash-wrap assertions are deterministic regardless of whether RTK happens
        // to be installed + hooked on the host machine.
        .env("LENS_DEFER_BASH_TO_RTK", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn hook");
    {
        let mut si = child.stdin.take().unwrap();
        si.write_all(payload.to_string().as_bytes()).unwrap();
    }
    let out = child.wait_with_output().expect("hook output");
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let parsed = serde_json::from_str(&raw).unwrap_or(Value::Null);
    (raw, parsed)
}

fn bash_payload(dir: &Path, session: &str, command: &str) -> Value {
    json!({
        "session_id": session,
        "cwd": dir.to_string_lossy(),
        "tool_name": "Bash",
        "tool_input": { "command": command },
    })
}

fn webfetch_payload(dir: &Path, session: &str) -> Value {
    json!({
        "session_id": session,
        "cwd": dir.to_string_lossy(),
        "tool_name": "WebFetch",
        "tool_input": { "url": "https://example.com/big" },
    })
}

// ---------------------------------------------------------------------------
// §4: LENS_ROUTING=off ⇒ PreToolUse output identical to today (empty)
// ---------------------------------------------------------------------------

#[test]
fn off_is_a_true_noop() {
    let d = tempfile::tempdir().unwrap();
    // Explicit `off` must be byte-identical "{}" (the safety contract). The unset
    // default is now `full`, exercised by the steering tests below.
    let envs = [("LENS_ROUTING", "off")];
    let (raw_wf, _) = run_hook(
        "PreToolUse",
        &webfetch_payload(d.path(), "s1"),
        &envs,
        d.path(),
    );
    assert_eq!(
        raw_wf, "{}",
        "off must be a byte-identical no-op for WebFetch"
    );
    let (raw_b, _) = run_hook(
        "PreToolUse",
        &bash_payload(d.path(), "s1", "find . -type f"),
        &envs,
        d.path(),
    );
    assert_eq!(raw_b, "{}", "off must be a byte-identical no-op for Bash");
}

// ---------------------------------------------------------------------------
// §2/§4: WebFetch → deny + steer (only when steering)
// ---------------------------------------------------------------------------

#[test]
fn webfetch_denies_when_steering() {
    let d = tempfile::tempdir().unwrap();
    // Distinct sessions per level: the WebFetch deny is one-shot per session.
    for (lvl, sess) in [("steer", "wf-steer"), ("full", "wf-full")] {
        let (_, v) = run_hook(
            "PreToolUse",
            &webfetch_payload(d.path(), sess),
            &[("LENS_ROUTING", lvl), ("LENS_ROUTING_MCP", "up")],
            d.path(),
        );
        let hso = &v["hookSpecificOutput"];
        assert_eq!(hso["hookEventName"], "PreToolUse");
        assert_eq!(
            hso["permissionDecision"], "deny",
            "WebFetch must deny at {lvl}"
        );
        assert!(
            hso["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .contains("lens_run"),
            "deny reason steers to the darkroom"
        );
    }
}

#[test]
fn webfetch_passes_through_at_wrap_only_level() {
    // `wrap` steers nothing — WebFetch should pass through untouched.
    let d = tempfile::tempdir().unwrap();
    let (raw, _) = run_hook(
        "PreToolUse",
        &webfetch_payload(d.path(), "s1"),
        &[("LENS_ROUTING", "wrap"), ("LENS_ROUTING_MCP", "up")],
        d.path(),
    );
    assert_eq!(raw, "{}");
}

// ---------------------------------------------------------------------------
// §2/§4: wrappable Bash → transparent wrap; stateful chains pass through
// ---------------------------------------------------------------------------

#[test]
fn wrappable_bash_rewrites_to_lens_wrap_at_full() {
    let d = tempfile::tempdir().unwrap();
    let (_, v) = run_hook(
        "PreToolUse",
        &bash_payload(d.path(), "s1", "find . -type f"),
        &[("LENS_ROUTING", "full"), ("LENS_ROUTING_MCP", "up")],
        d.path(),
    );
    let hso = &v["hookSpecificOutput"];
    assert_eq!(hso["permissionDecision"], "allow");
    let cmd = hso["updatedInput"]["command"].as_str().unwrap();
    assert!(
        cmd.contains("wrap -- "),
        "rewrite must invoke `wrap --`: {cmd}"
    );
    assert!(
        cmd.contains("find . -type f"),
        "original command preserved: {cmd}"
    );
    assert!(cmd.contains("lens"), "invokes the lens binary: {cmd}");
}

#[test]
fn git_subcommand_awareness() {
    let d = tempfile::tempdir().unwrap();
    let envs = [("LENS_ROUTING", "full"), ("LENS_ROUTING_MCP", "up")];
    // read-only subcommand → wrapped
    let (_, log) = run_hook(
        "PreToolUse",
        &bash_payload(d.path(), "s1", "git log --oneline -20"),
        &envs,
        d.path(),
    );
    assert_eq!(log["hookSpecificOutput"]["permissionDecision"], "allow");
    // mutating subcommand → passthrough
    let (raw, _) = run_hook(
        "PreToolUse",
        &bash_payload(d.path(), "s2", "git commit -m wip"),
        &envs,
        d.path(),
    );
    assert_eq!(raw, "{}", "git commit must never be wrapped");
}

#[test]
fn stateful_chain_passes_through_unchanged() {
    let d = tempfile::tempdir().unwrap();
    // `cd x && <wrappable>` must NOT be wrapped (would break persistent shell cwd).
    let (raw, _) = run_hook(
        "PreToolUse",
        &bash_payload(d.path(), "s1", "cd src && find . -type f"),
        &[("LENS_ROUTING", "full"), ("LENS_ROUTING_MCP", "up")],
        d.path(),
    );
    assert_eq!(raw, "{}", "cd-chain must pass through unchanged");
}

// ---------------------------------------------------------------------------
// §2/§4: nudge retirement — steer without wrap emits nothing for plain Bash
// ---------------------------------------------------------------------------

#[test]
fn wrappable_bash_is_a_pure_noop_at_steer() {
    // The per-call Bash nudge is retired: at steer (no wrap) a wrappable
    // command renders byte-identical `{}` every time.
    let d = tempfile::tempdir().unwrap();
    let envs = [("LENS_ROUTING", "steer"), ("LENS_ROUTING_MCP", "up")];
    let p = bash_payload(d.path(), "s1", "find . -type f");
    let (raw1, _) = run_hook("PreToolUse", &p, &envs, d.path());
    assert_eq!(raw1, "{}", "no nudge on the first wrappable Bash at steer");
    let (raw2, _) = run_hook("PreToolUse", &p, &envs, d.path());
    assert_eq!(raw2, "{}", "and none afterwards either");
}

// ---------------------------------------------------------------------------
// §2/§4: MCP-ready guard (port of context-mode mcpRedirect #230) — server down
// ⇒ only MCP-redirect decisions passthrough; wrap/nudges still fire.
// ---------------------------------------------------------------------------

#[test]
fn mcp_down_gates_redirects_not_wrap() {
    let d = tempfile::tempdir().unwrap();
    let envs = [("LENS_ROUTING", "full"), ("LENS_ROUTING_MCP", "down")];
    // WebFetch deny is an MCP redirect → suppressed to passthrough when down.
    let (raw_wf, _) = run_hook(
        "PreToolUse",
        &webfetch_payload(d.path(), "s1"),
        &envs,
        d.path(),
    );
    assert_eq!(raw_wf, "{}", "server down → WebFetch deny suppressed");
    // curl→lens_run is an MCP redirect → suppressed to passthrough when down.
    let (raw_curl, _) = run_hook(
        "PreToolUse",
        &bash_payload(d.path(), "s1", "curl https://api.example.com/data"),
        &envs,
        d.path(),
    );
    assert_eq!(raw_curl, "{}", "server down → curl redirect suppressed");
    // Wrap shells the lens CLI (not the MCP server) → still fires when down.
    let (_, v_b) = run_hook(
        "PreToolUse",
        &bash_payload(d.path(), "s2", "find . -type f"),
        &envs,
        d.path(),
    );
    assert_eq!(
        v_b["hookSpecificOutput"]["permissionDecision"], "allow",
        "server down → wrappable Bash still rewritten (wrap is CLI-backed, not MCP)"
    );
}

// ---------------------------------------------------------------------------
// §2/§4: SessionStart injects the routing block (only when steering)
// ---------------------------------------------------------------------------

#[test]
fn sessionstart_injects_routing_block_when_steering() {
    let d = tempfile::tempdir().unwrap();
    let payload =
        json!({ "session_id": "s1", "cwd": d.path().to_string_lossy(), "source": "startup" });
    let (_, v) = run_hook(
        "SessionStart",
        &payload,
        &[("LENS_ROUTING", "full"), ("LENS_ROUTING_MCP", "up")],
        d.path(),
    );
    let ctx = v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(
        ctx.contains("context_window_protection"),
        "routing block present: {ctx}"
    );
    for tool in [
        "lens_run",
        "lens_search",
        "lens_symbol",
        "lens_graph",
        "lens_recall",
        "lens_skeleton",
        "lens_overview",
        "lens_grep_ast",
        "lens_memory_query",
        "lens_memory_record",
    ] {
        assert!(ctx.contains(tool), "bootstrap select list names {tool}: {ctx}");
    }
    for removed in [
        "lens_run_file",
        "lens_index",
        "lens_map",
        "lens_find",
        "lens_links",
        "lens_path",
        "lens_stats",
    ] {
        assert!(
            !ctx.contains(removed),
            "bootstrap select list should not name removed tool {removed}: {ctx}"
        );
    }
    assert!(
        ctx.contains("include_bodies"),
        "block documents include_bodies"
    );
    assert!(
        ctx.contains("ToolSearch"),
        "deferred-tool bootstrap present"
    );
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");

    // With routing off, no block is injected.
    let (_, voff) = run_hook(
        "SessionStart",
        &payload,
        &[("LENS_ROUTING", "off")],
        d.path(),
    );
    let ctxoff = voff["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or("");
    assert!(
        !ctxoff.contains("context_window_protection"),
        "off injects no routing block"
    );
}

// ---------------------------------------------------------------------------
// §4: `lens wrap` — large stdout offloads losslessly; verify + stats
// ---------------------------------------------------------------------------

/// Pull the store ref out of a wrap preview footer (`... ref=<hex> ...`).
fn parse_ref(preview: &str) -> Option<String> {
    let i = preview.find("ref=")? + "ref=".len();
    let hex: String = preview[i..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    (!hex.is_empty()).then_some(hex)
}

#[test]
fn wrap_small_output_is_verbatim() {
    let d = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .args(["wrap", "--", "printf 'hello-world'"])
        .env("LENS_DIR", d.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "hello-world",
        "small output passes through byte-for-byte"
    );
}

#[test]
fn wrap_offloads_roundtrips_and_shows_on_stats() {
    let d = tempfile::tempdir().unwrap();
    // ~50 KB of read-only output via a portable generator.
    let gen = "head -c 50000 /dev/zero | tr '\\0' A";
    let out = Command::new(bin())
        .args(["wrap", "--", gen])
        .env("LENS_DIR", d.path())
        .output()
        .expect("wrap run");
    assert!(
        out.status.success(),
        "wrap exits 0 for a succeeding command"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.len() < 50000,
        "large output must be previewed, not inlined"
    );
    let reference = parse_ref(&stdout).unwrap_or_else(|| panic!("ref in preview: {stdout}"));

    // `verify --roundtrip` reproduces it byte-for-byte (PASS, exit 0).
    let v = Command::new(bin())
        .args(["verify", "--roundtrip", &reference])
        .env("LENS_DIR", d.path())
        .output()
        .expect("verify");
    assert!(v.status.success(), "roundtrip must exit 0 (PASS)");
    assert!(
        String::from_utf8_lossy(&v.stdout).contains("PASS"),
        "roundtrip prints PASS"
    );

    // The op is recorded with real savings, refs the stored blob.
    let ops = std::fs::read_to_string(d.path().join("ops.log")).unwrap();
    let rec: Value = ops
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|r| r["tool"] == "bash_wrap")
        .expect("a bash_wrap op was recorded");
    assert!(
        rec["tokens_saved_est"].as_i64().unwrap() > 0,
        "wrap shows real savings"
    );
    assert_eq!(rec["store_ref"].as_str().unwrap(), reference);

    // `lens stats` surfaces the wrapped op on the dashboard plane.
    let s = Command::new(bin())
        .args(["stats"])
        .env("LENS_DIR", d.path())
        .output()
        .expect("stats");
    let st = String::from_utf8_lossy(&s.stdout);
    assert!(st.contains("bash_wrap"), "stats lists the wrap op:\n{st}");

    // ...and the exact aggregate the web dashboard serves at /api/stats lists it.
    let snap = lens::obs::stats::snapshot_json(d.path(), None);
    let by_tool = snap["by_tool"].as_array().unwrap();
    assert!(
        by_tool.iter().any(|t| t["tool"] == "bash_wrap"),
        "dashboard /api/stats by_tool must include bash_wrap"
    );
}

// ---------------------------------------------------------------------------
// T3 §: lens_run with `path` (the folded lens_run_file) end-to-end via rmcp
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lens_run_with_path_e2e() {
    use rmcp::model::CallToolRequestParams;
    use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
    use rmcp::ServiceExt;
    use tokio::process::Command as TokioCommand;

    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    // A file whose CONTENTS must never enter context.
    let body = "S".repeat(40000);
    std::fs::write(repo.path().join("data.txt"), &body).unwrap();

    let repo_path = repo.path().to_path_buf();
    let data_path = data.path().to_path_buf();
    let transport = TokioChildProcess::new(TokioCommand::new(bin()).configure(|cmd| {
        cmd.current_dir(&repo_path)
            .env("LENS_DIR", &data_path)
            .env("LENS_MAX_INLINE", "8192");
    }))
    .unwrap();
    let client = ().serve(transport).await.expect("handshake");

    // The folded tool is advertised; the removed one is gone.
    let tools = client.list_tools(Default::default()).await.unwrap();
    assert!(
        tools.tools.iter().any(|t| t.name.as_ref() == "lens_run"),
        "lens_run must be advertised"
    );
    assert!(
        !tools
            .tools
            .iter()
            .any(|t| t.name.as_ref() == "lens_run_file"),
        "lens_run_file was folded into lens_run{{path}} and must not be advertised"
    );

    let call = |name: &'static str, args: Value| {
        let client = &client;
        async move {
            let mut p = CallToolRequestParams::new(name);
            p.arguments = args.as_object().cloned();
            client
                .call_tool(p)
                .await
                .unwrap()
                .structured_content
                .expect("structured content")
        }
    };

    // 1) The file path is injected as argv[1]; only the printed length returns,
    //    never the 40 KB of contents.
    let r = call(
        "lens_run",
        json!({
            "path": "data.txt",
            "language": "python",
            "code": "import sys; print(len(open(sys.argv[1]).read()))",
        }),
    )
    .await;
    assert_eq!(r["stdout"].as_str().unwrap().trim(), "40000");
    assert!(
        !r["stdout"].as_str().unwrap().contains("SSSS"),
        "file contents must not leak into context"
    );

    // 2) Large derived output is offloaded with a working retrieve_ref.
    let big = call(
        "lens_run",
        json!({
            "path": "data.txt",
            "language": "python",
            "code": "import sys; _ = open(sys.argv[1]).read(); print('A'*50000)",
        }),
    )
    .await;
    assert_eq!(big["truncated"], json!(true));
    let r2 = big["retrieve_ref"].as_str().unwrap().to_string();
    let recovered = call("lens_recall", json!({ "ref": r2 })).await;
    assert!(recovered["content"]
        .as_str()
        .unwrap()
        .contains(&"A".repeat(50000)));

    client.cancel().await.ok();
}

// ---------------------------------------------------------------------------
// T4: SessionStart pushes a repo-map digest from an existing graph.json (never
// builds one — load-or-skip keeps SessionStart fast).
// ---------------------------------------------------------------------------

/// A minimal but valid `Graph` (see `discovery::graph::Graph`) with enough
/// nodes/edges that `discovery::query::overview` renders a non-empty digest.
fn minimal_graph_json() -> Value {
    json!({
        "nodes": [
            {"id": "n1", "name": "alpha", "kind": "function", "file": "a.rs", "line": 1, "language": "rust"},
            {"id": "n2", "name": "beta", "kind": "function", "file": "a.rs", "line": 10, "language": "rust"},
        ],
        "edges": [
            {"from": "n1", "to": "n2", "kind": "calls"},
        ],
    })
}

/// Like `run_hook`, but also returns the process exit status (needed to assert
/// a clean 0 exit on the graph-absent path, which `run_hook` doesn't expose).
fn run_hook_with_status(
    event: &str,
    payload: &Value,
    envs: &[(&str, &str)],
    data_dir: &Path,
) -> (std::process::ExitStatus, Value) {
    let mut cmd = Command::new(bin());
    cmd.args(["hook", "claude", event])
        .env("LENS_DIR", data_dir)
        .env_remove("LENS_ROUTING")
        .env_remove("LENS_ROUTING_MCP")
        .env("LENS_DEFER_BASH_TO_RTK", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn hook");
    {
        let mut si = child.stdin.take().unwrap();
        si.write_all(payload.to_string().as_bytes()).unwrap();
    }
    let out = child.wait_with_output().expect("hook output");
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let parsed = serde_json::from_str(&raw).unwrap_or(Value::Null);
    (out.status, parsed)
}

#[test]
fn sessionstart_repo_map_digest_gated_on_graph_and_budget() {
    let d = tempfile::tempdir().unwrap();
    let payload =
        json!({ "session_id": "s1", "cwd": d.path().to_string_lossy(), "source": "startup" });

    // (a) a valid graph.json in the data dir -> digest injected, naming a
    // ranked symbol from it.
    std::fs::write(d.path().join("graph.json"), minimal_graph_json().to_string()).unwrap();
    let (_, v) = run_hook("SessionStart", &payload, &[], d.path());
    let ctx = v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or("");
    assert!(ctx.contains("<repo_map>"), "digest injected: {ctx}");
    assert!(
        ctx.contains("lens_symbol") && ctx.contains("lens_links") && ctx.contains("lens_path"),
        "digest points at graph-nav tools: {ctx}"
    );
    assert!(ctx.contains("alpha"), "digest names a ranked symbol: {ctx}");

    // (b) no graph.json at all -> no digest, and the hook still exits 0 cleanly.
    let d2 = tempfile::tempdir().unwrap();
    let payload2 =
        json!({ "session_id": "s2", "cwd": d2.path().to_string_lossy(), "source": "startup" });
    let (status2, v2) = run_hook_with_status("SessionStart", &payload2, &[], d2.path());
    assert!(status2.success(), "hook exits 0 with no graph.json present");
    assert_eq!(v2["hookSpecificOutput"]["hookEventName"], "SessionStart");
    let ctx2 = v2["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or("");
    assert!(!ctx2.contains("<repo_map>"), "no graph.json => no digest: {ctx2}");

    // (c) LENS_SESSION_OVERVIEW_BUDGET=0 disables the digest even with a valid
    // graph.json present.
    let (_, v3) = run_hook(
        "SessionStart",
        &payload,
        &[("LENS_SESSION_OVERVIEW_BUDGET", "0")],
        d.path(),
    );
    let ctx3 = v3["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or("");
    assert!(
        !ctx3.contains("<repo_map>"),
        "budget=0 disables the digest: {ctx3}"
    );
}

// ---------------------------------------------------------------------------
// T5: consecutive-read counter-deny (Serena `remind` pattern)
// ---------------------------------------------------------------------------

fn read_payload(dir: &Path, session: &str, file_path: &str) -> Value {
    json!({
        "session_id": session,
        "cwd": dir.to_string_lossy(),
        "tool_name": "Read",
        "tool_input": { "file_path": file_path },
    })
}

#[test]
fn read_denies_after_four_consecutive_code_reads_then_lens_call_resets() {
    let d = tempfile::tempdir().unwrap();
    let envs = [("LENS_ROUTING", "full"), ("LENS_ROUTING_MCP", "up")];
    let p = read_payload(d.path(), "s1", "src/server.rs");

    let mut fourth = Value::Null;
    for _ in 0..4 {
        let (_, v) = run_hook("PreToolUse", &p, &envs, d.path());
        fourth = v;
    }
    let hso = &fourth["hookSpecificOutput"];
    assert_eq!(
        hso["permissionDecision"], "deny",
        "4th consecutive code Read must deny: {fourth}"
    );
    let reason = hso["permissionDecisionReason"].as_str().unwrap();
    assert!(
        reason.contains("lens_skeleton"),
        "deny reason names lens_skeleton: {reason}"
    );

    // A lens tool call (PostToolUse) resets the counter, so the next Read passes.
    let lens_post = json!({
        "session_id": "s1",
        "cwd": d.path().to_string_lossy(),
        "tool_name": "mcp__lens__lens_skeleton",
        "tool_input": { "path": "src/server.rs" },
        "tool_response": "ok",
    });
    run_hook("PostToolUse", &lens_post, &envs, d.path());

    let (_, after) = run_hook("PreToolUse", &p, &envs, d.path());
    assert_ne!(
        after["hookSpecificOutput"]["permissionDecision"], "deny",
        "Read after a lens tool call must not deny: {after}"
    );
}

#[test]
fn find_trace_prompts_get_the_intent_nudge_at_prompt_submit() {
    let d = tempfile::tempdir().unwrap();
    let envs = [("LENS_ROUTING", "full")];
    let p = json!({
        "session_id": "s9",
        "cwd": d.path().to_string_lossy(),
        "prompt": "Where is the deny threshold defined and what calls it?",
    });
    let (_, v) = run_hook("UserPromptSubmit", &p, &envs, d.path());
    let ctx = v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or("");
    assert!(
        ctx.contains("lens_search") && ctx.contains("lens_graph"),
        "find/trace prompt must inject the tool mapping: {v}"
    );

    // Non-matching prompt: no injection.
    let plain = json!({
        "session_id": "s9",
        "cwd": d.path().to_string_lossy(),
        "prompt": "bump the version and tag the release",
    });
    let (_, v2) = run_hook("UserPromptSubmit", &plain, &envs, d.path());
    assert!(
        v2["hookSpecificOutput"]["additionalContext"].is_null(),
        "plain prompt must not inject: {v2}"
    );

    // Routing off: no injection even for a matching prompt.
    let d2 = tempfile::tempdir().unwrap();
    let (_, v3) = run_hook("UserPromptSubmit", &p, &[("LENS_ROUTING", "off")], d2.path());
    assert!(
        v3["hookSpecificOutput"]["additionalContext"].is_null(),
        "routing off must not inject: {v3}"
    );
}

#[test]
fn greps_share_the_deny_counter_and_edits_reset_it() {
    let d = tempfile::tempdir().unwrap();
    let envs = [("LENS_ROUTING", "full"), ("LENS_ROUTING_MCP", "up")];
    let grep = json!({
        "session_id": "s2",
        "cwd": d.path().to_string_lossy(),
        "tool_name": "Grep",
        "tool_input": { "pattern": "include_bodies" },
    });
    let read = read_payload(d.path(), "s2", "src/server.rs");

    // Grep,Read,Read then a 4th lookup: mixed calls share one counter.
    run_hook("PreToolUse", &grep, &envs, d.path());
    run_hook("PreToolUse", &read, &envs, d.path());
    run_hook("PreToolUse", &read, &envs, d.path());
    let (_, fourth) = run_hook("PreToolUse", &read, &envs, d.path());
    assert_eq!(
        fourth["hookSpecificOutput"]["permissionDecision"], "deny",
        "4th mixed Grep/Read lookup must deny: {fourth}"
    );

    // An Edit (PostToolUse) resets the counter: Read-before-Edit is not drift.
    run_hook("PreToolUse", &read, &envs, d.path());
    run_hook("PreToolUse", &read, &envs, d.path());
    let edit_post = json!({
        "session_id": "s2",
        "cwd": d.path().to_string_lossy(),
        "tool_name": "Edit",
        "tool_input": { "file_path": "src/server.rs" },
        "tool_response": "ok",
    });
    run_hook("PostToolUse", &edit_post, &envs, d.path());
    // Two more lookups stay under the threshold (counter restarted at 0).
    run_hook("PreToolUse", &read, &envs, d.path());
    let (_, after) = run_hook("PreToolUse", &read, &envs, d.path());
    assert_ne!(
        after["hookSpecificOutput"]["permissionDecision"], "deny",
        "lookups after an Edit reset must not deny: {after}"
    );
}

#[test]
fn find_trace_prompt_arms_a_one_shot_deny_on_the_first_grep() {
    let d = tempfile::tempdir().unwrap();
    // The deny gate now requires a populated index; make the tempdir look indexed.
    seed_index(d.path());
    let envs = [("LENS_ROUTING", "full"), ("LENS_ROUTING_MCP", "up")];
    let prompt = json!({
        "session_id": "s10",
        "cwd": d.path().to_string_lossy(),
        "prompt": "Where is the deny threshold defined and what calls it?",
    });
    let grep = json!({
        "session_id": "s10",
        "cwd": d.path().to_string_lossy(),
        "tool_name": "Grep",
        "tool_input": { "pattern": "deny_threshold" },
    });

    // The find/trace prompt arms the marker; the first Grep is denied once.
    run_hook("UserPromptSubmit", &prompt, &envs, d.path());
    let (_, first) = run_hook("PreToolUse", &grep, &envs, d.path());
    assert_eq!(
        first["hookSpecificOutput"]["permissionDecision"], "deny",
        "first Grep after a find/trace prompt must deny: {first}"
    );
    let reason = first["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(
        reason.contains("lens_search") && reason.contains("fires once"),
        "deny reason maps intents and states one-shot semantics: {reason}"
    );

    // The marker was consumed: the retried Grep passes.
    let (_, second) = run_hook("PreToolUse", &grep, &envs, d.path());
    assert_ne!(
        second["hookSpecificOutput"]["permissionDecision"], "deny",
        "retried Grep must pass (marker consumed): {second}"
    );

    // Re-armed by a new find/trace prompt, then DISARMED by a lens call:
    // the follow-up Grep is no longer drift and must pass.
    run_hook("UserPromptSubmit", &prompt, &envs, d.path());
    let lens_post = json!({
        "session_id": "s10",
        "cwd": d.path().to_string_lossy(),
        "tool_name": "mcp__lens__lens_search",
        "tool_input": { "queries": ["deny threshold"] },
        "tool_response": "ok",
    });
    run_hook("PostToolUse", &lens_post, &envs, d.path());
    let (_, third) = run_hook("PreToolUse", &grep, &envs, d.path());
    assert_ne!(
        third["hookSpecificOutput"]["permissionDecision"], "deny",
        "Grep after a lens call must pass (marker disarmed): {third}"
    );
}

// ---------------------------------------------------------------------------
// T8: reroute rails — deny-only classifiers wired into routing, all flags
// kill-switch polarity (default ON, `=0` disables), gated on level +
// mcp_ready + index_present, mirroring the shipped grep-scope deny. Every
// test drives the real binary.
// ---------------------------------------------------------------------------

/// The steering env every rail's gates expect (level + reachable MCP); each
/// test appends its rail's flag explicitly.
const FULL_UP: [(&str, &str); 2] = [("LENS_ROUTING", "full"), ("LENS_ROUTING_MCP", "up")];

/// All rail deny flags pinned OFF (`=0`). Kill-switch polarity means every
/// flag is ON by default, so a test isolating ONE rail must silence the
/// others or an unrelated rail can pre-empt the assertion (e.g. `impl Foo` is
/// BOTH a gsym- and a gast-shaped Grep pattern).
const ALL_RAILS_OFF: [(&str, &str); 9] = [
    ("LENS_GREP_SCOPE_DENY", "0"),
    ("LENS_GREP_SYMBOL_DENY", "0"),
    ("LENS_READ_SKELETON_DENY", "0"),
    ("LENS_GREP_AST_DENY", "0"),
    ("LENS_READ_OVERVIEW_DENY", "0"),
    ("LENS_BASH_AGG_DENY", "0"),
    ("LENS_EDIT_LINKS_DENY", "0"),
    ("LENS_BASH_GREP_DENY", "0"),
    ("LENS_READ_RUNFILE_DENY", "0"),
];

/// Build a deterministic env vec for a rail test: `level` (e.g. `FULL_UP`)
/// plus all rail deny flags pinned OFF, with `overrides` applied last so a
/// test can re-enable its own rail(s) (`=1`) or tweak level/mcp.
fn rail_envs<'a>(
    level: &[(&'a str, &'a str)],
    overrides: &[(&'a str, &'a str)],
) -> Vec<(&'a str, &'a str)> {
    let mut envs: Vec<(&str, &str)> = level.to_vec();
    envs.extend_from_slice(&ALL_RAILS_OFF);
    for &(k, v) in overrides {
        if let Some(slot) = envs.iter_mut().find(|(ek, _)| *ek == k) {
            slot.1 = v;
        } else {
            envs.push((k, v));
        }
    }
    envs
}

/// Like [`rail_envs`], but `unset` names rail flags to leave OUT of the
/// pinned-off baseline entirely, so they inherit the real unset/default-ON
/// polarity instead of being pinned to `0` — for locking deny-only-config
/// attribution (deny flag genuinely unset, not just left at its default via
/// an explicit `=1`).
fn rail_envs_leave_unset<'a>(
    level: &[(&'a str, &'a str)],
    unset: &[&str],
    overrides: &[(&'a str, &'a str)],
) -> Vec<(&'a str, &'a str)> {
    let mut envs: Vec<(&str, &str)> = level.to_vec();
    envs.extend(
        ALL_RAILS_OFF
            .iter()
            .copied()
            .filter(|(k, _)| !unset.contains(k)),
    );
    for &(k, v) in overrides {
        if let Some(slot) = envs.iter_mut().find(|(ek, _)| *ek == k) {
            slot.1 = v;
        } else {
            envs.push((k, v));
        }
    }
    envs
}

fn grep_payload(dir: &Path, session: &str, pattern: &str) -> Value {
    json!({
        "session_id": session,
        "cwd": dir.to_string_lossy(),
        "tool_name": "Grep",
        "tool_input": { "pattern": pattern },
    })
}

fn edit_payload(dir: &Path, session: &str, old: &str, new: &str) -> Value {
    json!({
        "session_id": session,
        "cwd": dir.to_string_lossy(),
        "tool_name": "Edit",
        "tool_input": { "file_path": "src/a.rs", "old_string": old, "new_string": new },
    })
}

/// A graph where `alpha` has 3 callers (>= the default elink threshold) and
/// `beta` has 1 (below it). Same JSON shape as `minimal_graph_json`.
fn callers_graph_json() -> Value {
    json!({
        "nodes": [
            {"id": "a1", "name": "alpha", "kind": "function", "file": "src/a.rs", "line": 1, "language": "rust"},
            {"id": "b1", "name": "beta", "kind": "function", "file": "src/b.rs", "line": 1, "language": "rust"},
            {"id": "c1", "name": "caller_one", "kind": "function", "file": "src/c.rs", "line": 1, "language": "rust"},
            {"id": "c2", "name": "caller_two", "kind": "function", "file": "src/c.rs", "line": 10, "language": "rust"},
            {"id": "c3", "name": "caller_three", "kind": "function", "file": "src/c.rs", "line": 20, "language": "rust"},
        ],
        "edges": [
            {"from": "c1", "to": "a1", "kind": "calls"},
            {"from": "c2", "to": "a1", "kind": "calls"},
            {"from": "c3", "to": "a1", "kind": "calls"},
            {"from": "c1", "to": "b1", "kind": "calls"},
        ],
    })
}

/// Two symbols (`alpha`, `gamma`) each with 3 callers — lets a test prove the
/// elink one-shot is scoped per (session, symbol), not per session.
fn two_qualifying_symbols_graph_json() -> Value {
    json!({
        "nodes": [
            {"id": "a1", "name": "alpha", "kind": "function", "file": "src/a.rs", "line": 1, "language": "rust"},
            {"id": "g1", "name": "gamma", "kind": "function", "file": "src/g.rs", "line": 1, "language": "rust"},
            {"id": "c1", "name": "caller_one", "kind": "function", "file": "src/c.rs", "line": 1, "language": "rust"},
            {"id": "c2", "name": "caller_two", "kind": "function", "file": "src/c.rs", "line": 10, "language": "rust"},
            {"id": "c3", "name": "caller_three", "kind": "function", "file": "src/c.rs", "line": 20, "language": "rust"},
        ],
        "edges": [
            {"from": "c1", "to": "a1", "kind": "calls"},
            {"from": "c2", "to": "a1", "kind": "calls"},
            {"from": "c3", "to": "a1", "kind": "calls"},
            {"from": "c1", "to": "g1", "kind": "calls"},
            {"from": "c2", "to": "g1", "kind": "calls"},
            {"from": "c3", "to": "g1", "kind": "calls"},
        ],
    })
}

/// A graph with a single node named `name` — enough for `graph_resolves`
/// (the gsym rail's graph-resolution gate, T2) to find it.
fn resolving_graph_json(name: &str) -> Value {
    json!({
        "nodes": [
            {"id": "n1", "name": name, "kind": "function", "file": "a.rs", "line": 1, "language": "rust"},
        ],
        "edges": [],
    })
}

fn is_deny(v: &Value) -> bool {
    v["hookSpecificOutput"]["permissionDecision"] == "deny"
}

#[test]
fn grep_symbol_deny_fires_once_with_flag_and_gates() {
    let d = tempfile::tempdir().unwrap();
    seed_index(d.path());
    std::fs::write(
        d.path().join("graph.json"),
        resolving_graph_json("handle_connection").to_string(),
    )
    .unwrap();
    let envs = rail_envs(&FULL_UP, &[("LENS_GREP_SYMBOL_DENY", "1")]);
    let p = grep_payload(d.path(), "gsym1", "fn handle_connection");

    let (_, first) = run_hook("PreToolUse", &p, &envs, d.path());
    assert!(is_deny(&first), "symbol-shaped Grep must deny with the flag on: {first}");
    let reason = first["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(
        reason.contains("lens_symbol(name=\"handle_connection\")") && reason.contains("ToolSearch"),
        "deny reason names the exact lens call + bootstrap: {reason}"
    );

    // One-shot: the verbatim retry passes.
    let (_, second) = run_hook("PreToolUse", &p, &envs, d.path());
    assert!(!is_deny(&second), "retried symbol Grep must pass: {second}");
}

#[test]
fn grep_symbol_deny_off_or_gated_stays_quiet() {
    // Flag off (explicit `=0` now that the kill-switch default is ON): never
    // denies, and once the generic grep tip is spent the call renders
    // byte-identical `{}`.
    let d = tempfile::tempdir().unwrap();
    seed_index(d.path());
    std::fs::write(
        d.path().join("graph.json"),
        resolving_graph_json("handle_connection").to_string(),
    )
    .unwrap();
    let off = rail_envs(&FULL_UP, &[("LENS_GREP_SYMBOL_DENY", "0")]);
    let p = grep_payload(d.path(), "gsym2", "fn handle_connection");
    let (_, first) = run_hook("PreToolUse", &p, &off, d.path());
    assert!(!is_deny(&first), "flag off must never deny: {first}");
    let (raw2, _) = run_hook("PreToolUse", &p, &off, d.path());
    assert_eq!(raw2, "{}", "flag off after the one-shot tip is a pure no-op");

    // Flag on but MCP down: no deny, and the blocked gate must NOT spend the
    // one-shot — the same session denies once the server is reachable.
    let down = rail_envs(
        &[("LENS_ROUTING", "full"), ("LENS_ROUTING_MCP", "down")],
        &[("LENS_GREP_SYMBOL_DENY", "1")],
    );
    let up = rail_envs(&FULL_UP, &[("LENS_GREP_SYMBOL_DENY", "1")]);
    let p3 = grep_payload(d.path(), "gsym3", "fn handle_connection");
    let (_, gated) = run_hook("PreToolUse", &p3, &down, d.path());
    assert!(!is_deny(&gated), "mcp down must gate the deny: {gated}");
    let (_, after) = run_hook("PreToolUse", &p3, &up, d.path());
    assert!(is_deny(&after), "gate-blocked one-shot must survive to fire later: {after}");

    // Flag on but no populated index: no deny; seeding the index un-gates it.
    let d2 = tempfile::tempdir().unwrap();
    std::fs::write(
        d2.path().join("graph.json"),
        resolving_graph_json("handle_connection").to_string(),
    )
    .unwrap();
    let p4 = grep_payload(d2.path(), "gsym4", "fn handle_connection");
    let (_, noindex) = run_hook("PreToolUse", &p4, &up, d2.path());
    assert!(!is_deny(&noindex), "no index must gate the deny: {noindex}");
    seed_index(d2.path());
    let (_, seeded) = run_hook("PreToolUse", &p4, &up, d2.path());
    assert!(is_deny(&seeded), "populated index un-gates the deny: {seeded}");
}

#[test]
fn grep_ast_deny_fires_once_with_flag_and_stays_quiet_off() {
    let d = tempfile::tempdir().unwrap();
    seed_index(d.path());
    let envs = rail_envs(&FULL_UP, &[("LENS_GREP_AST_DENY", "1")]);

    // An escaped/regex-form syntax-shaped Grep (the recall-fix case) denies
    // toward the translated lens_grep_ast query, once per session.
    let p = grep_payload(d.path(), "gastdeny1", r"impl.*Forge");
    let (_, first) = run_hook("PreToolUse", &p, &envs, d.path());
    assert!(is_deny(&first), "escaped syntax-shaped Grep must deny with the flag on: {first}");
    let reason = first["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(
        reason.contains("lens_grep_ast")
            && reason.contains("impl_item")
            && reason.contains("\"Forge\"")
            && reason.contains("ToolSearch"),
        "deny reason names the translated lens_grep_ast query + bootstrap: {reason}"
    );
    // One-shot: the verbatim retry passes (never denied again).
    let (_, second) = run_hook("PreToolUse", &p, &envs, d.path());
    assert!(!is_deny(&second), "retried syntax Grep must pass: {second}");

    // Flag off (explicit `=0` now that the kill-switch default is ON): the
    // same escaped pattern never denies.
    let d2 = tempfile::tempdir().unwrap();
    seed_index(d2.path());
    let off_env = rail_envs(&FULL_UP, &[]);
    let poff = grep_payload(d2.path(), "gastdeny2", r"impl.*Forge");
    let (_, off) = run_hook("PreToolUse", &poff, &off_env, d2.path());
    assert!(!is_deny(&off), "flag off must never deny: {off}");

    // Flag on but MCP down: the blocked gate must NOT spend the one-shot; the
    // same session denies once the server is reachable.
    let d3 = tempfile::tempdir().unwrap();
    seed_index(d3.path());
    let down = rail_envs(
        &[("LENS_ROUTING", "full"), ("LENS_ROUTING_MCP", "down")],
        &[("LENS_GREP_AST_DENY", "1")],
    );
    let up = rail_envs(&FULL_UP, &[("LENS_GREP_AST_DENY", "1")]);
    let p3 = grep_payload(d3.path(), "gastdeny3", r"impl.*Forge");
    let (_, gated) = run_hook("PreToolUse", &p3, &down, d3.path());
    assert!(!is_deny(&gated), "mcp down must gate the deny: {gated}");
    let (_, after) = run_hook("PreToolUse", &p3, &up, d3.path());
    assert!(is_deny(&after), "gate-blocked one-shot must survive to fire later: {after}");
}

#[test]
fn grep_ast_deny_resets_read_code_so_the_retry_passes() {
    let d = tempfile::tempdir().unwrap();
    seed_index(d.path());
    let envs = rail_envs(&FULL_UP, &[("LENS_GREP_AST_DENY", "1")]);
    let sess = "gastreset";

    // Drive the shared read-code lookup counter up with plain, non-syntax greps
    // that bump the counter but never deny (the 4th consecutive lookup is the
    // inspect_escalation deny threshold).
    let plain = grep_payload(d.path(), sess, "error message");
    for _ in 0..3 {
        run_hook("PreToolUse", &plain, &envs, d.path());
    }

    // A syntax-shaped grep now gast-denies, which resets read-code to zero.
    let syn = grep_payload(d.path(), sess, r"impl.*Forge");
    let (_, denied) = run_hook("PreToolUse", &syn, &envs, d.path());
    assert!(is_deny(&denied), "syntax grep must gast-deny: {denied}");

    // The verbatim retry must NOT be re-denied: the gast deny is one-shot AND it
    // reset read-code, so the retry can't trip inspect_escalation's 4th-lookup
    // deny. Without the reset the counter would sit at 3 and the retry would hit
    // n=4 and deny.
    let (_, retry) = run_hook("PreToolUse", &syn, &envs, d.path());
    assert!(!is_deny(&retry), "verbatim gast retry must pass (read-code reset): {retry}");
}

#[test]
fn read_skeleton_deny_fires_once_and_spares_edited_and_bounded_reads() {
    let d = tempfile::tempdir().unwrap();
    seed_index(d.path());
    let envs = rail_envs(&FULL_UP, &[("LENS_READ_SKELETON_DENY", "1")]);

    // A whole, unedited code-file Read is denied toward lens_skeleton, once
    // per file (`src/widget.rs` need not exist on disk — the classifier
    // doesn't stat).
    let p = read_payload(d.path(), "rskel1", "src/widget.rs");
    let (_, first) = run_hook("PreToolUse", &p, &envs, d.path());
    assert!(is_deny(&first), "whole code-file Read must deny: {first}");
    let reason = first["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(
        reason.contains("lens_skeleton(path=\"src/widget.rs\")") && reason.contains("include_bodies"),
        "deny reason names the exact skeleton call: {reason}"
    );
    let (_, second) = run_hook("PreToolUse", &p, &envs, d.path());
    assert!(!is_deny(&second), "retried Read must pass (per-file one-shot): {second}");
    // Per-file semantics: a DIFFERENT file gets its own deny in this session.
    let p_b = read_payload(d.path(), "rskel1", "src/widget_two.rs");
    let (_, second_file) = run_hook("PreToolUse", &p_b, &envs, d.path());
    assert!(
        is_deny(&second_file),
        "a different file gets its own skeleton deny: {second_file}"
    );

    // An edited path is spared: Read-before-the-next-Edit is the right tool.
    let edit_post = json!({
        "session_id": "rskel2",
        "cwd": d.path().to_string_lossy(),
        "tool_name": "Edit",
        "tool_input": { "file_path": "src/edited.rs" },
        "tool_response": "ok",
    });
    run_hook("PostToolUse", &edit_post, &envs, d.path());
    let edited = read_payload(d.path(), "rskel2", "src/edited.rs");
    let (_, spared) = run_hook("PreToolUse", &edited, &envs, d.path());
    assert!(!is_deny(&spared), "an edited path must never skeleton-deny: {spared}");
    // ...and its one-shot was NOT spent: an unedited file still denies.
    let other = read_payload(d.path(), "rskel2", "src/other.rs");
    let (_, denied) = run_hook("PreToolUse", &other, &envs, d.path());
    assert!(is_deny(&denied), "unedited file still denies in the same session: {denied}");

    // A bounded Read (offset/limit) is not rskel's shape (it is the runfile
    // arm's, pinned off by rail_envs here) — spared.
    let bounded = json!({
        "session_id": "rskel3",
        "cwd": d.path().to_string_lossy(),
        "tool_name": "Read",
        "tool_input": { "file_path": "src/widget.rs", "limit": 40 },
    });
    let (_, lean) = run_hook("PreToolUse", &bounded, &envs, d.path());
    assert!(!is_deny(&lean), "a bounded Read must never skeleton-deny: {lean}");

    // Flag off: no deny, byte-identical `{}`.
    let off_env = rail_envs(&FULL_UP, &[]);
    let poff = read_payload(d.path(), "rskel4", "src/widget.rs");
    let (_, off1) = run_hook("PreToolUse", &poff, &off_env, d.path());
    assert!(!is_deny(&off1), "flag off must never deny: {off1}");
    let (raw2, _) = run_hook("PreToolUse", &poff, &off_env, d.path());
    assert_eq!(raw2, "{}", "flag off is a pure no-op");
}

#[test]
fn bash_grep_deny_end_to_end_prompt_arms_then_denies() {
    // The full arming path through the real binary: a UserPromptSubmit (any
    // prompt, steering) arms the grep-scope budget; the next broad shell grep
    // is denied toward lens_search; the verbatim retry passes.
    let d = tempfile::tempdir().unwrap();
    seed_index(d.path());
    let envs: Vec<(&str, &str)> = FULL_UP.to_vec();
    let sess = "bgrep-e2e";
    let prompt = json!({
        "session_id": sess,
        "cwd": d.path().to_string_lossy(),
        "prompt": "clean up the fixture layout",
    });
    run_hook("UserPromptSubmit", &prompt, &envs, d.path());
    let grep = bash_payload(d.path(), sess, "grep -rn deny_threshold src/");
    let (_, first) = run_hook("PreToolUse", &grep, &envs, d.path());
    assert!(is_deny(&first), "armed broad shell grep must deny: {first}");
    let reason = first["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(
        reason.contains("lens_search(queries: [\"deny_threshold\"])"),
        "deny reason carries the ready-to-paste lens_search call: {reason}"
    );
    let (_, second) = run_hook("PreToolUse", &grep, &envs, d.path());
    assert!(
        !is_deny(&second),
        "the verbatim retry passes (budget consumed): {second}"
    );
}

#[test]
fn bash_agg_deny_fires_before_wrap_with_flag_and_wraps_off() {
    let d = tempfile::tempdir().unwrap();
    seed_index(d.path());
    let agg = "find . -name '*.rs' | wc -l";

    // At full the bagg deny sits BEFORE the wrap rewrite, so with the flag on the
    // aggregate pipeline is DENIED toward lens_run (pre-empting the wrap), once.
    let envs = rail_envs(&FULL_UP, &[("LENS_BASH_AGG_DENY", "1")]);
    let p = bash_payload(d.path(), "baggdeny1", agg);
    let (_, first) = run_hook("PreToolUse", &p, &envs, d.path());
    assert!(is_deny(&first), "aggregate Bash must deny with the flag on, pre-empting wrap: {first}");
    let reason = first["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(
        reason.contains("lens_run") && reason.contains("re-run it verbatim"),
        "deny reason names lens_run + the retry promise: {reason}"
    );
    // One-shot: the verbatim retry is not re-denied (at full it wraps instead).
    let (_, second) = run_hook("PreToolUse", &p, &envs, d.path());
    assert!(!is_deny(&second), "retried aggregate Bash must pass: {second}");

    // Flag off (explicit `=0` now that the kill-switch default is ON): at full
    // the aggregate is WRAPPED, never denied, byte-identical to master's
    // behavior.
    let d2 = tempfile::tempdir().unwrap();
    seed_index(d2.path());
    let off_env = rail_envs(&FULL_UP, &[]);
    let p2 = bash_payload(d2.path(), "baggdeny2", agg);
    let (_, wrapped) = run_hook("PreToolUse", &p2, &off_env, d2.path());
    assert!(!is_deny(&wrapped), "flag off must never deny: {wrapped}");
    assert_eq!(
        wrapped["hookSpecificOutput"]["permissionDecision"], "allow",
        "flag off at full still wraps the aggregate: {wrapped}"
    );
    assert!(
        wrapped["hookSpecificOutput"]["updatedInput"]["command"]
            .as_str()
            .unwrap()
            .contains("wrap -- "),
        "the aggregate is wrapped, not denied, when the flag is off: {wrapped}"
    );

    // MCP down gates the deny (a rail must never send the agent to a dead tool).
    let d3 = tempfile::tempdir().unwrap();
    seed_index(d3.path());
    let down = rail_envs(
        &[("LENS_ROUTING", "full"), ("LENS_ROUTING_MCP", "down")],
        &[("LENS_BASH_AGG_DENY", "1")],
    );
    let p3 = bash_payload(d3.path(), "baggdeny3", agg);
    let (_, gated) = run_hook("PreToolUse", &p3, &down, d3.path());
    assert!(!is_deny(&gated), "mcp down must gate the bagg deny: {gated}");
}

#[test]
fn edit_links_deny_covers_multiedit_and_stays_quiet_without_graph() {
    // MultiEdit + guard coverage for the deny arm (the retired nudge arm's
    // shape tests, re-homed): the first decl-touching edit in `edits[]` wins;
    // body-only edits, below-threshold symbols, flag-off, and a missing graph
    // all render byte-identical `{}` or pass through.
    let d = tempfile::tempdir().unwrap();
    seed_index(d.path());
    std::fs::write(d.path().join("graph.json"), callers_graph_json().to_string()).unwrap();
    let envs = rail_envs(&FULL_UP, &[("LENS_EDIT_LINKS_DENY", "1")]);

    // MultiEdit: the first decl-touching edit in `edits[]` wins.
    let pm = json!({
        "session_id": "elink-multi",
        "cwd": d.path().to_string_lossy(),
        "tool_name": "MultiEdit",
        "tool_input": { "file_path": "src/a.rs", "edits": [
            { "old_string": "// note", "new_string": "// notes" },
            { "old_string": "pub fn alpha() {", "new_string": "pub fn alpha(y: u8) {" },
        ]},
    });
    let (_, multi) = run_hook("PreToolUse", &pm, &envs, d.path());
    assert!(is_deny(&multi), "MultiEdit finds the decl-touching edit: {multi}");
    assert!(
        multi["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .contains("lens_graph(node=\"alpha\")"),
        "deny names the symbol: {multi}"
    );

    // Below the caller threshold: quiet.
    let pb = edit_payload(d.path(), "elink-low", "fn beta() {", "fn beta(x: u32) {");
    let (raw_b, _) = run_hook("PreToolUse", &pb, &envs, d.path());
    assert_eq!(raw_b, "{}", "a 1-caller symbol is below the K=3 gate");

    // A body-only edit extracts no symbol: quiet.
    let pbody = edit_payload(d.path(), "elink-body", "let x = 1;", "let x = 2;");
    let (raw_body, _) = run_hook("PreToolUse", &pbody, &envs, d.path());
    assert_eq!(raw_body, "{}", "body-only edits never deny");

    // Flag off: an Edit is a pure no-op (`{}`).
    let off_env = rail_envs(&FULL_UP, &[]);
    let poff = edit_payload(d.path(), "elink-off", "fn alpha() {", "fn alpha(x: u32) {");
    let (raw_off, _) = run_hook("PreToolUse", &poff, &off_env, d.path());
    assert_eq!(raw_off, "{}", "flag off: Edit renders byte-identical {{}}");

    // No graph on disk: the load is guarded, quiet.
    let d2 = tempfile::tempdir().unwrap();
    seed_index(d2.path());
    let png = edit_payload(d2.path(), "elink-nograph", "fn alpha() {", "fn alpha(x: u32) {");
    let (raw_ng, _) = run_hook("PreToolUse", &png, &envs, d2.path());
    assert_eq!(raw_ng, "{}", "graph absent: the rail stays quiet");
}

#[test]
fn reroute_follower_counters_split_live_and_shadow() {
    // Shadow arm (deny AND nudge flags explicitly `=0` — kill-switch polarity
    // means shadow now requires an explicit off, not silence): a would-fire
    // arms the shadow marker; the NEXT event bumps {p}_shadow_next_{class}.
    let d = tempfile::tempdir().unwrap();
    seed_index(d.path());
    std::fs::write(
        d.path().join("graph.json"),
        resolving_graph_json("handle_connection").to_string(),
    )
    .unwrap();
    let shadow = rail_envs(&FULL_UP, &[]);
    let grep = grep_payload(d.path(), "fc1", "fn handle_connection");
    run_hook("PreToolUse", &grep, &shadow, d.path());
    let read = read_payload(d.path(), "fc1", "src/server.rs");
    run_hook("PreToolUse", &read, &shadow, d.path());
    let store = lens::store::Store::open(d.path()).unwrap();
    assert_eq!(
        store.get_stat("gsym_would_fire").unwrap(),
        1,
        "the symbol grep would-fire is counted regardless of the flag"
    );
    assert_eq!(
        store.get_stat("gsym_shadow_next_read").unwrap(),
        1,
        "flag explicitly off arms the SHADOW marker; the follower Read lands there"
    );
    assert_eq!(store.get_stat("gsym_next_read").unwrap(), 0);

    // Live arm (deny flag ON, the kill-switch default): the follower lands in
    // {p}_next_{class} instead.
    let d2 = tempfile::tempdir().unwrap();
    seed_index(d2.path());
    std::fs::write(
        d2.path().join("graph.json"),
        resolving_graph_json("handle_connection").to_string(),
    )
    .unwrap();
    let on = rail_envs(&FULL_UP, &[("LENS_GREP_SYMBOL_DENY", "1")]);
    let grep2 = grep_payload(d2.path(), "fc2", "fn handle_connection");
    run_hook("PreToolUse", &grep2, &on, d2.path());
    let bash2 = bash_payload(d2.path(), "fc2", "echo hi");
    run_hook("PreToolUse", &bash2, &on, d2.path());
    let store2 = lens::store::Store::open(d2.path()).unwrap();
    assert_eq!(store2.get_stat("gsym_would_fire").unwrap(), 1);
    assert_eq!(
        store2.get_stat("gsym_next_bash").unwrap(),
        1,
        "flag on arms the LIVE marker; the follower Bash lands there"
    );
    assert_eq!(store2.get_stat("gsym_shadow_next_bash").unwrap(), 0);

    // The compliant follower classes as `lens` (a lens MCP call).
    let d3 = tempfile::tempdir().unwrap();
    seed_index(d3.path());
    std::fs::write(
        d3.path().join("graph.json"),
        resolving_graph_json("handle_connection").to_string(),
    )
    .unwrap();
    let grep3 = grep_payload(d3.path(), "fc3", "fn handle_connection");
    run_hook("PreToolUse", &grep3, &shadow, d3.path());
    let lens_call = json!({
        "session_id": "fc3",
        "cwd": d3.path().to_string_lossy(),
        "tool_name": "mcp__lens__lens_symbol",
        "tool_input": { "name": "handle_connection" },
    });
    run_hook("PreToolUse", &lens_call, &shadow, d3.path());
    let store3 = lens::store::Store::open(d3.path()).unwrap();
    assert_eq!(
        store3.get_stat("gsym_shadow_next_lens").unwrap(),
        1,
        "a lens follower classes as `lens`"
    );

    // A Read event's own would-fire (rskel) arms alongside: the shadow plane
    // tracks each rail independently on the same event stream.
    assert_eq!(
        store.get_stat("rskel_would_fire").unwrap(),
        1,
        "the follower Read itself would-fires the rskel rail"
    );
}

// ---------------------------------------------------------------------------
// T2: gsym deny gated on the symbol resolving in the graph — a classifier hit
// whose graph lookup would dead-end never denies toward it.
// ---------------------------------------------------------------------------
#[test]
fn grep_symbol_deny_gates_on_graph_resolution() {
    // Graph present but lacking the ident: the classifier matches, but the
    // deny never fires (a lens_symbol retry would come up empty).
    let d = tempfile::tempdir().unwrap();
    seed_index(d.path());
    std::fs::write(
        d.path().join("graph.json"),
        resolving_graph_json("some_other_symbol").to_string(),
    )
    .unwrap();
    let envs = rail_envs(&FULL_UP, &[("LENS_GREP_SYMBOL_DENY", "1")]);
    let p = grep_payload(d.path(), "gsymres1", "fn unresolved_symbol");
    let (_, first) = run_hook("PreToolUse", &p, &envs, d.path());
    assert!(!is_deny(&first), "graph lacking the ident must not deny: {first}");

    // Ident present in the graph: the deny fires once.
    let d2 = tempfile::tempdir().unwrap();
    seed_index(d2.path());
    std::fs::write(
        d2.path().join("graph.json"),
        resolving_graph_json("unresolved_symbol").to_string(),
    )
    .unwrap();
    let p2 = grep_payload(d2.path(), "gsymres2", "fn unresolved_symbol");
    let (_, second) = run_hook("PreToolUse", &p2, &envs, d2.path());
    assert!(is_deny(&second), "graph containing the ident denies once: {second}");
}

// ---------------------------------------------------------------------------
// T3: rskel edit-intent prompt exemption. An edit-shaped prompt arms the
// `edit-intent` marker at UserPromptSubmit; the rskel deny then spares the
// pre-first-edit Read (the harness mandates Read-before-Edit). Prompt-scoped:
// every non-edit prompt re-clears it. Drives the real binary across the
// UserPromptSubmit → PreToolUse process boundary (the marker survives via the
// on-disk nudge log).
// ---------------------------------------------------------------------------
fn prompt_payload(dir: &Path, session: &str, prompt: &str) -> Value {
    json!({
        "session_id": session,
        "cwd": dir.to_string_lossy(),
        "prompt": prompt,
    })
}

#[test]
fn rskel_edit_intent_prompt_exempts_the_pre_edit_read() {
    let d = tempfile::tempdir().unwrap();
    seed_index(d.path());
    let envs = rail_envs(&FULL_UP, &[("LENS_READ_SKELETON_DENY", "1")]);
    let sess = "rskeledit1";

    // An edit-shaped prompt arms the edit-intent exemption for this session.
    let prompt = prompt_payload(d.path(), sess, "Update the null check in src/widget.rs");
    run_hook("UserPromptSubmit", &prompt, &envs, d.path());

    // The whole, unedited code-file Read that rskel would normally deny now
    // passes through: the harness needs this Read before the Edit.
    let read = read_payload(d.path(), sess, "src/widget.rs");
    let (_, v) = run_hook("PreToolUse", &read, &envs, d.path());
    assert!(
        !is_deny(&v),
        "an edit-intent prompt must exempt the pre-edit Read from the rskel deny: {v}"
    );
}

#[test]
fn rskel_denies_after_a_neutral_prompt_leaves_edit_intent_clear() {
    let d = tempfile::tempdir().unwrap();
    seed_index(d.path());
    let envs = rail_envs(&FULL_UP, &[("LENS_READ_SKELETON_DENY", "1")]);
    let sess = "rskeledit2";

    // A neutral prompt (neither edit- nor find/trace-shaped) clears edit-intent,
    // so the rskel deny still fires on the whole-file Read.
    let prompt = prompt_payload(d.path(), sess, "Summarize the project layout for me.");
    run_hook("UserPromptSubmit", &prompt, &envs, d.path());

    let read = read_payload(d.path(), sess, "src/widget.rs");
    let (_, v) = run_hook("PreToolUse", &read, &envs, d.path());
    assert!(is_deny(&v), "a neutral prompt must not exempt the Read: {v}");
}

#[test]
fn rskel_edit_intent_is_recleared_by_a_later_non_edit_prompt() {
    let d = tempfile::tempdir().unwrap();
    seed_index(d.path());
    let envs = rail_envs(&FULL_UP, &[("LENS_READ_SKELETON_DENY", "1")]);
    let sess = "rskeledit3";

    // Edit-shaped prompt arms the exemption: the pre-edit Read passes (and,
    // being exempt, never spends the rskel one-shot).
    let edit_prompt = prompt_payload(d.path(), sess, "Refactor the deny gate in src/widget.rs");
    run_hook("UserPromptSubmit", &edit_prompt, &envs, d.path());
    let read = read_payload(d.path(), sess, "src/widget.rs");
    let (_, armed_read) = run_hook("PreToolUse", &read, &envs, d.path());
    assert!(
        !is_deny(&armed_read),
        "edit-intent prompt must exempt the Read (marker armed): {armed_read}"
    );

    // A later NON-edit prompt re-clears the marker; the same Read now denies,
    // proving the exemption is prompt-scoped, not sticky.
    let neutral_prompt = prompt_payload(d.path(), sess, "Give me a short tour of the codebase.");
    run_hook("UserPromptSubmit", &neutral_prompt, &envs, d.path());
    let (_, denied) = run_hook("PreToolUse", &read, &envs, d.path());
    assert!(
        is_deny(&denied),
        "a non-edit prompt must re-clear edit-intent so rskel denies again: {denied}"
    );
}

// ---------------------------------------------------------------------------
// T5: default-ON + new-arm coverage for the both-arms-per-rail polarity flip.
// ---------------------------------------------------------------------------

#[test]
fn default_on_smoke_symbol_grep_denies_with_no_rail_flags_set() {
    // Kill-switch polarity: with NO rail flags set at all (not even pinned
    // off), routing is live by default — the "just install and go" case this
    // whole plan exists for.
    let d = tempfile::tempdir().unwrap();
    seed_index(d.path());
    std::fs::write(
        d.path().join("graph.json"),
        resolving_graph_json("handle_connection").to_string(),
    )
    .unwrap();
    let p = grep_payload(d.path(), "defaulton1", "fn handle_connection");
    let (_, first) = run_hook("PreToolUse", &p, &FULL_UP, d.path());
    assert!(
        is_deny(&first),
        "a symbol-shaped Grep must deny by default with no rail flags set: {first}"
    );
    let reason = first["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(
        reason.contains("lens_symbol(name=\"handle_connection\")"),
        "deny reason names the exact lens call: {reason}"
    );

    // Verbatim retry passes (one-shot).
    let (_, second) = run_hook("PreToolUse", &p, &FULL_UP, d.path());
    assert!(!is_deny(&second), "retried symbol Grep must pass: {second}");
}

#[test]
fn read_overview_deny_fires_once_at_full_resets_reads_since_map_and_retry_passes() {
    let d = tempfile::tempdir().unwrap();
    seed_index(d.path());
    let envs = rail_envs(
        &FULL_UP,
        &[
            ("LENS_READ_OVERVIEW_DENY", "1"),
            ("LENS_READ_DENY_THRESHOLD", "0"),
        ],
    );
    let p = read_payload(d.path(), "rovrdeny1", "src/server.rs");

    // Reads 1-4: no overview deny yet.
    for i in 1..=4 {
        let (_, v) = run_hook("PreToolUse", &p, &envs, d.path());
        assert!(!is_deny(&v), "read {i} of 4 must not overview-deny: {v}");
    }
    // The 5th mapless read denies toward lens_overview.
    let (_, fifth) = run_hook("PreToolUse", &p, &envs, d.path());
    assert!(is_deny(&fifth), "the 5th mapless read must overview-deny: {fifth}");
    let reason = fifth["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(
        reason.contains("lens_overview") && reason.contains("re-run it verbatim"),
        "deny reason names lens_overview + the retry promise: {reason}"
    );

    // One-shot AND resets reads-since-map: the verbatim retry passes.
    let (_, retry) = run_hook("PreToolUse", &p, &envs, d.path());
    assert!(!is_deny(&retry), "verbatim retry must pass (reset + one-shot): {retry}");
}

#[test]
fn edit_links_deny_fires_once_per_symbol_at_full_and_never_touches_content() {
    let d = tempfile::tempdir().unwrap();
    seed_index(d.path());
    std::fs::write(
        d.path().join("graph.json"),
        two_qualifying_symbols_graph_json().to_string(),
    )
    .unwrap();
    let envs = rail_envs(&FULL_UP, &[("LENS_EDIT_LINKS_DENY", "1")]);

    // A decl-touching Edit of a 3-caller symbol denies toward lens_graph, once.
    let p = edit_payload(d.path(), "elinkdeny1", "fn alpha() {", "fn alpha(x: u32) {");
    let (_, first) = run_hook("PreToolUse", &p, &envs, d.path());
    assert!(
        is_deny(&first),
        "a decl-touching Edit of a multi-caller symbol must deny: {first}"
    );
    let reason = first["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(
        reason.contains("lens_graph(node=\"alpha\")") && reason.contains("3 callers"),
        "deny reason names the symbol and caller count: {reason}"
    );
    // A deny never carries updatedInput — Claude Code never applies the edit,
    // so the file content is untouched (the file-write-free contract).
    assert!(
        first["hookSpecificOutput"].get("updatedInput").is_none(),
        "a deny must never carry edit content: {first}"
    );

    // One-shot per symbol per session: the verbatim retry passes.
    let (_, second) = run_hook("PreToolUse", &p, &envs, d.path());
    assert!(!is_deny(&second), "retried Edit of the same symbol must pass: {second}");

    // A different symbol gets its own shot.
    let p2 = edit_payload(d.path(), "elinkdeny1", "fn gamma() {", "fn gamma(x: u32) {");
    let (_, third) = run_hook("PreToolUse", &p2, &envs, d.path());
    assert!(is_deny(&third), "a different symbol gets its own one-shot: {third}");
    let (_, fourth) = run_hook("PreToolUse", &p2, &envs, d.path());
    assert!(!is_deny(&fourth), "the second symbol's retry passes too: {fourth}");
}

#[test]
fn gast_deny_only_config_attributes_follower_to_live_not_shadow() {
    // Locks the live/shadow attribution: with the gast deny genuinely unset
    // (default-ON), its follower lands in gast_next_*, never in shadow.
    let d = tempfile::tempdir().unwrap();
    seed_index(d.path());
    let envs = rail_envs_leave_unset(&FULL_UP, &["LENS_GREP_AST_DENY"], &[]);
    let syn = grep_payload(d.path(), "gastattr1", "impl Forge");
    run_hook("PreToolUse", &syn, &envs, d.path());
    let bash = bash_payload(d.path(), "gastattr1", "echo hi");
    run_hook("PreToolUse", &bash, &envs, d.path());
    let store = lens::store::Store::open(d.path()).unwrap();
    assert_eq!(store.get_stat("gast_would_fire").unwrap(), 1);
    assert_eq!(
        store.get_stat("gast_next_bash").unwrap(),
        1,
        "deny-only config (deny default-ON, nudge=0) must attribute the follower to LIVE"
    );
    assert_eq!(
        store.get_stat("gast_shadow_next_bash").unwrap(),
        0,
        "deny-only config must never land the follower in shadow"
    );
}

#[test]
fn routing_garbage_value_behaves_as_full_fail_safe() {
    let d = tempfile::tempdir().unwrap();
    let envs = [("LENS_ROUTING", "gralbeurp"), ("LENS_ROUTING_MCP", "up")];
    let (_, v) = run_hook(
        "PreToolUse",
        &webfetch_payload(d.path(), "s1"),
        &envs,
        d.path(),
    );
    let hso = &v["hookSpecificOutput"];
    assert_eq!(
        hso["permissionDecision"], "deny",
        "a garbage LENS_ROUTING value must behave as full (fail-safe), denying WebFetch: {v}"
    );
}

#[test]
fn kill_switch_all_rail_flags_off_leaves_only_grep_first_and_escalation_denies() {
    let d = tempfile::tempdir().unwrap();
    seed_index(d.path());
    std::fs::write(
        d.path().join("graph.json"),
        resolving_graph_json("handle_connection").to_string(),
    )
    .unwrap();
    // Every rail flag explicitly OFF: a symbol-shaped Grep, a whole code-file
    // Read, a bounded Read, a shell grep, an aggregate Bash, and a
    // multi-caller Edit must all pass through with no rail deny at all.
    let envs = rail_envs(&FULL_UP, &[]);

    let grep = grep_payload(d.path(), "kill1", "fn handle_connection");
    let (_, gv) = run_hook("PreToolUse", &grep, &envs, d.path());
    assert!(!is_deny(&gv), "gsym must be silent under the kill-switch: {gv}");

    let read = read_payload(d.path(), "kill2", "src/widget.rs");
    let (_, rv) = run_hook("PreToolUse", &read, &envs, d.path());
    assert!(!is_deny(&rv), "rskel must be silent under the kill-switch: {rv}");

    let bounded = json!({
        "session_id": "kill2",
        "cwd": d.path().to_string_lossy(),
        "tool_name": "Read",
        "tool_input": { "file_path": "src/widget.rs", "limit": 40 },
    });
    let (_, bv_read) = run_hook("PreToolUse", &bounded, &envs, d.path());
    assert!(!is_deny(&bv_read), "runfile must be silent under the kill-switch: {bv_read}");

    let agg = bash_payload(d.path(), "kill3", "find . -name '*.rs' | wc -l");
    let (_, bv) = run_hook("PreToolUse", &agg, &envs, d.path());
    assert!(!is_deny(&bv), "bagg must be silent under the kill-switch: {bv}");

    // A prompt-armed broad shell grep: bash-grep off means no deny.
    let prompt = json!({
        "session_id": "kill3",
        "cwd": d.path().to_string_lossy(),
        "prompt": "tidy the fixtures",
    });
    run_hook("UserPromptSubmit", &prompt, &envs, d.path());
    let shell_grep = bash_payload(d.path(), "kill3", "grep -rn foo src/");
    let (_, sgv) = run_hook("PreToolUse", &shell_grep, &envs, d.path());
    assert!(!is_deny(&sgv), "bash-grep must be silent under the kill-switch: {sgv}");

    std::fs::write(d.path().join("graph.json"), callers_graph_json().to_string()).unwrap();
    let edit = edit_payload(d.path(), "kill4", "fn alpha() {", "fn alpha(x: u32) {");
    let (raw_e, _) = run_hook("PreToolUse", &edit, &envs, d.path());
    assert_eq!(raw_e, "{}", "elink must be a pure no-op under the kill-switch");

    // The always-on mechanisms outside the rail-flag matrix (grep-first deny,
    // read-escalation deny) are unaffected by the kill-switch.
    let sess = "kill5";
    let plain_read = read_payload(d.path(), sess, "src/other_file.rs");
    let mut fourth = Value::Null;
    for _ in 0..4 {
        let (_, v) = run_hook("PreToolUse", &plain_read, &envs, d.path());
        fourth = v;
    }
    assert!(
        is_deny(&fourth),
        "the read-escalation deny is unaffected by the rail kill-switches: {fourth}"
    );
}
