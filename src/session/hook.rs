//! `ctxforge hook <platform> <event>` — the active lifecycle entrypoint.
//!
//! Claude Code invokes this on PreToolUse / PostToolUse / UserPromptSubmit /
//! PreCompact / SessionStart, passing a JSON payload on stdin. We read it, do
//! the per-event work against the session store, and write the required
//! response on stdout (the hook response channel). All logging goes to stderr,
//! and every error is swallowed so a hook can never block the session.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};

use super::{extract, snapshot, store::SessionStore, Event, RawEvent};
use crate::index::Index;
use crate::routing;

/// Parsed subset of the Claude Code hook stdin payload.
#[derive(Debug, Default, Deserialize)]
struct HookInput {
    session_id: Option<String>,
    transcript_path: Option<String>,
    cwd: Option<String>,
    source: Option<String>,
    #[allow(dead_code)]
    trigger: Option<String>,
    prompt: Option<String>,
    message: Option<String>,
    tool_name: Option<String>,
    tool_input: Option<Value>,
    tool_response: Option<Value>,
}

impl HookInput {
    fn session_id(&self) -> String {
        if let Some(tp) = &self.transcript_path {
            if let Some(stem) = Path::new(tp).file_stem().and_then(|s| s.to_str()) {
                if !stem.is_empty() {
                    return stem.to_string();
                }
            }
        }
        if let Some(sid) = &self.session_id {
            if !sid.is_empty() {
                return sid.clone();
            }
        }
        format!("pid-{}", std::process::id())
    }

    fn project(&self) -> PathBuf {
        if let Some(c) = &self.cwd {
            if !c.is_empty() {
                return PathBuf::from(c);
            }
        }
        if let Some(c) = std::env::var_os("CLAUDE_PROJECT_DIR") {
            return PathBuf::from(c);
        }
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    }

    fn tool_response_str(&self) -> String {
        match &self.tool_response {
            Some(Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
            None => String::new(),
        }
    }
}

/// CLI entry: `args` is everything after `hook` (i.e. `[platform, event]`).
/// Always exits 0 and prints a valid hook response, even on malformed input.
pub fn run_cli(args: &[String]) -> anyhow::Result<()> {
    // args[0] = platform (e.g. "claude"), args[1] = event name.
    let event = args.get(1).cloned().unwrap_or_default();

    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let input: HookInput = serde_json::from_str(&raw).unwrap_or_default();

    let stdout = handle(&event, &input).unwrap_or_else(|e| {
        eprintln!("ctxforge hook {event}: {e}");
        default_response(&event)
    });
    println!("{stdout}");
    Ok(())
}

/// Route a single event. Returns the stdout JSON string per the contract.
fn handle(event: &str, input: &HookInput) -> anyhow::Result<String> {
    let project = input.project();
    let project_str = project.to_string_lossy().to_string();
    let session_id = input.session_id();
    let data_dir = super::resolve_data_dir(&project);
    let store = SessionStore::open(&data_dir)?;
    let ts = super::now_ts();

    match event {
        "PreToolUse" => {
            // Routing is gated by CTXFORGE_ROUTING; `off` (the default) is a
            // true no-op that returns `{}` without touching the store.
            let level = routing::Level::from_env();
            if level == routing::Level::Off {
                return Ok("{}".to_string());
            }
            let tool = input.tool_name.clone().unwrap_or_default();
            let ti = input.tool_input.clone().unwrap_or(json!({}));
            let bin = std::env::current_exe()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "ctxforge".to_string());
            let rc = routing::RouteCtx {
                level,
                mcp_ready: routing::mcp_ready(&data_dir),
                bin: &bin,
                data_dir: &data_dir,
                session_id: &session_id,
            };
            let decision = routing::route(&tool, &ti, &rc);
            Ok(routing::to_hook_json(&decision).to_string())
        }
        "PostToolUse" => {
            store.ensure_session(&session_id, &project_str, ts)?;
            let tool = input.tool_name.clone().unwrap_or_default();
            let ti = input.tool_input.clone().unwrap_or(json!({}));
            let resp = input.tool_response_str();
            let raws = extract::extract_tool_events(&tool, &ti, &resp);
            let events = attribute(raws, &session_id, &project_str, ts, "PostToolUse");
            store.insert_events(&events)?;
            Ok("{}".to_string())
        }
        "UserPromptSubmit" => {
            let prompt = input
                .prompt
                .clone()
                .or_else(|| input.message.clone())
                .unwrap_or_default();
            if !is_system_message(&prompt) && !prompt.trim().is_empty() {
                store.ensure_session(&session_id, &project_str, ts)?;
                let raws = extract::extract_user_events(&prompt);
                let events = attribute(raws, &session_id, &project_str, ts, "UserPromptSubmit");
                store.insert_events(&events)?;
            }
            Ok("{}".to_string())
        }
        "PreCompact" => {
            let events = store.events_for_session(&session_id)?;
            if !events.is_empty() {
                let compacts = store.compact_count(&session_id)? + 1;
                let snap = snapshot::build_snapshot(&events, super::snapshot_budget(), compacts);
                store.upsert_resume(&session_id, &project_str, &snap, events.len() as i64, ts)?;
                store.increment_compact_count(&session_id)?;
            }
            Ok("{}".to_string())
        }
        "SessionStart" => {
            let source = input.source.clone().unwrap_or_else(|| "startup".into());
            let ctx = session_start(&store, &data_dir, &session_id, &project, &project_str, ts, &source)?;
            // Prepend the routing tool-selection guide when steering is active
            // and the MCP server is reachable; otherwise leave `ctx` untouched.
            let level = routing::Level::from_env();
            let ctx = if level.steers() && routing::mcp_ready(&data_dir) {
                let b = routing::session_block(level);
                if ctx.is_empty() {
                    b
                } else {
                    format!("{b}\n\n{ctx}")
                }
            } else {
                ctx
            };
            Ok(serde_json::to_string(&json!({
                "hookSpecificOutput": {
                    "hookEventName": "SessionStart",
                    "additionalContext": ctx,
                }
            }))?)
        }
        other => {
            eprintln!("ctxforge hook: unknown event {other}");
            Ok(default_response(other))
        }
    }
}

/// SessionStart logic per lifecycle source. Returns the additionalContext to
/// inject (empty string for startup/clear).
fn session_start(
    store: &SessionStore,
    data_dir: &Path,
    session_id: &str,
    project: &Path,
    project_str: &str,
    ts: i64,
    source: &str,
) -> anyhow::Result<String> {
    match source {
        "compact" => {
            // Mark the stored resume consumed, emit the guide, index events.
            if let Some(r) = store.get_resume(session_id, project_str)? {
                if !r.consumed {
                    store.mark_resume_consumed(session_id, project_str)?;
                }
            }
            let events = store.events_for_session(session_id)?;
            index_events(data_dir, session_id, &events);
            let guide = if let Some(r) = store.get_resume(session_id, project_str)? {
                r.snapshot
            } else if !events.is_empty() {
                snapshot::build_snapshot(&events, super::snapshot_budget(), store.compact_count(session_id)?)
            } else {
                String::new()
            };
            Ok(guide)
        }
        "resume" => {
            let events = store.events_for_session(session_id)?;
            if !events.is_empty() {
                index_events(data_dir, session_id, &events);
                Ok(snapshot::build_snapshot(&events, super::snapshot_budget(), store.compact_count(session_id)?))
            } else if let Some(snap) = store.claim_latest_unconsumed_resume(project_str, session_id)? {
                Ok(snap)
            } else {
                Ok(String::new())
            }
        }
        "startup" => {
            // Fresh session = clean slate: clear prior live events for project.
            store.clear_project_events(project_str)?;
            store.ensure_session(session_id, project_str, ts)?;
            // Capture project rule files as P1 events (CLAUDE.md / AGENTS.md).
            let raws = capture_rules(project);
            if !raws.is_empty() {
                let events = attribute(raws, session_id, project_str, ts, "SessionStart");
                store.insert_events(&events)?;
            }
            Ok(String::new())
        }
        _ => Ok(String::new()), // "clear" and unknown — no injection
    }
}

/// Read project rule files from disk and turn them into rule events
/// (path + content; content is what gets indexed for `ctx_search`).
fn capture_rules(project: &Path) -> Vec<RawEvent> {
    let mut out = Vec::new();
    let candidates = [
        project.join("CLAUDE.md"),
        project.join(".claude").join("CLAUDE.md"),
        project.join("AGENTS.md"),
    ];
    for p in candidates {
        if let Ok(content) = std::fs::read_to_string(&p) {
            if !content.trim().is_empty() {
                out.push(RawEvent::new(
                    "rule",
                    1,
                    json!({"path": p.to_string_lossy(), "content": content}),
                ));
            }
        }
    }
    out
}

/// Write detailed events into the FTS5 index so the model can `ctx_search`
/// them on demand after resume. Best-effort.
fn index_events(data_dir: &Path, session_id: &str, events: &[Event]) {
    let idx = match Index::open(data_dir) {
        Ok(i) => i,
        Err(_) => return,
    };
    let records: Vec<(String, String, String)> = events
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let path = format!("session://{session_id}/{}", e.category);
            let chunk_id = format!("session://{session_id}#{i}");
            let content = format!("[{}] {}", e.category, payload_text(&e.payload));
            (path, chunk_id, content)
        })
        .collect();
    let _ = idx.index_records(&records);
}

/// Flatten a payload object into searchable text.
fn payload_text(payload: &Value) -> String {
    match payload {
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| match v {
                Value::String(s) => format!("{k}: {s}"),
                other => format!("{k}: {other}"),
            })
            .collect::<Vec<_>>()
            .join(" | "),
        other => other.to_string(),
    }
}

fn attribute(raws: Vec<RawEvent>, session: &str, project: &str, ts: i64, hook: &str) -> Vec<Event> {
    raws.into_iter()
        .map(|r| r.attribute(session, project, ts, hook))
        .collect()
}

fn is_system_message(prompt: &str) -> bool {
    let t = prompt.trim_start();
    t.starts_with("<task-notification>")
        || t.starts_with("<system-reminder>")
        || t.starts_with("<context_guidance>")
        || t.starts_with("<tool-result>")
        || t.starts_with("<local-command")
        || t.starts_with("<command-")
}

fn default_response(event: &str) -> String {
    if event == "SessionStart" {
        serde_json::to_string(&json!({
            "hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": ""}
        }))
        .unwrap_or_else(|_| "{}".to_string())
    } else {
        "{}".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn run(event: &str, input: HookInput) -> (String, SessionStore, PathBuf) {
        let dir = input.project();
        let data_dir = super::super::resolve_data_dir(&dir);
        let out = handle(event, &input).unwrap();
        let store = SessionStore::open(&data_dir).unwrap();
        (out, store, data_dir)
    }

    fn input_for(dir: &Path) -> HookInput {
        HookInput {
            session_id: Some("sess1".into()),
            cwd: Some(dir.to_string_lossy().to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn posttooluse_stores_file_event_and_returns_empty_obj() {
        let dir = tempdir().unwrap();
        let mut input = input_for(dir.path());
        input.tool_name = Some("Edit".into());
        input.tool_input = Some(json!({"file_path": "src/x.rs"}));
        input.tool_response = Some(json!("ok"));
        let (out, store, _) = run("PostToolUse", input);
        assert_eq!(out, "{}");
        let evs = store.events_for_session("sess1").unwrap();
        assert!(evs.iter().any(|e| e.category == "file" && e.payload["path"] == "src/x.rs"));
    }

    #[test]
    fn userpromptsubmit_skips_system_messages() {
        let dir = tempdir().unwrap();
        let mut input = input_for(dir.path());
        input.prompt = Some("<system-reminder>noise</system-reminder>".into());
        let (_out, store, _) = run("UserPromptSubmit", input);
        assert_eq!(store.events_for_session("sess1").unwrap().len(), 0);
    }

    #[test]
    fn precompact_builds_and_stores_snapshot() {
        let dir = tempdir().unwrap();
        // seed events
        let mut p = input_for(dir.path());
        p.prompt = Some("implement the cache".into());
        run("UserPromptSubmit", p);
        let mut t = input_for(dir.path());
        t.tool_name = Some("Edit".into());
        t.tool_input = Some(json!({"file_path": "cache.rs"}));
        t.tool_response = Some(json!("ok"));
        run("PostToolUse", t);

        let (out, store, _) = run("PreCompact", input_for(dir.path()));
        assert_eq!(out, "{}");
        let r = store.get_resume("sess1", &dir.path().to_string_lossy()).unwrap().unwrap();
        assert!(r.snapshot.contains("## Files Modified"));
        assert!(r.snapshot.contains("cache.rs"));
    }

    #[test]
    fn sessionstart_compact_injects_guide() {
        let dir = tempdir().unwrap();
        let mut t = input_for(dir.path());
        t.tool_name = Some("Edit".into());
        t.tool_input = Some(json!({"file_path": "cache.rs"}));
        t.tool_response = Some(json!("ok"));
        run("PostToolUse", t);
        run("PreCompact", input_for(dir.path()));

        let mut ss = input_for(dir.path());
        ss.source = Some("compact".into());
        let out = handle("SessionStart", &ss).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let ctx = v["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
        assert!(ctx.contains("Session Guide"));
        assert!(ctx.contains("cache.rs"));
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");
    }

    #[test]
    fn sessionstart_startup_clears_prior_events() {
        let dir = tempdir().unwrap();
        let mut t = input_for(dir.path());
        t.tool_name = Some("Edit".into());
        t.tool_input = Some(json!({"file_path": "old.rs"}));
        t.tool_response = Some(json!("ok"));
        run("PostToolUse", t);

        let mut ss = input_for(dir.path());
        ss.source = Some("startup".into());
        handle("SessionStart", &ss).unwrap();

        let store = SessionStore::open(&super::super::resolve_data_dir(dir.path())).unwrap();
        assert_eq!(store.count_events_for_project(&dir.path().to_string_lossy()).unwrap(), 0);
    }

    #[test]
    fn session_id_from_transcript_path() {
        let input = HookInput {
            transcript_path: Some("/x/y/abc-123.jsonl".into()),
            ..Default::default()
        };
        assert_eq!(input.session_id(), "abc-123");
    }
}
