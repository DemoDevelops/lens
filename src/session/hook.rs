//! `lens hook <platform> <event>` — the active lifecycle entrypoint.
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
        let candidate = self.candidate_project();
        // The hook fires from whatever directory the current sub-agent / skill /
        // cd'd shell happens to be in — which may be a subdirectory of the
        // project. The data dir must stay anchored to the repo root so we reuse
        // the single `.lens` the long-lived MCP server captured at startup,
        // and never scatter a nested stray `.lens` through the source tree
        // (untracked dirs there break globbing build tools like xcodegen). Climb
        // to the enclosing repo root if we can find one; else use the candidate.
        repo_root(&candidate).unwrap_or(candidate)
    }

    /// The raw project path from the payload, before repo-root anchoring:
    /// the payload `cwd`, else `$CLAUDE_PROJECT_DIR`, else the process cwd.
    fn candidate_project(&self) -> PathBuf {
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

/// Nearest enclosing repo root at or above `start`: the deepest ancestor that
/// holds a `.git` entry, or — failing that — one that already holds a
/// `.lens` data dir. `.git` is preferred so a pre-existing stray `.lens`
/// in a subdirectory can't pin the search below the real root. Returns `None`
/// when neither marker is found, leaving the caller's candidate untouched (e.g.
/// a tempdir under `/var` in tests).
fn repo_root(start: &Path) -> Option<PathBuf> {
    let mut ctx_root = None;
    for dir in start.ancestors() {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        if ctx_root.is_none() && dir.join(".lens").is_dir() {
            ctx_root = Some(dir.to_path_buf());
        }
    }
    ctx_root
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
        eprintln!("lens hook {event}: {e}");
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
    // Publish the active session id where the long-lived MCP server can read it: the
    // server process never receives the per-event hook payload, so this file is the
    // only channel that lets it stamp its op records with the current session.
    write_current_session(&data_dir, &session_id);
    let store = SessionStore::open(&data_dir)?;
    let ts = super::now_ts();

    match event {
        "PreToolUse" => {
            // Routing is gated by LENS_ROUTING; `off` (the default) is a
            // true no-op that returns `{}` without touching the store.
            let level = routing::Level::from_env();
            if level == routing::Level::Off {
                return Ok("{}".to_string());
            }
            let tool = input.tool_name.clone().unwrap_or_default();
            let ti = input.tool_input.clone().unwrap_or(json!({}));
            let bin = std::env::current_exe()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "lens".to_string());
            let rc = routing::RouteCtx {
                level,
                mcp_ready: routing::mcp_ready(&data_dir),
                bin: &bin,
                data_dir: &data_dir,
                session_id: &session_id,
                rtk_active: crate::rtk::rtk_active(&data_dir),
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
            // Scale-aware search steer: a Grep whose result floods context gets a
            // one-shot nudge toward lens_search (lens_search only beats grep at scale).
            // Capture above runs regardless of routing level; the nudge fires whenever
            // nudges are active.
            let level = routing::Level::from_env();
            if level.nudges() {
                let rc = routing::RouteCtx {
                    level,
                    mcp_ready: false,
                    bin: "",
                    data_dir: &data_dir,
                    session_id: &session_id,
                    rtk_active: false,
                };
                let decision = routing::post_route(&tool, &resp, &rc);
                return Ok(routing::to_post_hook_json(&decision).to_string());
            }
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
            let events = store.resolved_events_for_session(&session_id)?;
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
            let ctx = session_start(
                &store,
                &data_dir,
                &session_id,
                &project,
                &project_str,
                ts,
                &source,
            )?;
            // Prepend the routing tool-selection guide whenever nudges are active.
            // NOT gated on mcp_ready (unlike PreToolUse): at SessionStart the MCP
            // server is registered in the same config as this hook and is still
            // booting, so its heartbeat (`server.pid`) usually isn't fresh yet. Gating
            // here loses that race in every fresh session/worktree and suppresses the
            // guide for the whole session — the model then never learns to reach for
            // (or ToolSearch-load) the ctx tools. The guide is pure context, not a tool
            // interception, so injecting it before the server is reachable is safe; the
            // mcp_ready rail still gates PreToolUse, where denying/rewriting a call the
            // server can't back would be wrong.
            let level = routing::Level::from_env();
            let ctx = if level.nudges() {
                // Tailor the guide's per-tool bullets to the tools active this
                // session; with no tool history (fresh startup) fall back to full.
                let (bash, file) = active_tool_groups(&store, &session_id);
                let b = routing::session_block_for(level, bash, file);
                if ctx.is_empty() {
                    b
                } else {
                    format!("{b}\n\n{ctx}")
                }
            } else {
                ctx
            };
            // Append a one-line "update available" nudge on fresh startups only. Reads a
            // cached check (never blocks), refreshing it detached when stale.
            let ctx = if source == "startup" {
                match crate::setup::update_nudge_line() {
                    Some(line) if ctx.is_empty() => line,
                    Some(line) => format!("{ctx}\n\n{line}"),
                    None => ctx,
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
            eprintln!("lens hook: unknown event {other}");
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
            let events = store.resolved_events_for_session(session_id)?;
            index_events(data_dir, session_id, &events);
            let guide = if let Some(r) = store.get_resume(session_id, project_str)? {
                r.snapshot
            } else if !events.is_empty() {
                snapshot::build_snapshot(
                    &events,
                    super::snapshot_budget(),
                    store.compact_count(session_id)?,
                )
            } else {
                String::new()
            };
            Ok(guide)
        }
        "resume" => {
            let events = store.resolved_events_for_session(session_id)?;
            if !events.is_empty() {
                index_events(data_dir, session_id, &events);
                Ok(snapshot::build_snapshot(
                    &events,
                    super::snapshot_budget(),
                    store.compact_count(session_id)?,
                ))
            } else if let Some(snap) =
                store.claim_latest_unconsumed_resume(project_str, session_id)?
            {
                Ok(snap)
            } else {
                Ok(String::new())
            }
        }
        "startup" => {
            // Fresh session = clean slate for the live event log.
            store.clear_project_events(project_str)?;
            store.ensure_session(session_id, project_str, ts)?;
            // Capture project rule files as P1 events (CLAUDE.md / AGENTS.md).
            let raws = capture_rules(project);
            if !raws.is_empty() {
                let events = attribute(raws, session_id, project_str, ts, "SessionStart");
                store.insert_events(&events)?;
            }
            // Re-inject durable project memory (decisions/constraints/rules captured in
            // prior sessions) so a fresh session resumes with them despite the clear.
            Ok(snapshot::render_project_memory(
                &store.project_memory(project_str)?,
            ))
        }
        _ => Ok(String::new()), // "clear" and unknown — no injection
    }
}

/// Best-effort: publish the active session id to `<data_dir>/current_session` so the
/// MCP server (a separate, long-lived process) can stamp its op records with it.
/// Atomic via temp-file + rename; any IO error is ignored — a hook must never fail.
fn write_current_session(data_dir: &Path, session_id: &str) {
    let _ = std::fs::create_dir_all(data_dir);
    let tmp = data_dir.join(format!("current_session.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, session_id).is_ok() {
        let _ = std::fs::rename(&tmp, data_dir.join("current_session"));
    }
}

/// Read project rule files from disk and turn them into rule events
/// (path + content; content is what gets indexed for `lens_search`).
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

/// Write detailed events into the FTS5 index so the model can `lens_search`
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

/// Which tool groups has this session used, from the stored event categories?
/// Returns `(bash, file)`; `(false, false)` means no tool history, so the caller
/// injects the full guide. `git`/`environment` are Bash-only signals; `file`
/// covers Read/Edit/Write.
fn active_tool_groups(store: &SessionStore, session_id: &str) -> (bool, bool) {
    let cats = match store.activity(Some(session_id), None) {
        Ok(a) => a.by_category,
        Err(_) => return (false, false),
    };
    let mut bash = false;
    let mut file = false;
    for (cat, _) in cats {
        match cat.as_str() {
            "git" | "environment" => bash = true,
            "file" => file = true,
            _ => {}
        }
    }
    (bash, file)
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
        assert!(evs
            .iter()
            .any(|e| e.category == "file" && e.payload["path"] == "src/x.rs"));
    }

    #[test]
    fn handle_publishes_current_session_for_server() {
        let dir = tempdir().unwrap();
        let mut input = input_for(dir.path());
        input.tool_name = Some("Edit".into());
        input.tool_input = Some(json!({"file_path": "x.rs"}));
        input.tool_response = Some(json!("ok"));
        let (_out, _store, data_dir) = run("PostToolUse", input);
        let got = std::fs::read_to_string(data_dir.join("current_session")).unwrap();
        assert_eq!(got.trim(), "sess1");
    }

    #[test]
    fn hook_anchors_data_dir_to_repo_root_not_subdir() {
        // Regression: a hook fired with cwd set to a SUBDIRECTORY of the repo must
        // resolve its data dir to the repo-root `.lens` and must NOT scatter a
        // nested stray `.lens` under the subdir (that broke an xcodegen build).
        let repo = tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        let subdir = repo.path().join("Sources").join("Core");
        std::fs::create_dir_all(&subdir).unwrap();

        let mut input = input_for(&subdir); // cwd = deep subdirectory
        input.tool_name = Some("Edit".into());
        input.tool_input = Some(json!({"file_path": "x.swift"}));
        input.tool_response = Some(json!("ok"));

        // project() climbs to the repo root (the .git dir), not the subdir.
        assert_eq!(input.project().as_path(), repo.path());

        handle("PostToolUse", &input).unwrap();

        // Canonical data dir at the repo root; nothing scattered under the subdir.
        assert!(repo.path().join(".lens").is_dir());
        assert!(!subdir.join(".lens").exists());
        assert!(!repo.path().join("Sources").join(".lens").exists());
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
        let r = store
            .get_resume("sess1", &dir.path().to_string_lossy())
            .unwrap()
            .unwrap();
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
        let ctx = v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
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
        assert_eq!(
            store
                .count_events_for_project(&dir.path().to_string_lossy())
                .unwrap(),
            0
        );
    }

    #[test]
    fn sessionstart_injects_routing_guide_even_when_mcp_not_ready() {
        // Regression: the SessionStart tool-selection guide must NOT be gated on a
        // fresh server.pid. In a fresh worktree the MCP server is still booting when
        // this hook fires, so server.pid isn't fresh yet (mcp_ready == false) — yet
        // the guide has to inject anyway, or the model never learns to use the ctx
        // tools. tempdir() has no server.pid, so mcp_ready is false here; the guide
        // must still appear. (LENS_ROUTING is read by no other test.)
        let dir = tempdir().unwrap();
        let prev = std::env::var("LENS_ROUTING").ok();
        std::env::set_var("LENS_ROUTING", "full");

        let mut ss = input_for(dir.path());
        ss.source = Some("startup".into());
        let out = handle("SessionStart", &ss).unwrap();

        match prev {
            Some(v) => std::env::set_var("LENS_ROUTING", v),
            None => std::env::remove_var("LENS_ROUTING"),
        }

        let v: Value = serde_json::from_str(&out).unwrap();
        let ctx = v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(
            ctx.contains("<context_window_protection>"),
            "guide must inject even when the MCP server isn't reachable yet"
        );
        assert!(
            ctx.contains("ToolSearch"),
            "carries the deferred-tool bootstrap"
        );
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
