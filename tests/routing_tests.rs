//! Integration tests for the CTXFORGE_ROUTING layer (plan §4).
//!
//! These drive the REAL compiled binary the way Claude Code does:
//!   * `ctxforge hook claude PreToolUse/SessionStart` over stdin → assert the
//!     exact hook JSON per routing level (and that `=off` is a byte-identical
//!     no-op — the safety contract);
//!   * `ctxforge wrap` → offload large stdout, then `verify --roundtrip` (PASS)
//!     and `stats` (the op surfaces with real savings);
//!   * `ctx_execute_file` end-to-end through the rmcp client.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ctxforge")
}

/// Run `ctxforge hook claude <event>` with `payload` on stdin under a clean,
/// explicit routing env. Returns (trimmed stdout, parsed JSON or Null).
fn run_hook(
    event: &str,
    payload: &Value,
    envs: &[(&str, &str)],
    data_dir: &Path,
) -> (String, Value) {
    let mut cmd = Command::new(bin());
    cmd.args(["hook", "claude", event])
        .env("CTXFORGE_DIR", data_dir)
        // Determinism: never inherit routing env from the test runner.
        .env_remove("CTXFORGE_ROUTING")
        .env_remove("CTXFORGE_ROUTING_MCP")
        // RTK coexistence (plan T4): force the defer-Bash-to-RTK gate OFF so these
        // Bash-wrap assertions are deterministic regardless of whether RTK happens
        // to be installed + hooked on the host machine.
        .env("CTXFORGE_DEFER_BASH_TO_RTK", "0")
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
// §4: CTXFORGE_ROUTING=off ⇒ PreToolUse output identical to today (empty)
// ---------------------------------------------------------------------------

#[test]
fn off_is_a_true_noop() {
    let d = tempfile::tempdir().unwrap();
    // Explicit `off` AND unset (the default) must both be byte-identical "{}".
    for envs in [vec![("CTXFORGE_ROUTING", "off")], vec![]] {
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
}

// ---------------------------------------------------------------------------
// §2/§4: WebFetch → deny + steer (only when steering)
// ---------------------------------------------------------------------------

#[test]
fn webfetch_denies_when_steering() {
    let d = tempfile::tempdir().unwrap();
    for lvl in ["steer", "full"] {
        let (_, v) = run_hook(
            "PreToolUse",
            &webfetch_payload(d.path(), "s1"),
            &[("CTXFORGE_ROUTING", lvl), ("CTXFORGE_ROUTING_MCP", "up")],
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
                .contains("ctx_execute"),
            "deny reason steers to the sandbox"
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
        &[("CTXFORGE_ROUTING", "wrap"), ("CTXFORGE_ROUTING_MCP", "up")],
        d.path(),
    );
    assert_eq!(raw, "{}");
}

// ---------------------------------------------------------------------------
// §2/§4: wrappable Bash → transparent wrap; stateful chains pass through
// ---------------------------------------------------------------------------

#[test]
fn wrappable_bash_rewrites_to_ctxforge_wrap_at_full() {
    let d = tempfile::tempdir().unwrap();
    let (_, v) = run_hook(
        "PreToolUse",
        &bash_payload(d.path(), "s1", "find . -type f"),
        &[("CTXFORGE_ROUTING", "full"), ("CTXFORGE_ROUTING_MCP", "up")],
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
    assert!(
        cmd.contains("ctxforge"),
        "invokes the ctxforge binary: {cmd}"
    );
}

#[test]
fn git_subcommand_awareness() {
    let d = tempfile::tempdir().unwrap();
    let envs = [("CTXFORGE_ROUTING", "full"), ("CTXFORGE_ROUTING_MCP", "up")];
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
        &[("CTXFORGE_ROUTING", "full"), ("CTXFORGE_ROUTING_MCP", "up")],
        d.path(),
    );
    assert_eq!(raw, "{}", "cd-chain must pass through unchanged");
}

// ---------------------------------------------------------------------------
// §2/§4: nudges are throttled to once per session
// ---------------------------------------------------------------------------

#[test]
fn bash_nudge_fires_once_then_passthrough_at_steer() {
    let d = tempfile::tempdir().unwrap();
    let envs = [
        ("CTXFORGE_ROUTING", "steer"),
        ("CTXFORGE_ROUTING_MCP", "up"),
    ];
    let p = bash_payload(d.path(), "s1", "find . -type f");
    let (_, v1) = run_hook("PreToolUse", &p, &envs, d.path());
    assert!(
        v1["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "first wrappable Bash at steer should emit a nudge"
    );
    let (raw2, _) = run_hook("PreToolUse", &p, &envs, d.path());
    assert_eq!(raw2, "{}", "nudge throttled to once per session");
}

// ---------------------------------------------------------------------------
// §2/§4: MCP-ready guard (port of context-mode mcpRedirect #230) — server down
// ⇒ only MCP-redirect decisions passthrough; wrap/nudges still fire.
// ---------------------------------------------------------------------------

#[test]
fn mcp_down_gates_redirects_not_wrap() {
    let d = tempfile::tempdir().unwrap();
    let envs = [
        ("CTXFORGE_ROUTING", "full"),
        ("CTXFORGE_ROUTING_MCP", "down"),
    ];
    // WebFetch deny is an MCP redirect → suppressed to passthrough when down.
    let (raw_wf, _) = run_hook(
        "PreToolUse",
        &webfetch_payload(d.path(), "s1"),
        &envs,
        d.path(),
    );
    assert_eq!(raw_wf, "{}", "server down → WebFetch deny suppressed");
    // curl→ctx_execute is an MCP redirect → suppressed to passthrough when down.
    let (raw_curl, _) = run_hook(
        "PreToolUse",
        &bash_payload(d.path(), "s1", "curl https://api.example.com/data"),
        &envs,
        d.path(),
    );
    assert_eq!(raw_curl, "{}", "server down → curl redirect suppressed");
    // Wrap shells the ctxforge CLI (not the MCP server) → still fires when down.
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
        &[("CTXFORGE_ROUTING", "full"), ("CTXFORGE_ROUTING_MCP", "up")],
        d.path(),
    );
    let ctx = v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(
        ctx.contains("context_window_protection"),
        "routing block present: {ctx}"
    );
    assert!(
        ctx.contains("ctx_execute"),
        "tool hierarchy names ctx_execute"
    );
    assert!(
        ctx.contains("ctx_search"),
        "tool hierarchy names ctx_search"
    );
    assert!(
        ctx.contains("graph_query"),
        "tool hierarchy names the graph"
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
        &[("CTXFORGE_ROUTING", "off")],
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
// §4: `ctxforge wrap` — large stdout offloads losslessly; verify + stats
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
        .env("CTXFORGE_DIR", d.path())
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
        .env("CTXFORGE_DIR", d.path())
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
        .env("CTXFORGE_DIR", d.path())
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

    // `ctxforge stats` surfaces the wrapped op on the dashboard plane.
    let s = Command::new(bin())
        .args(["stats"])
        .env("CTXFORGE_DIR", d.path())
        .output()
        .expect("stats");
    let st = String::from_utf8_lossy(&s.stdout);
    assert!(st.contains("bash_wrap"), "stats lists the wrap op:\n{st}");

    // ...and the exact aggregate the web dashboard serves at /api/stats lists it.
    let snap = ctxforge::obs::stats::snapshot_json(d.path(), None);
    let by_tool = snap["by_tool"].as_array().unwrap();
    assert!(
        by_tool.iter().any(|t| t["tool"] == "bash_wrap"),
        "dashboard /api/stats by_tool must include bash_wrap"
    );
}

// ---------------------------------------------------------------------------
// T3 §: ctx_execute_file end-to-end via the rmcp client
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ctx_execute_file_e2e() {
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
            .env("CTXFORGE_DIR", &data_path)
            .env("CTXFORGE_MAX_INLINE", "8192");
    }))
    .unwrap();
    let client = ().serve(transport).await.expect("handshake");

    // The new tool is advertised.
    let tools = client.list_tools(Default::default()).await.unwrap();
    assert!(
        tools
            .tools
            .iter()
            .any(|t| t.name.as_ref() == "ctx_execute_file"),
        "ctx_execute_file must be advertised"
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
        "ctx_execute_file",
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
        "ctx_execute_file",
        json!({
            "path": "data.txt",
            "language": "python",
            "code": "import sys; _ = open(sys.argv[1]).read(); print('A'*50000)",
        }),
    )
    .await;
    assert_eq!(big["truncated"], json!(true));
    let r2 = big["retrieve_ref"].as_str().unwrap().to_string();
    let recovered = call("ctx_retrieve", json!({ "ref": r2 })).await;
    assert!(recovered["content"]
        .as_str()
        .unwrap()
        .contains(&"A".repeat(50000)));

    client.cancel().await.ok();
}
