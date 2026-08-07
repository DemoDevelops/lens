//! `lens hook <platform> <event>` — the active lifecycle entrypoint.
//!
//! Claude Code or opencode invokes this on PreToolUse / PostToolUse / UserPromptSubmit /
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

/// Parsed subset of the Claude Code / opencode hook stdin payload.
/// Parsing is tolerant: missing fields fall back to cwd / defaults.
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
    /// the payload `cwd`, else `$CLAUDE_PROJECT_DIR`/`$OPENCODE_PROJECT_DIR`, else process cwd.
    /// Tolerant for opencode hook payloads (may omit cwd or use different env).
    fn candidate_project(&self) -> PathBuf {
        if let Some(c) = &self.cwd {
            if !c.is_empty() {
                return PathBuf::from(c);
            }
        }
        for key in ["CLAUDE_PROJECT_DIR", "OPENCODE_PROJECT_DIR"] {
            if let Some(c) = std::env::var_os(key) {
                if !c.is_empty() {
                    return PathBuf::from(c);
                }
            }
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
    if normalize_tool_name(tool).starts_with("lens_") || is_lens_toolsearch {
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

/// Normalize bare `lens_*` (from opencode) or `mcp__lens__*` (from claude) so
/// both are treated as lens tools in classifiers. Keeps original tool name for
/// extract / route (client specific).
fn normalize_tool_name(t: &str) -> String {
    if let Some(rest) = t.strip_prefix("mcp__lens__") {
        if rest.starts_with("lens_") {
            rest.to_string()
        } else {
            format!("lens_{rest}")
        }
    } else {
        t.to_string()
    }
}

/// Would a live elink arm fire on this Edit/MultiEdit? The classifier half of
/// the follower-counter derivation: a decl-touching edit whose symbol has >=K
/// callers in the graph AND whose `elink:{sym}` one-shot hasn't already been
/// spent. The marker check gives the mirror the same one-shot the live arm has:
/// once a live deny/nudge marks `elink:{sym}`, every later Edit of that symbol
/// passes through, so it no longer "would fire" — without this the verbatim
/// retry after an elink deny would double-count `elink_would_fire` and mis-arm
/// a follower the live arm never emitted. The graph load is guarded (a missing/
/// unreadable `graph.json` is a silent no) and only reached when a declaration
/// was actually touched, so non-decl edits never pay for it.
fn elink_would_fire(data_dir: &Path, session_id: &str, tool: &str, tool_input: &Value) -> bool {
    let Some(sym) = crate::routing::edited_decl_symbol(tool, tool_input) else {
        return false;
    };
    if crate::routing::throttle::fired(data_dir, session_id, &format!("elink:{sym}")) {
        return false;
    }
    let Ok(graph) = crate::discovery::graph::Graph::load(&data_dir.join("graph.json")) else {
        return false;
    };
    let k = crate::routing::reroute::edit_callers::min_callers();
    crate::routing::reroute::edit_callers::caller_count(&graph, &sym).is_some_and(|n| n >= k)
}

/// Is `cmd` a grep-shaped Bash segment that would classify to something other
/// than the deliberate `NarrowExact` escape hatch? Powers
/// `bash_grep_would_fire` via T1's `reroute::bash_grep::parse_grep_seg` +
/// `classify` — a single-file scope is left alone (the rail's escape hatch),
/// so it must not count as a would-fire.
fn bash_grep_shape(cmd: &str) -> bool {
    routing::reroute::bash_grep::parse_grep_seg(cmd)
        .map(|seg| routing::reroute::bash_grep::classify(&seg))
        .is_some_and(|c| !matches!(c, routing::reroute::bash_grep::BashGrepClass::NarrowExact))
}

/// Nearest enclosing project root at or above `start`, via the shared
/// `discovery::anchor_root` walk: the deepest ancestor holding a `.git` entry,
/// else one holding any other project marker (`.lens`, Cargo.toml,
/// package.json, ...). `.git` is preferred so a stray marker in a subdirectory
/// can't pin the search below the real root; markers at `$HOME` are ignored.
/// Returns `None` when no marker is found, leaving the caller's candidate
/// untouched (e.g. a tempdir under `/var` in tests).
fn repo_root(start: &Path) -> Option<PathBuf> {
    crate::discovery::anchor_root(start)
}

/// The denylist entry covering `project`, if any: `crate::disabled::covering_entry`
/// in production. In test builds, a per-thread override (set by
/// `tests::with_disabled_override`) takes priority when present: `covering_entry`
/// reads `rtk::home_root()` (`$LENS_HOME`), a process-global env var, and this repo's
/// tests run in parallel on their own OS threads — mutating `$LENS_HOME` from one
/// test would race every OTHER concurrently-running test that resolves a data dir
/// through the same shared resolver. The `thread_local` override sidesteps that
/// entirely (each test thread sees only its own value) while still exercising the
/// exact `handle()` code path the real hook runs.
fn disabled_entry_for(project: &Path) -> Option<PathBuf> {
    #[cfg(test)]
    {
        if let Some(entry) = tests::disabled_override() {
            return Some(entry);
        }
    }
    crate::disabled::covering_entry(project)
}

/// CLI entry: `args` is everything after `hook` (i.e. `[platform, event]`).
/// Always exits 0 and prints a valid hook response, even on malformed input.
pub fn run_cli(args: &[String]) -> anyhow::Result<()> {
    // args[0] = platform ("claude" or "opencode"), args[1] = event name.
    let platform = args.first().cloned().unwrap_or_default();
    let event = args.get(1).cloned().unwrap_or_default();

    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let input: HookInput = serde_json::from_str(&raw).unwrap_or_default();

    let stdout = handle(&platform, &event, &input).unwrap_or_else(|e| {
        eprintln!("lens hook {platform} {event}: {e}");
        default_response(&event)
    });
    println!("{stdout}");
    Ok(())
}

/// Route a single event. Returns the stdout JSON string per the contract.
fn handle(platform: &str, event: &str, input: &HookInput) -> anyhow::Result<String> {
    if platform == "opencode" && !matches!(event, "PreToolUse" | "PostToolUse" | "UserPromptSubmit" | "PreCompact" | "SessionStart") {
        return Ok("{}".to_string());
    }

    let project = input.project();
    // Scope guard: a session rooted in a non-project tree (a home directory: no
    // project marker, over the probe budget) gets no lens, and neither does a
    // tree the user explicitly disabled via `lens off`. No `.lens` dir
    // scattered there, no store writes, no routing guide, and no auto-index of a
    // million-file tree. SessionStart says so in one line; every other event
    // returns its default no-op response. The denylist check runs FIRST: it's a
    // cheap file read (vs. the probe's directory walk) and the message differs.
    if let Some(entry) = disabled_entry_for(&project) {
        if event == "SessionStart" {
            return Ok(serde_json::to_string(&json!({
                "hookSpecificOutput": {
                    "hookEventName": "SessionStart",
                    "additionalContext": format!(
                        "lens off: {} is disabled (lens off {}). Run `lens on {}` to \
                         re-enable.",
                        project.display(),
                        entry.display(),
                        entry.display()
                    ),
                }
            }))?);
        }
        return Ok(default_response(event));
    }
    // Cheap for real projects: the first marker hit answers the classification.
    if !crate::discovery::indexable_root(&project) {
        if event == "SessionStart" {
            return Ok(serde_json::to_string(&json!({
                "hookSpecificOutput": {
                    "hookEventName": "SessionStart",
                    "additionalContext": format!(
                        "lens idle: {} is not a code project (no .git or build \
                         manifest), so lens tools are inactive this session. Start \
                         sessions from a project directory to enable them.",
                        project.display()
                    ),
                }
            }))?);
        }
        return Ok(default_response(event));
    }
    let project_str = project.to_string_lossy().to_string();
    let session_id = input.session_id();
    let data_dir = super::resolve_data_dir(&project);
    // Register the root -> data-dir pair once per session: the hook can be the
    // only plane that ever opens this dir (MCP server absent or failed), and an
    // unregistered central dir reads to `lens clean` as an orphan it would
    // reclaim out from under a live project. SessionStart-gated so the
    // per-tool-call hot path never pays the registry read.
    if event == "SessionStart" {
        crate::obs::record_registry(&project, &data_dir);
    }
    // Publish the active session id where the long-lived MCP server can read it: the
    // server process never receives the per-event hook payload, so this file is the
    // only channel that lets it stamp its op records with the current session.
    write_current_session(&data_dir, &session_id);
    // Opened lazily per arm: PreToolUse (the per-tool-call hot path) never
    // touches the session store, and an open costs two sqlite opens (local +
    // global mirror) — measured at ~0.4s of the hook's ~0.6s wall on a
    // contended machine (2026-07-21 audit).
    let open_store = || SessionStore::open(&data_dir);
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
            let class = if normalize_tool_name(&tool).starts_with("lens_") || is_lens_toolsearch {
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
            for p in [
                "gsym",
                "rskel",
                "bagg",
                "elink",
                "gast",
                "rovr",
                "bash_grep",
                "read_runfile",
            ] {
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
                normalize_tool_name(tool.as_str()).as_str(),
                "lens_map" | "lens_overview"
            ) {
                routing::throttle::reset(&data_dir, &session_id, "reads-since-map");
                0
            } else {
                0
            };

            // Computed once per event and reused for `rc.rtk_active` further
            // below: on rtk machines the Bash wrap/rewrite stage defers to
            // rtk (H0), so the Bash-rail would-fire denominators must split
            // out events rtk would have deferred anyway (H4) rather than
            // count them against a near-zero live denominator.
            let rtk_active = crate::rtk::rtk_active(&data_dir);

            // Reroute-rail follower counters (six rails, grep-scope shape):
            // re-derive each rail's would-fire on THIS event with the same
            // classifier + gates route_inner uses, bump `{p}_would_fire`
            // regardless of the rail's flags, and arm the live-vs-shadow
            // follower marker the loop above consumes on the NEXT event. A rail
            // is LIVE when EITHER of its arms would fire at the current level —
            // the deny arm (under `steers()`) OR the nudge arm — so a deny-only
            // config (nudge flag `=0`) still lands its follower in `{p}_next_*`,
            // not `{p}_shadow_next_*`. gsym/rskel/rovr/elink nudges fire only at
            // `Level::Nudge` (their deny owns the steering levels); gast/bagg
            // nudges fire at every nudging level (their deny's shared one-shot
            // key prevents a double-fire), so their live union drops the
            // `!steers()` term — mirroring `route_inner` EXACTLY.
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
                // Bash-rail variant of `arm` (H4): bumps
                // `{prefix}_would_fire_rtk_deferred` instead of
                // `{prefix}_would_fire` when rtk is deferring this Bash call,
                // so dashboard adoption % is never computed against rtk's
                // near-zero live denominator.
                let arm_rtk_aware = |prefix: &str, enabled: bool| {
                    if let Some(s) = &stats_store {
                        let key = if rtk_active {
                            format!("{prefix}_would_fire_rtk_deferred")
                        } else {
                            format!("{prefix}_would_fire")
                        };
                        let _ = s.bump_stat(&key, 1);
                    }
                    let pk = if enabled {
                        format!("{prefix}-live-pending")
                    } else {
                        format!("{prefix}-shadow-pending")
                    };
                    routing::throttle::bump(&data_dir, &session_id, &pk);
                };
                // Live-arm gate per rail, matching route_inner EXACTLY. Nudge
                // arms are retired (T5); a rail is live iff its deny arm is
                // enabled at a steering level.
                let live = |deny: bool| deny && level.steers();
                match tool.as_str() {
                    "Grep" => {
                        let pat = ti.get("pattern").and_then(Value::as_str).unwrap_or("");
                        // Union level: gsym's nudge arm fires at Level::Nudge,
                        // so the classifier gate widens to nudges() (was
                        // steers() when gsym was deny-only).
                        let gsym_shape = level.nudges()
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
                                arm("gsym", live(routing::grep_symbol_deny_enabled()));
                            } else if gsym_shape {
                                if let Some(s) = &stats_store {
                                    let _ = s.bump_stat("gsym_graph_miss", 1);
                                }
                            }
                            if gast {
                                arm("gast", live(routing::grep_ast_deny_enabled()));
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
                        // arm that actually fires, not a looser one. Union level:
                        // the rskel nudge arm fires at Level::Nudge, so widen to
                        // nudges() (was steers() when rskel was deny-only).
                        let rskel = level.nudges()
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
                                arm("rskel", live(routing::read_skeleton_deny_enabled()));
                            }
                            if rovr {
                                arm("rovr", live(routing::read_overview_deny_enabled()));
                            }
                        }
                        // H5 would-fire mirror: offset/limit code Reads (T3's
                        // `read_is_analysis_shaped`) — the shape whose
                        // correct target is `lens_run_file`. No rtk split
                        // needed — rtk only defers Bash, not Read. No live
                        // deny arm exists yet, so `enabled` stays false — T5
                        // wires the real flag/classify gate when it lands.
                        if level.nudges()
                            && routing::reroute::read_skeleton::read_is_analysis_shaped(&ti)
                            && mcp_ready
                            && routing::index_present(&data_dir)
                        {
                            arm("read_runfile", false);
                        }
                    }
                    "Bash" => {
                        let cmd = ti.get("command").and_then(Value::as_str).unwrap_or("");
                        if level.nudges()
                            && routing::reroute::bash_aggregate::is_data_aggregate(cmd)
                            && mcp_ready
                            && routing::index_present(&data_dir)
                        {
                            arm_rtk_aware("bagg", live(routing::bash_agg_deny_enabled()));
                        }
                        // H1 would-fire mirror: grep-shaped Bash commands
                        // (T1's classifier), same rtk-active denominator
                        // split as bagg above. No live deny arm exists yet,
                        // so `enabled` stays false — T5 wires the real
                        // flag/classify gate into `bash_decision` when it
                        // lands.
                        if level.nudges()
                            && bash_grep_shape(cmd)
                            && mcp_ready
                            && routing::index_present(&data_dir)
                        {
                            arm_rtk_aware("bash_grep", false);
                        }
                    }
                    // Graph load only on Edit events, guarded — see
                    // `elink_would_fire` (which also honors the `elink:{sym}`
                    // one-shot the live deny/nudge consumes).
                    "Edit" | "MultiEdit"
                        if level.nudges()
                            && mcp_ready
                            && routing::index_present(&data_dir)
                            && elink_would_fire(&data_dir, &session_id, &tool, &ti) =>
                    {
                        arm("elink", live(routing::edit_links_deny_enabled()));
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
                rtk_active,
                reads_since_map,
            };
            let decision = routing::route(&tool, &ti, &rc);
            Ok(routing::to_hook_json(&decision).to_string())
        }
        "PostToolUse" => {
            let store = open_store()?;
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
            let is_lens = normalize_tool_name(&tool).starts_with("lens_");
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
                let store = open_store()?;
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
            let store = open_store()?;
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
            let store = open_store()?;
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
            // booting, so its heartbeat file usually isn't written yet. Gating
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

/// One-line pointer to the durable-memory MCP tools for a fresh SessionStart.
/// lens never injects the memory itself (retrieve-on-demand); this tells the
/// session the tools exist and, when notes are on record, how many and how
/// recent — a recency signal so it can decide whether `lens_memory_query()`
/// (newest last) is worth a call, without paying to inject stale content.
fn memory_tools_hint(count: i64, latest_ts: Option<i64>, now: i64) -> String {
    match latest_ts {
        Some(ts) if count > 0 => format!(
            "({count} durable project notes on record (most recent {}). Read them with \
             ToolSearch(query: \"select:lens_memory_query,lens_memory_record\") then \
             lens_memory_query() (newest last) when a prior decision/constraint/\
             rejected-approach/rule is relevant.)",
            rel_age(now.saturating_sub(ts))
        ),
        _ => "(No durable project memory yet. To record one that outlives the session: \
              ToolSearch(query: \"select:lens_memory_query,lens_memory_record\") then \
              lens_memory_record(category, text) — decision/constraint/rejected-approach/rule.)"
            .to_string(),
    }
}

/// Coarse "N ago" for the memory-hint recency cue (unix-second delta).
fn rel_age(delta: i64) -> String {
    match delta {
        d if d < 90 => "just now".to_string(),
        d if d < 90 * 60 => format!("{}m ago", d / 60),
        d if d < 36 * 3600 => format!("{}h ago", d / 3600),
        d => format!("{}d ago", d / 86400),
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
            // Post-compact the SessionStart digest may be summarized away;
            // clearing ovrb:digest so the re-buy rail does not deny a map the
            // model no longer has.
            routing::throttle::reset(data_dir, session_id, "ovrb:digest");
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
            // Same as compact: a resumed session may lack the original digest.
            routing::throttle::reset(data_dir, session_id, "ovrb:digest");
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
            // Durable project memory is deliberately NOT injected here. lens is
            // retrieve-on-demand; dumping every accreted decision/constraint (most of
            // them months old) into a cold session is exactly the mandatory-injection
            // cost lens exists to avoid. Instead point the session at the memory tools
            // with a live count + recency cue, so it can pull the recent ones via
            // lens_memory_query() iff a prior note is relevant.
            let (mem_count, mem_latest) = store.project_memory_summary(project_str)?;
            let hint = memory_tools_hint(mem_count, mem_latest, ts);
            Ok(match repo_map_block(data_dir) {
                Some(block) => {
                    // Digest actually injected → arm the ovrb re-buy rail.
                    routing::throttle::mark(data_dir, session_id, "ovrb:digest");
                    format!("{block}\n\n{hint}")
                }
                None => hint,
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
    let digest = cap_bytes(
        &crate::discovery::query::overview(&graph, budget, &std::collections::HashMap::new()),
        REPO_MAP_CAP_BYTES,
    );
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

    // Serializes the two tests here that mutate process-global routing env
    // (`LENS_ROUTING` / `LENS_ROUTING_MCP` / rail flags) so they can't race each
    // other. No other lib test mutates `LENS_ROUTING`; the mod.rs `mcp_ready`
    // test's `LENS_ROUTING_MCP` window is dodged by forcing the env override
    // right before the read. Poison-tolerant so one panicking test can't cascade.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    thread_local! {
        // See `disabled_entry_for`: a per-thread stand-in for `disabled::covering_entry`
        // so the disabled-project test doesn't have to mutate `$LENS_HOME`.
        static DISABLED_OVERRIDE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
    }

    pub(super) fn disabled_override() -> Option<PathBuf> {
        DISABLED_OVERRIDE.with(|o| o.borrow().clone())
    }

    /// Runs `f` with `DISABLED_OVERRIDE` set to `entry` for the current test
    /// thread, then clears it again (even on panic) so a later test recycled
    /// onto the same pooled thread never inherits it.
    fn with_disabled_override<T>(entry: PathBuf, f: impl FnOnce() -> T) -> T {
        DISABLED_OVERRIDE.with(|o| *o.borrow_mut() = Some(entry));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        DISABLED_OVERRIDE.with(|o| *o.borrow_mut() = None);
        match result {
            Ok(v) => v,
            Err(e) => std::panic::resume_unwind(e),
        }
    }

    /// Every call in this file that resolves a data dir goes through
    /// `obs::data_dir_for`, which falls back to `rtk::home_root()`
    /// (`$LENS_HOME`, process-global) once a fresh tempdir project has no
    /// `LENS_DIR` pin and no pre-existing in-tree artifacts. Other test files
    /// (`session::store`, `obs`, `rtk`) mutate `$LENS_HOME` under
    /// `crate::rtk::env_test_lock()`; every test here that resolves a data dir
    /// must hold the SAME lock for its whole body, or a concurrently-running
    /// mutator can flip the value mid-test and two calls for the same nominal
    /// project resolve to two different actual directories.
    fn run(event: &str, input: HookInput) -> (String, SessionStore, PathBuf) {
        let dir = input.project();
        let data_dir = super::super::resolve_data_dir(&dir);
        let out = handle("claude", event, &input).unwrap();
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

    /// A project covered by the `lens off` denylist makes SessionStart return the
    /// off note (naming the covering entry) instead of the usual guide, and
    /// leaves no data dir behind anywhere -- the disabled check runs before
    /// `resolve_data_dir` is ever called, so nothing is created in the tree or
    /// under the (temp, injected) home. Mutates process-global `LENS_HOME`, so
    /// it's serialized against every other test that does the same via the
    /// crate-wide `env_test_lock`.
    #[test]
    fn sessionstart_on_disabled_project_returns_off_note_and_creates_no_data_dir() {
        let project = tempdir().unwrap();
        let entry = project.path().to_path_buf();

        let out = with_disabled_override(entry.clone(), || {
            handle("claude", "SessionStart", &input_for(project.path())).unwrap()
        });
        assert!(out.contains("lens off:"), "{out}");
        assert!(out.contains("is disabled"), "{out}");
        assert!(out.contains(&entry.display().to_string()), "{out}");
        assert!(!project.path().join(".lens").exists());

        // Every other event idles the same way: no store writes, no `.lens`.
        let post_out = with_disabled_override(entry, || {
            let mut edit = input_for(project.path());
            edit.tool_name = Some("Edit".into());
            handle("claude", "PostToolUse", &edit).unwrap()
        });
        assert_eq!(post_out, "{}");
        assert!(!project.path().join(".lens").exists());
    }

    /// SessionStart registers the root -> data-dir pair in the machine-global
    /// registry: the hook can be the only plane that ever opens this dir (MCP
    /// server absent or failed), and an unregistered central dir reads to
    /// `lens clean` as an orphan it would reclaim out from under a live
    /// project. Mutates process-global `LENS_HOME` (and lifts the
    /// `LENS_NO_GLOBAL_MIRROR` that `.cargo/config.toml` sets for every
    /// cargo-launched process, which the registry respects), so it holds the
    /// crate-wide `env_test_lock` like every other mutator.
    #[test]
    fn sessionstart_records_the_data_dir_in_the_registry() {
        let _guard = crate::rtk::env_test_lock();
        let prev_home = std::env::var_os("LENS_HOME");
        let prev_mirror = std::env::var_os("LENS_NO_GLOBAL_MIRROR");
        let home = tempdir().unwrap();
        std::env::set_var("LENS_HOME", home.path());
        std::env::remove_var("LENS_NO_GLOBAL_MIRROR");

        let project = tempdir().unwrap();
        let input = input_for(project.path());
        let dir = input.project();
        let out = handle("claude", "SessionStart", &input);
        let expected = format!(
            "{}\t{}",
            dir.display(),
            super::super::resolve_data_dir(&dir).display()
        );
        let raw = std::fs::read_to_string(home.path().join("registry.tsv")).unwrap_or_default();

        match prev_home {
            Some(v) => std::env::set_var("LENS_HOME", v),
            None => std::env::remove_var("LENS_HOME"),
        }
        if let Some(v) = prev_mirror {
            std::env::set_var("LENS_NO_GLOBAL_MIRROR", v);
        }

        out.unwrap();
        assert!(
            raw.lines().any(|l| l == expected),
            "SessionStart must register the data dir, got: {raw:?}"
        );
    }

    /// A giant marker-less dir (home-directory shape) makes the hook idle:
    /// SessionStart returns only the one-line idle note, no `.lens` data dir is
    /// created in the tree, and every other event gets its default no-op.
    #[test]
    fn unscoped_project_idles_hook_without_lens_dir() {
        let dir = tempdir().unwrap();
        for i in 0..10_001 {
            std::fs::write(dir.path().join(format!("f{i}")), "").unwrap();
        }
        let out = handle("claude", "SessionStart", &input_for(dir.path())).unwrap();
        assert!(out.contains("lens idle"), "{out}");
        assert!(!dir.path().join(".lens").exists());
        let mut input = input_for(dir.path());
        input.tool_name = Some("Edit".into());
        assert_eq!(handle("claude", "PostToolUse", &input).unwrap(), "{}");
        assert!(!dir.path().join(".lens").exists());
    }

    #[test]
    fn posttooluse_stores_file_event_and_returns_empty_obj() {
        let _guard = crate::rtk::env_test_lock();
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
        let _guard = crate::rtk::env_test_lock();
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
        let _guard = crate::rtk::env_test_lock();
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
        let _guard = crate::rtk::env_test_lock();
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
        let _guard = crate::rtk::env_test_lock();
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

        handle("claude", "PostToolUse", &input).unwrap();

        // Canonical data dir resolves for the repo root (the shared `obs`
        // resolver, not a hardcoded in-tree path); nothing scattered under the
        // subdir either way.
        let data_dir = super::super::resolve_data_dir(repo.path());
        assert!(data_dir.is_dir());
        assert!(!subdir.join(".lens").exists());
        assert!(!repo.path().join("Sources").join(".lens").exists());
    }

    #[test]
    fn userpromptsubmit_skips_system_messages() {
        let _guard = crate::rtk::env_test_lock();
        let dir = tempdir().unwrap();
        let mut input = input_for(dir.path());
        input.prompt = Some("<system-reminder>noise</system-reminder>".into());
        let (_out, store, _) = run("UserPromptSubmit", input);
        assert_eq!(store.events_for_session("sess1").unwrap().len(), 0);
    }

    #[test]
    fn precompact_builds_and_stores_snapshot() {
        let _guard = crate::rtk::env_test_lock();
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
        let _guard = crate::rtk::env_test_lock();
        let dir = tempdir().unwrap();
        let mut t = input_for(dir.path());
        t.tool_name = Some("Edit".into());
        t.tool_input = Some(json!({"file_path": "cache.rs"}));
        t.tool_response = Some(json!("ok"));
        run("PostToolUse", t);
        run("PreCompact", input_for(dir.path()));

        let mut ss = input_for(dir.path());
        ss.source = Some("compact".into());
        let out = handle("claude", "SessionStart", &ss).unwrap();
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
        let _guard = crate::rtk::env_test_lock();
        let dir = tempdir().unwrap();
        let mut t = input_for(dir.path());
        t.tool_name = Some("Edit".into());
        t.tool_input = Some(json!({"file_path": "old.rs"}));
        t.tool_response = Some(json!("ok"));
        run("PostToolUse", t);

        let mut ss = input_for(dir.path());
        ss.source = Some("startup".into());
        handle("claude", "SessionStart", &ss).unwrap();

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
        // fresh heartbeat. In a fresh worktree the MCP server is still booting when
        // this hook fires, so no heartbeat file exists yet (mcp_ready == false) — yet
        // the guide has to inject anyway, or the model never learns to use the ctx
        // tools. tempdir() has no heartbeats dir, so mcp_ready is false here; the guide
        // must still appear. (LENS_ROUTING is read by no other test.)
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _rtk_guard = crate::rtk::env_test_lock();
        let dir = tempdir().unwrap();
        let prev = std::env::var("LENS_ROUTING").ok();
        std::env::set_var("LENS_ROUTING", "full");

        let mut ss = input_for(dir.path());
        ss.source = Some("startup".into());
        let out = handle("claude", "SessionStart", &ss).unwrap();

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

    // T4 attribution fix: a deny-only config (nudge flag `=0`, deny arm ON by
    // default) at a steering level must attribute the reroute follower to
    // `gast_next_*` (LIVE), NOT `gast_shadow_next_*`. Before the fix the mirror
    // armed on the NUDGE flag alone, so a deny-only gast landed in shadow.
    #[test]
    fn deny_only_rail_attributes_follower_live_not_shadow() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Also serializes with mod.rs's `mcp_ready` tests: both mutate the
        // process-global `LENS_ROUTING_MCP` var and run in the same `cargo
        // test --lib` binary, so a single crate-wide lock is required.
        let _mcp_guard = crate::routing::MCP_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _rtk_guard = crate::rtk::env_test_lock();
        let dir = tempdir().unwrap();
        let data_dir = super::super::resolve_data_dir(dir.path());
        std::fs::create_dir_all(&data_dir).unwrap();
        crate::routing::seed_index(&data_dir); // index_present() == true

        let prev_routing = std::env::var("LENS_ROUTING").ok();
        let prev_mcp = std::env::var("LENS_ROUTING_MCP").ok();
        let prev_nudge = std::env::var("LENS_GREP_AST_NUDGE").ok();
        let prev_deny = std::env::var("LENS_GREP_AST_DENY").ok();
        std::env::set_var("LENS_ROUTING", "full"); // steers()
        std::env::set_var("LENS_ROUTING_MCP", "up"); // mcp_ready() == true
        std::env::set_var("LENS_GREP_AST_NUDGE", "0"); // nudge arm OFF
        std::env::remove_var("LENS_GREP_AST_DENY"); // deny arm ON (default)

        // Event 1: a syntax-shaped Grep — the gast classifier hits and, because
        // the deny arm is live at `full`, the mirror arms `gast-live-pending`.
        let mut g = input_for(dir.path());
        g.tool_name = Some("Grep".into());
        g.tool_input = Some(json!({"pattern": "impl Forge"}));
        handle("claude", "PreToolUse", &g).unwrap();

        // Event 2: the compliant follower (a lens call). The follower proxy
        // consumes `gast-live-pending` and bumps `gast_next_lens`.
        let mut f = input_for(dir.path());
        f.tool_name = Some("mcp__lens__lens_grep_ast".into());
        f.tool_input = Some(json!({}));
        handle("claude", "PreToolUse", &f).unwrap();

        // Restore env before asserting (an assert panic must not leak state).
        let restore = |k: &str, v: Option<String>| match v {
            Some(v) => std::env::set_var(k, v),
            None => std::env::remove_var(k),
        };
        restore("LENS_ROUTING", prev_routing);
        restore("LENS_ROUTING_MCP", prev_mcp);
        restore("LENS_GREP_AST_NUDGE", prev_nudge);
        restore("LENS_GREP_AST_DENY", prev_deny);

        let store = crate::store::Store::open(&data_dir).unwrap();
        let sum = |prefix: &str| -> i64 {
            ["lens", "grep", "read", "bash", "edit", "other"]
                .iter()
                .map(|c| store.get_stat(&format!("{prefix}_{c}")).unwrap())
                .sum()
        };
        assert_eq!(
            store.get_stat("gast_would_fire").unwrap(),
            1,
            "gast would-fire is counted exactly once"
        );
        assert_eq!(
            sum("gast_next"),
            1,
            "deny-only gast follower must land LIVE (gast_next_*)"
        );
        assert_eq!(
            sum("gast_shadow_next"),
            0,
            "deny-only gast follower must NOT land in shadow (gast_shadow_next_*)"
        );
    }

    #[test]
    fn opencode_hook_posttooluse_with_bare_lens_tool_is_safe_and_recognizes_in_is_lens() {
        let _guard = crate::rtk::env_test_lock();
        let dir = tempdir().unwrap();
        let mut input = input_for(dir.path());
        input.tool_name = Some("lens_search".into());
        input.tool_input = Some(json!({}));
        input.tool_response = Some(json!("ok"));
        // uses platform=opencode, bare lens_* tool; is_lens path now hits via normalize
        let out = handle("opencode", "PostToolUse", &input).unwrap();
        assert_eq!(out, "{}");
    }

    #[test]
    fn follower_class6_recognizes_bare_lens_and_mcp_equiv() {
        let ti = json!({});
        assert_eq!(follower_class6("lens_search", &ti), "lens");
        assert_eq!(follower_class6("lens_map", &ti), "lens");
        assert_eq!(follower_class6("lens_overview", &ti), "lens");
        assert_eq!(follower_class6("mcp__lens__lens_search", &ti), "lens");
        assert_eq!(follower_class6("Grep", &ti), "grep");
        // opencode bare never matches ToolSearch wrapper (direct lens_*)
        assert_eq!(follower_class6("lens_foo", &json!({"query": "lens"})), "lens");
    }

    #[test]
    fn normalize_tool_name_covers_bare_vs_mcp_prefix() {
        assert_eq!(normalize_tool_name("mcp__lens__lens_search"), "lens_search");
        assert_eq!(normalize_tool_name("mcp__lens__foo_bar"), "lens_foo_bar");
        assert_eq!(normalize_tool_name("lens_index"), "lens_index");
        assert_eq!(normalize_tool_name("lens_"), "lens_");
        assert_eq!(normalize_tool_name("Edit"), "Edit");
        assert_eq!(normalize_tool_name("Bash"), "Bash");
    }

    #[test]
    fn opencode_event_handling_returns_empty_for_unsupported() {
        let _guard = crate::rtk::env_test_lock();
        let dir = tempdir().unwrap();
        let mut input = input_for(dir.path());
        input.tool_name = Some("lens_search".into());
        // unsupported event for opencode platform must safe-return {}
        let out = handle("opencode", "SessionEnd", &input).unwrap();
        assert_eq!(out, "{}");
        // supported but non-lens event ok
        let out2 = handle("opencode", "UserPromptSubmit", &input).unwrap();
        // may store or {}, but must not panic
        assert!(out2 == "{}" || out2.contains("hookSpecificOutput"));
    }
}
