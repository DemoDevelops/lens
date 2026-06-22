//! End-to-end session continuity: drive the real `ctxforge hook …` and
//! `ctxforge session …` subcommands over stdin/stdout, exercising the actual
//! Claude Code hook contract (not just the library internals).

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

/// Run `ctxforge <args…>` with `stdin_json` on stdin and `CTXFORGE_DIR` set to
/// `data`. Returns (stdout, exit_ok).
fn run(
    args: &[&str],
    data: &std::path::Path,
    stdin_json: &Value,
    extra_env: &[(&str, &str)],
) -> (String, bool) {
    let bin = env!("CARGO_BIN_EXE_ctxforge");
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .env("CTXFORGE_DIR", data)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn ctxforge");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin_json.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        out.status.success(),
    )
}

fn payload(project: &std::path::Path) -> Value {
    json!({ "session_id": "e2e-sess", "cwd": project.to_string_lossy() })
}

#[test]
fn hook_io_contract_per_event() {
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    // PostToolUse(Edit) → "{}" and stores a file event.
    let mut p = payload(project.path());
    p["tool_name"] = json!("Edit");
    p["tool_input"] = json!({ "file_path": "src/widget.rs" });
    p["tool_response"] = json!("ok");
    let (out, ok) = run(&["hook", "claude", "PostToolUse"], data.path(), &p, &[]);
    assert!(ok);
    assert_eq!(out.trim(), "{}");

    // UserPromptSubmit with a correction.
    let mut up = payload(project.path());
    up["prompt"] = json!("Use ripgrep instead of grep for the search step");
    let (out, ok) = run(
        &["hook", "claude", "UserPromptSubmit"],
        data.path(),
        &up,
        &[],
    );
    assert!(ok);
    assert_eq!(out.trim(), "{}");

    // Bash error → stored as unresolved error.
    let mut b = payload(project.path());
    b["tool_name"] = json!("Bash");
    b["tool_input"] = json!({ "command": "cargo build" });
    b["tool_response"] = json!("error[E0432]: unresolved import `foo`");
    run(&["hook", "claude", "PostToolUse"], data.path(), &b, &[]);

    // PreCompact → "{}", builds the snapshot.
    let (out, ok) = run(
        &["hook", "claude", "PreCompact"],
        data.path(),
        &payload(project.path()),
        &[],
    );
    assert!(ok);
    assert_eq!(out.trim(), "{}");

    // SessionStart(compact) → injects the guide via hookSpecificOutput.
    let mut ss = payload(project.path());
    ss["source"] = json!("compact");
    let (out, ok) = run(&["hook", "claude", "SessionStart"], data.path(), &ss, &[]);
    assert!(ok);
    let v: Value = serde_json::from_str(out.trim()).expect("valid JSON");
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");
    let ctx = v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();

    // Working state reconstructed: file, decision, unresolved error, last prompt.
    assert!(ctx.contains("Session Guide"), "guide header missing: {ctx}");
    assert!(ctx.contains("src/widget.rs"), "edited file missing");
    assert!(ctx.contains("ripgrep"), "user decision missing");
    assert!(
        ctx.contains("UNRESOLVED") && ctx.contains("E0432"),
        "unresolved error missing"
    );
    assert!(ctx.contains("Use ripgrep instead"), "last prompt missing");
}

#[test]
fn session_install_conflict_and_uninstall() {
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let settings = project.path().join("settings.json");
    let settings_str = settings.to_string_lossy().to_string();

    // Clean install succeeds and registers all five events.
    let (out, ok) = run(
        &["session", "install"],
        data.path(),
        &json!({}),
        &[("CTXFORGE_SETTINGS", &settings_str)],
    );
    assert!(ok, "install should succeed: {out}");
    let root: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    for ev in [
        "PreToolUse",
        "PostToolUse",
        "UserPromptSubmit",
        "PreCompact",
        "SessionStart",
    ] {
        assert!(root["hooks"].get(ev).is_some(), "missing {ev}");
    }

    // Conflict guard: with context-mode enabled, install refuses (non-zero).
    let conflict = project.path().join("conflict.json");
    std::fs::write(
        &conflict,
        serde_json::to_string(&json!({ "enabledPlugins": { "context-mode@context-mode": true } }))
            .unwrap(),
    )
    .unwrap();
    let (out, ok) = run(
        &["session", "install"],
        data.path(),
        &json!({}),
        &[("CTXFORGE_SETTINGS", &conflict.to_string_lossy())],
    );
    assert!(!ok, "install must refuse on conflict");
    let _ = out;

    // Uninstall cleanly removes ctxforge entries.
    let (_out, ok) = run(
        &["session", "uninstall"],
        data.path(),
        &json!({}),
        &[("CTXFORGE_SETTINGS", &settings_str)],
    );
    assert!(ok);
    let root: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    let hooks = root["hooks"].as_object().cloned().unwrap_or_default();
    assert!(hooks.is_empty(), "all ctxforge hooks should be removed");
}

#[test]
fn fresh_session_clears_then_resume_rehydrates() {
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    // Seed + compact under session A.
    let mut p = json!({ "session_id": "A", "cwd": project.path().to_string_lossy() });
    p["tool_name"] = json!("Write");
    p["tool_input"] = json!({ "file_path": "main.rs" });
    p["tool_response"] = json!("ok");
    run(&["hook", "claude", "PostToolUse"], data.path(), &p, &[]);
    run(
        &["hook", "claude", "PreCompact"],
        data.path(),
        &json!({ "session_id": "A", "cwd": project.path().to_string_lossy() }),
        &[],
    );

    // Fresh startup under a *new* session B clears prior live events.
    let mut start = json!({ "session_id": "B", "cwd": project.path().to_string_lossy() });
    start["source"] = json!("startup");
    run(
        &["hook", "claude", "SessionStart"],
        data.path(),
        &start,
        &[],
    );

    // /resume under fresh session C falls back to A's stored snapshot.
    let mut resume = json!({ "session_id": "C", "cwd": project.path().to_string_lossy() });
    resume["source"] = json!("resume");
    let (out, _ok) = run(
        &["hook", "claude", "SessionStart"],
        data.path(),
        &resume,
        &[],
    );
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    let ctx = v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(
        ctx.contains("main.rs"),
        "resume should rehydrate prior snapshot: {ctx}"
    );
}
