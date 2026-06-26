//! PreToolUse routing policy — pass through, deny, rewrite, or nudge a tool call.
//!
//! Gated by `LENS_ROUTING` (off|nudge|steer|wrap|full); default `full`. Concerns layer:
//!
//!   * **nudge** — emit once-per-session non-blocking nudges toward lens tools for
//!     `Bash` / `Grep` / `Read` (structurally-bounded commands are skipped); a
//!     periodic nudge for external (non-lens) MCP tools; inject the tool-selection
//!     guide into every sub-agent (`Agent`/`Task`) prompt and at `SessionStart`.
//!     Never denies, redirects, or rewrites a call. (Ports of context-mode's
//!     `hooks/core/routing.mjs`.)
//!   * **steer** — nudge, plus deny `WebFetch` and redirect curl/wget/build/
//!     inline-HTTP `Bash` commands into `lens_run`.
//!   * **wrap** — transparently rewrite a read-only, high-output `Bash` command
//!     into `lens wrap -- <cmd>` so its output is offloaded losslessly.
//!   * **full** — both steer and wrap.
//!
//! Safety rails: MCP-redirect decisions (WebFetch deny, curl/build rewrites) are
//! gated on [`mcp_ready`] via [`mcp_redirect`] so the agent is never sent to a dead
//! tool (nudges + sub-agent injection fire regardless); and stateful shell commands
//! (anything that mutates shell state — `cd`, `export`, assignments, …) are always
//! passed through untouched, because rewriting them would silently change behavior.

use std::path::Path;

use serde_json::{json, Value};

mod classify;
mod log;
pub mod throttle;

pub use classify::is_structurally_bounded;

/// Active routing level, parsed from `LENS_ROUTING`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// True no-op: PreToolUse returns `{}`, SessionStart unchanged.
    Off,
    /// Emit one-shot nudges + inject the SessionStart guide, but never deny,
    /// redirect, or rewrite a call. The least-surprising way to drive adoption.
    Nudge,
    /// Nudge, plus deny WebFetch + redirect curl/build Bash into the darkroom.
    Steer,
    /// Transparently wrap read-only high-output Bash commands.
    Wrap,
    /// Steer and wrap together.
    Full,
}

impl Level {
    /// Parse a level name (case-insensitive, surrounding whitespace trimmed).
    /// Anything unrecognized — including the empty string — is [`Level::Off`].
    pub fn parse(s: &str) -> Level {
        match s.trim().to_ascii_lowercase().as_str() {
            "nudge" => Level::Nudge,
            "steer" => Level::Steer,
            "wrap" => Level::Wrap,
            "full" => Level::Full,
            _ => Level::Off,
        }
    }

    /// Read the level from `LENS_ROUTING` (unset => `full`).
    pub fn from_env() -> Level {
        Level::parse(&std::env::var("LENS_ROUTING").unwrap_or_else(|_| "full".to_string()))
    }

    /// Whether this level redirects: WebFetch deny + curl/build Bash rewrites.
    /// Nudges are gated by [`Level::nudges`] instead, so `Nudge` is excluded here.
    pub(crate) fn steers(self) -> bool {
        matches!(self, Level::Steer | Level::Full)
    }

    /// Whether this level emits nudges (Bash/Grep/Read/Agent/external-MCP/
    /// grep-flood) and injects the SessionStart guide. `Nudge` does this without
    /// denying or redirecting anything.
    pub(crate) fn nudges(self) -> bool {
        matches!(self, Level::Nudge | Level::Steer | Level::Full)
    }

    /// Whether this level rewrites read-only Bash commands into `lens wrap`.
    pub(crate) fn wraps(self) -> bool {
        matches!(self, Level::Wrap | Level::Full)
    }
}

/// The outcome of routing one tool call.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Let the tool run unchanged (serializes to `{}`).
    Passthrough,
    /// Block the tool; `String` is the reason shown to the model.
    Deny(String),
    /// Allow the tool but run it with `updated_input` instead of the original.
    Modify {
        reason: String,
        updated_input: Value,
    },
    /// Inject extra context without blocking or modifying (a soft nudge).
    Context(String),
}

/// Render a [`Decision`] into the exact Claude Code PreToolUse hook JSON.
///
/// The shape follows the public PreToolUse contract: a `hookSpecificOutput`
/// object tagged with `hookEventName: "PreToolUse"`. `permissionDecision`
/// (`deny`/`allow`) makes the hook authoritative for that call; omitting it
/// (the `Context` arm) leaves the call permitted and only appends context.
pub fn to_hook_json(d: &Decision) -> Value {
    match d {
        Decision::Passthrough => json!({}),
        Decision::Deny(reason) => json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        }),
        Decision::Modify {
            reason,
            updated_input,
        } => json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "allow",
                "permissionDecisionReason": reason,
                "updatedInput": updated_input,
            }
        }),
        Decision::Context(ctx) => json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "additionalContext": ctx,
            }
        }),
    }
}

/// Render a PostToolUse nudge. [`to_hook_json`] hardcodes `PreToolUse`, so the
/// PostToolUse arm ([`post_route`]) needs its own renderer. Only `Context` carries a
/// payload; anything else is an empty (no-op) object.
pub fn to_post_hook_json(d: &Decision) -> Value {
    match d {
        Decision::Context(ctx) => json!({
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "additionalContext": ctx,
            }
        }),
        _ => json!({}),
    }
}

/// Short decision tag for the routing log.
fn decision_label(d: &Decision) -> String {
    match d {
        Decision::Passthrough => "passthrough",
        Decision::Deny(_) => "deny",
        Decision::Modify { .. } => "modify",
        Decision::Context(_) => "context",
    }
    .to_string()
}

/// Decision reason for the routing log (empty for passthrough; a tag for nudges).
fn decision_reason(d: &Decision) -> String {
    match d {
        Decision::Passthrough => String::new(),
        Decision::Deny(r) | Decision::Modify { reason: r, .. } => r.clone(),
        Decision::Context(_) => "nudge".to_string(),
    }
}

/// Everything [`route`] needs that isn't the tool call itself.
pub struct RouteCtx<'a> {
    /// Active routing level.
    pub level: Level,
    /// Whether the MCP server is currently reachable (the master safety gate).
    pub mcp_ready: bool,
    /// Absolute path to the `lens` binary (for the wrap rewrite).
    pub bin: &'a str,
    /// lens data dir (holds throttle markers + `server.pid`).
    pub data_dir: &'a Path,
    /// Current session id (scopes one-shot nudge throttling).
    pub session_id: &'a str,
    /// True when RTK owns Bash (see [`crate::rtk::rtk_active`]); makes [`route`]
    /// pass Bash through so RTK's hook and lens's never double-wrap.
    pub rtk_active: bool,
}

// ── Reason / nudge prose (original wording) ────────────────────────────────

/// Shown when a WebFetch is denied under steering.
pub const WEBFETCH_REASON: &str = "lens routing: fetch+process web content in the darkroom instead — use lens_run (python) to fetch the URL and print only what you need; the full response stays out of context and is recoverable via lens_recall.";

/// Shown when a read-only Bash command is wrapped under `wrap`/`full`.
pub const WRAP_REASON: &str = "lens: wrapped a read-only command to offload large output losslessly (recover full output via lens_recall).";

/// Shown when a network fetch (curl/wget/inline HTTP) is redirected into lens_run.
pub const NET_REDIRECT_REASON: &str = "lens routing: redirected a network fetch into the lens_run darkroom (the raw response stays out of context).";

/// Shown when a build command (gradle/mvn/sbt) is redirected into lens_run.
pub const BUILD_REDIRECT_REASON: &str = "lens routing: redirected a build command into the lens_run darkroom (the verbose log stays out of context).";

/// Shown when the tool-selection guide is injected into a sub-agent prompt.
pub const AGENT_INJECT_REASON: &str = "lens routing: injected the tool-selection guide into the sub-agent prompt so it reaches for lens tools.";

// Per-tool `<context_guidance>` injected on PreToolUse (prose adapted from
// context-mode's routing-block factory functions; tool names mapped to lens).
// Re-injected periodically (see `throttle_periodic`), not once per session.

/// Contextual guidance when a read-only/high-output Bash command is observed.
pub const BASH_NUDGE: &str = "<context_guidance>\n  <tip>\n    When you intend to PROCESS the output (filter, count, parse, aggregate), use lens_run(language: \"shell\", code: \"...\") — the raw output stays in the darkroom and only what you print enters your conversation. Bash stays the right surface when you intend to OBSERVE a short fixed output or when you are mutating state (git, mkdir, rm, mv, navigation).\n  </tip>\n</context_guidance>";

/// Contextual guidance steering Grep toward indexed search / the graph.
pub const GREP_NUDGE: &str = "<context_guidance>\n  <tip>\n    Grep results may be larger than you expect. When you intend to count, filter, or aggregate matches (not just spot-check one), run the search through lens_run(language: \"shell\", code: \"...\") — the raw match list stays in the darkroom and only your derived answer enters your conversation. For \"where is X\" or \"who calls X\", prefer lens_search (after lens_index) or lens_symbol over scanning files.\n  </tip>\n</context_guidance>";

/// Contextual guidance steering analysis-reads into the darkroom, and
/// navigational code-reads toward the graph.
pub const READ_NUDGE: &str = "<context_guidance>\n  <tip>\n    Reading to Edit the file? Read is correct — Edit needs the exact bytes in your conversation to match against.\n    Reading to analyze, summarize, or extract from one file? Use lens_run_file(path, language, code) — the bytes stay in the darkroom and only what your code prints enters your conversation.\n    Reading one file to see its API (signatures, types, what's defined) without the bodies? Use lens_skeleton(path): signatures + nesting at a fraction of the tokens, the full file one lens_recall away.\n    Reading code to see how it connects — who calls this, what it calls, where a symbol is defined, how A reaches B? Don't read file after file; query the graph with lens_symbol / lens_links / lens_path (run lens_map once if it's empty).\n  </tip>\n</context_guidance>";

/// Contextual guidance emitted AFTER a Grep whose result set floods context. A
/// result this large is exactly where lens_search (ranked top-K, flat with corpus
/// size) beats grep (every matching line). PostToolUse, not before, so it fires only
/// when the grep actually flooded — below the threshold grep is as lean and we stay
/// quiet.
pub const SEARCH_NUDGE: &str = "<context_guidance>\n  <tip>\n    That grep returned a large match set — more than lens_search would. For a result set this size, lens_search (after lens_index) returns the ranked top hits and keeps the rest out of your context; re-run the search through lens_search if you need more than the matches already shown.\n  </tip>\n</context_guidance>";

/// Periodic guidance for external (non-lens) MCP tools whose payloads flood
/// context (port of context-mode's `createExternalMcpGuidance`, tools mapped to
/// lens's).
pub const EXTERNAL_MCP_NUDGE: &str = "<context_guidance>\n  <tip>\n    External MCP tools commonly return large payloads (channel history, file content, search results) that enter your conversation in full. When you intend to filter, count, or aggregate that data, pipe it through lens_run(language, code) — the raw payload stays in the darkroom and only the derived answer enters your conversation. For content you'll query later, lens_index it then lens_search(queries).\n  </tip>\n</context_guidance>";

/// Port of context-mode's `mcpRedirect` (core/routing.mjs #230): a decision that
/// redirects the agent to an MCP-backed tool (deny WebFetch, rewrite curl/build into
/// `lens_run`) is only safe to emit when the server is reachable — otherwise the
/// agent is told to use a tool that isn't there and stalls. So gate ONLY these on
/// `mcp_ready`; nudges and sub-agent injection are NOT gated (they fire regardless,
/// matching context-mode, which never wraps `guidanceOnce`/Agent in `mcpRedirect`).
fn mcp_redirect(ctx: &RouteCtx, d: Decision) -> Decision {
    if ctx.mcp_ready {
        d
    } else {
        Decision::Passthrough
    }
}

/// Route one PreToolUse call to a [`Decision`].
///
/// `Off` short-circuits to [`Decision::Passthrough`]. There is NO blanket
/// `!mcp_ready` gate (that was a divergence from context-mode): readiness gates
/// only the MCP-redirect decisions via [`mcp_redirect`], so nudges and sub-agent
/// injection keep firing even before the server's heartbeat lands.
pub fn route(tool: &str, tool_input: &Value, ctx: &RouteCtx) -> Decision {
    let decision = route_inner(tool, tool_input, ctx);
    if log::enabled() {
        log::emit(
            ctx.data_dir,
            log::RoutingEvent {
                session: ctx.session_id.to_string(),
                tool: tool.to_string(),
                cmd: tool_input
                    .get("command")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                decision: decision_label(&decision),
                reason: decision_reason(&decision),
            },
        );
    }
    decision
}

/// The routing policy; [`route`] wraps this to emit the optional routing log.
fn route_inner(tool: &str, tool_input: &Value, ctx: &RouteCtx) -> Decision {
    if ctx.level == Level::Off {
        return Decision::Passthrough;
    }

    match tool {
        "WebFetch" => {
            if ctx.level.steers() {
                mcp_redirect(ctx, Decision::Deny(WEBFETCH_REASON.to_string()))
            } else {
                Decision::Passthrough
            }
        }
        "Bash" => {
            if ctx.rtk_active {
                Decision::Passthrough
            } else {
                bash_decision(tool_input, ctx)
            }
        }
        "Grep" => {
            if ctx.level.nudges() && nudge_once(ctx, "grep") {
                Decision::Context(GREP_NUDGE.to_string())
            } else {
                Decision::Passthrough
            }
        }
        // Read nudges whenever steering. A general analysis tip fires once per
        // session; on top of that, code-file reads escalate toward the graph as
        // they pile up (the "reading file after file to trace structure" pattern
        // the graph replaces). See [`read_decision`].
        "Read" => read_decision(tool_input, ctx),
        // Sub-agents never receive the SessionStart guide, so without this they
        // default to Read/Grep/Bash and never touch the lens tools. Inject the
        // guide into the sub-agent's prompt (every call — each is a fresh context).
        "Agent" | "Task" => {
            if ctx.level.nudges() {
                agent_inject(tool_input, ctx)
            } else {
                Decision::Passthrough
            }
        }
        // External (non-lens) MCP tools return large payloads (channel history,
        // file content, search results). Periodically nudge toward lens_run — a
        // single one-shot nudge gets lost in long MCP-heavy sessions (port of
        // context-mode's #529/#567 periodic external-MCP guidance).
        other if ctx.level.nudges() && is_external_mcp_tool(other) => {
            if throttle_periodic(ctx, "external-mcp", EXTERNAL_MCP_PERIOD) {
                Decision::Context(EXTERNAL_MCP_NUDGE.to_string())
            } else {
                Decision::Passthrough
            }
        }
        _ => Decision::Passthrough,
    }
}

/// PostToolUse routing: a Grep whose result floods context gets a one-shot nudge
/// toward lens_search. This is the scale-aware search steer — lens_search only beats
/// grep once the match set is large (measured crossover: ~parity at fixture scale,
/// ~91% leaner than grep at 10x). So unlike the PreToolUse Grep nudge (which fires
/// before the result size is known), this fires only when the grep actually flooded.
/// Steering-only; not gated on `mcp_ready` (a nudge, like the graph escalation).
/// `tool_response` is the serialized Grep result. One-shot per session.
pub fn post_route(tool: &str, tool_response: &str, ctx: &RouteCtx) -> Decision {
    if !ctx.level.nudges() {
        return Decision::Passthrough;
    }
    if tool == "Grep"
        && tool_response.len() > grep_flood_bytes()
        && nudge_once(ctx, "grep-flood")
    {
        Decision::Context(SEARCH_NUDGE.to_string())
    } else {
        Decision::Passthrough
    }
}

/// Inject the lens tool-selection guide into a sub-agent's prompt. Port of
/// context-mode's PreToolUse `Agent` branch (`hooks/core/routing.mjs`). No
/// throttle: each sub-agent is a fresh context that needs its own copy, including
/// the block's ToolSearch bootstrap so the deferred ctx_* tools are loadable
/// inside the sub-agent (which doesn't inherit the parent's loaded schemas).
fn agent_inject(tool_input: &Value, ctx: &RouteCtx) -> Decision {
    // The Agent tool carries the sub-agent instructions under one of these keys
    // (Claude uses `prompt`; the rest mirror context-mode's field list).
    const FIELDS: &[&str] = &[
        "prompt",
        "request",
        "objective",
        "question",
        "query",
        "task",
    ];
    let field = match FIELDS
        .iter()
        .copied()
        .find(|f| tool_input.get(*f).and_then(Value::as_str).is_some())
    {
        Some(f) => f,
        None => return Decision::Passthrough, // unknown sub-agent shape — leave it
    };
    let original = tool_input[field].as_str().unwrap_or("");
    let mut updated = tool_input.clone();
    updated[field] = Value::String(format!("{original}\n\n{}", session_block(ctx.level)));
    Decision::Modify {
        reason: AGENT_INJECT_REASON.to_string(),
        updated_input: updated,
    }
}

/// After this many code-file reads in a session, the Read nudge escalates from
/// the general tip to a graph-specific one — the point where "reading file after
/// file to trace structure" is clearly underway and the graph wins. Past it, the
/// graph nudge repeats every [`READ_GRAPH_PERIOD`]-th code read so it keeps
/// landing without firing on every single read.
const READ_GRAPH_THRESHOLD_DEFAULT: u64 = 3;
const READ_GRAPH_PERIOD: u64 = 3;

/// The escalation threshold, overridable via `LENS_READ_GRAPH_THRESHOLD` so
/// an A/B can disable the graph escalation (set it very high) without a recompile.
/// Falls back to [`READ_GRAPH_THRESHOLD_DEFAULT`] when unset or unparseable.
fn read_graph_threshold() -> u64 {
    std::env::var("LENS_READ_GRAPH_THRESHOLD")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(READ_GRAPH_THRESHOLD_DEFAULT)
}

/// Grep result-size (bytes) above which the result is a "flood" worth steering to
/// lens_search, overridable via `LENS_GREP_FLOOD_BYTES` (so an A/B can disable it
/// by setting it very high). Default 16384: comfortably above lens_search's flat
/// ranked-top-K payload, so we only nudge once grep is the heavier option. Below this,
/// grep is as lean and the nudge stays silent. (Measured crossover: grep ~parity at
/// fixture scale, ~91% heavier than lens_search at 10x.)
const GREP_FLOOD_BYTES_DEFAULT: usize = 16384;
fn grep_flood_bytes() -> usize {
    std::env::var("LENS_GREP_FLOOD_BYTES")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(GREP_FLOOD_BYTES_DEFAULT)
}

/// Read routing: a general analysis tip once per session, plus escalation of
/// code-file reads toward the graph. Only files the graph indexes
/// ([`crate::discovery::extract::spec_for_extension`]) count toward escalation —
/// reading a doc/config/data file shouldn't push the agent at the graph. Once the
/// session's code-read count crosses [`READ_GRAPH_THRESHOLD`] the graph-specific
/// nudge fires, then again every [`READ_GRAPH_PERIOD`]-th read after.
fn read_decision(tool_input: &Value, ctx: &RouteCtx) -> Decision {
    if !ctx.level.nudges() {
        return Decision::Passthrough;
    }
    let is_code = tool_input["file_path"]
        .as_str()
        .and_then(file_extension)
        .map(|ext| crate::discovery::extract::spec_for_extension(&ext).is_some())
        .unwrap_or(false);
    if is_code {
        let n = throttle::bump(ctx.data_dir, ctx.session_id, "read-code");
        let threshold = read_graph_threshold();
        if n >= threshold && (n - threshold).is_multiple_of(READ_GRAPH_PERIOD) {
            return Decision::Context(read_graph_nudge(n));
        }
    }
    if nudge_once(ctx, "read") {
        Decision::Context(READ_NUDGE.to_string())
    } else {
        Decision::Passthrough
    }
}

/// The escalation nudge: names the three graph tools and frames them as the
/// replacement for reading file-by-file. `n` is the running code-read count, so
/// the agent sees how much reading it has already done.
fn read_graph_nudge(n: u64) -> String {
    format!(
        "<context_guidance>\n  <tip>\n    You've read {n} code files this session. If you're tracing how the code fits together — who calls a function, what it calls, where a symbol is defined, how one part reaches another — stop reading file by file and query the graph instead: lens_symbol to locate a symbol, lens_links for its callers/callees, lens_path for how A reaches B. One query replaces many reads and keeps their bytes out of your context. (Run lens_map once if the graph is empty.)\n  </tip>\n</context_guidance>"
    )
}

/// Lowercased file extension of a path, if any (`src/Foo.RS` → `rs`).
fn file_extension(path: &str) -> Option<String> {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// Bash-specific routing: wrap or nudge read-only high-output commands, but
/// never touch stateful or non-allowlisted ones.
fn bash_decision(tool_input: &Value, ctx: &RouteCtx) -> Decision {
    let cmd = tool_input["command"].as_str().unwrap_or("");
    if cmd.is_empty() {
        return Decision::Passthrough;
    }
    // Compute once; thread through to avoid recomputing in is_stateful /
    // bash_redirect / is_wrappable.
    let segs = segments(cmd);
    // Stateful commands mutate shell state; rewriting them would change behavior.
    if is_stateful_segs(cmd, &segs) {
        return Decision::Passthrough;
    }
    // Network/build/inline-HTTP → hard redirect into lens_run (port of
    // context-mode's Bash redirects). Steering only; under wrap-only these fall
    // through to the generic output-wrap below. Gated on `mcp_ready` via
    // `mcp_redirect` (these point at lens_run); when the server is down the
    // command passes through untouched rather than redirecting into a dead tool.
    if ctx.level.steers() {
        if let Some(d) = bash_redirect_segs(cmd, &segs) {
            return mcp_redirect(ctx, d);
        }
    }
    // Structurally-bounded commands (git status, ls, --version probes, …) produce
    // little output — nudging or wrapping them is noise that trains the agent to
    // ignore the advisory. Skip both (port of context-mode #463).
    if classify::classify(cmd) == classify::Risk::Safe {
        return Decision::Passthrough;
    }
    if is_wrappable_segs(&segs) {
        if ctx.level.wraps() {
            let mut updated = tool_input.clone();
            let rewritten = format!("{} wrap -- {}", q(ctx.bin), q(cmd));
            updated["command"] = Value::String(rewritten);
            Decision::Modify {
                reason: WRAP_REASON.to_string(),
                updated_input: updated,
            }
        } else if ctx.level.nudges() && nudge_once(ctx, "bash") {
            Decision::Context(BASH_NUDGE.to_string())
        } else {
            Decision::Passthrough
        }
    } else {
        Decision::Passthrough
    }
}

/// Port of context-mode's curl/wget/build/inline-HTTP Bash redirects
/// (`hooks/core/routing.mjs`): replace a context-flooding network or build command
/// with an `echo` that tells the model to run it through `lens_run` instead, so
/// the raw output stays in the darkroom. `None` for commands that don't match.
/// Accepts pre-computed `segs` from the caller to avoid a redundant allocation.
fn bash_redirect_segs(cmd: &str, segs: &[String]) -> Option<Decision> {
    // Per-segment: a curl/wget that would dump the body to stdout, or a build tool.
    for seg in segs {
        match basename(first_token(seg)) {
            "curl" | "wget" if is_unsafe_fetch(seg) => return Some(net_redirect()),
            "gradle" | "gradlew" | "mvn" | "mvnw" | "sbt" => return Some(build_redirect(cmd)),
            _ => {}
        }
    }
    // Whole-command: an interpreter one-liner that makes an HTTP call. Scanned on
    // the full command (not per-segment) because the inlined code may contain
    // `;`/`|` that `segments` would split mid-string.
    if matches!(
        basename(first_token(cmd)),
        "python" | "python3" | "node" | "ruby" | "deno" | "bun" | "php" | "perl"
    ) && has_inline_http(cmd)
    {
        return Some(net_redirect());
    }
    None
}

/// A curl/wget segment floods context unless it writes the body to a file
/// (`-o`/`-O`/`>`) and not back to stdout (`-o -`, `/dev/stdout`).
fn is_unsafe_fetch(seg: &str) -> bool {
    let has_file_out = seg.contains(" -o ")
        || seg.contains(" --output ")
        || seg.contains(" -O ")
        || seg.contains(" --output-document ")
        || seg.contains('>');
    let stdout_alias =
        seg.contains(" -o -") || seg.contains(" -O -") || seg.contains("/dev/stdout");
    !has_file_out || stdout_alias
}

/// Inline HTTP inside an interpreter one-liner (`python -c 'requests.get(...)'`).
fn has_inline_http(cmd: &str) -> bool {
    (cmd.contains("fetch(") && (cmd.contains("http://") || cmd.contains("https://")))
        || cmd.contains("requests.get(")
        || cmd.contains("requests.post(")
        || cmd.contains("requests.put(")
        || cmd.contains("http.get(")
        || cmd.contains("http.request(")
}

/// Replace the command with guidance to fetch via `lens_run` in the darkroom.
fn net_redirect() -> Decision {
    let msg = "lens routing: network fetch redirected. Call lens_run(language, code) to fetch the URL, derive your answer in code, and print only the result — the raw response body stays in the darkroom instead of entering your conversation. Full network access; retry the same call on a transient DNS error (EAI_AGAIN, ETIMEDOUT).";
    Decision::Modify {
        reason: NET_REDIRECT_REASON.to_string(),
        updated_input: json!({ "command": format!("echo {}", q(msg)) }),
    }
}

/// Replace a build command with guidance to run it through `lens_run`, keeping
/// only the tail of the (verbose) log.
fn build_redirect(cmd: &str) -> Decision {
    let msg = format!(
        "lens routing: build command redirected. Run it in the darkroom so the verbose log stays out of context: lens_run(language: shell, code: \"{cmd} 2>&1 | tail -30\"). Swap tail for a grep over error/warning/FAIL lines to narrow further — only what you print returns."
    );
    Decision::Modify {
        reason: BUILD_REDIRECT_REASON.to_string(),
        updated_input: json!({ "command": format!("echo {}", q(&msg)) }),
    }
}

/// POSIX single-quote a string for safe interpolation into a shell command line:
/// wrap in `'…'`, replacing every embedded `'` with `'\''`. Handles binary paths
/// that contain spaces or quotes.
fn q(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Split a command line into segments on shell control operators
/// (`&&`, `||`, `;`, `|`, `&`, newline). Two-character operators are matched
/// before their single-character prefixes so `&&` doesn't split as two `&`.
fn segments(cmd: &str) -> Vec<String> {
    let bytes = cmd.as_bytes();
    let mut segs = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let two = if i + 1 < bytes.len() {
            &cmd[i..i + 2]
        } else {
            ""
        };
        if two == "&&" || two == "||" {
            segs.push(std::mem::take(&mut cur));
            i += 2;
            continue;
        }
        let c = bytes[i] as char;
        if c == ';' || c == '|' || c == '&' || c == '\n' {
            segs.push(std::mem::take(&mut cur));
            i += 1;
            continue;
        }
        cur.push(c);
        i += 1;
    }
    segs.push(cur);
    segs
}

/// First whitespace-delimited token of a segment, or `""` if the segment is
/// blank.
fn first_token(seg: &str) -> &str {
    seg.split_whitespace().next().unwrap_or("")
}

/// Does this command mutate shell state? If so it must never be wrapped, since
/// the wrapper runs in a child process and the mutation would be lost (or worse,
/// silently change semantics). Conservative: any segment that *looks* stateful
/// taints the whole line.
/// Single-argument wrapper kept for tests; production callers use [`is_stateful_segs`].
#[cfg(test)]
fn is_stateful(cmd: &str) -> bool {
    is_stateful_segs(cmd, &segments(cmd))
}

/// Segments-accepting variant used by [`bash_decision`] to avoid recomputing.
fn is_stateful_segs(cmd: &str, segs: &[String]) -> bool {
    // Backtick command substitution and function definitions are hard to reason
    // about; treat the whole command as stateful.
    if cmd.contains('`') {
        return true;
    }
    const STATEFUL: &[&str] = &[
        "cd", "export", "source", ".", "alias", "unalias", "set", "unset", "pushd", "popd", "eval",
        "trap",
    ];
    for seg in segs {
        let tok = first_token(seg);
        if tok.is_empty() {
            continue;
        }
        if STATEFUL.contains(&tok) {
            return true;
        }
        // Assignment leader: `FOO=...` (optionally as a command prefix).
        if is_assignment(tok) {
            return true;
        }
        // Function definition: `name()` anywhere in the segment.
        if contains_fn_def(seg) {
            return true;
        }
    }
    false
}

/// `^[A-Za-z_][A-Za-z0-9_]*=` — a shell variable assignment leader.
fn is_assignment(tok: &str) -> bool {
    let mut chars = tok.char_indices();
    match chars.next() {
        Some((_, c)) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    for (_, c) in chars {
        if c == '=' {
            return true;
        }
        if !(c == '_' || c.is_ascii_alphanumeric()) {
            return false;
        }
    }
    false
}

/// Heuristic for a function definition `name()` (e.g. `foo() { ... }`).
fn contains_fn_def(seg: &str) -> bool {
    let s = seg.trim_start();
    let bytes = s.as_bytes();
    let mut i = 0;
    // leading identifier
    let start = i;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == '_' || c.is_ascii_alphanumeric() {
            i += 1;
        } else {
            break;
        }
    }
    if i == start {
        return false;
    }
    // optional whitespace, then `()`
    while i < bytes.len() && (bytes[i] as char).is_whitespace() {
        i += 1;
    }
    let rest = &s[i..];
    rest.starts_with("()")
}

/// Strip a leading `./` and any directory, leaving the program basename.
fn basename(token: &str) -> &str {
    let t = token.strip_prefix("./").unwrap_or(token);
    match t.rsplit('/').next() {
        Some(b) if !b.is_empty() => b,
        _ => t,
    }
}

/// For tools that mix read-only and mutating subcommands, the subcommand
/// (token[1]) must be in the read-only set for the segment to be allowlisted.
fn subcommand_ok(prog: &str, sub: &str) -> Option<bool> {
    let set: &[&str] = match prog {
        "git" => &[
            "log",
            "diff",
            "show",
            "status",
            "blame",
            "shortlog",
            "reflog",
            "whatchanged",
            "ls-files",
            "ls-tree",
            "rev-parse",
            "describe",
            "grep",
        ],
        "cargo" => &["test", "build", "check", "clippy", "bench", "tree", "doc"],
        "go" => &["test", "build", "vet", "list"],
        "npm" | "yarn" | "pnpm" => &[
            "test", "run", "build", "ci", "audit", "outdated", "list", "ls",
        ],
        _ => return None,
    };
    Some(set.contains(&sub))
}

/// Is a single segment's program read-only and allowlisted?
fn segment_allowlisted(seg: &str) -> bool {
    let trimmed = seg.trim();
    if trimmed.is_empty() {
        return false;
    }
    let mut tokens = trimmed.split_whitespace();
    let prog = match tokens.next() {
        Some(t) => basename(t),
        None => return false,
    };
    if let Some(ok) = subcommand_ok(prog, tokens.next().unwrap_or("")) {
        return ok;
    }
    classify::is_safe_command(prog)
}

/// Port of context-mode's `isExternalMcpTool` (#529): a non-lens MCP tool, whose
/// large payloads we nudge (periodically) toward `lens_run`. Claude's wire shape is
/// `mcp__<server>__<tool>`; lens's own server is excluded (its tools have dedicated
/// handling / are the redirect target).
fn is_external_mcp_tool(tool: &str) -> bool {
    match tool.strip_prefix("mcp__") {
        Some(rest) => {
            let server = rest.split("__").next().unwrap_or("");
            !server.is_empty() && !server.contains("lens")
        }
        None => false,
    }
}

/// Is the whole command line safe to wrap? Every pipeline/chain segment's
/// leading program must be read-only and allowlisted, so a single mutating stage
/// (`find … | xargs rm`) disqualifies the line.
/// Single-argument wrapper kept for tests; production callers use [`is_wrappable_segs`].
#[cfg(test)]
fn is_wrappable(cmd: &str) -> bool {
    is_wrappable_segs(&segments(cmd))
}

/// Segments-accepting variant used by [`bash_decision`] to avoid recomputing.
fn is_wrappable_segs(segs: &[String]) -> bool {
    let mut any = false;
    for seg in segs {
        if seg.trim().is_empty() {
            continue;
        }
        any = true;
        if !segment_allowlisted(seg) {
            return false;
        }
    }
    any
}

/// External-MCP nudge cadence: fire on the 1st, then every `EXTERNAL_MCP_PERIOD`-th
/// matching call. context-mode's default (`EXTERNAL_MCP_NUDGE_DEFAULT`) is 10 — keeps
/// the guidance fresh across an MCP-heavy run (50+ calls) without flooding context.
/// Bash/Grep use [`nudge_once`] (one shot); Read mixes a one-shot general tip
/// with a periodic graph escalation (see [`read_decision`]); external MCP repeats.
pub const EXTERNAL_MCP_PERIOD: u64 = 10;

/// Fire a nudge at most once per (session, key): true only on the first call
/// (the in-memory successor to the old `guidance_once` marker file).
fn nudge_once(ctx: &RouteCtx, key: &str) -> bool {
    if throttle::fired(ctx.data_dir, ctx.session_id, key) {
        false
    } else {
        throttle::mark(ctx.data_dir, ctx.session_id, key);
        true
    }
}

/// Fire a periodic nudge per (session, key): true on calls 1, period+1, … Backed
/// by the [`throttle::bump`] counter.
fn throttle_periodic(ctx: &RouteCtx, key: &str, period: u64) -> bool {
    let next = throttle::bump(ctx.data_dir, ctx.session_id, key);
    period <= 1 || next % period == 1
}

/// Is the MCP server reachable right now?
///
/// `LENS_ROUTING_MCP` forces the answer when set (`up`/`1`/`on`/`true` =>
/// reachable; `down`/`0`/`off`/`false` => not). Otherwise the server's
/// heartbeat file `<data_dir>/server.pid` is consulted: it counts as reachable
/// only while its mtime is within the TTL (`LENS_MCP_TTL` seconds, default
/// 90 — three heartbeat intervals). Missing, stale, or unreadable => not ready.
pub fn mcp_ready(data_dir: &Path) -> bool {
    if let Ok(v) = std::env::var("LENS_ROUTING_MCP") {
        match v.trim().to_ascii_lowercase().as_str() {
            "up" | "1" | "on" | "true" => return true,
            "down" | "0" | "off" | "false" => return false,
            _ => {} // fall through to the heartbeat check
        }
    }
    let ttl = std::env::var("LENS_MCP_TTL")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(90);
    let pid = data_dir.join("server.pid");
    match std::fs::metadata(&pid).and_then(|m| m.modified()) {
        Ok(mtime) => match mtime.elapsed() {
            Ok(age) => age.as_secs() <= ttl,
            Err(_) => false, // mtime in the future (clock skew) — treat as stale
        },
        Err(_) => false,
    }
}

/// The authoritative tool-selection directive injected at `SessionStart` while
/// steering is active. context-mode's `<context_window_protection>` pattern
/// (prose adapted from `hooks/routing-block.mjs`, tool names mapped to lens): the
/// *why*, a hierarchy that puts the code graph first, a nuanced when-NOT-to-use,
/// and a deferred-tool ToolSearch bootstrap. `_level` is unused (the block is
/// level-agnostic; the caller already gates injection on `level.nudges()`). The
/// full block; [`session_block_for`] tailors the when-not-to-use bullets.
pub fn session_block(_level: Level) -> String {
    session_block_with(true, true)
}

/// Tailored variant: emit the per-tool when-not-to-use bullets only for the tool
/// groups active this session (`bash`, `file`). With neither active (a fresh
/// session with no tool history) this falls back to the full block.
pub fn session_block_for(_level: Level, bash: bool, file: bool) -> String {
    if !bash && !file {
        session_block_with(true, true)
    } else {
        session_block_with(bash, file)
    }
}

/// Assemble the block with the selected per-tool bullets. The WebFetch bullet is
/// always present (a universal deny, not a per-tool tip).
fn session_block_with(bash: bool, file: bool) -> String {
    let mut s = String::from(BLOCK_HEAD);
    if bash {
        s.push_str(BULLET_BASH);
    }
    if file {
        s.push_str(BULLET_READ);
        s.push_str(BULLET_SEARCH);
    }
    s.push_str(BULLET_WEBFETCH);
    s.push_str(BLOCK_TAIL);
    s
}

/// `<context_window_protection>` through the open `<when_not_to_use>` tag.
const BLOCK_HEAD: &str = r##"<context_window_protection>
  <priority_instructions>
    Every byte a tool returns enters your conversation memory and costs reasoning capacity for the rest of the session. The lens tools let you do the work in a darkroom and surface only the derived answer — the raw bytes stay out. Think-in-Code: program the analysis, do not compute it by reading raw data into your conversation.
  </priority_instructions>
  <deferred_tool_bootstrap>
    The lens tools may be DEFERRED in your harness — their schemas are not loaded yet, so calling them directly fails ("tool not found" / InputValidationError). Load them ONCE before your first ctx_*/graph_* call:
    ToolSearch(query: "select:lens_run,lens_run_file,lens_search,lens_index,lens_map,lens_symbol,lens_links,lens_path,lens_recall")
    After that they are callable. If any ctx_*/graph_* call fails as not-found, ToolSearch it and retry — do NOT fall back to Bash/Read/Grep just because the schema was not loaded yet.
  </deferred_tool_bootstrap>
  <tool_selection_hierarchy>
    1. STRUCTURE: lens_map (once per repo), then lens_symbol / lens_links / lens_path.
       - Who-calls-what, imports, how A reaches B, where a symbol lives: query a scoped subgraph instead of reading many files. Expand any compacted node with lens_recall.
    2. SEARCH: lens_index, then lens_search(queries: ["q1", "q2", ...]).
       - "Where is X mentioned" across code and notes. Batch related questions in one array; ranked snippets return, not whole files.
    3. PROCESSING: lens_run(language, code) | lens_run_file(path, language, code).
       - Derive answers FROM data: filter, count, aggregate, parse, transform. Only what you print() enters your conversation; the raw bytes stay in the darkroom.
    4. RECOVER: lens_recall(ref) — pull back the full version of an offloaded result only when you actually need it.
  </tool_selection_hierarchy>
  <when_not_to_use>"##;

const BULLET_BASH: &str = "\n    - You intend to PROCESS the output (filter, count, parse, aggregate) → use lens_run. Bash stays correct when you intend to OBSERVE a short fixed output (git status on a clean tree, whoami, pwd) or when you are mutating state (git, mkdir, rm, mv, navigation).";

const BULLET_READ: &str = "\n    - You want to analyze, summarize, or extract from a file → use lens_run_file. Read stays correct when you intend to Edit the file (Edit needs the exact bytes in your conversation to match against).";

const BULLET_SEARCH: &str = "\n    - You want to find where something is, or who calls it → use lens_search or lens_symbol, not repeated Read/Grep over many files.";

const BULLET_WEBFETCH: &str = "\n    - WebFetch is denied — fetch and reduce a URL with lens_run (python): fetch in the darkroom and print only what you need; the full response stays out of context and is recoverable via lens_recall.";

/// Close `</when_not_to_use>` through `</context_window_protection>`.
const BLOCK_TAIL: &str = r##"
  </when_not_to_use>
  <session_continuity>
    Skills, roles, and directives set during this session remain active until the user revokes them. Do not drop these behavioral directives as context grows.
  </session_continuity>
</context_window_protection>"##;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // ── Level::parse (env-free) ────────────────────────────────────────────

    #[test]
    fn parse_known_levels_case_insensitive_and_trimmed() {
        assert_eq!(Level::parse("steer"), Level::Steer);
        assert_eq!(Level::parse(" Steer "), Level::Steer);
        assert_eq!(Level::parse("WRAP"), Level::Wrap);
        assert_eq!(Level::parse("Full"), Level::Full);
        assert_eq!(Level::parse("full\n"), Level::Full);
    }

    #[test]
    fn parse_unknown_and_empty_is_off() {
        assert_eq!(Level::parse(""), Level::Off);
        assert_eq!(Level::parse("   "), Level::Off);
        assert_eq!(Level::parse("off"), Level::Off);
        assert_eq!(Level::parse("nudge"), Level::Nudge);
        assert_eq!(Level::parse("nonsense"), Level::Off);
    }

    #[test]
    fn steers_and_wraps_flags() {
        assert!(!Level::Off.steers() && !Level::Off.wraps() && !Level::Off.nudges());
        assert!(Level::Nudge.nudges() && !Level::Nudge.steers() && !Level::Nudge.wraps());
        assert!(Level::Steer.steers() && Level::Steer.nudges() && !Level::Steer.wraps());
        assert!(!Level::Wrap.steers() && !Level::Wrap.nudges() && Level::Wrap.wraps());
        assert!(Level::Full.steers() && Level::Full.nudges() && Level::Full.wraps());
    }

    // ── to_hook_json golden payloads ───────────────────────────────────────

    #[test]
    fn hook_json_passthrough() {
        assert_eq!(to_hook_json(&Decision::Passthrough), json!({}));
    }

    #[test]
    fn hook_json_deny() {
        let v = to_hook_json(&Decision::Deny("nope".into()));
        assert_eq!(
            v,
            json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": "nope",
                }
            })
        );
    }

    #[test]
    fn hook_json_modify_nests_updated_input_directly() {
        let v = to_hook_json(&Decision::Modify {
            reason: "r".into(),
            updated_input: json!({"command": "x"}),
        });
        assert_eq!(
            v,
            json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "allow",
                    "permissionDecisionReason": "r",
                    "updatedInput": {"command": "x"},
                }
            })
        );
        // updatedInput is a direct child of hookSpecificOutput (not nested
        // under a permissionDecision object).
        assert!(v["hookSpecificOutput"]["updatedInput"].is_object());
    }

    #[test]
    fn hook_json_context_has_no_permission_decision() {
        let v = to_hook_json(&Decision::Context("hint".into()));
        assert_eq!(
            v,
            json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "additionalContext": "hint",
                }
            })
        );
        assert!(v["hookSpecificOutput"]["permissionDecision"].is_null());
    }

    // ── q() POSIX single-quoting ───────────────────────────────────────────

    #[test]
    fn q_wraps_and_escapes_single_quotes() {
        assert_eq!(q("abc"), "'abc'");
        assert_eq!(q("a b"), "'a b'");
        assert_eq!(q("it's"), "'it'\\''s'");
        assert_eq!(q("/p ath/lens"), "'/p ath/lens'");
    }

    // ── is_stateful ────────────────────────────────────────────────────────

    #[test]
    fn stateful_detects_mutators_and_assignments() {
        assert!(is_stateful("cd /tmp"));
        assert!(is_stateful("export FOO=1"));
        assert!(is_stateful("source ./env.sh"));
        assert!(is_stateful(". ./env.sh"));
        assert!(is_stateful("FOO=bar find ."));
        assert!(is_stateful("cd x && find /"));
        assert!(is_stateful("find / ; cd /tmp"));
        assert!(is_stateful("foo() { echo hi; }"));
        assert!(is_stateful("echo `whoami`"));
        assert!(is_stateful("eval ls"));
    }

    #[test]
    fn non_stateful_commands() {
        assert!(!is_stateful("find ."));
        assert!(!is_stateful("find . | head"));
        assert!(!is_stateful("git log --oneline"));
        assert!(!is_stateful("ls -la"));
    }

    // ── is_wrappable ───────────────────────────────────────────────────────

    #[test]
    fn wrappable_plain_and_pipelines() {
        assert!(is_wrappable("find ."));
        assert!(is_wrappable("find . -name '*.rs'"));
        assert!(is_wrappable("find . | head"));
        assert!(is_wrappable("cat a.txt | sort | uniq -c"));
        assert!(is_wrappable("./gradlew test"));
        assert!(is_wrappable("rg pattern"));
    }

    #[test]
    fn wrappable_git_and_cargo_subcommands() {
        assert!(is_wrappable("git log --oneline"));
        assert!(is_wrappable("git diff HEAD~1"));
        assert!(is_wrappable("cargo test"));
        assert!(is_wrappable("cargo build --release"));
        assert!(!is_wrappable("git commit -m x"));
        assert!(!is_wrappable("git push"));
        assert!(!is_wrappable("cargo run"));
        assert!(!is_wrappable("npm publish"));
        assert!(is_wrappable("npm test"));
    }

    #[test]
    fn not_wrappable_when_any_stage_is_mutating() {
        assert!(!is_wrappable("find . | xargs rm"));
        assert!(!is_wrappable("cat x | tee out"));
        assert!(!is_wrappable("rm -rf build"));
        assert!(!is_wrappable("echo hi"));
        // cd-chain handled by is_stateful, but as a pure allowlist check the
        // `cd` segment is also not allowlisted:
        assert!(!is_wrappable("cd x && find /"));
    }

    // is_structurally_bounded / classify accept-set lives in `classify.rs` tests.

    #[test]
    fn bounded_wrappable_command_passes_through_instead_of_wrapping() {
        let d = tempdir().unwrap();
        // `git status` and `ls` are both is_wrappable AND structurally bounded —
        // they must passthrough (no wrap, no nudge) under wrap/full.
        for cmd in ["git status", "ls -la"] {
            assert_eq!(
                route(
                    "Bash",
                    &json!({"command": cmd}),
                    &rc(Level::Full, true, d.path())
                ),
                Decision::Passthrough,
                "{cmd:?} is bounded → not wrapped"
            );
        }
        // `git log` (unbounded) is still wrapped.
        assert!(matches!(
            route(
                "Bash",
                &json!({"command": "git log"}),
                &rc(Level::Wrap, true, d.path())
            ),
            Decision::Modify { .. }
        ));
    }

    // ── route(): MCP-ready gate ────────────────────────────────────────────

    fn rc<'a>(level: Level, mcp_ready: bool, dir: &'a Path) -> RouteCtx<'a> {
        // A unique session per ctx keeps the process-global in-memory throttle
        // isolated across tests (the old per-tempdir markers gave this for free).
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let sid: &'static str = Box::leak(format!("sess-{id}").into_boxed_str());
        RouteCtx {
            level,
            mcp_ready,
            bin: "/path with space/lens",
            data_dir: dir,
            session_id: sid,
            rtk_active: false,
        }
    }

    #[test]
    fn off_level_always_passthrough() {
        let d = tempdir().unwrap();
        let ctx = rc(Level::Off, true, d.path());
        assert_eq!(
            route("WebFetch", &json!({"url": "http://x"}), &ctx),
            Decision::Passthrough
        );
        assert_eq!(
            route("Bash", &json!({"command": "find ."}), &ctx),
            Decision::Passthrough
        );
    }

    #[test]
    fn mcp_not_ready_gates_only_redirects_not_nudges() {
        // Port of context-mode's mcpRedirect (#230): when the server is unreachable,
        // ONLY the MCP-redirect decisions passthrough (so the agent isn't sent to a
        // dead tool). Nudges, wrap, and sub-agent injection still fire — they don't
        // depend on the MCP server. (Old behavior blanket-passed everything; that was
        // the divergence.)
        let d = tempdir().unwrap();
        let ctx = rc(Level::Full, false, d.path());
        // MCP-redirect decisions → suppressed to passthrough when not ready:
        assert_eq!(
            route("WebFetch", &json!({"url": "http://x"}), &ctx),
            Decision::Passthrough,
            "WebFetch deny is an MCP redirect — suppressed when server down"
        );
        assert_eq!(
            route(
                "Bash",
                &json!({"command": "curl https://api.example.com/data"}),
                &ctx
            ),
            Decision::Passthrough,
            "curl→lens_run redirect suppressed when server down"
        );
        // Non-redirect decisions → still fire (not gated on mcp_ready):
        assert!(
            matches!(
                route("Bash", &json!({"command": "find ."}), &ctx),
                Decision::Modify { .. }
            ),
            "wrap rewrite uses the lens CLI, not the MCP — fires regardless"
        );
        assert_eq!(
            route("Grep", &json!({"pattern": "x"}), &ctx),
            Decision::Context(GREP_NUDGE.to_string()),
            "Grep nudge is not an MCP redirect — fires regardless"
        );
        assert_eq!(
            route("Read", &json!({"file_path": "x"}), &ctx),
            Decision::Context(READ_NUDGE.to_string()),
            "Read nudge is not an MCP redirect — fires regardless"
        );
    }

    // ── route(): WebFetch ──────────────────────────────────────────────────

    #[test]
    fn webfetch_denied_when_steering_else_passthrough() {
        let d = tempdir().unwrap();
        let url = json!({"url": "http://x"});
        assert_eq!(
            route("WebFetch", &url, &rc(Level::Steer, true, d.path())),
            Decision::Deny(WEBFETCH_REASON.to_string())
        );
        assert_eq!(
            route("WebFetch", &url, &rc(Level::Full, true, d.path())),
            Decision::Deny(WEBFETCH_REASON.to_string())
        );
        // wrap-only doesn't steer → not denied
        assert_eq!(
            route("WebFetch", &url, &rc(Level::Wrap, true, d.path())),
            Decision::Passthrough
        );
    }

    // ── route(): Bash wrap rewrite ─────────────────────────────────────────

    #[test]
    fn bash_wrapped_under_wrap_level() {
        let d = tempdir().unwrap();
        let ti = json!({"command": "find . -name '*.rs'"});
        match route("Bash", &ti, &rc(Level::Wrap, true, d.path())) {
            Decision::Modify {
                reason,
                updated_input,
            } => {
                assert_eq!(reason, WRAP_REASON);
                let got = updated_input["command"].as_str().unwrap();
                let expected = format!(
                    "{} wrap -- {}",
                    q("/path with space/lens"),
                    q("find . -name '*.rs'")
                );
                assert_eq!(got, expected);
            }
            other => panic!("expected Modify, got {other:?}"),
        }
    }

    #[test]
    fn bash_stateful_never_wrapped() {
        let d = tempdir().unwrap();
        let ti = json!({"command": "cd x && find /"});
        assert_eq!(
            route("Bash", &ti, &rc(Level::Full, true, d.path())),
            Decision::Passthrough
        );
    }

    #[test]
    fn bash_non_allowlisted_never_wrapped() {
        let d = tempdir().unwrap();
        let ti = json!({"command": "echo hi"});
        assert_eq!(
            route("Bash", &ti, &rc(Level::Wrap, true, d.path())),
            Decision::Passthrough
        );
    }

    #[test]
    fn bash_defers_to_rtk_when_active() {
        let d = tempdir().unwrap();
        // RTK active: Bash passes through (RTK's hook owns the rewrite) while
        // WebFetch is unaffected (still denied under steer/full).
        let active = RouteCtx {
            level: Level::Full,
            mcp_ready: true,
            bin: "/path with space/lens",
            data_dir: d.path(),
            session_id: "sess-1",
            rtk_active: true,
        };
        assert_eq!(
            route("Bash", &json!({"command": "find . -type f"}), &active),
            Decision::Passthrough
        );
        assert_eq!(
            route("WebFetch", &json!({"url": "http://x"}), &active),
            Decision::Deny(WEBFETCH_REASON.to_string())
        );
        // Same ctx but RTK inactive: today's wrap behavior is unchanged.
        let inactive = RouteCtx {
            rtk_active: false,
            ..active
        };
        assert!(matches!(
            route("Bash", &json!({"command": "find . -type f"}), &inactive),
            Decision::Modify { .. }
        ));
    }

    #[test]
    fn bash_empty_command_passthrough() {
        let d = tempdir().unwrap();
        assert_eq!(
            route("Bash", &json!({}), &rc(Level::Full, true, d.path())),
            Decision::Passthrough
        );
        assert_eq!(
            route(
                "Bash",
                &json!({"command": ""}),
                &rc(Level::Full, true, d.path())
            ),
            Decision::Passthrough
        );
    }

    #[test]
    fn bash_nudges_once_under_steer_only() {
        let d = tempdir().unwrap();
        let ti = json!({"command": "find ."});
        let ctx = rc(Level::Steer, true, d.path());
        // first call nudges, second is throttled to passthrough
        assert_eq!(
            route("Bash", &ti, &ctx),
            Decision::Context(BASH_NUDGE.to_string())
        );
        assert_eq!(route("Bash", &ti, &ctx), Decision::Passthrough);
    }

    // ── route(): Grep + Read nudges ────────────────────────────────────────

    #[test]
    fn grep_nudges_once_when_steering() {
        let d = tempdir().unwrap();
        let ti = json!({"pattern": "foo"});
        let ctx = rc(Level::Steer, true, d.path());
        assert_eq!(
            route("Grep", &ti, &ctx),
            Decision::Context(GREP_NUDGE.to_string())
        );
        assert_eq!(route("Grep", &ti, &ctx), Decision::Passthrough);
        // wrap-only doesn't steer
        let d2 = tempdir().unwrap();
        assert_eq!(
            route("Grep", &ti, &rc(Level::Wrap, true, d2.path())),
            Decision::Passthrough
        );
    }

    #[test]
    fn read_general_tip_fires_once_when_steering() {
        let d = tempdir().unwrap();
        // A non-code file gets the general analysis tip, once per session (no
        // graph escalation: it isn't in the graph).
        let ti = json!({"file_path": "README.md"});
        let ctx = rc(Level::Steer, true, d.path());
        assert_eq!(
            route("Read", &ti, &ctx),
            Decision::Context(READ_NUDGE.to_string()),
            "Read nudges at steer (context-mode nudges Read whenever routing is active)"
        );
        assert_eq!(
            route("Read", &ti, &ctx),
            Decision::Passthrough,
            "general tip is one-shot per session"
        );
        // wrap-only does not steer → no Read nudge.
        let d2 = tempdir().unwrap();
        assert_eq!(
            route("Read", &ti, &rc(Level::Wrap, true, d2.path())),
            Decision::Passthrough
        );
    }

    #[test]
    fn read_nudge_offers_skeleton_at_every_nudging_level() {
        // lens_skeleton must be surfaced on file reads at all nudging levels
        // (nudge/steer/full), not just full, and never when routing is off.
        assert!(
            READ_NUDGE.contains("lens_skeleton"),
            "the read nudge must name lens_skeleton"
        );
        let ti = json!({"file_path": "README.md"});
        for level in [Level::Nudge, Level::Steer, Level::Full] {
            let d = tempdir().unwrap();
            match route("Read", &ti, &rc(level, true, d.path())) {
                Decision::Context(c) => assert!(
                    c.contains("lens_skeleton"),
                    "{level:?}: read nudge should offer lens_skeleton"
                ),
                other => panic!("{level:?}: expected a read nudge, got {other:?}"),
            }
        }
        let d = tempdir().unwrap();
        assert_eq!(
            route("Read", &ti, &rc(Level::Off, true, d.path())),
            Decision::Passthrough,
            "off: no nudge"
        );
    }

    #[test]
    fn read_code_files_escalate_to_the_graph() {
        let d = tempdir().unwrap();
        let ctx = rc(Level::Steer, true, d.path());
        let code = json!({"file_path": "src/server.rs"});
        // 1st code read: the general tip (which now names the graph among its options).
        assert_eq!(
            route("Read", &code, &ctx),
            Decision::Context(READ_NUDGE.to_string())
        );
        // 2nd: throttled (general tip spent, threshold not yet reached).
        assert_eq!(route("Read", &code, &ctx), Decision::Passthrough);
        // 3rd (threshold): graph-specific escalation naming all three graph tools.
        match route("Read", &code, &ctx) {
            Decision::Context(c) => assert!(
                c.contains("lens_symbol") && c.contains("lens_links") && c.contains("lens_path"),
                "escalation names the graph tools: {c}"
            ),
            other => panic!("expected graph nudge, got {other:?}"),
        }
        // 4th, 5th quiet; 6th fires again (every READ_GRAPH_PERIOD-th read).
        assert_eq!(route("Read", &code, &ctx), Decision::Passthrough);
        assert_eq!(route("Read", &code, &ctx), Decision::Passthrough);
        assert!(matches!(route("Read", &code, &ctx), Decision::Context(_)));
    }

    #[test]
    fn read_non_code_files_never_escalate_to_graph() {
        let d = tempdir().unwrap();
        let ctx = rc(Level::Steer, true, d.path());
        let doc = json!({"file_path": "DECISIONS.md"});
        // general tip once, then silence — files not in the graph never escalate.
        assert_eq!(
            route("Read", &doc, &ctx),
            Decision::Context(READ_NUDGE.to_string())
        );
        for _ in 0..6 {
            assert_eq!(route("Read", &doc, &ctx), Decision::Passthrough);
        }
    }

    #[test]
    fn read_nudge_offers_the_graph() {
        assert!(READ_NUDGE.contains("lens_symbol"));
        assert!(READ_NUDGE.contains("lens_run_file"));
    }

    // ── post_route(): scale-aware search nudge ──────────────────────────────

    #[test]
    fn post_route_nudges_flooding_grep_once_when_steering() {
        let d = tempdir().unwrap();
        let ctx = rc(Level::Full, true, d.path());
        let big = "x".repeat(grep_flood_bytes() + 1);
        assert_eq!(
            post_route("Grep", &big, &ctx),
            Decision::Context(SEARCH_NUDGE.to_string())
        );
        // one-shot per session
        assert_eq!(post_route("Grep", &big, &ctx), Decision::Passthrough);
    }

    #[test]
    fn post_route_quiet_on_small_grep_other_tools_and_non_steering() {
        let big = "x".repeat(grep_flood_bytes() + 1);
        // small grep result -> below the flood threshold -> quiet
        let d = tempdir().unwrap();
        assert_eq!(
            post_route(
                "Grep",
                "x".repeat(100).as_str(),
                &rc(Level::Full, true, d.path())
            ),
            Decision::Passthrough
        );
        // a big result from another tool is not a grep flood
        let d2 = tempdir().unwrap();
        assert_eq!(
            post_route("Read", &big, &rc(Level::Full, true, d2.path())),
            Decision::Passthrough
        );
        // wrap-only / off do not steer
        let d3 = tempdir().unwrap();
        assert_eq!(
            post_route("Grep", &big, &rc(Level::Wrap, true, d3.path())),
            Decision::Passthrough
        );
    }

    #[test]
    fn to_post_hook_json_tags_posttooluse() {
        let v = to_post_hook_json(&Decision::Context("hi".into()));
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        assert_eq!(v["hookSpecificOutput"]["additionalContext"], "hi");
        assert_eq!(to_post_hook_json(&Decision::Passthrough), json!({}));
    }

    #[test]
    fn external_mcp_tool_nudged_periodically_when_steering() {
        let d = tempdir().unwrap();
        let ctx = rc(Level::Full, true, d.path());
        let ti = json!({});
        // First call to a non-lens MCP tool nudges; throttled after.
        assert_eq!(
            route("mcp__slack__search", &ti, &ctx),
            Decision::Context(EXTERNAL_MCP_NUDGE.to_string())
        );
        assert_eq!(
            route("mcp__slack__search", &ti, &ctx),
            Decision::Passthrough
        );
        // lens's own MCP tools are NOT treated as external.
        assert_eq!(
            route("mcp__lens__lens_run", &ti, &ctx),
            Decision::Passthrough
        );
        // not steering → no nudge.
        let d2 = tempdir().unwrap();
        assert_eq!(
            route("mcp__slack__search", &ti, &rc(Level::Wrap, true, d2.path())),
            Decision::Passthrough
        );
    }

    #[test]
    fn external_mcp_detection() {
        assert!(is_external_mcp_tool("mcp__slack__post"));
        assert!(is_external_mcp_tool("mcp__github__list"));
        assert!(!is_external_mcp_tool("mcp__lens__lens_search"));
        assert!(!is_external_mcp_tool("Bash"));
        assert!(!is_external_mcp_tool("mcp__"));
    }

    #[test]
    fn unknown_tool_passthrough() {
        let d = tempdir().unwrap();
        assert_eq!(
            route(
                "Edit",
                &json!({"file_path": "x"}),
                &rc(Level::Full, true, d.path())
            ),
            Decision::Passthrough
        );
    }

    // ── route(): Agent / Task sub-agent prompt injection ───────────────────

    #[test]
    fn agent_prompt_injected_when_steering() {
        let d = tempdir().unwrap();
        let ti = json!({"prompt": "map the auth subsystem", "subagent_type": "Explore"});
        match route("Agent", &ti, &rc(Level::Full, true, d.path())) {
            Decision::Modify {
                reason,
                updated_input,
            } => {
                assert_eq!(reason, AGENT_INJECT_REASON);
                let p = updated_input["prompt"].as_str().unwrap();
                assert!(
                    p.starts_with("map the auth subsystem"),
                    "original prompt preserved"
                );
                assert!(p.contains("<context_window_protection>"), "guide appended");
                assert!(
                    p.contains("ToolSearch"),
                    "carries the deferred-tool bootstrap"
                );
                // sibling fields are untouched
                assert_eq!(updated_input["subagent_type"], json!("Explore"));
            }
            other => panic!("expected Modify, got {other:?}"),
        }
        // `Task` is treated identically.
        assert!(matches!(
            route(
                "Task",
                &json!({"prompt": "x"}),
                &rc(Level::Steer, true, d.path())
            ),
            Decision::Modify { .. }
        ));
    }

    #[test]
    fn agent_passthrough_when_not_steering_or_unknown_shape() {
        let d = tempdir().unwrap();
        let ti = json!({"prompt": "x"});
        // wrap-only / off do not steer → no injection
        assert_eq!(
            route("Agent", &ti, &rc(Level::Wrap, true, d.path())),
            Decision::Passthrough
        );
        assert_eq!(
            route("Agent", &ti, &rc(Level::Off, true, d.path())),
            Decision::Passthrough
        );
        // no recognized prompt field → leave the call alone
        assert_eq!(
            route(
                "Agent",
                &json!({"foo": "bar"}),
                &rc(Level::Full, true, d.path())
            ),
            Decision::Passthrough
        );
    }

    // ── route(): Bash network / build / inline-HTTP redirects ──────────────

    #[test]
    fn bash_curl_to_stdout_redirected_to_lens_run() {
        let d = tempdir().unwrap();
        let ti = json!({"command": "curl https://api.example.com/data | jq ."});
        match route("Bash", &ti, &rc(Level::Full, true, d.path())) {
            Decision::Modify {
                reason,
                updated_input,
            } => {
                assert_eq!(reason, NET_REDIRECT_REASON);
                let c = updated_input["command"].as_str().unwrap();
                assert!(c.starts_with("echo "), "command neutered to an echo: {c}");
                assert!(c.contains("lens_run"), "guidance points to lens_run");
            }
            other => panic!("expected Modify, got {other:?}"),
        }
    }

    #[test]
    fn bash_curl_to_file_is_wrapped_not_redirected() {
        let d = tempdir().unwrap();
        // silent download to a file doesn't flood context → falls through to wrap
        let ti = json!({"command": "curl -s -o out.json https://api.example.com/data"});
        match route("Bash", &ti, &rc(Level::Full, true, d.path())) {
            Decision::Modify { reason, .. } => {
                assert_eq!(reason, WRAP_REASON, "wrapped, not redirected")
            }
            other => panic!("expected wrap Modify, got {other:?}"),
        }
    }

    #[test]
    fn bash_build_tool_redirected_to_lens_run() {
        let d = tempdir().unwrap();
        let ti = json!({"command": "./gradlew test"});
        match route("Bash", &ti, &rc(Level::Full, true, d.path())) {
            Decision::Modify {
                reason,
                updated_input,
            } => {
                assert_eq!(reason, BUILD_REDIRECT_REASON);
                let c = updated_input["command"].as_str().unwrap();
                assert!(c.starts_with("echo "));
                assert!(c.contains("lens_run") && c.contains("tail -30"));
            }
            other => panic!("expected Modify, got {other:?}"),
        }
    }

    #[test]
    fn bash_inline_http_one_liner_redirected() {
        let d = tempdir().unwrap();
        let ti = json!({"command": "python3 -c 'import requests; requests.get(\"http://x\")'"});
        match route("Bash", &ti, &rc(Level::Full, true, d.path())) {
            Decision::Modify { reason, .. } => assert_eq!(reason, NET_REDIRECT_REASON),
            other => panic!("expected Modify, got {other:?}"),
        }
    }

    #[test]
    fn bash_redirect_only_when_steering() {
        let d = tempdir().unwrap();
        // wrap-only steers nothing → curl is wrapped, not redirected
        let ti = json!({"command": "curl https://api.example.com/data"});
        match route("Bash", &ti, &rc(Level::Wrap, true, d.path())) {
            Decision::Modify { reason, .. } => assert_eq!(reason, WRAP_REASON),
            other => panic!("expected wrap Modify, got {other:?}"),
        }
    }

    // ── throttle_periodic ───────────────────────────────────────────────────

    #[test]
    fn throttle_fires_on_first_then_every_period() {
        let d = tempdir().unwrap();
        let ctx = rc(Level::Steer, true, d.path());
        // period 3: fire on calls 1 and 4 (1, period+1), suppress 2,3,5,6.
        let fires: Vec<bool> = (0..6).map(|_| throttle_periodic(&ctx, "k", 3)).collect();
        assert_eq!(fires, vec![true, false, false, true, false, false]);
        // a different key has an independent counter.
        assert!(throttle_periodic(&ctx, "other", 3));
        assert!(!throttle_periodic(&ctx, "other", 3));
        // period 1 fires every time.
        assert!(throttle_periodic(&ctx, "always", 1));
        assert!(throttle_periodic(&ctx, "always", 1));
    }

    // ── mcp_ready ──────────────────────────────────────────────────────────
    // NOTE: these touch LENS_ROUTING_MCP / LENS_MCP_TTL, so they are
    // grouped into one serialized test to avoid env races with other tests.

    #[test]
    fn mcp_ready_env_override_and_heartbeat() {
        let d = tempdir().unwrap();
        // No pidfile, no override → not ready.
        std::env::remove_var("LENS_ROUTING_MCP");
        std::env::remove_var("LENS_MCP_TTL");
        assert!(!mcp_ready(d.path()));

        // Override up/down wins regardless of pidfile.
        std::env::set_var("LENS_ROUTING_MCP", "up");
        assert!(mcp_ready(d.path()));
        std::env::set_var("LENS_ROUTING_MCP", "off");
        assert!(!mcp_ready(d.path()));
        std::env::remove_var("LENS_ROUTING_MCP");

        // Fresh pidfile within TTL → ready.
        std::fs::write(d.path().join("server.pid"), "123").unwrap();
        assert!(mcp_ready(d.path()));

        // TTL of 0 makes any nonzero age stale (sleep a moment to be safe).
        std::env::set_var("LENS_MCP_TTL", "0");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert!(!mcp_ready(d.path()));
        std::env::remove_var("LENS_MCP_TTL");
    }

    // ── session_block ──────────────────────────────────────────────────────

    #[test]
    fn session_block_mentions_tools_and_behaviors() {
        let b = session_block(Level::Full);
        assert!(b.starts_with("<context_window_protection>"));
        assert!(b.contains("</context_window_protection>"));
        for needle in [
            "lens_run",
            "lens_index",
            "lens_search",
            "lens_run_file",
            "lens_map",
            "lens_symbol",
            "lens_links",
            "lens_path",
            "lens_recall",
            "tool_selection_hierarchy",
            "WebFetch is denied",
            // the highest-impact additions over the old soft nudge:
            "ToolSearch",      // deferred-tool bootstrap
            "Think-in-Code",   // authoritative framing
            "when_not_to_use", // nuanced credibility
        ] {
            assert!(b.contains(needle), "session_block missing {needle:?}");
        }
    }
}
