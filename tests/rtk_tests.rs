//! End-to-end tests for the lens⇄RTK integration, driving the REAL compiled
//! binary the way Claude Code / a user would. A stub `rtk` (a tiny shell script,
//! pinned via `$LENS_RTK_BIN`) stands in for the user's own install and answers
//! `--version` / `gain --format json` with canned output. lens does not install
//! RTK, so there is no download path to test.
//!
//! Covered: `rtk status` (against the stub) → `rtk sync` (one `rtk_shell` op whose
//! `tokens_saved_est` == Δ`total_saved`; idempotent on no-op) → `lens stats` & the
//! `/api/stats` aggregate both surface the RTK shell-savings plane → routing defers
//! Bash to RTK when active (and is unchanged when not). All additive: with no RTK
//! present everything is a no-op.

#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use lens::store::Store;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_lens")
}

/// `total_saved` the stub reports for `rtk gain --format json`.
const STUB_TOTAL_SAVED: i64 = 123_456;
const STUB_COMMANDS: i64 = 42;

/// Write an executable stub `rtk` at `<home>/bin/rtk` that emulates the subset of
/// the RTK CLI lens shells out to. `total_saved` lets a test grow RTK's
/// cumulative figure between syncs. Point `$LENS_RTK_BIN` at the written path.
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
fn rtk_e2e_status_sync_stats_against_stub() {
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let settings = home.path().join("settings.json");
    write_stub_rtk(home.path(), STUB_TOTAL_SAVED, STUB_COMMANDS);

    let home_s = home.path().to_str().unwrap();
    let data_s = data.path().to_str().unwrap();
    let settings_s = settings.to_str().unwrap();
    let stub = home.path().join("bin").join("rtk");
    let stub_s = stub.to_str().unwrap();
    // LENS_RTK_BIN pins resolution at the stub, so the host's own rtk (if any)
    // can't answer instead. LENS_CLAUDE_SETTINGS is the settings file lens reads
    // when detecting whether rtk's hook is registered.
    let base = [
        ("HOME", home_s),
        ("LENS_HOME", home_s),
        ("LENS_RTK_BIN", stub_s),
        ("LENS_CLAUDE_SETTINGS", settings_s),
    ];
    let with_data = [
        ("HOME", home_s),
        ("LENS_HOME", home_s),
        ("LENS_RTK_BIN", stub_s),
        ("LENS_CLAUDE_SETTINGS", settings_s),
        ("LENS_DIR", data_s),
    ];

    // status: reports the detected binary + version. The hook is rtk's to register,
    // so with an unpatched settings file it reads "not registered".
    let (ok, out, err) = run(&["rtk", "status"], &base);
    let s = format!("{out}{err}");
    assert!(ok, "rtk status must succeed: {s}");
    assert!(s.contains("0.28.2"), "status shows the version: {s}");
    assert!(
        s.contains("not registered"),
        "status reports hook registration state: {s}"
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
    // The rtk block resolves in-process here, so pin the stub tightly.
    std::env::set_var("LENS_HOME", home_s);
    std::env::set_var("LENS_RTK_BIN", stub_s);
    let snap = lens::obs::stats::snapshot_json(data.path(), None);
    std::env::remove_var("LENS_RTK_BIN");
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

    // lens owns no rtk lifecycle commands: install/uninstall are rtk's own.
    for sub in ["install", "uninstall"] {
        let (ok, out, err) = run(&["rtk", sub], &base);
        assert!(!ok, "`lens rtk {sub}` must not exist: {out}{err}");
        assert!(
            format!("{out}{err}").contains("unknown subcommand"),
            "`lens rtk {sub}` should report an unknown subcommand: {out}{err}"
        );
    }
    assert!(
        !settings.exists(),
        "lens must not have written the settings file: rtk owns its own hook"
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
// H4 (plan T2): Bash would-fire counters split on rtk_active.
// ---------------------------------------------------------------------------

/// Seed a populated `index.db` in `data_dir` so `routing::index_present`
/// returns true — the in-crate `seed_index` fixture is `#[cfg(test)]` and
/// unreachable from this integration crate (mirrors `routing_tests.rs`'s
/// copy of the same fixture).
fn seed_index(data_dir: &Path) {
    let conn = rusqlite::Connection::open(data_dir.join("index.db")).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS file_manifest(path TEXT PRIMARY KEY, mtime INTEGER NOT NULL);
         INSERT OR REPLACE INTO file_manifest(path, mtime) VALUES ('src/f0.rs', 123);",
    )
    .unwrap();
}

#[test]
fn rtk_active_splits_bagg_would_fire_denominator() {
    let d = tempfile::tempdir().unwrap();
    seed_index(d.path());
    let bagg_bash = json!({
        "session_id": "s1", "cwd": d.path().to_string_lossy(),
        "tool_name": "Bash", "tool_input": { "command": "wc -l src/f0.rs" }
    });

    // rtk active: the plain `bagg_would_fire` key must NOT bump; the split
    // `bagg_would_fire_rtk_deferred` key must.
    let active = [
        ("LENS_ROUTING", "full"),
        ("LENS_ROUTING_MCP", "up"),
        ("LENS_DEFER_BASH_TO_RTK", "1"),
    ];
    run_pretooluse(&bagg_bash, &active, d.path());

    let store = Store::open(d.path()).unwrap();
    assert_eq!(
        store.get_stat("bagg_would_fire").unwrap(),
        0,
        "rtk active must not bump the plain bagg_would_fire key"
    );
    assert_eq!(
        store.get_stat("bagg_would_fire_rtk_deferred").unwrap(),
        1,
        "rtk active bumps the split bagg_would_fire_rtk_deferred key"
    );

    // rtk inactive: the plain key bumps as before; the split key stays put.
    let inactive = [
        ("LENS_ROUTING", "full"),
        ("LENS_ROUTING_MCP", "up"),
        ("LENS_DEFER_BASH_TO_RTK", "0"),
    ];
    run_pretooluse(&bagg_bash, &inactive, d.path());
    assert_eq!(
        store.get_stat("bagg_would_fire").unwrap(),
        1,
        "rtk inactive bumps the plain bagg_would_fire key"
    );
    assert_eq!(
        store.get_stat("bagg_would_fire_rtk_deferred").unwrap(),
        1,
        "rtk inactive must not bump the rtk-deferred key further"
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

    // status reports the absence without erroring.
    let (ok, out, err) = run(&["rtk", "status"], &envs);
    assert!(ok, "status must not error when RTK is absent");
    assert!(
        format!("{out}{err}").contains("not on PATH"),
        "status says rtk is not on PATH: {out}{err}"
    );
}
