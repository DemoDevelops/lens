//! End-to-end tests for the lens⇄RTK integration (plan §4 T5), driving the
//! REAL compiled binary the way Claude Code / a user would — but **network-free**:
//! a stub `rtk` (a tiny shell script on the managed `LENS_HOME/bin` path)
//! stands in for the downloaded binary and answers `--version` / `gain --format
//! json` / `init` with canned output. The real download path is verified
//! on-machine only (T1), never here.
//!
//! Covered: `rtk install`/`status` (idempotent, against the stub) → `rtk sync`
//! (one `rtk_shell` op whose `tokens_saved_est` == Δ`total_saved`; idempotent on
//! no-op) → `lens stats` & the `/api/stats` aggregate both surface the RTK
//! shell-savings plane → routing defers Bash to RTK when active (and is unchanged
//! when not). All additive: with no RTK present everything is a no-op.

#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_lens")
}

/// `total_saved` the stub reports for `rtk gain --format json`.
const STUB_TOTAL_SAVED: i64 = 123_456;
const STUB_COMMANDS: i64 = 42;

/// Write an executable stub `rtk` at `<home>/bin/rtk` that emulates the subset of
/// the RTK CLI lens shells out to. `total_saved` lets a test grow RTK's
/// cumulative figure between syncs.
fn write_stub_rtk(home: &Path, total_saved: i64, commands: i64) {
    let bindir = home.join("bin");
    std::fs::create_dir_all(&bindir).unwrap();
    let script = format!(
        "#!/bin/sh\n\
case \"$1\" in\n\
  --version) echo 'rtk 0.28.2' ;;\n\
  gain) printf '%s' '{{\"summary\":{{\"total_commands\":{commands},\"total_input\":1000000,\"total_output\":400000,\"total_saved\":{total_saved},\"avg_savings_pct\":61.5,\"total_time_ms\":50000,\"avg_time_ms\":119}}}}' ;;\n\
  init) exit 0 ;;\n\
  *) exit 0 ;;\n\
esac\n"
    );
    let path = bindir.join("rtk");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Run `lens <args…>` with `envs` applied, return (success, stdout, stderr).
fn run(args: &[&str], envs: &[(&str, &str)]) -> (bool, String, String) {
    let mut cmd = Command::new(bin());
    cmd.args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn lens");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// Run `lens hook claude PreToolUse` with `payload` on stdin; return trimmed stdout.
fn run_pretooluse(payload: &Value, envs: &[(&str, &str)], data_dir: &Path) -> String {
    let mut cmd = Command::new(bin());
    cmd.args(["hook", "claude", "PreToolUse"])
        .env("LENS_DIR", data_dir)
        .env_remove("LENS_ROUTING")
        .env_remove("LENS_ROUTING_MCP")
        .env_remove("LENS_DEFER_BASH_TO_RTK")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn hook");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("hook output");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn rtk_shell_lines(ops_log: &Path) -> Vec<Value> {
    let raw = std::fs::read_to_string(ops_log).unwrap_or_default();
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|r| r["tool"] == "rtk_shell")
        .collect()
}

// ---------------------------------------------------------------------------
// The full lifecycle against a stub rtk (one test → no intra-file env races).
// ---------------------------------------------------------------------------

#[test]
fn rtk_e2e_install_status_sync_stats_against_stub() {
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let settings = home.path().join("settings.json");
    write_stub_rtk(home.path(), STUB_TOTAL_SAVED, STUB_COMMANDS);

    let home_s = home.path().to_str().unwrap();
    let data_s = data.path().to_str().unwrap();
    let settings_s = settings.to_str().unwrap();
    // HOME scopes rtk's default hook-script dir into the tempdir, so registration is
    // hermetic: the stub's `init` writes no script and lens registers the entry
    // by path. LENS_CLAUDE_SETTINGS is the settings file lens patches itself.
    let base = [
        ("HOME", home_s),
        ("LENS_HOME", home_s),
        ("LENS_CLAUDE_SETTINGS", settings_s),
    ];
    let with_data = [
        ("HOME", home_s),
        ("LENS_HOME", home_s),
        ("LENS_CLAUDE_SETTINGS", settings_s),
        ("LENS_DIR", data_s),
    ];

    // install: the stub is already at LENS_HOME/bin/rtk, so install takes the
    // idempotent path (verifies --version, registers the hook) WITHOUT a download.
    let (ok, out, err) = run(&["rtk", "install"], &base);
    assert!(
        ok,
        "rtk install (idempotent, stub) must succeed: {out}{err}"
    );

    // install patches the config-dir settings.json itself (rtk init can't target
    // $CLAUDE_CONFIG_DIR): the PreToolUse entry now references rtk-rewrite.sh.
    let written = std::fs::read_to_string(&settings).unwrap_or_default();
    assert!(
        written.contains("rtk-rewrite.sh") && written.contains("PreToolUse"),
        "install must register the RTK hook in the settings file: {written}"
    );

    // status: reports installed + version + hook-registered.
    let (ok, out, err) = run(&["rtk", "status"], &base);
    let s = format!("{out}{err}");
    assert!(ok, "rtk status must succeed: {s}");
    assert!(s.contains("0.28.2"), "status shows the version: {s}");
    assert!(
        s.to_lowercase().contains("regist"),
        "status reports hook registration: {s}"
    );

    // sync #1: one rtk_shell op whose tokens_saved_est == Δtotal_saved (full, since
    // the watermark starts at zero).
    let (ok, out, err) = run(&["rtk", "sync"], &with_data);
    assert!(ok, "first rtk sync must succeed: {out}{err}");
    let lines = rtk_shell_lines(&data.path().join("ops.log"));
    assert_eq!(lines.len(), 1, "first sync writes exactly one rtk_shell op");
    assert_eq!(
        lines[0]["tokens_saved_est"].as_i64().unwrap(),
        STUB_TOTAL_SAVED,
        "tokens_saved_est == Δrtk gain total_saved"
    );
    // RTK measures tokens, not bytes — the byte planes stay clean.
    assert_eq!(lines[0]["raw_bytes_in"].as_i64().unwrap(), 0);
    assert_eq!(lines[0]["bytes_returned"].as_i64().unwrap(), 0);

    // sync #2: no new rtk activity (same stub output) ⇒ watermark holds, no new op.
    let (ok, _, _) = run(&["rtk", "sync"], &with_data);
    assert!(ok, "second rtk sync must succeed");
    assert_eq!(
        rtk_shell_lines(&data.path().join("ops.log")).len(),
        1,
        "idempotent: no new savings recorded when total_saved is unchanged"
    );

    // lens stats: surfaces the RTK shell-savings plane + the synced op.
    let (ok, out, _) = run(&["stats"], &with_data);
    assert!(ok);
    assert!(
        out.contains("rtk_shell"),
        "stats lists the rtk_shell op:\n{out}"
    );
    assert!(
        out.contains("RTK shell savings"),
        "stats renders the RTK plane:\n{out}"
    );
    assert!(
        out.contains(&STUB_TOTAL_SAVED.to_string()),
        "stats shows RTK's measured total_saved:\n{out}"
    );

    // /api/stats aggregate (what the dashboard serves) carries the rtk block sourced
    // from `rtk gain`, plus rtk_shell under by_tool and "shell" under by_mechanism.
    // LENS_HOME is needed in-process here for the rtk block; set tightly.
    std::env::set_var("LENS_HOME", home_s);
    let snap = lens::obs::stats::snapshot_json(data.path(), None);
    std::env::remove_var("LENS_HOME");
    assert_eq!(
        snap["rtk"]["installed"],
        json!(true),
        "rtk block installed:true"
    );
    assert_eq!(
        snap["rtk"]["total_saved"].as_i64().unwrap(),
        STUB_TOTAL_SAVED,
        "rtk block shows RTK's own total_saved (not a lens re-estimate)"
    );
    assert!(
        snap["by_tool"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["tool"] == "rtk_shell"),
        "by_tool includes rtk_shell"
    );
    assert!(
        snap["by_mechanism"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["mechanism"] == "shell"),
        "by_mechanism buckets rtk_shell under 'shell'"
    );

    // uninstall: removes lens's hook entry from the config-dir settings.json.
    let (ok, _, _) = run(&["rtk", "uninstall"], &base);
    assert!(ok, "rtk uninstall must succeed");
    let after = std::fs::read_to_string(&settings).unwrap_or_default();
    assert!(
        !after.contains("rtk-rewrite.sh"),
        "uninstall must remove the RTK hook entry: {after}"
    );
}

// ---------------------------------------------------------------------------
// Routing coexistence: RTK owns Bash when active; lens unchanged when not.
// (Child-process only — no in-process env mutation, so race-free.)
// ---------------------------------------------------------------------------

#[test]
fn routing_defers_bash_to_rtk_only_when_active() {
    let d = tempfile::tempdir().unwrap();
    let bash = json!({
        "session_id": "s1", "cwd": d.path().to_string_lossy(),
        "tool_name": "Bash", "tool_input": { "command": "find . -type f" }
    });
    let webfetch = json!({
        "session_id": "s1", "cwd": d.path().to_string_lossy(),
        "tool_name": "WebFetch", "tool_input": { "url": "https://example.com/big" }
    });

    // RTK active (forced via env): Bash passes through (RTK owns it), WebFetch still denies.
    let active = [
        ("LENS_ROUTING", "full"),
        ("LENS_ROUTING_MCP", "up"),
        ("LENS_DEFER_BASH_TO_RTK", "1"),
    ];
    assert_eq!(
        run_pretooluse(&bash, &active, d.path()),
        "{}",
        "RTK active ⇒ lens defers Bash (passthrough)"
    );
    let wf: Value = serde_json::from_str(&run_pretooluse(&webfetch, &active, d.path())).unwrap();
    assert_eq!(
        wf["hookSpecificOutput"]["permissionDecision"], "deny",
        "WebFetch still denies when RTK active (only Bash defers)"
    );

    // RTK inactive: prior behavior — wrappable Bash is rewritten to `lens wrap`.
    let inactive = [
        ("LENS_ROUTING", "full"),
        ("LENS_ROUTING_MCP", "up"),
        ("LENS_DEFER_BASH_TO_RTK", "0"),
    ];
    let b: Value = serde_json::from_str(&run_pretooluse(&bash, &inactive, d.path())).unwrap();
    assert_eq!(b["hookSpecificOutput"]["permissionDecision"], "allow");
    assert!(
        b["hookSpecificOutput"]["updatedInput"]["command"]
            .as_str()
            .unwrap()
            .contains("wrap -- "),
        "RTK inactive ⇒ Bash wrap behaves exactly as before"
    );
}

// ---------------------------------------------------------------------------
// Default-off / additive: absent RTK, every new surface is a clean no-op.
// ---------------------------------------------------------------------------

#[test]
fn rtk_absent_is_a_noop() {
    let home = tempfile::tempdir().unwrap(); // empty: no bin/rtk
    let data = tempfile::tempdir().unwrap();
    let envs = [
        ("LENS_HOME", home.path().to_str().unwrap()),
        ("LENS_DIR", data.path().to_str().unwrap()),
        // Minimal PATH (sh available, no rtk) so "absent" is hermetic regardless of
        // whatever rtk the host happens to have on PATH.
        ("PATH", "/usr/bin:/bin"),
    ];

    // sync is a no-op (no rtk) and must not create an rtk_shell op.
    let (ok, _, _) = run(&["rtk", "sync"], &envs);
    assert!(ok, "rtk sync with no RTK installed must succeed as a no-op");
    assert!(
        rtk_shell_lines(&data.path().join("ops.log")).is_empty(),
        "no rtk_shell op when RTK is absent"
    );

    // status reports "not installed" without erroring.
    let (ok, out, err) = run(&["rtk", "status"], &envs);
    assert!(ok, "status must not error when RTK is absent");
    assert!(
        format!("{out}{err}")
            .to_lowercase()
            .contains("not installed"),
        "status says not installed: {out}{err}"
    );
}
