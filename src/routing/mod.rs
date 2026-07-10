//! PreToolUse routing policy — pass through, deny, rewrite, or nudge a tool call.
//!
//! Gated by `LENS_ROUTING` (off|nudge|steer|wrap|full); default `full`. Concerns layer:
//!
//!   * **nudge** — emit once-per-session non-blocking nudges toward lens tools for
//!     `Bash` / `Grep` / `Read` (structurally-bounded commands are skipped); a
//!     periodic nudge for external (non-lens) MCP tools; inject the tool-selection
//!     guide into every sub-agent (`Agent`/`Task`) prompt and at `SessionStart`.
//!     Never denies, redirects, or rewrites a call.
//!   * **steer** — nudge, plus deny `WebFetch` and redirect curl/wget/build/
//!     inline-HTTP `Bash` commands into `lens_run`.
//!   * **wrap** — transparently rewrite a read-only, high-output `Bash` command
//!     into `lens wrap -- <cmd>` so its output is offloaded losslessly.
//!   * **full** — both steer and wrap.
//!
//! Under steer/full a broad-scope Grep (its `path` spans a directory or the whole
//! repo, see [`grep_scope`]) is denied at most once per prompt toward a lens call —
//! gated on a populated index ([`index_present`]) — via the always-on first-Grep
//! deny and the dark-launched grep-scope deny ([`grep_scope_deny_enabled`]).
//!
//! Safety rails: MCP-redirect decisions (WebFetch deny, curl/build rewrites) are
//! gated on [`mcp_ready`] via [`mcp_redirect`] so the agent is never sent to a dead
//! tool (nudges + sub-agent injection fire regardless); and stateful shell commands
//! (anything that mutates shell state — `cd`, `export`, assignments, …) are always
//! passed through untouched, because rewriting them would silently change behavior.

use std::collections::HashSet;
use std::path::Path;

use serde_json::{json, Value};

mod classify;
mod log;
pub mod throttle;
pub(crate) mod reroute;

pub use classify::{grep_scope, is_structurally_bounded, GrepScope};

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
    /// Code-file Reads this session since the last `lens_map`/`lens_overview`
    /// call — the read-overview (rovr) rail's counter. Fed by the session hook
    /// from the `reads-since-map` throttle counter on Read events; 0 at every
    /// other construction site.
    pub reads_since_map: u64,
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

/// Shown when [`inspect_escalation`] denies a Read or Grep after too many
/// consecutive manual code lookups with no intervening lens tool call (Serena
/// `remind` pattern). Factual, maps each intent to its lens tool, and states
/// that the counter was reset so the caller isn't walled off if it still
/// needs the plain tool.
pub const READ_DENY_REASON: &str = "Too many consecutive Read/Grep calls on code without any lens tool. Where is X / where does an idea appear: lens_search(queries: [...]) or lens_symbol(name). What calls X, what does X call: lens_links. How does A reach B: lens_path. A file's shape: lens_skeleton(path), with include_bodies for the functions you need. The counter was reset — the same call will pass now if you still need it.";

// Per-tool `<context_guidance>` injected on PreToolUse (tool names mapped to lens).
// Re-injected periodically (see `throttle_periodic`), not once per session.

/// Contextual guidance when a read-only/high-output Bash command is observed.
pub const BASH_NUDGE: &str = "<context_guidance>\n  <tip>\n    About to take this command's output and count, filter, or reshape it? Run it through lens_run(language: \"shell\", code: \"...\") instead — it executes in the darkroom and only what you print comes back. A plain Bash call is the right tool when you just need to see a short result or you're changing state (git, file moves, and the like).\n  </tip>\n</context_guidance>";

/// Contextual guidance steering Grep toward indexed search / the graph. The
/// measured drift signature is Grep -> Read -> Read: line hits name a file,
/// the file gets read whole, repeat. So this maps each find/trace intent to
/// the lens tool that answers it directly, instead of describing categories.
pub const GREP_NUDGE: &str = "<context_guidance>\n  <tip>\n    About to grep to find something? Map the intent to the tool that answers it directly: where is X defined or used — lens_search(queries: [\"...\"]) (ranked snippets, several questions per call) or lens_symbol(name) if you know the exact name; what calls X / what does X call — lens_links; how does A reach B — lens_path; you only know what it does, not its name — lens_find. A grep here usually starts a chain: line hits, then a whole-file Read per hit — that chain is what these replace. Grep stays right for a quick check you'll eyeball in one file, and lens_run(language: \"shell\") for match lists you'll tally or reshape.\n  </tip>\n</context_guidance>";

/// Contextual guidance steering analysis-reads into the darkroom, and
/// navigational code-reads toward the graph.
pub const READ_NUDGE: &str = "<context_guidance>\n  <tip>\n    Reading this file to Edit it? Stay with Read — Edit has to match the exact bytes you're holding. Reading it to understand, summarize, or extract a few facts? Send it through lens_run_file(path, language, code) and return only what you derived. To see a file's API — signatures and structure without the bodies — use lens_skeleton(path) first, then lens_skeleton(path, include_bodies: [\"the_fn\"]) for just the bodies you need, with the full text always a lens_recall away. And when you're tracing how code connects (callers, callees, where a symbol is defined, how A reaches B), don't read file after file — query the graph with lens_symbol / lens_links / lens_path (run lens_map once if it's empty). Four consecutive code Reads/Greps with no lens tool call between them trigger a one-time deny — that's not a wall, just the point where the graph or skeleton clearly beats looking further by hand.\n  </tip>\n</context_guidance>";

/// Contextual guidance emitted AFTER a Grep whose result set floods context. A
/// result this large is exactly where lens_search (ranked top-K, flat with corpus
/// size) beats grep (every matching line). PostToolUse, not before, so it fires only
/// when the grep actually flooded — below the threshold grep is as lean and we stay
/// quiet.
pub const SEARCH_NUDGE: &str = "<context_guidance>\n  <tip>\n    That grep returned a large match set — more than lens_search would. For a result set this size, lens_search (after lens_index) returns the ranked top hits and keeps the rest out of your context; re-run the search through lens_search if you need more than the matches already shown.\n  </tip>\n</context_guidance>";

/// One-line mapping injected at UserPromptSubmit when the prompt reads as a
/// find/trace question. First-tool choice is decided by what's in context
/// BEFORE the first call — PreToolUse nudges arrive one call too late and the
/// SessionStart block alone doesn't overcome the Grep prior (measured:
/// find/trace tasks stayed Grep-first with the block in place). This lands at
/// the decision point itself.
pub const PROMPT_INTENT_NUDGE: &str = "<lens_hint>\n  Find/trace question — answer it from the index/graph, not by grepping: lens_search(queries: [\"...\"]) or lens_symbol(name) to locate; lens_links for callers/callees; lens_path for how A reaches B; lens_skeleton(path) for one file's shape. Grep's line hits pull a whole-file Read per hit — that chain costs more than one lens call.\n</lens_hint>";

/// Shown when the FIRST Grep after a find/trace-shaped prompt is denied (the
/// `grep-first` marker armed at UserPromptSubmit, consumed here). Measured:
/// the intent nudge alone flips some tasks but most Grep-first ones ignore
/// every prompt-level hint — this is the Serena FORBIDDEN pattern applied at
/// the exact decision point. One-shot: the marker is consumed before the deny
/// returns, so the same Grep passes on retry.
pub const GREP_FIRST_DENY_REASON: &str = "This prompt is a find/trace question — answer it with one lens call instead of a grep chain. Where is X / where does an idea appear: lens_search(queries: [...]) or lens_symbol(name). What calls X, what does X call: lens_links. How does A reach B: lens_path. A file's shape: lens_skeleton(path). If the lens tools aren't loaded yet, load them first: ToolSearch(query: \"select:lens_search,lens_symbol,lens_links,lens_path,lens_skeleton\"). This fires once per prompt — the same Grep will pass if you re-run it, but the lens call answers in one step.";
/// Shown when a Grep whose `path` spans a directory or the whole repo is denied
/// under the grep-scope gate (`LENS_GREP_SCOPE_DENY`, dark-launch — see
/// [`grep_scope_deny_enabled`]). Same shape as [`GREP_FIRST_DENY_REASON`] but
/// keyed on the call's scope rather than the prompt's phrasing.
pub const GREP_SCOPE_DENY_REASON: &str = "This grep spans a directory or the whole repo — one lens call answers it without the grep→Read chain. Where is X / where does an idea appear: lens_search(queries: [...]) or lens_symbol(name). What calls X, what does X call: lens_links. How does A reach B: lens_path. A file's shape: lens_skeleton(path). If the lens tools aren't loaded yet, load them first: ToolSearch(query: \"select:lens_search,lens_symbol,lens_links,lens_path,lens_skeleton\"). This fires at most once per prompt — the same Grep will pass if you re-run it.";

/// Whether a user prompt reads as a find/trace question worth the
/// [`PROMPT_INTENT_NUDGE`]. High-precision substrings only — firing on every
/// prompt would train the model to ignore the hint. Prompts that already name
/// a lens tool are skipped (the user is steering explicitly).
pub fn prompt_wants_find_trace(prompt: &str) -> bool {
    let p = prompt.to_ascii_lowercase();
    if p.contains("lens_") {
        return false;
    }
    const SHAPES: &[&str] = &[
        "where is",
        "where does",
        "where are",
        "where do ",
        "what calls",
        "who calls",
        "callers of",
        "what does it call",
        "which file",
        "which function",
        "which module",
        "which struct",
        "find where",
        "find the",
        "locate the",
        "trace ",
        "entry point",
        "defined in",
        "is it registered",
        "how does",
        "how is",
        "search this repo",
        "search the repo",
        "every occurrence",
        "every place",
        "all usages",
        "all uses of",
        "is referenced",
    ];
    SHAPES.iter().any(|s| p.contains(s))
}

/// Periodic guidance for external (non-lens) MCP tools whose payloads flood
/// context.
pub const EXTERNAL_MCP_NUDGE: &str = "<context_guidance>\n  <tip>\n    Other MCP tools tend to hand back large results — message history, file contents, search hits — and all of it lands in the transcript whole. When you mean to filter, count, or summarize that, route it through lens_run(language, code) and keep only the answer. If it's something you'll want to search later, lens_index it and query with lens_search.\n  </tip>\n</context_guidance>";

/// A decision that redirects the agent to an MCP-backed tool (deny WebFetch, rewrite
/// curl/build into `lens_run`) is only safe to emit when the server is reachable —
/// otherwise the agent is told to use a tool that isn't there and stalls. So gate
/// ONLY these on `mcp_ready`; nudges and sub-agent injection are never wrapped in
/// `mcp_redirect` and fire regardless.
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
/// `!mcp_ready` gate: readiness gates
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
        // Grep counts toward the same consecutive-lookup counter as code Reads:
        // the measured drift signature (Grep → Read → Read) starts here, and a
        // Read-only counter never catches it. Escalation first, then the
        // one-shot intent-mapping tip.
        "Grep" => {
            if !ctx.level.nudges() {
                return Decision::Passthrough;
            }
            // Scope-gated Grep deny: a broad-scope Grep (path spanning a dir or
            // the whole repo) can be denied once per prompt toward a lens call.
            // Two mechanisms share ONE deny budget — the always-on first-Grep
            // deny (armed by a find/trace prompt) and the dark-launched
            // grep-scope deny (armed each prompt while steering). The gate needs
            // a populated index so we never send the agent to a search that
            // can't answer. Each `take` runs LAST in its chain so a blocked gate
            // keeps the marker armed; on a deny BOTH markers are consumed and the
            // `read-code` counter is reset so the verbatim retry always passes.
            let scope = grep_scope(tool_input.get("path").and_then(Value::as_str));
            if scope == GrepScope::Broad
                && ctx.level.steers()
                && ctx.mcp_ready
                && index_present(ctx.data_dir)
            {
                let first = grep_first_deny_enabled()
                    && throttle::take(ctx.data_dir, ctx.session_id, "grep-first");
                let scoped = !first
                    && grep_scope_deny_enabled()
                    && throttle::take(ctx.data_dir, ctx.session_id, "grep-scope");
                if first || scoped {
                    // One Grep deny per prompt across both mechanisms: consume the
                    // other marker and reset the lookup counter so the verbatim
                    // retry always passes (the reason strings promise it). The
                    // reroute grep-symbol deny shares the same budget — spend its
                    // one-shot too so the retry can't be denied twice.
                    if first {
                        throttle::take(ctx.data_dir, ctx.session_id, "grep-scope");
                    }
                    throttle::mark(ctx.data_dir, ctx.session_id, "grep-symbol");
                    // Neutralize the gast deny too when it can fire, so the same
                    // prompt never denies twice (gast is session-scoped and does
                    // not gate on the prompt markers). Gated so it is inert (and
                    // byte-identical to master) while the gast deny flag is off.
                    if grep_ast_deny_enabled() {
                        throttle::mark(ctx.data_dir, ctx.session_id, "grep-ast");
                    }
                    throttle::reset(ctx.data_dir, ctx.session_id, "read-code");
                    let reason = if first {
                        GREP_FIRST_DENY_REASON
                    } else {
                        GREP_SCOPE_DENY_REASON
                    };
                    return Decision::Deny(reason.to_string());
                }
            }
            // Reroute rail 1a (gsym): a Grep whose pattern is itself a symbol
            // lookup — a definition shape (`fn foo`) or a bare identifier — is
            // denied once per session toward lens_symbol/lens_find. Dark-launched
            // behind LENS_GREP_SYMBOL_DENY with the scope deny's gates. The
            // `nudge_once` runs LAST so a blocked gate never spends the one-shot;
            // on a deny the other grep markers are consumed and the lookup
            // counter reset so the verbatim retry always passes.
            let pat = tool_input.get("pattern").and_then(Value::as_str).unwrap_or("");
            if grep_symbol_deny_enabled()
                && ctx.level.steers()
                && ctx.mcp_ready
                && reroute::grep_symbol::symbol_grep(pat).is_some()
                && index_present(ctx.data_dir)
                && nudge_once(ctx, "grep-symbol")
            {
                throttle::take(ctx.data_dir, ctx.session_id, "grep-first");
                throttle::take(ctx.data_dir, ctx.session_id, "grep-scope");
                // Neutralize the gast deny too (see the scope-deny branch above):
                // gated so it stays byte-identical to master while gast is off.
                if grep_ast_deny_enabled() {
                    throttle::mark(ctx.data_dir, ctx.session_id, "grep-ast");
                }
                throttle::reset(ctx.data_dir, ctx.session_id, "read-code");
                return Decision::Deny(reroute::grep_symbol::reason(pat));
            }
            // Reroute rail 2b (gast) DENY: a syntax-shaped pattern (an impl
            // block, an attribute, a method call, …) is denied once per session
            // toward lens_grep_ast's tree-sitter query. Placed AFTER the gsym
            // deny and BEFORE the gast nudge; dark-launched behind
            // LENS_GREP_AST_DENY with the scope deny's gates, so the nudge below
            // stays reachable when the deny flag is off. Mirrors the gsym deny:
            // `nudge_once` runs LAST so a blocked gate never spends the one-shot;
            // on a deny the shared prompt markers are consumed and the peer gsym
            // rail neutralized (mark "grep-symbol") so no prompt denies twice,
            // and the read-code reset lets the verbatim retry pass.
            if grep_ast_deny_enabled() && ctx.level.steers() && ctx.mcp_ready {
                if let Some(hint) = reroute::grep_ast::syntax_shape(pat) {
                    if index_present(ctx.data_dir) && nudge_once(ctx, "grep-ast") {
                        throttle::take(ctx.data_dir, ctx.session_id, "grep-first");
                        throttle::take(ctx.data_dir, ctx.session_id, "grep-scope");
                        throttle::mark(ctx.data_dir, ctx.session_id, "grep-symbol");
                        throttle::reset(ctx.data_dir, ctx.session_id, "read-code");
                        return Decision::Deny(reroute::grep_ast::deny_reason(&hint));
                    }
                }
            }
            // Reroute rail 2b (gast) NUDGE: a syntax-shaped pattern (an impl
            // block, an attribute, a method call, …) gets a one-shot nudge
            // carrying the translated tree-sitter query. Context only, never
            // blocks (LENS_GREP_AST_NUDGE, same gates).
            if grep_ast_nudge_enabled() && ctx.level.nudges() && ctx.mcp_ready {
                if let Some(hint) = reroute::grep_ast::syntax_shape(pat) {
                    if index_present(ctx.data_dir) && nudge_once(ctx, "grep-ast") {
                        return Decision::Context(reroute::grep_ast::nudge(&hint));
                    }
                }
            }
            if let Some(d) = inspect_escalation(ctx) {
                return d;
            }
            if nudge_once(ctx, "grep") {
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
        // Reroute rail 2a (elink): an Edit that touches a symbol's DECLARATION
        // line, when that symbol has >=K callers in the graph, gets a lens_links
        // nudge (once per session+symbol) so the blast radius is visible before
        // the signature changes. Context only — a state-changing Edit is NEVER
        // denied. Dark-launched behind LENS_EDIT_LINKS_NUDGE; the graph load is
        // last so the flag-off default costs nothing.
        "Edit" | "MultiEdit" => {
            if !(edit_links_nudge_enabled()
                && ctx.level.nudges()
                && ctx.mcp_ready
                && index_present(ctx.data_dir))
            {
                return Decision::Passthrough;
            }
            let Some(sym) = edited_decl_symbol(tool, tool_input) else {
                return Decision::Passthrough;
            };
            let key = format!("elink:{sym}");
            if throttle::fired(ctx.data_dir, ctx.session_id, &key) {
                return Decision::Passthrough;
            }
            let Ok(graph) = crate::discovery::graph::Graph::load(&ctx.data_dir.join("graph.json"))
            else {
                return Decision::Passthrough;
            };
            let k = reroute::edit_callers::min_callers();
            match reroute::edit_callers::caller_nudge(&graph, &sym, k) {
                Some(msg) => {
                    throttle::mark(ctx.data_dir, ctx.session_id, &key);
                    Decision::Context(msg)
                }
                None => Decision::Passthrough,
            }
        }
        // External (non-lens) MCP tools return large payloads (channel history,
        // file content, search results). Periodically nudge toward lens_run — a
        // single one-shot nudge gets lost in long MCP-heavy sessions (periodic
        // external-MCP guidance).
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

/// Inject the lens tool-selection guide into a sub-agent's prompt. No
/// throttle: each sub-agent is a fresh context that needs its own copy, including
/// the block's ToolSearch bootstrap so the deferred ctx_* tools are loadable
/// inside the sub-agent (which doesn't inherit the parent's loaded schemas).
fn agent_inject(tool_input: &Value, ctx: &RouteCtx) -> Decision {
    // The Agent tool carries the sub-agent instructions under one of these keys
    // (Claude uses `prompt`; the rest are common Agent tool field names).
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

/// After this many CONSECUTIVE manual code lookups (code-file Reads and Greps)
/// with no intervening lens tool call, [`inspect_escalation`] denies the call
/// once instead of nudging — Serena's `remind` pattern: the counter resets on
/// deny and again on the next lens tool call or file edit (see the PostToolUse
/// arm in `session::hook`), so this is a single blocking stop per drift
/// episode, never a hard wall. 4, not 6: the measured drift signature is a
/// short `Grep,Read,Read` chain, which a threshold of 6 never catches inside
/// a focused task.
const READ_DENY_THRESHOLD_DEFAULT: u64 = 4;

/// The deny threshold, overridable via `LENS_READ_DENY_THRESHOLD` so an A/B
/// can disable it (`0`) without a recompile. Falls back to
/// [`READ_DENY_THRESHOLD_DEFAULT`] when unset or unparseable.
fn read_deny_threshold() -> u64 {
    std::env::var("LENS_READ_DENY_THRESHOLD")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(READ_DENY_THRESHOLD_DEFAULT)
}

/// First-Grep deny kill-switch: `LENS_GREP_FIRST_DENY=0` disables the armed
/// deny without a recompile (the per-mechanism ablation knob). On by default.
fn grep_first_deny_enabled() -> bool {
    std::env::var("LENS_GREP_FIRST_DENY").map_or(true, |v| v.trim() != "0")
}
/// Grep-scope deny dark-launch switch: `LENS_GREP_SCOPE_DENY=1` enables it.
/// Default OFF (unlike [`grep_first_deny_enabled`]'s default-ON kill-switch) —
/// this mechanism is being dark-launched on a subset of machines before any
/// default-on flip, so the polarity is intentionally the opposite one.
pub fn grep_scope_deny_enabled() -> bool {
    std::env::var("LENS_GREP_SCOPE_DENY").is_ok_and(|v| v.trim() == "1")
}

// ── Reroute-rail dark-launch switches ───────────────────────────────────────
// One per rail (see `reroute` for the counter-key contract), all with the
// grep-scope polarity: `=1` enables, default OFF, independently reversible.

/// Grep-symbol deny rail (`gsym`): `LENS_GREP_SYMBOL_DENY=1` enables it.
pub fn grep_symbol_deny_enabled() -> bool {
    std::env::var("LENS_GREP_SYMBOL_DENY").is_ok_and(|v| v.trim() == "1")
}
/// Read-skeleton deny rail (`rskel`): `LENS_READ_SKELETON_DENY=1` enables it.
pub fn read_skeleton_deny_enabled() -> bool {
    std::env::var("LENS_READ_SKELETON_DENY").is_ok_and(|v| v.trim() == "1")
}
/// Bash-aggregate nudge rail (`bagg`): `LENS_BASH_AGG_NUDGE=1` enables it.
pub fn bash_agg_nudge_enabled() -> bool {
    std::env::var("LENS_BASH_AGG_NUDGE").is_ok_and(|v| v.trim() == "1")
}
/// Bash-aggregate deny rail (`bagg`): `LENS_BASH_AGG_DENY=1` enables it.
pub fn bash_agg_deny_enabled() -> bool {
    std::env::var("LENS_BASH_AGG_DENY").is_ok_and(|v| v.trim() == "1")
}
/// Edit-callers nudge rail (`elink`): `LENS_EDIT_LINKS_NUDGE=1` enables it.
pub fn edit_links_nudge_enabled() -> bool {
    std::env::var("LENS_EDIT_LINKS_NUDGE").is_ok_and(|v| v.trim() == "1")
}
/// Grep-ast nudge rail (`gast`): `LENS_GREP_AST_NUDGE=1` enables it.
pub fn grep_ast_nudge_enabled() -> bool {
    std::env::var("LENS_GREP_AST_NUDGE").is_ok_and(|v| v.trim() == "1")
}
/// Grep-ast deny rail (`gast`): `LENS_GREP_AST_DENY=1` enables it.
pub fn grep_ast_deny_enabled() -> bool {
    std::env::var("LENS_GREP_AST_DENY").is_ok_and(|v| v.trim() == "1")
}
/// Read-overview nudge rail (`rovr`): `LENS_READ_OVERVIEW_NUDGE=1` enables it.
pub fn read_overview_nudge_enabled() -> bool {
    std::env::var("LENS_READ_OVERVIEW_NUDGE").is_ok_and(|v| v.trim() == "1")
}

/// The symbol whose declaration this Edit/MultiEdit touches, or `None`. Edit
/// carries `old_string`/`new_string` at the top level; MultiEdit carries an
/// `edits[]` array — the first decl-touching edit wins. Shared by the elink
/// arm in [`route_inner`] and the hook's shadow-counter plane so the live and
/// would-fire derivations can never drift apart.
pub(crate) fn edited_decl_symbol(tool: &str, tool_input: &Value) -> Option<String> {
    let symbol_of = |edit: &Value| {
        let old = edit.get("old_string")?.as_str()?;
        let new = edit.get("new_string").and_then(Value::as_str).unwrap_or("");
        reroute::edit_callers::edited_symbol(old, new)
    };
    match tool {
        "Edit" => symbol_of(tool_input),
        "MultiEdit" => tool_input.get("edits")?.as_array()?.iter().find_map(symbol_of),
        _ => None,
    }
}

/// The read-skeleton rail's `edited` set for one Read: contains `path` iff an
/// edit to it was recorded this session (the PostToolUse hook marks
/// `editpath:{path}` on Edit/MultiEdit/Write). Shared by [`read_decision`] and
/// the hook's shadow-counter plane.
pub(crate) fn edited_paths_for(data_dir: &Path, session_id: &str, path: &str) -> HashSet<String> {
    let mut edited = HashSet::new();
    if throttle::fired(data_dir, session_id, &format!("editpath:{path}")) {
        edited.insert(path.to_string());
    }
    edited
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
/// code-file reads toward the graph via [`inspect_escalation`]. Only CODE files
/// the graph indexes ([`crate::discovery::extract::spec_for_extension`]) count
/// toward escalation — reading a doc/config/data file shouldn't push the agent
/// at the graph. Markdown is graph-indexed too (headings/links), but it is prose
/// read linearly, not code navigation, so it is excluded here: reading a doc is
/// not the manual-code-tracing drift this escalation exists to catch.
fn read_decision(tool_input: &Value, ctx: &RouteCtx) -> Decision {
    if !ctx.level.nudges() {
        return Decision::Passthrough;
    }
    let path = tool_input["file_path"].as_str().unwrap_or("");
    // Reroute rail 1b (rskel): a whole, unedited code-file Read is denied once
    // per session toward lens_skeleton (LENS_READ_SKELETON_DENY, the scope
    // deny's gates). Runs BEFORE the escalation so the two denies can't stack;
    // `nudge_once` runs last so a blocked gate never spends the one-shot, and
    // the deny resets the lookup counter so the verbatim retry always passes.
    if read_skeleton_deny_enabled()
        && ctx.level.steers()
        && ctx.mcp_ready
        && index_present(ctx.data_dir)
    {
        let has_offset_or_limit =
            tool_input.get("offset").is_some() || tool_input.get("limit").is_some();
        let edited = edited_paths_for(ctx.data_dir, ctx.session_id, path);
        if reroute::read_skeleton::read_is_skeletonizable(path, has_offset_or_limit, &edited)
            && nudge_once(ctx, "read-skeleton")
        {
            throttle::reset(ctx.data_dir, ctx.session_id, "read-code");
            return Decision::Deny(reroute::read_skeleton::reason(path));
        }
    }
    // Reroute rail 2c (rovr): the Nth code Read with no repo map yet gets a
    // one-shot lens_overview nudge (LENS_READ_OVERVIEW_NUDGE). The count is
    // fed by the hook via `ctx.reads_since_map`, zeroed by any
    // lens_map/lens_overview call; level is already gated above (nudges).
    if read_overview_nudge_enabled()
        && ctx.mcp_ready
        && reroute::read_overview::overview_due(
            ctx.reads_since_map,
            reroute::read_overview::threshold(),
        )
        && index_present(ctx.data_dir)
        && nudge_once(ctx, "read-overview")
    {
        return Decision::Context(reroute::read_overview::nudge(ctx.reads_since_map));
    }
    let is_code = tool_input["file_path"]
        .as_str()
        .and_then(file_extension)
        .map(|ext| {
            crate::discovery::extract::spec_for_extension(&ext)
                .is_some_and(|s| s.name != "markdown")
        })
        .unwrap_or(false);
    if is_code {
        if let Some(d) = inspect_escalation(ctx) {
            return d;
        }
    }
    if nudge_once(ctx, "read") {
        Decision::Context(READ_NUDGE.to_string())
    } else {
        Decision::Passthrough
    }
}

/// Shared consecutive-lookup escalation for code Reads and Greps (the
/// `read-code` counter, reset by any lens tool call or file edit in
/// PostToolUse — see `session::hook`). Once the count crosses
/// [`READ_GRAPH_THRESHOLD`] the graph-specific nudge fires, then again every
/// [`READ_GRAPH_PERIOD`]-th lookup. Past [`read_deny_threshold`] consecutive
/// lookups (while steering), the call is denied once instead — see
/// [`READ_DENY_REASON`]. `None` → the caller falls through to its one-shot tip.
fn inspect_escalation(ctx: &RouteCtx) -> Option<Decision> {
    let n = throttle::bump(ctx.data_dir, ctx.session_id, "read-code");
    let deny_threshold = read_deny_threshold();
    if deny_threshold > 0 && n >= deny_threshold && ctx.level.steers() {
        // Deny once per drift episode, never a hard wall: reset the
        // counter first so the immediate retry passes if still needed.
        throttle::reset(ctx.data_dir, ctx.session_id, "read-code");
        return Some(mcp_redirect(ctx, Decision::Deny(READ_DENY_REASON.to_string())));
    }
    let threshold = read_graph_threshold();
    if n >= threshold && (n - threshold).is_multiple_of(READ_GRAPH_PERIOD) {
        return Some(Decision::Context(read_graph_nudge(n)));
    }
    None
}

/// The escalation nudge: names the three graph tools and frames them as the
/// replacement for looking file-by-file. `n` is the running lookup count, so
/// the agent sees how much manual searching it has already done.
fn read_graph_nudge(n: u64) -> String {
    format!(
        "<context_guidance>\n  <tip>\n    You've made {n} manual code lookups (Read/Grep) this session. If you're tracing how the code fits together — who calls a function, what it calls, where a symbol is defined, how one part reaches another — stop looking file by file and query the graph instead: lens_symbol to locate a symbol, lens_links for its callers/callees, lens_path for how A reaches B, lens_search(queries: [...]) for where an idea appears. Just need one file's shape? lens_skeleton(path), with include_bodies: [\"the_fn\"] for the bodies you actually need. One query replaces many lookups and keeps their bytes out of your context. (Run lens_map once if the graph is empty.) At 4 consecutive code Reads/Greps with no lens tool call between them, the next call is denied once — a nudge to switch, not a hard wall.\n  </tip>\n</context_guidance>"
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
    // Network/build/inline-HTTP → hard redirect into lens_run. Steering only;
    // under wrap-only these fall
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
    // ignore the advisory. Skip both.
    if classify::classify(cmd) == classify::Risk::Safe {
        return Decision::Passthrough;
    }
    // Reroute rail 1c (bagg) DENY: a data-aggregate pipeline (`wc -l`,
    // `sort | uniq`, …) is denied once per session toward `lens_run`'s darkroom.
    // Placed BEFORE the wrap rewrite below so at `full` (where `wraps()` would
    // otherwise rewrite it first) the deny still reaches. Dark-launched behind
    // LENS_BASH_AGG_DENY, gated on `steers()` like the grep deny rails. Kept
    // CONSERVATIVE per T2's precision note (the classifier is lexical: shell
    // session-state coupling and substring FPs), so the LENS_BASH_AGG_NUDGE
    // nudge stays the default arm and the deny only pre-empts the wrap when its
    // own flag is on. The stateful check above already guarantees a
    // state-changing command never reaches here. `nudge_once` runs LAST so a
    // blocked gate never spends the one-shot; consuming it is what lets the
    // verbatim retry fall through and pass (Bash has no read-code-style counter
    // to reset, unlike the grep deny rails).
    if bash_agg_deny_enabled()
        && ctx.level.steers()
        && ctx.mcp_ready
        && reroute::bash_aggregate::is_data_aggregate(cmd)
        && index_present(ctx.data_dir)
        && nudge_once(ctx, "bash-agg")
    {
        return Decision::Deny(reroute::bash_aggregate::deny_reason(cmd));
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
        } else if let Some(d) = bash_agg_nudge(cmd, ctx) {
            d
        } else if ctx.level.nudges() && nudge_once(ctx, "bash") {
            Decision::Context(BASH_NUDGE.to_string())
        } else {
            Decision::Passthrough
        }
    } else if let Some(d) = bash_agg_nudge(cmd, ctx) {
        d
    } else {
        Decision::Passthrough
    }
}

/// Reroute rail 1c (bagg): a one-shot Context nudge for a Bash pipeline that
/// counts, sorts, or reshapes data — the transform belongs in `lens_run`'s
/// darkroom (LENS_BASH_AGG_NUDGE, the scope deny's gates). Slotted only where
/// [`bash_decision`] would otherwise emit the generic nudge or pass through:
/// the wrap rewrite and the net/build redirects keep priority, and a
/// state-changing command never reaches this (the stateful check runs first).
/// `nudge_once` runs last so a blocked gate never spends the one-shot.
fn bash_agg_nudge(cmd: &str, ctx: &RouteCtx) -> Option<Decision> {
    (bash_agg_nudge_enabled()
        && ctx.level.nudges()
        && ctx.mcp_ready
        && reroute::bash_aggregate::is_data_aggregate(cmd)
        && index_present(ctx.data_dir)
        && nudge_once(ctx, "bash-agg"))
    .then(|| Decision::Context(reroute::bash_aggregate::reason()))
}

/// Redirect a context-flooding network or build command: replace it with an `echo`
/// that tells the model to run it through `lens_run` instead, so
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

/// A non-lens MCP tool, whose
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
/// matching call. The default is 10 — keeps
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
/// Is the content index populated? Read-only check: opens `<data_dir>/index.db`
/// and looks for at least one row in the `file_manifest` table — the Tantivy-era
/// populated-index signal (SQLite now holds only the mtime manifest + a backend
/// version marker; the pre-Tantivy FTS5 `chunks` table is dropped on every
/// `Index::open`, see `src/index/schema.rs`). ANY error (missing file, missing
/// table, empty result) reads as not present — a grep-scope gate must never
/// assume search will answer when the index isn't there yet. Deliberately a raw
/// read-only `rusqlite` open, never `Index::open`: the latter's `init()` runs a
/// DROP/VACUUM migration on open, which must never fire on the routing hot path.
pub fn index_present(data_dir: &Path) -> bool {
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        data_dir.join("index.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) else {
        return false;
    };
    conn.query_row("SELECT 1 FROM file_manifest LIMIT 1", [], |_| Ok(()))
        .is_ok()
}
/// Test fixture: seed `<dir>/index.db` with a populated `file_manifest` table so
/// [`index_present`] reads true. `pub(crate)` so T2's grep-scope-arm tests
/// (same crate, different module) can reuse it.
#[cfg(test)]
pub(crate) fn seed_index(dir: &Path) {
    let conn = rusqlite::Connection::open(dir.join("index.db")).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS file_manifest (path TEXT PRIMARY KEY, mtime INTEGER NOT NULL);
         INSERT INTO file_manifest (path, mtime) VALUES ('src/f0.rs', 123);",
    )
    .unwrap();
}

/// The authoritative tool-selection directive injected at `SessionStart` while
/// steering is active. The `<context_window_protection>` block: the
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

/// `<context_window_protection>` through the open `<when_plain_tools_win>` tag.
const BLOCK_HEAD: &str = r##"<context_window_protection>
  <why>
    Raw tool results sit in the transcript and get re-read on every later turn, so one large dump keeps taxing the model long after it was useful. lens exists to avoid that: it runs the work in a subprocess (the "darkroom") and hands back only the finished answer. The habit to build: compute over data in code, rather than pulling the data into the conversation to read it.
  </why>
  <loading_lens_tools>
    lens's tools may start out unregistered in this harness — their schemas aren't loaded, so a direct call errors ("tool not found" or a validation error). Register them once, before your first lens_* call:
    ToolSearch(query: "select:lens_run,lens_run_file,lens_search,lens_index,lens_map,lens_symbol,lens_links,lens_path,lens_recall,lens_skeleton,lens_overview,lens_find,lens_grep_ast")
    If a lens_* call later comes back not-found, re-run that ToolSearch and retry instead of falling back to Bash/Read/Grep.
  </loading_lens_tools>
  <which_tool>
    - How the code fits together (callers, callees, where a symbol is defined, how one part reaches another, imports): run lens_map once, then walk it with lens_symbol / lens_links / lens_path instead of opening file after file. lens_recall expands anything returned compacted.
    - Where a string or idea appears across the tree: lens_index once, then lens_search(queries: [...]) — batch several questions into the array and get ranked snippets, not whole files.
    - Turning data into an answer (filter, count, parse, reshape, summarize): lens_run(language, code) or lens_run_file(path, language, code). Only what you print returns; the inputs stay in the darkroom.
    - Recovering something offloaded or truncated: lens_recall(ref).
    - You know the symbol's exact name: lens_symbol. You only know what it does, not its name: lens_find. You have a syntax-shape pattern (a call, a signature shape) rather than a name or plain-text idea: lens_grep_ast.
    - Whole-repo orientation — how the codebase is put together before you've read anything: lens_overview. A digest is already pushed into context at session start; treat that as the first call's answer and expand from it with lens_symbol / lens_links rather than re-running lens_overview.
    - One file's shape — signatures and structure without the bodies: lens_skeleton(path); pass include_bodies: ["the_fn"] to get back the full text of just the functions you need, in the same call.
    - Worked examples: `lens_grep_ast(language="rust", query="(impl_item type: (type_identifier) @t (#eq? @t \"Forge\"))")` finds all impl blocks matching a syntax shape, not text. `lens_find(query="where sessions are persisted")` locates a symbol when you know its behavior but not its name. `lens_links(node_id)` shows all callers and callees before you change a declaration. `lens_path(from="route_inner", to="bump_stat")` traces how one symbol reaches another.
  </which_tool>
  <when_plain_tools_win>"##;

const BULLET_BASH: &str = "\n    - Bash: keep it for commands that change something, or whose output is short and you just want to glance at it (pwd, a clean git status, moving a file). The moment you'd pipe that output onward to count, grep, or reshape it, give it to lens_run instead so the bulk never lands in the transcript.";

const BULLET_READ: &str = "\n    - Need to understand a file? lens_skeleton(path) first; then lens_skeleton(path, include_bodies: [\"the_fn\"]) for the one body you need — not a second Read. Read is for when you are about to Edit (Edit must match exact bytes). Already Read the full file this session? Use what you have — do not re-analyse it with lens tools.\n    - Common rationalizations that lead to waste: \"the file is small\", \"I already know the path\", \"one Read beats two lens calls\" — measured across sessions these produce whole-file dumps that tax every later turn.";

const BULLET_SEARCH: &str = "\n    - Finding or tracing something? Map the intent, don't grep: where is X / where does an idea appear — lens_search(queries: [...]) or lens_symbol(name); what calls X / what does X call — lens_links; how does A reach B — lens_path; know the behavior but not the name — lens_find. Grep's line hits pull in a whole-file Read per hit; that chain is the drift these replace. \"A quick grep is lighter\" is the rationalization that starts it — one lens_search is the lighter call.";

const BULLET_WEBFETCH: &str = "\n    - WebFetch is off here: pull a URL with lens_run (python), keep only the part of the response you need, and print that. The full page stays in the darkroom, retrievable via lens_recall.";

/// Close `</when_plain_tools_win>` through `</context_window_protection>`.
const BLOCK_TAIL: &str = r##"
  </when_plain_tools_win>
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
            reads_since_map: 0,
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
        // When the server is unreachable,
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
            reads_since_map: 0,
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
            "Read nudges at steer (Read is nudged whenever routing is active)"
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
        // Reaches the default deny threshold (4) at its last read, so it must
        // not race `read_denies_at_threshold_then_resets_and_respects_override`,
        // which overrides `LENS_READ_DENY_THRESHOLD` process-wide.
        let _guard = READ_DENY_ENV_LOCK.lock().unwrap();
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
        // 4th: the deny threshold — deny takes priority over the periodic nudge
        // (see `read_denies_at_threshold_then_resets_and_respects_override`).
        assert!(matches!(route("Read", &code, &ctx), Decision::Deny(_)));
    }

    #[test]
    fn prompt_intent_matcher_hits_find_trace_shapes_only() {
        for p in [
            "Where is the retry logic defined?",
            "what calls Forge::load_graph?",
            "How does the SessionStart digest get built?",
            "Which function parses the stream-json transcript?",
            "trace the deny decision from hook to output",
        ] {
            assert!(prompt_wants_find_trace(p), "should match: {p}");
        }
        for p in [
            "fix the failing test in throttle.rs",
            "bump the version and tag the release",
            "add a --runs flag",
            "where is lens_symbol's handler? use lens_links after", // names a lens tool: user is steering
        ] {
            assert!(!prompt_wants_find_trace(p), "should not match: {p}");
        }
    }

    #[test]
    fn greps_count_toward_the_same_deny_counter_as_code_reads() {
        // The measured drift signature is Grep → Read → Read → Read: mixed
        // lookups must share one counter, denying the 4th call.
        let _guard = READ_DENY_ENV_LOCK.lock().unwrap();
        let d = tempdir().unwrap();
        // Populate the index so the broad Greps here reach the shared lookup
        // counter through the (now index-gated) scope block rather than being
        // skipped for lack of an index.
        seed_index(d.path());
        let ctx = rc(Level::Steer, true, d.path());
        let grep = json!({"pattern": "include_bodies"});
        let code = json!({"file_path": "src/server.rs"});
        // 1st (Grep): the one-shot intent-mapping tip.
        assert_eq!(
            route("Grep", &grep, &ctx),
            Decision::Context(GREP_NUDGE.to_string())
        );
        // 2nd (Read): the one-shot general tip.
        assert_eq!(
            route("Read", &code, &ctx),
            Decision::Context(READ_NUDGE.to_string())
        );
        // 3rd (Read, graph threshold): escalation.
        assert!(matches!(route("Read", &code, &ctx), Decision::Context(_)));
        // 4th (Read): deny, reason maps find/trace intents to the graph tools.
        match route("Read", &code, &ctx) {
            Decision::Deny(reason) => assert!(
                reason.contains("lens_search") && reason.contains("lens_links"),
                "deny reason maps find/trace intents: {reason}"
            ),
            other => panic!("4th mixed lookup should deny, got {other:?}"),
        }
        // Deny reset the counter: a fresh Grep passes through (tips spent).
        assert_eq!(route("Grep", &grep, &ctx), Decision::Passthrough);
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

    // ── read_decision(): consecutive-code-read deny (T5) ────────────────────
    // NOTE: default-vs-override behavior is grouped into one test (like
    // `mcp_ready_env_override_and_heartbeat`) to avoid racing another test's
    // view of `LENS_READ_DENY_THRESHOLD`. `read_code_files_escalate_to_the_graph`
    // (above) also depends on the default value, so both share this lock.
    // The other deny tests below never touch the env var, so they're safe to
    // run in parallel.
    static READ_DENY_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // Serializes tests that toggle `LENS_GREP_SCOPE_DENY` (mirrors
    // `READ_DENY_ENV_LOCK`). The one test that needs both a stable
    // `LENS_GREP_FIRST_DENY` and a toggled `LENS_GREP_SCOPE_DENY` acquires
    // READ_DENY_ENV_LOCK first, then SCOPE_ENV_LOCK — never the reverse.
    static SCOPE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn armed_first_grep_denies_once_and_respects_gates() {
        let _guard = READ_DENY_ENV_LOCK.lock().unwrap();
        std::env::remove_var("LENS_GREP_FIRST_DENY");
        let d = tempdir().unwrap();
        // The scope gate now requires a populated index; seed one so the armed
        // grep-first deny (this test's subject) can still fire on a broad Grep.
        seed_index(d.path());
        let ctx = rc(Level::Full, true, d.path());
        let g = json!({"pattern": "deny_threshold"});

        // Unarmed: no deny.
        assert!(!matches!(route("Grep", &g, &ctx), Decision::Deny(_)));

        // Armed but MCP not ready: passthrough, marker STAYS armed.
        throttle::bump(ctx.data_dir, ctx.session_id, "grep-first");
        let not_ready = RouteCtx {
            mcp_ready: false,
            ..ctx
        };
        assert!(!matches!(route("Grep", &g, &not_ready), Decision::Deny(_)));

        // MCP ready: the armed deny fires with the intent mapping.
        match route("Grep", &g, &ctx) {
            Decision::Deny(reason) => assert!(
                reason.contains("find/trace") && reason.contains("lens_search"),
                "deny reason maps intents: {reason}"
            ),
            other => panic!("armed first Grep should deny, got {other:?}"),
        }
        // Consumed: the retried Grep passes.
        assert!(!matches!(route("Grep", &g, &ctx), Decision::Deny(_)));

        // Kill-switch: armed but LENS_GREP_FIRST_DENY=0 disables the deny.
        throttle::reset(ctx.data_dir, ctx.session_id, "read-code");
        throttle::bump(ctx.data_dir, ctx.session_id, "grep-first");
        std::env::set_var("LENS_GREP_FIRST_DENY", "0");
        assert!(!matches!(route("Grep", &g, &ctx), Decision::Deny(_)));
        std::env::remove_var("LENS_GREP_FIRST_DENY");
    }

    // ── route(): grep-scope gate + shared one-per-prompt deny budget ────────

    #[test]
    fn single_file_grep_leaves_grep_first_armed() {
        // A Grep scoped to a real file is SingleFile → passthrough bias: the
        // scope block is skipped entirely, so an armed grep-first survives and a
        // later broad Grep still fires it.
        let _guard = READ_DENY_ENV_LOCK.lock().unwrap();
        std::env::remove_var("LENS_GREP_FIRST_DENY");
        let d = tempdir().unwrap();
        seed_index(d.path());
        let ctx = rc(Level::Full, true, d.path());
        throttle::bump(ctx.data_dir, ctx.session_id, "grep-first");

        let file = d.path().join("real.rs");
        std::fs::write(&file, "fn x() {}").unwrap();
        let single = json!({"pattern": "x", "path": file.to_str().unwrap()});
        assert!(
            !matches!(route("Grep", &single, &ctx), Decision::Deny(_)),
            "single-file grep must never hit the scope deny"
        );

        // The grep-first marker survived → the following broad Grep IS denied.
        let broad = json!({"pattern": "x"});
        assert!(
            matches!(route("Grep", &broad, &ctx), Decision::Deny(_)),
            "broad grep after the single-file one fires the still-armed grep-first deny"
        );
        std::env::remove_var("LENS_GREP_FIRST_DENY");
    }

    #[test]
    fn grep_first_retry_passes_even_at_counter_threshold() {
        // Double-deny regression: at read-code=3 with grep-first armed, the
        // grep-first deny must reset read-code so the verbatim retry isn't then
        // caught by the inspect-escalation deny at 4.
        let _guard = READ_DENY_ENV_LOCK.lock().unwrap();
        std::env::remove_var("LENS_GREP_FIRST_DENY");
        std::env::remove_var("LENS_READ_DENY_THRESHOLD");
        let d = tempdir().unwrap();
        seed_index(d.path());
        let ctx = rc(Level::Full, true, d.path());
        // Prime the shared lookup counter to 3 (one below the deny threshold).
        for _ in 0..3 {
            throttle::bump(ctx.data_dir, ctx.session_id, "read-code");
        }
        throttle::bump(ctx.data_dir, ctx.session_id, "grep-first");

        let broad = json!({"pattern": "x"});
        match route("Grep", &broad, &ctx) {
            Decision::Deny(reason) => assert!(
                reason.contains("find/trace"),
                "the grep-first deny fires, not the counter deny: {reason}"
            ),
            other => panic!("expected the armed grep-first deny, got {other:?}"),
        }
        // Verbatim retry: read-code was reset by the deny and grep-first is
        // consumed → neither deny fires.
        assert!(
            !matches!(route("Grep", &broad, &ctx), Decision::Deny(_)),
            "the verbatim retry must pass (read-code reset, grep-first consumed)"
        );
        std::env::remove_var("LENS_READ_DENY_THRESHOLD");
    }

    #[test]
    fn scope_deny_fires_once_per_arm_and_respects_flag() {
        // grep-scope deny in isolation: OFF by default, ON only under the flag,
        // one deny per arm, verbatim retry passes, re-arming denies again.
        let _guard = SCOPE_ENV_LOCK.lock().unwrap();
        let d = tempdir().unwrap();
        seed_index(d.path());
        let ctx = rc(Level::Full, true, d.path());
        let broad = json!({"pattern": "x"});
        throttle::bump(ctx.data_dir, ctx.session_id, "grep-scope");

        // Flag off → never denies (marker untouched).
        std::env::remove_var("LENS_GREP_SCOPE_DENY");
        assert!(
            !matches!(route("Grep", &broad, &ctx), Decision::Deny(_)),
            "flag off → no scope deny"
        );

        // Flag on → the armed grep-scope deny fires once with its own reason.
        std::env::set_var("LENS_GREP_SCOPE_DENY", "1");
        match route("Grep", &broad, &ctx) {
            Decision::Deny(reason) => assert_eq!(reason, GREP_SCOPE_DENY_REASON),
            other => panic!("armed grep-scope deny should fire, got {other:?}"),
        }
        // Consumed → retry passes.
        assert!(
            !matches!(route("Grep", &broad, &ctx), Decision::Deny(_)),
            "grep-scope consumed → retry passes"
        );
        // Re-arm → denies again.
        throttle::bump(ctx.data_dir, ctx.session_id, "grep-scope");
        assert!(
            matches!(route("Grep", &broad, &ctx), Decision::Deny(_)),
            "re-arming grep-scope denies again"
        );
        std::env::remove_var("LENS_GREP_SCOPE_DENY");
    }

    #[test]
    fn grep_first_deny_consumes_scope_marker() {
        // Both markers armed + flag on: exactly ONE deny (grep-first wins) that
        // also consumes the grep-scope marker, so the verbatim retry passes.
        // Needs a stable LENS_GREP_FIRST_DENY (READ_DENY_ENV_LOCK) and a set
        // LENS_GREP_SCOPE_DENY (SCOPE_ENV_LOCK); acquire in that order.
        let _read_guard = READ_DENY_ENV_LOCK.lock().unwrap();
        let _scope_guard = SCOPE_ENV_LOCK.lock().unwrap();
        std::env::remove_var("LENS_GREP_FIRST_DENY");
        std::env::set_var("LENS_GREP_SCOPE_DENY", "1");
        let d = tempdir().unwrap();
        seed_index(d.path());
        let ctx = rc(Level::Full, true, d.path());
        let broad = json!({"pattern": "x"});
        throttle::bump(ctx.data_dir, ctx.session_id, "grep-first");
        throttle::bump(ctx.data_dir, ctx.session_id, "grep-scope");

        match route("Grep", &broad, &ctx) {
            Decision::Deny(reason) => assert_eq!(reason, GREP_FIRST_DENY_REASON),
            other => panic!("grep-first should win the single deny, got {other:?}"),
        }
        // Both markers consumed by the one deny → retry passes (no second deny).
        assert!(
            !matches!(route("Grep", &broad, &ctx), Decision::Deny(_)),
            "the single deny consumed both markers → retry passes"
        );
        std::env::remove_var("LENS_GREP_SCOPE_DENY");
        std::env::remove_var("LENS_GREP_FIRST_DENY");
    }

    #[test]
    fn deny_requires_populated_index() {
        // No populated index → the scope gate short-circuits BEFORE the throttle
        // take, so an armed grep-first is neither fired nor consumed.
        let _guard = READ_DENY_ENV_LOCK.lock().unwrap();
        std::env::remove_var("LENS_GREP_FIRST_DENY");
        let d = tempdir().unwrap(); // no index.db
        let ctx = rc(Level::Full, true, d.path());
        let broad = json!({"pattern": "x"});
        throttle::bump(ctx.data_dir, ctx.session_id, "grep-first");
        assert!(
            !index_present(ctx.data_dir),
            "precondition: the tempdir has no populated index"
        );
        assert!(
            !matches!(route("Grep", &broad, &ctx), Decision::Deny(_)),
            "no populated index → the scope block is skipped, no deny"
        );
        // The marker survived: seeding the index and grepping now denies.
        seed_index(d.path());
        assert!(
            matches!(route("Grep", &broad, &ctx), Decision::Deny(_)),
            "grep-first was never consumed (take unreached) → now it denies"
        );
        std::env::remove_var("LENS_GREP_FIRST_DENY");
    }

    #[test]
    fn unknown_scope_passes() {
        // A nonexistent path classifies as Unknown → passthrough bias, never
        // denied, even with grep-first armed and the index populated. The marker
        // survives (scope != Broad short-circuits the block).
        let _guard = READ_DENY_ENV_LOCK.lock().unwrap();
        std::env::remove_var("LENS_GREP_FIRST_DENY");
        let d = tempdir().unwrap();
        seed_index(d.path());
        let ctx = rc(Level::Full, true, d.path());
        let ghost = d.path().join("does-not-exist");
        let unknown = json!({"pattern": "x", "path": ghost.to_str().unwrap()});
        throttle::bump(ctx.data_dir, ctx.session_id, "grep-first");
        assert!(
            !matches!(route("Grep", &unknown, &ctx), Decision::Deny(_)),
            "Unknown scope must pass through"
        );
        // grep-first was not consumed → a broad Grep now denies.
        let broad = json!({"pattern": "x"});
        assert!(
            matches!(route("Grep", &broad, &ctx), Decision::Deny(_)),
            "Unknown scope left grep-first armed"
        );
        std::env::remove_var("LENS_GREP_FIRST_DENY");
    }

    #[test]
    fn read_denies_at_threshold_then_resets_and_respects_override() {
        let _guard = READ_DENY_ENV_LOCK.lock().unwrap();
        std::env::remove_var("LENS_READ_DENY_THRESHOLD");
        let d = tempdir().unwrap();
        let ctx = rc(Level::Full, true, d.path());
        let code = json!({"file_path": "src/server.rs"});
        for i in 1..=3 {
            assert!(
                !matches!(route("Read", &code, &ctx), Decision::Deny(_)),
                "read {i} of 3 must not deny (default threshold is 4)"
            );
        }
        match route("Read", &code, &ctx) {
            Decision::Deny(reason) => assert!(
                reason.contains("lens_skeleton"),
                "deny reason names lens_skeleton: {reason}"
            ),
            other => panic!("4th consecutive code read should deny by default, got {other:?}"),
        }
        // The deny reset the counter, so the immediate retry (5th) passes.
        assert!(
            !matches!(route("Read", &code, &ctx), Decision::Deny(_)),
            "5th read (post-reset) must not deny"
        );

        // LENS_READ_DENY_THRESHOLD=0 disables the deny entirely.
        std::env::set_var("LENS_READ_DENY_THRESHOLD", "0");
        let d2 = tempdir().unwrap();
        let ctx2 = rc(Level::Full, true, d2.path());
        for i in 1..=12 {
            assert!(
                !matches!(route("Read", &code, &ctx2), Decision::Deny(_)),
                "read {i}: threshold=0 must never deny"
            );
        }
        std::env::remove_var("LENS_READ_DENY_THRESHOLD");
    }

    #[test]
    fn read_deny_never_bumped_by_non_code_files() {
        let d = tempdir().unwrap();
        let ctx = rc(Level::Full, true, d.path());
        let doc = json!({"file_path": "NOTES.md"});
        for i in 1..=10 {
            assert!(
                !matches!(route("Read", &doc, &ctx), Decision::Deny(_)),
                "read {i}: non-code files never count toward the deny counter"
            );
        }
    }

    #[test]
    fn read_deny_gated_on_mcp_ready() {
        let d = tempdir().unwrap();
        let ctx = rc(Level::Full, false, d.path());
        let code = json!({"file_path": "src/server.rs"});
        for i in 1..=8 {
            assert!(
                !matches!(route("Read", &code, &ctx), Decision::Deny(_)),
                "read {i}: mcp not ready must downgrade the deny to passthrough"
            );
        }
    }

    #[test]
    fn read_deny_never_fires_at_nudge_level() {
        let d = tempdir().unwrap();
        let ctx = rc(Level::Nudge, true, d.path());
        let code = json!({"file_path": "src/server.rs"});
        for i in 1..=8 {
            assert!(
                !matches!(route("Read", &code, &ctx), Decision::Deny(_)),
                "read {i}: Level::Nudge does not steer, so it must never deny"
            );
        }
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

    // ── index_present ─────────────────────────────────────────────────────
    #[test]
    fn index_present_false_when_db_missing() {
        let d = tempdir().unwrap();
        assert!(!index_present(d.path()));
    }

    #[test]
    fn index_present_false_when_file_manifest_empty() {
        let d = tempdir().unwrap();
        let conn = rusqlite::Connection::open(d.path().join("index.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS file_manifest (path TEXT PRIMARY KEY, mtime INTEGER NOT NULL);",
        )
        .unwrap();
        assert!(!index_present(d.path()));
    }

    #[test]
    fn index_present_true_when_populated() {
        let d = tempdir().unwrap();
        seed_index(d.path());
        assert!(index_present(d.path()));
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
            "lens_skeleton",
            "lens_overview",
            "lens_find",
            "lens_grep_ast",
            "include_bodies",
            "which_tool",
            "WebFetch is off",
            // the highest-impact additions over the old soft nudge:
            "ToolSearch",                // deferred-tool bootstrap
            "compute over data in code", // authoritative framing
            "when_plain_tools_win",      // nuanced credibility
        ] {
            assert!(b.contains(needle), "session_block missing {needle:?}");
        }
    }
}
