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

/// A one-line supersession notice when an edit invalidates stored file
/// snapshots (`lens_skeleton` refs): skeletons or recalled contents of the file
/// from earlier in the conversation no longer match it. Compares content hashes
/// rather than mtimes, so a revert that restores the captured bytes stays
/// silent; the file read only happens when refs exist for the path (the rare
/// case). Throttled once per (session, file) via the routing nudge throttle.
fn supersession_notice(
    data_dir: &Path,
    session_id: &str,
    tool: &str,
    tool_input: &Value,
) -> Option<String> {
    if !matches!(tool, "Edit" | "MultiEdit" | "NotebookEdit" | "Write") {
        return None;
    }
    let path = tool_input
        .get("file_path")
        .and_then(Value::as_str)
        .or_else(|| tool_input.get("notebook_path").and_then(Value::as_str))?;
    let key = format!("stale-file:{path}");
    if routing::throttle::fired(data_dir, session_id, &key) {
        return None;
    }
    let store = crate::store::Store::open(data_dir).ok()?;
    let sources = store.sources_for_path(path).ok()?;
    if sources.is_empty() {
        return None; // common case: lens never snapshotted this file
    }
    let bytes = std::fs::read(path).ok()?;
    let current = blake3::hash(&bytes).to_hex().to_string();
    let n = sources.iter().filter(|s| s.hash != current).count();
    if n == 0 {
        return None;
    }
    routing::throttle::mark(data_dir, session_id, &key);
    Some(format!(
        "<context_guidance>\n  <tip>\n    This edit supersedes {n} lens snapshot(s) of {path}: \
         skeletons or recalled contents of this file from earlier in the conversation no longer \
         match it. Don't rely on them — re-run lens_skeleton (or Read) for the current file; \
         lens_recall on the old refs will flag them stale.\n  </tip>\n</context_guidance>"
    ))
}

/// Six-class follower split for the reroute-rail counter plane
/// (`{p}_next_{class}` / `{p}_shadow_next_{class}`): which tool the agent
/// reached for on the event AFTER a rail's would-fire. `lens` covers both a
/// direct lens MCP call and a ToolSearch that loads lens tools (the bootstrap
/// step IS the compliant next move for a rail's deny/nudge). Distinct from the
/// grep-scope plane's 4-class split above, which stays untouched.
fn follower_class6(tool: &str, tool_input: &Value) -> &'static str {
    let is_lens_toolsearch = tool == "ToolSearch"
        && tool_input
            .get("query")
            .and_then(Value::as_str)
            .is_some_and(|q| q.contains("lens"));
    if tool.starts_with("mcp__lens__") || is_lens_toolsearch {
        "lens"
    } else if tool == "Grep" {
        "grep"
    } else if tool == "Read" {
        "read"
    } else if tool == "Bash" {
        "bash"
    } else if matches!(tool, "Edit" | "MultiEdit" | "Write") {
        "edit"
    } else {
        "other"
    }
}

/// Would the elink rail fire on this Edit/MultiEdit? The classifier half of
/// the shadow-counter derivation: a decl-touching edit whose symbol has >=K
/// callers in the graph. The graph load is guarded (a missing/unreadable
/// `graph.json` is a silent no) and only reached when a declaration was
/// actually touched, so non-decl edits never pay for it.
fn elink_would_fire(data_dir: &Path, tool: &str, tool_input: &Value) -> bool {
    let Some(sym) = crate::routing::edited_decl_symbol(tool, tool_input) else {
        return false;
    };
    let Ok(graph) = crate::discovery::graph::Graph::load(&data_dir.join("graph.json")) else {
        return false;
    };
    let k = crate::routing::reroute::edit_callers::min_callers();
    crate::routing::reroute::edit_callers::caller_nudge(&graph, &sym, k).is_some()
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
            let mcp_ready = routing::mcp_ready(&data_dir);

            // Follower proxy, for EVERY tool: consume a pending shadow/deny
            // marker armed by the PREVIOUS Grep's scope check below (the
            // compliant next step after a grep-scope deny/shadow may be any
            // tool, not just another Grep). Classed on the CURRENT tool so
            // the counters show what actually ran next. `take` zeroes the
            // marker on read, so this fires at most once per arm.
            let is_lens_toolsearch = tool == "ToolSearch"
                && ti
                    .get("query")
                    .and_then(Value::as_str)
                    .is_some_and(|q| q.contains("lens"));
            let class = if tool.starts_with("mcp__lens__") || is_lens_toolsearch {
                "lens"
            } else if tool == "Grep" {
                "grep"
            } else if tool == "Bash"
                && ti.get("command").and_then(Value::as_str).is_some_and(|c| {
                    c.split_whitespace()
                        .any(|t| matches!(t, "grep" | "egrep" | "rg"))
                })
            {
                "shellgrep"
            } else {
                "other"
            };
            let shadow_pending =
                routing::throttle::take(&data_dir, &session_id, "scope-shadow-pending");
            let deny_pending =
                routing::throttle::take(&data_dir, &session_id, "scope-deny-pending");
            let stats_store = crate::store::Store::open(&data_dir).ok();
            if shadow_pending {
                if let Some(s) = &stats_store {
                    let _ = s.bump_stat(&format!("shadow_next_{class}"), 1);
                }
            }
            if deny_pending {
                if let Some(s) = &stats_store {
                    let _ = s.bump_stat(&format!("deny_next_{class}"), 1);
                }
            }
            // Reroute-rail follower proxy (same consume-then-arm shape, six
            // rails, 6-class follower split — the grep-scope plane above keeps
            // its own 4-class split untouched). A pending marker armed by the
            // PREVIOUS event's would-fire is consumed here and classed on the
            // CURRENT tool: `{p}_next_{class}` for a live arm (rail flag ON),
            // `{p}_shadow_next_{class}` for a shadow arm (flag OFF).
            let class6 = follower_class6(&tool, &ti);
            for p in ["gsym", "rskel", "bagg", "elink", "gast", "rovr"] {
                if routing::throttle::take(&data_dir, &session_id, &format!("{p}-live-pending")) {
                    if let Some(s) = &stats_store {
                        let _ = s.bump_stat(&format!("{p}_next_{class6}"), 1);
                    }
                }
                if routing::throttle::take(&data_dir, &session_id, &format!("{p}-shadow-pending"))
                {
                    if let Some(s) = &stats_store {
                        let _ = s.bump_stat(&format!("{p}_shadow_next_{class6}"), 1);
                    }
                }
            }

            // Grep-scope shadow counters: classify this Grep's path scope, and
            // on a broad scope that would trip the deny gate, arm the marker
            // the follower proxy above consumes on the NEXT tool call.
            if tool == "Grep" {
                let scope = routing::grep_scope(ti.get("path").and_then(Value::as_str));
                if let Some(s) = &stats_store {
                    let key = match scope {
                        routing::GrepScope::SingleFile => "grep_scope_single",
                        routing::GrepScope::Broad => "grep_scope_broad",
                        routing::GrepScope::Unknown => "grep_scope_unknown",
                    };
                    let _ = s.bump_stat(key, 1);
                }
                if scope == routing::GrepScope::Broad
                    && level.steers()
                    && mcp_ready
                    && routing::index_present(&data_dir)
                {
                    if let Some(s) = &stats_store {
                        let _ = s.bump_stat("grep_scope_would_deny", 1);
                    }
                    let pending_key = if routing::grep_scope_deny_enabled() {
                        "scope-deny-pending"
                    } else {
                        "scope-shadow-pending"
                    };
                    routing::throttle::bump(&data_dir, &session_id, pending_key);
                }
            }

            // Rail 2c feed: count code-file Reads since the last repo-map call.
            // Any lens_map/lens_overview zeroes it; the count rides into
            // RouteCtx so the rovr rail can fire on the Nth mapless read.
            let reads_since_map = if tool == "Read" {
                let is_code = ti
                    .get("file_path")
                    .and_then(Value::as_str)
                    .and_then(|p| {
                        Path::new(p)
                            .extension()
                            .and_then(|e| e.to_str())
                            .map(str::to_ascii_lowercase)
                    })
                    .is_some_and(|ext| {
                        crate::discovery::extract::spec_for_extension(&ext).is_some()
                    });
                if is_code {
                    routing::throttle::bump(&data_dir, &session_id, "reads-since-map")
                } else {
                    0
                }
            } else if matches!(
                tool.as_str(),
                "mcp__lens__lens_map" | "mcp__lens__lens_overview"
            ) {
                routing::throttle::reset(&data_dir, &session_id, "reads-since-map");
                0
            } else {
                0
            };

            // Reroute-rail shadow counters (six rails, grep-scope shape):
            // re-derive each rail's would-fire on THIS event with the same
            // classifier + gates route_inner uses, bump `{p}_would_fire`
            // regardless of the rail's flag, and arm the live/shadow follower
            // marker the loop above consumes on the NEXT event.
            {
                let arm = |prefix: &str, enabled: bool| {
                    if let Some(s) = &stats_store {
                        let _ = s.bump_stat(&format!("{prefix}_would_fire"), 1);
                    }
                    let pk = if enabled {
                        format!("{prefix}-live-pending")
                    } else {
                        format!("{prefix}-shadow-pending")
                    };
                    routing::throttle::bump(&data_dir, &session_id, &pk);
                };
                match tool.as_str() {
                    "Grep" => {
                        let pat = ti.get("pattern").and_then(Value::as_str).unwrap_or("");
                        let gsym_shape = level.steers()
                            && routing::reroute::grep_symbol::symbol_grep(pat).is_some();
                        let gast = level.nudges()
                            && routing::reroute::grep_ast::syntax_shape(pat).is_some();
                        if (gsym_shape || gast) && mcp_ready && routing::index_present(&data_dir) {
                            // Same graph-resolution gate as route_inner: gsym's
                            // would_fire counts exactly the deny that fires; a
                            // classifier hit whose graph lookup dead-ends bumps
                            // the diagnostic gsym_graph_miss counter instead.
                            let gsym = gsym_shape
                                && routing::reroute::grep_symbol::graph_resolves(&data_dir, pat);
                            if gsym {
                                arm("gsym", routing::grep_symbol_deny_enabled());
                            } else if gsym_shape {
                                if let Some(s) = &stats_store {
                                    let _ = s.bump_stat("gsym_graph_miss", 1);
                                }
                            }
                            if gast {
                                arm("gast", routing::grep_ast_nudge_enabled());
                            }
                        }
                    }
                    "Read" => {
                        let path = ti.get("file_path").and_then(Value::as_str).unwrap_or("");
                        let has_offset_or_limit =
                            ti.get("offset").is_some() || ti.get("limit").is_some();
                        let edited = routing::edited_paths_for(&data_dir, &session_id, path);
                        // Mirror read_decision's rskel gate EXACTLY (incl. the
                        // edit-intent exemption) so `rskel_would_fire` counts the
                        // deny that actually fires, not a looser one.
                        let rskel = level.steers()
                            && !routing::rskel_edit_exempt(&data_dir, &session_id)
                            && routing::reroute::read_skeleton::read_is_skeletonizable(
                                path,
                                has_offset_or_limit,
                                &edited,
                            );
                        let rovr = level.nudges()
                            && routing::reroute::read_overview::overview_due(
                                reads_since_map,
                                routing::reroute::read_overview::threshold(),
                            );
                        if (rskel || rovr) && mcp_ready && routing::index_present(&data_dir) {
                            if rskel {
                                arm("rskel", routing::read_skeleton_deny_enabled());
                            }
                            if rovr {
                                arm("rovr", routing::read_overview_nudge_enabled());
                            }
                        }
                    }
                    "Bash" => {
                        let cmd = ti.get("command").and_then(Value::as_str).unwrap_or("");
                        if level.nudges()
                            && routing::reroute::bash_aggregate::is_data_aggregate(cmd)
                            && mcp_ready
                            && routing::index_present(&data_dir)
                        {
                            arm("bagg", routing::bash_agg_nudge_enabled());
                        }
                    }
                    // Graph load only on Edit events, guarded — see
                    // `elink_would_fire`.
                    "Edit" | "MultiEdit"
                        if level.nudges()
                            && mcp_ready
                            && routing::index_present(&data_dir)
                            && elink_would_fire(&data_dir, &tool, &ti) =>
                    {
                        arm("elink", routing::edit_links_nudge_enabled());
                    }
                    _ => {}
                }
            }

            let bin = std::env::current_exe()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "lens".to_string());
            let rc = routing::RouteCtx {
                level,
                mcp_ready,
                bin: &bin,
                data_dir: &data_dir,
                session_id: &session_id,
                rtk_active: crate::rtk::rtk_active(&data_dir),
                reads_since_map,
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
            // A lens tool call is itself the "checked the graph" signal the
            // consecutive-lookup deny counter (`routing::inspect_escalation`)
            // is watching for: reset it so "read-code" measures lookups-since-
            // last-lens-call, not cumulative-per-session. A file edit resets it
            // too — Read-before-Edit was the right tool, not drift, so an
            // edit-heavy session never accumulates toward the deny.
            let is_lens = tool
                .strip_prefix("mcp__")
                .and_then(|rest| rest.split("__").next())
                .is_some_and(|server| server.contains("lens"));
            if is_lens || matches!(tool.as_str(), "Edit" | "Write" | "MultiEdit" | "NotebookEdit") {
                routing::throttle::reset(&data_dir, &session_id, "read-code");
                // Also disarm the first-Grep and grep-scope denies: a lens call
                // answered the find/trace question, so a follow-up Grep is no
                // longer drift.
                routing::throttle::reset(&data_dir, &session_id, "grep-first");
                routing::throttle::reset(&data_dir, &session_id, "grep-scope");
            }
            // Record the edited path so the read-skeleton rail leaves later
            // Reads of this file alone (Read-before-the-next-Edit is the right
            // tool, not drift — see `reroute::read_skeleton`).
            if matches!(tool.as_str(), "Edit" | "MultiEdit" | "Write") {
                if let Some(p) = ti.get("file_path").and_then(Value::as_str) {
                    routing::throttle::mark(&data_dir, &session_id, &format!("editpath:{p}"));
                }
            }
            // An edit that invalidates stored lens snapshots of the file gets a
            // one-line supersession notice (once per session+file). Fires at every
            // routing level: it is a correctness signal about content already in
            // the model's context, not a tool-selection nudge. Kept below the
            // deny reset so an edit still disarms grep-first before returning.
            if let Some(note) = supersession_notice(&data_dir, &session_id, &tool, &ti) {
                return Ok(
                    routing::to_post_hook_json(&routing::Decision::Context(note)).to_string(),
                );
            }
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
                    reads_since_map: 0,
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
                let level = routing::Level::from_env();
                // Arm the grep-scope deny for this prompt when the dark-launch
                // flag is on: a broad Grep becomes deniable regardless of
                // whether the prompt itself reads as find/trace-shaped (unlike
                // grep-first, grep-scope is gated on the call's scope, not the
                // prompt's phrasing, so it must arm independently of the
                // find/trace branch below). `bump` just increments; the Grep
                // arm's `take` zeroes the whole count in one shot, so repeated
                // arming per prompt can't stack denies.
                if level.steers() && routing::grep_scope_deny_enabled() {
                    routing::throttle::bump(&data_dir, &session_id, "grep-scope");
                }
                // Arm or clear the rskel edit-intent exemption for THIS prompt.
                // Prompt-scoped: every steering prompt either sets it (edit
                // intent) or clears it (anything else), so a marker from a prior
                // edit prompt can't leak forward and exempt a later unrelated
                // Read. `read_decision`'s rskel gate reads it via
                // `rskel_edit_exempt`. `bump` (not `mark`) is required: `mark`
                // only inserts on a vacant key, so after a `reset` it is a
                // silent no-op and would fail to re-arm on a neutral→edit prompt
                // sequence. Placed before the find/trace early-return so it runs
                // on every non-system prompt regardless of shape.
                if level.steers() {
                    if routing::prompt_wants_edit(&prompt) {
                        routing::throttle::bump(&data_dir, &session_id, "edit-intent");
                    } else {
                        routing::throttle::reset(&data_dir, &session_id, "edit-intent");
                    }
                }
                // Find/trace prompts get the tool mapping injected HERE, at the
                // decision point: first-tool choice is made from what's in
                // context before the first call, which PreToolUse nudges are
                // too late for (measured: find/trace tasks stayed Grep-first
                // on SessionStart steering alone).
                if level.nudges() && routing::prompt_wants_find_trace(&prompt) {
                    // Arm the one-shot first-Grep deny for this prompt (the
                    // Grep arm in `routing::route_inner` consumes it; any
                    // lens call or edit disarms it via the PostToolUse reset).
                    if level.steers() {
                        routing::throttle::bump(&data_dir, &session_id, "grep-first");
                    }
                    return Ok(json!({
                        "hookSpecificOutput": {
                            "hookEventName": "UserPromptSubmit",
                            "additionalContext": routing::PROMPT_INTENT_NUDGE,
                        }
                    })
                    .to_string());
                }
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

/// Bootstrap hint for the durable-memory MCP tools, appended to every fresh
/// SessionStart (not just when memory already exists) so a session discovers
/// them even on a brand-new project with nothing recorded yet.
const MEMORY_TOOLS_HINT: &str = "(Durable project memory across sessions: \
    ToolSearch(query: \"select:lens_memory_query,lens_memory_record\"), then \
    lens_memory_query() to read it or lens_memory_record(category, text) to add \
    a decision/constraint/rejected-approach/rule.)";

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
            let memory = snapshot::render_project_memory(&store.project_memory(project_str)?);
            let body = match repo_map_block(data_dir) {
                Some(block) if memory.is_empty() => block,
                Some(block) => format!("{memory}\n\n{block}"),
                None => memory,
            };
            Ok(if body.is_empty() {
                MEMORY_TOOLS_HINT.to_string()
            } else {
                format!("{body}\n\n{MEMORY_TOOLS_HINT}")
            })
        }
        _ => Ok(String::new()), // "clear" and unknown — no injection
    }
}

/// Default token budget for the pushed repo-map digest (see `repo_map_block`);
/// overridable via `LENS_SESSION_OVERVIEW_BUDGET`, `0` disables it entirely.
const REPO_MAP_BUDGET_DEFAULT: usize = 1200;
/// Hard byte cap on the emitted `<repo_map>` block — there's no outer byte
/// budget on the SessionStart context string, so this digest caps itself.
const REPO_MAP_CAP_BYTES: usize = 6 * 1024;

/// A whole-repo structural digest (aider-style repo map): the most-connected
/// symbols, ranked, so a fresh session can expand from a graph query instead of
/// reading files cold. Loads an existing `graph.json` only — NEVER builds one
/// here, so SessionStart stays fast; a missing/unreadable graph silently skips
/// this (`None`), same as "no digest available yet".
fn repo_map_block(data_dir: &Path) -> Option<String> {
    let budget: usize = std::env::var("LENS_SESSION_OVERVIEW_BUDGET")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(REPO_MAP_BUDGET_DEFAULT);
    if budget == 0 {
        return None;
    }
    let graph = crate::discovery::graph::Graph::load(&data_dir.join("graph.json")).ok()?;
    let digest = cap_bytes(&crate::discovery::query::overview(&graph, budget), REPO_MAP_CAP_BYTES);
    Some(format!(
        "<repo_map>\nGraph overview of this repo (most-connected symbols; expand any of these with lens_symbol / lens_links / lens_path):\n{digest}\n</repo_map>"
    ))
}

/// Truncate `s` to at most `max` bytes at a char boundary, appending `…` when
/// cut. Unlike `snapshot::cap` this preserves newlines — the digest's line
/// structure (one symbol per line) is the point.
fn cap_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
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

/// Write detailed events into the full-text index so the model can `lens_search`
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
    fn posttooluse_edit_fires_supersession_notice_once() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("widget.rs");
        std::fs::write(&file, "pub fn one() -> i32 { 2 }\n").unwrap();
        // A snapshot of the file's earlier contents, as lens_skeleton records it.
        let data_dir = super::super::resolve_data_dir(dir.path());
        let store = crate::store::Store::open(&data_dir).unwrap();
        let old = store.put("pub fn one() -> i32 { 1 }\n").unwrap();
        store.record_source(&old, &file.to_string_lossy()).unwrap();

        let edit_input = || {
            let mut input = input_for(dir.path());
            input.tool_name = Some("Edit".into());
            input.tool_input = Some(json!({"file_path": file.to_string_lossy()}));
            input.tool_response = Some(json!("ok"));
            input
        };
        let (out, _store, _) = run("PostToolUse", edit_input());
        assert!(
            out.contains("additionalContext"),
            "expected a supersession notice, got: {out}"
        );
        assert!(
            out.contains("widget.rs"),
            "notice should name the file: {out}"
        );

        // Once per (session, file): a second edit stays silent.
        let (again, _, _) = run("PostToolUse", edit_input());
        assert_eq!(again, "{}");
    }

    #[test]
    fn posttooluse_edit_matching_snapshot_stays_silent() {
        // The file's bytes still equal the recorded snapshot (e.g. a revert):
        // nothing in context went stale, so no notice.
        let dir = tempdir().unwrap();
        let file = dir.path().join("widget.rs");
        let content = "pub fn one() -> i32 { 1 }\n";
        std::fs::write(&file, content).unwrap();
        let data_dir = super::super::resolve_data_dir(dir.path());
        let store = crate::store::Store::open(&data_dir).unwrap();
        let hash = store.put(content).unwrap();
        store.record_source(&hash, &file.to_string_lossy()).unwrap();

        let mut input = input_for(dir.path());
        input.tool_name = Some("Edit".into());
        input.tool_input = Some(json!({"file_path": file.to_string_lossy()}));
        input.tool_response = Some(json!("ok"));
        let (out, _store, _) = run("PostToolUse", input);
        assert_eq!(out, "{}");
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
