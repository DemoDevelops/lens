//! PreToolUse routing policy — pass through, deny, or rewrite a tool call.
//!
//! Gated by `LENS_ROUTING` (off|nudge|steer|wrap|full); default `full`. Concerns layer:
//!
//!   * **nudge** — bootstrap-only back-compat level: injects the tool-selection
//!     guide into every sub-agent (`Agent`/`Task`) prompt and at `SessionStart`,
//!     but never denies, redirects, or rewrites a call. The per-call nudge arms
//!     this level once gated were retired (measured conversion 0-33% vs 51-71%
//!     for denies); the level name stays parseable so existing configs keep
//!     working.
//!   * **steer** — deny `WebFetch` (once per session, MCP-gated), redirect
//!     curl/wget/build/inline-HTTP `Bash` commands into `lens_run`, and run the
//!     deny-grade reroute rails below.
//!   * **wrap** — transparently rewrite a read-only, high-output `Bash` command
//!     into `lens wrap -- <cmd>` so its output is offloaded losslessly.
//!   * **full** — both steer and wrap.
//!
//! Under steer/full a broad-scope Grep (its `path` spans a directory or the whole
//! repo, see [`grep_scope`]) is denied at most once per prompt toward a lens call —
//! gated on a populated index ([`index_present`]) — via the always-on first-Grep
//! deny and the grep-scope deny ([`grep_scope_deny_enabled`], default ON,
//! `=0` disables). Shell `grep`/`rg`/`git grep` in a Bash command shares that
//! same one-per-prompt deny budget (see [`reroute::bash_grep`]). The reroute
//! rails (see [`reroute`]) are deny-only, all behind the same kill-switch
//! polarity.
//!
//! Safety rails: MCP-redirect decisions (WebFetch deny, curl/build rewrites,
//! every rail deny) are gated on [`mcp_ready`] so the agent is never sent to a
//! dead tool (sub-agent injection fires regardless); stateful shell commands
//! (anything that mutates shell state — `export`, assignments, backticks, a
//! mid-chain `cd`, …) are always passed through untouched, because rewriting
//! them would silently change behavior. A LEADING `cd <dir>` only repositions
//! the shell, so the remainder of the chain is still classified for the deny
//! rails — but never rewritten (a rewrite would drop the cwd persistence the
//! leading `cd` exists for). When RTK owns Bash rewriting ([`RouteCtx::rtk_active`])
//! lens still issues every verdict (deny/passthrough) and defers only the
//! wrap/redirect rewrites to RTK's own hook.

use std::collections::HashSet;
use std::path::Path;

use serde_json::{json, Value};

mod classify;
mod log;
pub(crate) mod reroute;
pub mod throttle;

pub use classify::{grep_scope, is_structurally_bounded, GrepScope};

/// Active routing level, parsed from `LENS_ROUTING`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// True no-op: PreToolUse returns `{}`, SessionStart unchanged.
    Off,
    /// Bootstrap-only back-compat level: injects the SessionStart guide and the
    /// sub-agent prompt block, but never denies, redirects, or rewrites a call.
    /// The per-call nudge arms were retired (deny converts 51-71%, nudge
    /// 0-33%); the variant stays parseable so `LENS_ROUTING=nudge` configs
    /// keep working.
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
    /// The empty string is [`Level::Off`]; any other unrecognized value is
    /// [`Level::Full`] — fail-safe, so a typo in `LENS_ROUTING` can't silently
    /// disable routing (unset already defaults to `full` via [`Level::from_env`]).
    pub fn parse(s: &str) -> Level {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "off" => Level::Off,
            "nudge" => Level::Nudge,
            "steer" => Level::Steer,
            "wrap" => Level::Wrap,
            "full" => Level::Full,
            _ => Level::Full,
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

    /// Whether this level is routing-active for the advisory plane: SessionStart
    /// guide injection, sub-agent prompt injection, and the lookup-counter
    /// bookkeeping the escalation deny reads. `Nudge` does this without denying
    /// or redirecting anything (bootstrap-only back-compat).
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
    /// lens data dir (holds throttle markers + `heartbeats/`).
    pub data_dir: &'a Path,
    /// Current session id (scopes one-shot nudge throttling).
    pub session_id: &'a str,
    /// True when RTK owns Bash (see [`crate::rtk::rtk_active`]); makes [`route`]
    /// pass Bash through so RTK's hook and lens's never double-wrap.
    pub rtk_active: bool,
    /// Code-file Reads this session since the last `lens_overview`
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
/// `remind` pattern). Factual, maps each intent to its lens tool — naming BOTH
/// `lens_skeleton` (structure) and `lens_run` (analysis) for file reads,
/// and `lens_graph` for reachability (directed since the traversal merge) — and
/// states that the counter was reset so the caller isn't walled off if it
/// still needs the plain tool.
pub const READ_DENY_REASON: &str = "Too many consecutive Read/Grep calls on code without any lens tool. Where is X / where does an idea appear: lens_search(queries: [...]) or lens_symbol(name). What calls X, what does X call: lens_graph(node). A file's structure without the bodies: lens_skeleton(path), with include_bodies for the functions you need. Analyzing a file's contents (count, extract, summarize): lens_run(path, language, code) — only what you print returns. Or trace reachability with lens_graph(node, to) — edges are directed, so it answers whether A actually reaches B. The counter was reset — the same call will pass now if you still need it.";

/// One-line mapping injected at UserPromptSubmit when the prompt reads as a
/// find/trace question. First-tool choice is decided by what's in context
/// BEFORE the first call — PreToolUse nudges arrive one call too late and the
/// SessionStart block alone doesn't overcome the Grep prior (measured:
/// find/trace tasks stayed Grep-first with the block in place). This lands at
/// the decision point itself.
pub const PROMPT_INTENT_NUDGE: &str = "<lens_hint>\n  Find/trace question — answer it from the index/graph, not by grepping: lens_search(queries: [\"...\"]) or lens_symbol(name) to locate; lens_graph for callers/callees or how A reaches B; lens_skeleton(path) for one file's shape. Grep's line hits pull a whole-file Read per hit — that chain costs more than one lens call.\n</lens_hint>";

/// Shown when the FIRST Grep after a find/trace-shaped prompt is denied (the
/// `grep-first` marker armed at UserPromptSubmit, consumed here). Measured:
/// the intent nudge alone flips some tasks but most Grep-first ones ignore
/// every prompt-level hint — this is the Serena FORBIDDEN pattern applied at
/// the exact decision point. One-shot: the marker is consumed before the deny
/// returns, so the same Grep passes on retry.
pub const GREP_FIRST_DENY_REASON: &str = "This prompt is a find/trace question — answer it with one lens call instead of a grep chain. Where is X / where does an idea appear: lens_search(queries: [...]) or lens_symbol(name). What calls X, what does X call: lens_graph(node). How does A reach B: lens_graph(node, to). A file's shape: lens_skeleton(path). If the lens tools aren't loaded yet, load them first: ToolSearch(query: \"select:lens_search,lens_symbol,lens_graph,lens_skeleton\"). This fires once per prompt — the same Grep will pass if you re-run it, but the lens call answers in one step.";
/// Shown when a Grep whose `path` spans a directory or the whole repo is denied
/// under the grep-scope gate (`LENS_GREP_SCOPE_DENY`, default ON — see
/// [`grep_scope_deny_enabled`]). Same shape as [`GREP_FIRST_DENY_REASON`] but
/// keyed on the call's scope rather than the prompt's phrasing.
pub const GREP_SCOPE_DENY_REASON: &str = "This grep spans a directory or the whole repo — one lens call answers it without the grep→Read chain. Where is X / where does an idea appear: lens_search(queries: [...]) or lens_symbol(name), which also falls back to a meaning match when nothing matches by name. What calls X, what does X call: lens_graph(node). How does A reach B: lens_graph(node, to). A file's shape: lens_skeleton(path). If the lens tools aren't loaded yet, load them first: ToolSearch(query: \"select:lens_search,lens_symbol,lens_graph,lens_skeleton\"). This fires at most once per prompt — the same Grep will pass if you re-run it.";

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

/// Whether a user prompt reads as an edit-intent request. Used to EXEMPT the
/// pre-first-edit Read from the rskel skeleton-first deny: the harness mandates
/// a Read before an Edit, so denying that Read would block a legitimate write
/// path (measured: 4/16 live rskel denials were followed immediately by an
/// Edit).
///
/// Find/trace WINS: a prompt that also reads as a find/trace question is never
/// treated as edit-intent, because an investigation that ends in an edit still
/// benefits from skeleton-first reading. High-precision and whole-token
/// (word-boundary), so a substring like `additional` never counts as `add`.
pub fn prompt_wants_edit(prompt: &str) -> bool {
    if prompt_wants_find_trace(prompt) {
        return false;
    }
    let p = prompt.to_ascii_lowercase();
    p.split(|c: char| !c.is_ascii_alphanumeric())
        .any(token_is_edit_verb)
}

/// Whole-token test for an edit verb: its base form or a common inflection
/// (`-s`, `-es`, `-ed`, `-d`, `-ing`, including the drop-final-`e` present
/// participle like `removing`/`renaming`). Whole-token, never a substring, so
/// `additional` is not `add` and `fixture` is not `fix`.
fn token_is_edit_verb(tok: &str) -> bool {
    const VERBS: &[&str] = &[
        "fix",
        "change",
        "edit",
        "update",
        "implement",
        "add",
        "remove",
        "rename",
        "refactor",
        "rewrite",
        "delete",
        "patch",
    ];
    VERBS.iter().any(|v| {
        if let Some(rest) = tok.strip_prefix(v) {
            if matches!(rest, "" | "s" | "es" | "ed" | "d" | "ing") {
                return true;
            }
        }
        // drop-final-`e` present participle: remove→removing, rename→renaming.
        v.strip_suffix('e')
            .and_then(|stem| tok.strip_prefix(stem))
            .is_some_and(|rest| rest == "ing")
    })
}

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

    // achain chain-break: any tool that is not an atomic lens exploration call
    // zeroes the consecutive-atomic counter, EXCEPT the ToolSearch schema
    // bootstrap (loading deferred schemas mid-chain is ceremony, not a change
    // of approach). Guarded on `armed` so a session that never chains never
    // appends a reset line per tool call.
    let atomic_lens = tool
        .strip_prefix("mcp__lens__")
        .is_some_and(reroute::atomic_chain::is_atomic);
    if !atomic_lens
        && tool != "ToolSearch"
        && throttle::armed(ctx.data_dir, ctx.session_id, "achain-run")
    {
        throttle::reset(ctx.data_dir, ctx.session_id, "achain-run");
    }

    match tool {
        // WebFetch deny: only when the replacement (lens_run via MCP) is
        // actually reachable, and at most ONCE per session — a subagent whose
        // tool config can't reach lens must not be walled off from the web
        // (observed live: a legitimate doc-fetch blocked 3x). `nudge_once`
        // runs LAST so a not-ready gate never spends the one-shot.
        "WebFetch" => {
            if ctx.level.steers() && ctx.mcp_ready && nudge_once(ctx, "webfetch-deny") {
                Decision::Deny(WEBFETCH_REASON.to_string())
            } else {
                Decision::Passthrough
            }
        }
        // Bash routing always runs — even when RTK owns Bash rewriting, lens
        // still issues the verdict (deny/passthrough); only the Modify stages
        // inside `bash_decision` defer to RTK (see `rewrites_allowed` there).
        "Bash" => bash_decision(tool_input, ctx),
        // Grep counts toward the same consecutive-lookup counter as code Reads:
        // the measured drift signature (Grep → Read → Read) starts here, and a
        // Read-only counter never catches it. Escalation first, then the
        // one-shot intent-mapping tip.
        "Grep" => {
            if !ctx.level.nudges() {
                return Decision::Passthrough;
            }
            // Reroute rail 3 (grevf) DENY: a graph-surfaced file's 2nd+ Grep
            // this session — see `graph_reverify_decision`. Only a Grep whose
            // `path` names that exact file (not a directory it lives under)
            // is in scope; a directory/whole-repo Grep never matches. Runs
            // first so no other Grep rail's one-shot gets spent on a call
            // this one is about to deny instead.
            if let Some(d) = graph_reverify_decision(
                tool_input.get("path").and_then(Value::as_str).unwrap_or(""),
                ctx,
            ) {
                return d;
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
                    // not gate on the prompt markers). Gated so it stays inert
                    // while the gast deny is kill-switched off.
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
            // Reroute rail 1a (gsym) DENY: a Grep whose pattern is itself a
            // symbol lookup — a definition shape (`fn foo`) or a bare identifier
            // — is denied once per session toward lens_symbol.
            // Kill-switched by LENS_GREP_SYMBOL_DENY (default ON) with the scope
            // deny's gates, plus `graph_resolves` so a lookup that would come up
            // empty in the graph never gets denied toward it. The `nudge_once`
            // runs LAST so a blocked gate never spends the one-shot; on a deny
            // the other grep markers are consumed and the lookup counter reset
            // so the verbatim retry always passes.
            let pat = tool_input
                .get("pattern")
                .and_then(Value::as_str)
                .unwrap_or("");
            if grep_symbol_deny_enabled()
                && ctx.level.steers()
                && ctx.mcp_ready
                && reroute::grep_symbol::symbol_grep(pat).is_some()
                && index_present(ctx.data_dir)
                && reroute::grep_symbol::graph_resolves(ctx.data_dir, pat)
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
                return Decision::Deny(reroute::grep_symbol::deny_reason(pat));
            }
            // Reroute rail 2b (gast) DENY: a syntax-shaped pattern (an impl
            // block, an attribute, a method call, …) is denied once per session
            // toward lens_grep_ast's tree-sitter query. Placed AFTER the gsym
            // deny and BEFORE the gast nudge; kill-switched by
            // LENS_GREP_AST_DENY with the scope deny's gates, so the nudge below
            // stays reachable when the deny is switched off. Mirrors the gsym deny:
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
            if let Some(d) = inspect_escalation(ctx) {
                return d;
            }
            Decision::Passthrough
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
                agent_inject(tool_input)
            } else {
                Decision::Passthrough
            }
        }
        // Reroute rail 2a (elink): an Edit that touches a symbol's DECLARATION
        // line in a CODE file, when that symbol has >=K callers in the graph,
        // is routed toward lens_graph so the blast radius is visible before the
        // signature changes. While steering the deny arm (LENS_EDIT_LINKS_DENY)
        // blocks it AT MOST ONCE PER SYMBOL PER SESSION: the `elink:{sym}`
        // marker is set before the deny returns, so the verbatim retry — and
        // every later Edit of that symbol — always passes, and the edit content
        // is never modified. The guarded graph load runs LAST so the
        // switched-off / no-graph default costs nothing.
        "Edit" | "MultiEdit" => {
            let deny_armed = edit_links_deny_enabled() && ctx.level.steers();
            if !(deny_armed && ctx.mcp_ready && index_present(ctx.data_dir)) {
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
            let Some(n) = reroute::edit_callers::caller_count(&graph, &sym).filter(|&n| n >= k)
            else {
                return Decision::Passthrough;
            };
            // Mark BEFORE returning: one blocked Edit per symbol per
            // session, ever — the verbatim retry always passes.
            throttle::mark(ctx.data_dir, ctx.session_id, &key);
            Decision::Deny(reroute::edit_callers::deny_reason(&sym, n))
        }
        // Reroute rail (ovrb) BEFORE achain: an unfocused lens_overview after
        // SessionStart already injected the <repo_map> digest is a re-buy of
        // the same map. Once per session (elink pattern: mark ovrb:done BEFORE
        // Deny so the verbatim retry always passes). Stands down when rovr
        // already pushed toward overview this session, and when the call is
        // focused (`query` set). Kill-switched by LENS_OVERVIEW_REBUY_DENY.
        // Runs before the achain bump so a denied call does not advance the
        // consecutive-atomic counter.
        t if t.starts_with("mcp__lens__") => {
            if t == "mcp__lens__lens_overview"
                && overview_rebuy_deny_enabled()
                && ctx.level.steers()
                && ctx.mcp_ready
                && throttle::armed(ctx.data_dir, ctx.session_id, "ovrb:digest")
                && reroute::overview_rebuy::is_rebuy(tool_input)
                && !throttle::fired(ctx.data_dir, ctx.session_id, "read-overview")
                && !throttle::fired(ctx.data_dir, ctx.session_id, "ovrb:done")
            {
                throttle::mark(ctx.data_dir, ctx.session_id, "ovrb:done");
                return Decision::Deny(reroute::overview_rebuy::deny_reason().to_string());
            }
            // Reroute rail (achain): the 3rd CONSECUTIVE atomic lens exploration
            // call (graph/skeleton/symbol/recall/overview, with nothing but
            // ToolSearch between) is denied toward one composed call — the
            // measured 0.11-dev loop shapes are lens_graph hop-by-hop instead of
            // `transitive: true`, lens_recall body-chasing instead of
            // `include_bodies`, and per-file skeletons instead of one lens_run
            // program. Denies ONCE PER SESSION (`achain:done`): the first deny
            // teaches composition; the 2026-07-21 bad-set transcript audit
            // measured repeat denies burning a full round each (up to 3 per run
            // on 0083) without converting mid-chain. The counter still resets on
            // fire so the verbatim retry passes. Kill-switched by
            // LENS_ATOMIC_CHAIN_DENY (default ON); the bump only runs while the
            // deny can fire, so a kill-switched or non-steering session never
            // writes the counter.
            if atomic_lens && atomic_chain_deny_enabled() && ctx.level.steers() && ctx.mcp_ready {
                let n = throttle::bump(ctx.data_dir, ctx.session_id, "achain-run");
                // try_mark, not fired+mark: two parallel tool calls in one
                // message run as concurrent hook processes, and the stale-cache
                // check-then-act doubled the deny (30 sessions in the v0.10
                // gate log).
                if n >= ATOMIC_CHAIN_THRESHOLD
                    && throttle::try_mark(ctx.data_dir, ctx.session_id, "achain:done")
                {
                    throttle::reset(ctx.data_dir, ctx.session_id, "achain-run");
                    return Decision::Deny(reroute::atomic_chain::deny_reason(
                        t.strip_prefix("mcp__lens__").unwrap_or(t),
                        tool_input,
                    ));
                }
            }
            Decision::Passthrough
        }
        _ => Decision::Passthrough,
    }
}

/// PostToolUse routing entry point (the session hook renders its result via
/// [`to_post_hook_json`]). The one mechanism this carried — the grep-flood
/// nudge toward lens_search — was retired with the other nudge arms (measured
/// conversion 0-33% vs 51-71% for denies). This seam now backs the
/// `graph_reverify` (grevf) rail's other half: a `lens_graph` response's
/// per-node files are marked here (`graphfile:{file}`, via
/// [`reroute::graph_reverify::files_in_graph_response`]) so `route_inner`'s
/// Read/Grep arms can deny that file's 2nd+ visit toward the composed
/// `lens.callers(transitive=True)` program instead of a second whole-file
/// look (see [`graph_reverify_decision`]). PostToolUse can only observe, never
/// deny — [`post_route`] itself always passes through; the deny fires later,
/// at the next PreToolUse Read/Grep.
pub fn post_route(tool: &str, tool_response: &str, ctx: &RouteCtx) -> Decision {
    if tool == "mcp__lens__lens_graph" && graph_reverify_enabled() {
        for file in reroute::graph_reverify::files_in_graph_response(tool_response) {
            throttle::mark(ctx.data_dir, ctx.session_id, &format!("graphfile:{file}"));
        }
    }
    Decision::Passthrough
}

/// The compact sub-agent injection block: the deferred-tool ToolSearch
/// bootstrap (a sub-agent inherits no loaded schemas, so without this the lens
/// tools are unreachable inside it) plus a one-line intent→tool map. The full
/// `session_block` prose stays SessionStart-only — a sub-agent prompt is task
/// text, not a place for a page of guidance.
const AGENT_TOOL_BLOCK: &str = "<lens_tools>\n  Load the lens tools once before first use: ToolSearch(query: \"select:lens_search,lens_symbol,lens_graph,lens_skeleton,lens_overview,lens_recall,lens_run,lens_grep_ast,lens_memory_query,lens_memory_record\")\n  Map: where text/ideas appear — lens_search(queries: [...]); exact symbol or behavior — lens_symbol(name); callers/callees or neighborhood — lens_graph(node); does A reach B — lens_graph(node, to); one file's shape — lens_skeleton(path); repo map — lens_overview; syntax shape — lens_grep_ast; compute over data or a file — lens_run(code); inside scripts, compose with lens.symbol/callers/path/skeleton/grep_ast/search/overview; recover offloaded output — lens_recall.\n</lens_tools>";

/// Inject the compact lens tool block ([`AGENT_TOOL_BLOCK`]) into a sub-agent's
/// prompt. No throttle: each sub-agent is a fresh context that needs its own
/// copy of the ToolSearch bootstrap.
fn agent_inject(tool_input: &Value) -> Decision {
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
    updated[field] = Value::String(format!("{original}\n\n{AGENT_TOOL_BLOCK}"));
    Decision::Modify {
        reason: AGENT_INJECT_REASON.to_string(),
        updated_input: updated,
    }
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
/// Grep-scope deny kill-switch: `LENS_GREP_SCOPE_DENY=0` disables it.
/// On by default — the same polarity as [`grep_first_deny_enabled`].
pub fn grep_scope_deny_enabled() -> bool {
    std::env::var("LENS_GREP_SCOPE_DENY").map_or(true, |v| v.trim() != "0")
}

// ── Reroute-rail kill-switches ──────────────────────────────────────────────
// One flag per rail arm (see `reroute` for the counter-key/env-flag contract),
// all with the `grep_first_deny` kill-switch polarity: on by default, `=0`
// disables, independently reversible.

/// Grep-symbol deny arm (`gsym`): `LENS_GREP_SYMBOL_DENY=0` disables it.
pub fn grep_symbol_deny_enabled() -> bool {
    std::env::var("LENS_GREP_SYMBOL_DENY").map_or(true, |v| v.trim() != "0")
}
/// Read-skeleton deny arm (`rskel`): `LENS_READ_SKELETON_DENY=0` disables it.
pub fn read_skeleton_deny_enabled() -> bool {
    std::env::var("LENS_READ_SKELETON_DENY").map_or(true, |v| v.trim() != "0")
}
/// Bash-aggregate deny arm (`bagg`): `LENS_BASH_AGG_DENY=0` disables it.
pub fn bash_agg_deny_enabled() -> bool {
    std::env::var("LENS_BASH_AGG_DENY").map_or(true, |v| v.trim() != "0")
}
/// Edit-callers deny arm (`elink`): `LENS_EDIT_LINKS_DENY=0` disables it.
pub fn edit_links_deny_enabled() -> bool {
    std::env::var("LENS_EDIT_LINKS_DENY").map_or(true, |v| v.trim() != "0")
}
/// Atomic-chain deny arm (`achain`): `LENS_ATOMIC_CHAIN_DENY=0` disables it.
pub fn atomic_chain_deny_enabled() -> bool {
    std::env::var("LENS_ATOMIC_CHAIN_DENY").map_or(true, |v| v.trim() != "0")
}
/// Overview-rebuy deny arm (`ovrb`): `LENS_OVERVIEW_REBUY_DENY=0` disables it.
pub fn overview_rebuy_deny_enabled() -> bool {
    std::env::var("LENS_OVERVIEW_REBUY_DENY").map_or(true, |v| v.trim() != "0")
}

/// The achain deny threshold: the Nth consecutive atomic lens call is denied.
/// 3, because every measured 0.11-dev chain (graph x8 on 0060, skeleton x6 on
/// 0083, recall x2 after a skeleton on 0068/0070) is already unambiguous at
/// the third hop, and a chain of two is often legitimate disambiguation.
const ATOMIC_CHAIN_THRESHOLD: u64 = 3;
/// Grep-ast deny arm (`gast`): `LENS_GREP_AST_DENY=0` disables it.
pub fn grep_ast_deny_enabled() -> bool {
    std::env::var("LENS_GREP_AST_DENY").map_or(true, |v| v.trim() != "0")
}
/// Read-overview deny arm (`rovr`): `LENS_READ_OVERVIEW_DENY=0` disables it.
pub fn read_overview_deny_enabled() -> bool {
    std::env::var("LENS_READ_OVERVIEW_DENY").map_or(true, |v| v.trim() != "0")
}
/// Bash-grep deny arm (shell `grep`/`rg`/`git grep` → lens search surface):
/// `LENS_BASH_GREP_DENY=0` disables it.
pub fn bash_grep_deny_enabled() -> bool {
    std::env::var("LENS_BASH_GREP_DENY").map_or(true, |v| v.trim() != "0")
}
/// Bounded-Read→lens_run deny arm: `LENS_READ_RUNFILE_DENY=0` disables it.
pub fn read_runfile_deny_enabled() -> bool {
    std::env::var("LENS_READ_RUNFILE_DENY").map_or(true, |v| v.trim() != "0")
}
/// Graph-reverify deny arm (`grevf`): `LENS_GRAPH_REVERIFY=0` disables it.
pub fn graph_reverify_enabled() -> bool {
    std::env::var("LENS_GRAPH_REVERIFY").map_or(true, |v| v.trim() != "0")
}

/// The symbol whose declaration this Edit/MultiEdit touches, or `None`. Edit
/// carries `old_string`/`new_string` at the top level; MultiEdit carries an
/// `edits[]` array — the first decl-touching edit wins. Only CODE files count:
/// an edit to a doc/config file (`.md`, `.txt`, a plan file) can contain text
/// that merely LOOKS like a declaration (`name` in a plan table matched as a
/// decl symbol, observed live 2026-07-19), so a non-code `file_path` is never
/// a decl edit. Shared by the elink arm in [`route_inner`] and the hook's
/// shadow-counter plane so the live and would-fire derivations can never
/// drift apart.
pub(crate) fn edited_decl_symbol(tool: &str, tool_input: &Value) -> Option<String> {
    let is_code = tool_input
        .get("file_path")
        .and_then(Value::as_str)
        .and_then(file_extension)
        .and_then(|ext| crate::discovery::extract::spec_for_extension(&ext))
        .is_some_and(|s| s.name != "markdown");
    if !is_code {
        return None;
    }
    let symbol_of = |edit: &Value| {
        let old = edit.get("old_string")?.as_str()?;
        let new = edit.get("new_string").and_then(Value::as_str).unwrap_or("");
        reroute::edit_callers::edited_symbol(old, new)
    };
    match tool {
        "Edit" => symbol_of(tool_input),
        "MultiEdit" => tool_input
            .get("edits")?
            .as_array()?
            .iter()
            .find_map(symbol_of),
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

/// The rskel deny's edit-intent exemption: true when the current prompt read as
/// edit-intent (the `edit-intent` marker armed at UserPromptSubmit — see
/// [`prompt_wants_edit`]). A pure, non-consuming read of the throttle marker.
/// Shared by [`read_decision`] and the hook's shadow-counter plane so the live
/// deny and the `rskel_would_fire` counter gate on the IDENTICAL condition and
/// can never drift on the marker key.
pub(crate) fn rskel_edit_exempt(data_dir: &Path, session_id: &str) -> bool {
    throttle::armed(data_dir, session_id, "edit-intent")
}

/// Deny reason for the bounded-Read→`lens_run` arm: names the exact call
/// with a ready-to-adapt analysis sketch, and states the per-file one-shot so
/// the model knows the verbatim retry passes.
fn read_runfile_reason(path: &str) -> String {
    format!(
        "This bounded Read pulls a slice of {path} into context to analyze by eye — derive the answer in the darkroom instead: lens_run(path: \"{path}\", language: \"python\", code: \"import sys; text = open(sys.argv[1]).read(); print(...)\") — your code gets the file path as argv[1] and only what you print returns. Just need the file's structure? lens_skeleton(path=\"{path}\"). If the lens tools aren't loaded yet, load them first: ToolSearch(query: \"select:lens_run,lens_skeleton,lens_recall\"). This fires once per file — the same Read will pass if you re-run it."
    )
}

/// Read routing: deny-grade rails only. Order: rskel (whole-file → skeleton),
/// rovr (Nth mapless read → overview), runfile (bounded read → darkroom), then
/// the consecutive-lookup escalation deny via [`inspect_escalation`]. Only CODE
/// files the graph indexes ([`crate::discovery::extract::spec_for_extension`])
/// count toward escalation — reading a doc/config/data file shouldn't push the
/// agent at the graph. Markdown is graph-indexed too (headings/links), but it
/// is prose read linearly, not code navigation, so it is excluded here.
fn read_decision(tool_input: &Value, ctx: &RouteCtx) -> Decision {
    if !ctx.level.nudges() {
        return Decision::Passthrough;
    }
    let path = tool_input["file_path"].as_str().unwrap_or("");
    // Reroute rail 3 (grevf) DENY: a graph-surfaced file's 2nd+ Read this
    // session — see `graph_reverify_decision`. Runs first so no other rail's
    // one-shot gets spent on a call this one is about to deny instead.
    if let Some(d) = graph_reverify_decision(path, ctx) {
        return d;
    }
    // Reroute rail 1b (rskel) DENY: a whole, unedited code-file Read is denied
    // toward lens_skeleton, once per FILE per session (`read-skeleton:{path}`,
    // the elink per-key pattern) — kill-switch LENS_READ_SKELETON_DENY, the
    // scope deny's gates. Runs FIRST in this arm so no two denies can stack;
    // `nudge_once` runs last so a blocked gate never spends the one-shot, and
    // the deny resets the lookup counter so the verbatim retry always passes.
    // Exempt when the prompt read as edit-intent: the harness requires a Read
    // before the first Edit, so skeleton-denying it would block the write
    // path. Safe at any file size — lens_skeleton budgets its output and
    // hands back a skeleton_ref for the remainder.
    if read_skeleton_deny_enabled()
        && ctx.level.steers()
        && ctx.mcp_ready
        && index_present(ctx.data_dir)
        && !rskel_edit_exempt(ctx.data_dir, ctx.session_id)
    {
        let has_offset_or_limit =
            tool_input.get("offset").is_some() || tool_input.get("limit").is_some();
        let edited = edited_paths_for(ctx.data_dir, ctx.session_id, path);
        if reroute::read_skeleton::read_is_skeletonizable(path, has_offset_or_limit, &edited)
            && nudge_once(ctx, &reroute::read_skeleton::rskel_key(path))
        {
            throttle::reset(ctx.data_dir, ctx.session_id, "read-code");
            return Decision::Deny(reroute::read_skeleton::deny_reason(path));
        }
    }
    // Reroute rail 2c (rovr) DENY: while steering, the Nth code Read with no
    // repo map yet is denied once per session toward lens_overview
    // (kill-switch LENS_READ_OVERVIEW_DENY). The count is fed by the hook via
    // `ctx.reads_since_map`, zeroed by any lens_overview call.
    // `nudge_once` runs LAST so a blocked gate never spends the one-shot; on a
    // deny the lookup counter and the reads-since-map counter are reset, and —
    // when this call is also runfile-shaped — the runfile arm's per-file
    // one-shot is spent too, so the verbatim retry always passes (no arm below
    // can catch it).
    if ctx.level.steers()
        && read_overview_deny_enabled()
        && ctx.mcp_ready
        && reroute::read_overview::overview_due(
            ctx.reads_since_map,
            reroute::read_overview::threshold(),
        )
        && index_present(ctx.data_dir)
        && nudge_once(ctx, "read-overview")
    {
        throttle::reset(ctx.data_dir, ctx.session_id, "read-code");
        throttle::reset(ctx.data_dir, ctx.session_id, "reads-since-map");
        if reroute::read_skeleton::read_is_analysis_shaped(tool_input) {
            throttle::mark(ctx.data_dir, ctx.session_id, &format!("read-runfile:{path}"));
        }
        return Decision::Deny(reroute::read_overview::deny_reason(ctx.reads_since_map));
    }
    // Bounded-Read→lens_run DENY: an offset/limit Read of a code file is
    // analysis work — its correct target is the darkroom, not a slice-by-eye.
    // Kill-switch LENS_READ_RUNFILE_DENY; per-FILE one-shot
    // (`read-runfile:{path}`), so a second bounded Read of the same file always
    // passes — never hard-wall a file the model insists on reading. The
    // edit-intent exemption applies (a bounded pre-edit Read is legitimate),
    // and the deny resets the lookup counter so the verbatim retry can't be
    // caught by the escalation deny either.
    if read_runfile_deny_enabled()
        && ctx.level.steers()
        && ctx.mcp_ready
        && index_present(ctx.data_dir)
        && !rskel_edit_exempt(ctx.data_dir, ctx.session_id)
        && reroute::read_skeleton::read_is_analysis_shaped(tool_input)
        && nudge_once(ctx, &format!("read-runfile:{path}"))
    {
        throttle::reset(ctx.data_dir, ctx.session_id, "read-code");
        return Decision::Deny(read_runfile_reason(path));
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
    Decision::Passthrough
}

/// Reroute rail 3 (grevf) DENY: after a session's `lens_graph` call (marked by
/// `post_route`'s file-marking side effect — see below), the graph answer
/// already carries per-node witnesses proving every edge and, for a
/// transitive closure, a `complete: true` claim — so re-reading one of its
/// files a SECOND time is a re-verification, not new evidence. The FIRST
/// Read/Grep of a graph-surfaced file always passes (the agent still needs to
/// look at it once): a per-file `grevf-seen:{path}` throttle key remembers
/// that first visit, silently, without denying. Only that SAME file's 2nd+
/// visit is eligible to deny, and — the `edit_callers.rs` one-shot-per-symbol
/// precedent applied per-file here — at most ONCE per file per session
/// (`grevf:{path}` marks the deny fired), so the verbatim retry, and every
/// later visit to that file, passes. `None` → the caller falls through to its
/// other checks unchanged. Kill-switch `LENS_GRAPH_REVERIFY` (default ON, see
/// [`graph_reverify_enabled`]); gated on `mcp_ready` like the other
/// MCP-redirect denies, since the deny points at `lens_run`.
fn graph_reverify_decision(path: &str, ctx: &RouteCtx) -> Option<Decision> {
    if path.is_empty() || !graph_reverify_enabled() || !ctx.level.steers() || !ctx.mcp_ready {
        return None;
    }
    if !throttle::fired(ctx.data_dir, ctx.session_id, &format!("graphfile:{path}")) {
        return None; // this file never appeared in a lens_graph answer
    }
    let seen_key = format!("grevf-seen:{path}");
    if !throttle::fired(ctx.data_dir, ctx.session_id, &seen_key) {
        // First look at a graph-surfaced file always passes — just remember it.
        throttle::mark(ctx.data_dir, ctx.session_id, &seen_key);
        return None;
    }
    if !nudge_once(ctx, &format!("grevf:{path}")) {
        return None; // already denied this file once — every later visit passes
    }
    throttle::reset(ctx.data_dir, ctx.session_id, "read-code");
    Some(Decision::Deny(reroute::graph_reverify::deny_reason(path)))
}

/// Shared consecutive-lookup escalation for code Reads and Greps (the
/// `read-code` counter, reset by any lens tool call or file edit in
/// PostToolUse — see `session::hook`). Past [`read_deny_threshold`]
/// consecutive lookups (while steering), the call is denied once — see
/// [`READ_DENY_REASON`]. No priming step before the deny: denies convert
/// (51-71% measured) without a Context warm-up, so the counter stays silent
/// until the threshold. `None` → the caller passes the call through.
fn inspect_escalation(ctx: &RouteCtx) -> Option<Decision> {
    let n = throttle::bump(ctx.data_dir, ctx.session_id, "read-code");
    let deny_threshold = read_deny_threshold();
    if deny_threshold > 0 && n >= deny_threshold && ctx.level.steers() {
        // Deny once per drift episode, never a hard wall: reset the
        // counter first so the immediate retry passes if still needed.
        throttle::reset(ctx.data_dir, ctx.session_id, "read-code");
        return Some(mcp_redirect(
            ctx,
            Decision::Deny(READ_DENY_REASON.to_string()),
        ));
    }
    None
}

/// Lowercased file extension of a path, if any (`src/Foo.RS` → `rs`).
fn file_extension(path: &str) -> Option<String> {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// Bash-specific routing: deny-classify grep-family and aggregate commands,
/// redirect network/build fetches, wrap read-only high-output commands — but
/// never touch stateful ones. Verdict stages (deny/passthrough) always run;
/// the Modify stages (redirect, wrap) are skipped when RTK owns Bash
/// rewriting ([`RouteCtx::rtk_active`]) or when a leading `cd` was stripped
/// (rewriting a cd-chain would silently drop its cwd persistence).
fn bash_decision(tool_input: &Value, ctx: &RouteCtx) -> Decision {
    let cmd = tool_input["command"].as_str().unwrap_or("");
    if cmd.is_empty() {
        return Decision::Passthrough;
    }
    // Compute once; thread through to avoid recomputing in is_stateful /
    // bash_redirect / is_wrappable.
    let all_segs = segments(cmd);
    // A LEADING `cd <dir>` only repositions the shell before the real command:
    // classify what runs after it instead of blanket-passing the whole line. A
    // `cd` (or any stateful segment) later in the chain still passes through
    // via `is_stateful_segs` below.
    let segs = strip_leading_cd(&all_segs);
    let had_leading_cd = segs.len() != all_segs.len();
    // Stateful commands mutate shell state; rewriting them would change behavior.
    if is_stateful_segs(cmd, segs) {
        return Decision::Passthrough;
    }
    // Bash-grep DENY: a grep-family lead segment (`grep`/`rg`/`egrep`/`fgrep`/
    // `git grep`) is classified toward the lens search surface, sharing the
    // Grep arm's one-deny-per-prompt budget. Single-file greps (NarrowExact)
    // and non-grep commands are never denied by this arm — the deliberate
    // scoped escape (grep genuinely wins there).
    if bash_grep_deny_enabled()
        && ctx.level.steers()
        && ctx.mcp_ready
        && index_present(ctx.data_dir)
    {
        if let Some(d) = bash_grep_deny(segs, ctx) {
            return d;
        }
    }
    // The Modify stages below rewrite the command. Deferred to RTK when its
    // hook owns Bash rewriting (lens keeps the verdicts above either way), and
    // suppressed for cd-led chains (see the function doc).
    let rewrites_allowed = !ctx.rtk_active && !had_leading_cd;
    // Network/build/inline-HTTP → hard redirect into lens_run. Steering only;
    // under wrap-only these fall through to the generic output-wrap below.
    // Gated on `mcp_ready` via `mcp_redirect` (these point at lens_run); when
    // the server is down the command passes through untouched rather than
    // redirecting into a dead tool.
    if rewrites_allowed && ctx.level.steers() {
        if let Some(d) = bash_redirect_segs(cmd, segs) {
            return mcp_redirect(ctx, d);
        }
    }
    // Structurally-bounded commands (git status, ls, --version probes, …) produce
    // little output — wrapping them is noise that trains the agent to
    // ignore the advisory. Skip.
    if classify::classify(cmd) == classify::Risk::Safe {
        return Decision::Passthrough;
    }
    // Reroute rail 1c (bagg) DENY: a data-aggregate pipeline (`wc -l`,
    // `sort | uniq`, …) is denied once per session toward `lens_run`'s darkroom.
    // Placed BEFORE the wrap rewrite below so at `full` (where `wraps()` would
    // otherwise rewrite it first) the deny still reaches. Kill-switched by
    // LENS_BASH_AGG_DENY, gated on `steers()` like the grep deny rails. Kept
    // CONSERVATIVE per the classifier's precision note (it is lexical: shell
    // session-state coupling and substring FPs — see
    // `reroute::bash_aggregate::deny_reason`), and the stateful check above
    // already guarantees a state-changing command never reaches here.
    // `nudge_once` runs LAST so a blocked gate never spends the one-shot;
    // consuming it is what lets the verbatim retry fall through and pass (Bash
    // has no read-code-style counter to reset, unlike the grep deny rails).
    if bash_agg_deny_enabled()
        && ctx.level.steers()
        && ctx.mcp_ready
        && reroute::bash_aggregate::is_data_aggregate(cmd)
        && index_present(ctx.data_dir)
        && nudge_once(ctx, "bash-agg")
    {
        return Decision::Deny(reroute::bash_aggregate::deny_reason(cmd));
    }
    if rewrites_allowed && is_wrappable_segs(segs) && ctx.level.wraps() {
        let mut updated = tool_input.clone();
        let rewritten = format!("{} wrap -- {}", q(ctx.bin), q(cmd));
        updated["command"] = Value::String(rewritten);
        Decision::Modify {
            reason: WRAP_REASON.to_string(),
            updated_input: updated,
        }
    } else {
        Decision::Passthrough
    }
}

/// Classify the lead segment of a (cd-stripped) Bash command as a grep-family
/// call and deny it toward the matching lens tool. Only the FIRST non-empty
/// segment is considered: a grep later in a pipeline (`git log | grep fix`)
/// filters stdin, not the filesystem, and must never be denied. Shares the
/// Grep arm's one-deny-per-prompt budget (the `grep-first`/`grep-scope`
/// markers, armed at UserPromptSubmit): the deny consumes BOTH markers, spends
/// the gsym/gast one-shots, and resets the lookup counter, so no prompt is
/// ever denied twice and the verbatim retry always passes. `None` → not a
/// grep, a single-file grep (the scoped escape), or no budget armed.
fn bash_grep_deny(segs: &[String], ctx: &RouteCtx) -> Option<Decision> {
    let lead = segs.iter().find(|s| !s.trim().is_empty())?;
    let seg = reroute::bash_grep::parse_grep_seg(lead)?;
    let reason = match reroute::bash_grep::classify(&seg) {
        reroute::bash_grep::BashGrepClass::NarrowExact => return None,
        reroute::bash_grep::BashGrepClass::Broad => bash_grep_broad_reason(&seg.pattern),
        reroute::bash_grep::BashGrepClass::Symbol { ident } => {
            reroute::grep_symbol::deny_reason(&ident)
        }
        reroute::bash_grep::BashGrepClass::Ast => {
            match reroute::grep_ast::syntax_shape(&seg.pattern) {
                Some(hint) => reroute::grep_ast::deny_reason(&hint),
                None => bash_grep_broad_reason(&seg.pattern),
            }
        }
    };
    // Budget check LAST so a non-grep shape above never consumes a marker.
    let first = throttle::take(ctx.data_dir, ctx.session_id, "grep-first");
    let scoped = !first && throttle::take(ctx.data_dir, ctx.session_id, "grep-scope");
    if !(first || scoped) {
        return None;
    }
    if first {
        throttle::take(ctx.data_dir, ctx.session_id, "grep-scope");
    }
    throttle::mark(ctx.data_dir, ctx.session_id, "grep-symbol");
    if grep_ast_deny_enabled() {
        throttle::mark(ctx.data_dir, ctx.session_id, "grep-ast");
    }
    throttle::reset(ctx.data_dir, ctx.session_id, "read-code");
    Some(Decision::Deny(reason))
}

/// Deny reason for a broad shell grep: the shape of [`GREP_SCOPE_DENY_REASON`]
/// with the actual pattern substituted into a ready-to-paste `lens_search`
/// call, plus `lens_symbol`'s meaning-match fallback for when the term itself
/// is the unknown.
fn bash_grep_broad_reason(pattern: &str) -> String {
    format!(
        "This shell grep spans a directory or the whole repo — one lens call answers it without the grep→Read chain: lens_search(queries: [\"{pattern}\"]) returns ranked snippets (batch several questions into the array). Know the exact symbol name? lens_symbol(name=\"{pattern}\"), which also falls back to a meaning match when nothing matches by name. If the lens tools aren't loaded yet, load them first: ToolSearch(query: \"select:lens_search,lens_symbol\"). This fires at most once per prompt — the same command will pass if you re-run it."
    )
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

/// Drop LEADING `cd <dir>` segments (`cd sub && grep …` → `grep …`): they only
/// reposition the shell before the real command, so the remainder is what
/// deserves classification. Only the leading run is stripped — a `cd` later in
/// the chain (`find / ; cd /tmp`) still marks the line stateful via
/// [`is_stateful_segs`].
fn strip_leading_cd(segs: &[String]) -> &[String] {
    let mut i = 0;
    while i < segs.len() && first_token(&segs[i]) == "cd" {
        i += 1;
    }
    &segs[i..]
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

/// Fire a decision at most once per (session, key): true only on the first
/// call (the in-memory successor to the old `guidance_once` marker file).
/// Despite the historical name this is the one-shot gate for the DENY arms —
/// each rail's throttle key runs through it.
fn nudge_once(ctx: &RouteCtx, key: &str) -> bool {
    if throttle::fired(ctx.data_dir, ctx.session_id, key) {
        false
    } else {
        throttle::mark(ctx.data_dir, ctx.session_id, key);
        true
    }
}

/// Serializes every test in the crate (in `mod.rs` and `session/hook.rs` alike)
/// that reads or sets the process-global `LENS_ROUTING_MCP` var. `cargo test --lib`
/// runs both files' `#[cfg(test)]` modules in one binary, so a hook.rs test setting
/// this var races mod.rs's `mcp_ready` tests unless both sides share one lock.
#[cfg(test)]
pub(crate) static MCP_ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Is the MCP server reachable right now?
///
/// `LENS_ROUTING_MCP` forces the answer when set (`up`/`1`/`on`/`true` =>
/// reachable; `down`/`0`/`off`/`false` => not). Otherwise `<data_dir>/heartbeats/`
/// is consulted: reachable iff at least one file in it has an mtime within the
/// TTL (`LENS_MCP_TTL` seconds, default 90 — three heartbeat intervals). Each
/// server process owns one file named after its own pid, so this reflects
/// whether ANY lens server sharing this data dir is alive — a second session's
/// server exiting cleanly never zeroes out a fresh sibling's heartbeat, unlike
/// the old single-`server.pid` scheme. Missing dir, empty dir, or all-stale =>
/// not ready.
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
    let Ok(entries) = std::fs::read_dir(data_dir.join("heartbeats")) else {
        return false;
    };
    entries.flatten().any(
        |entry| match std::fs::metadata(entry.path()).and_then(|m| m.modified()) {
            Ok(mtime) => match mtime.elapsed() {
                Ok(age) => age.as_secs() <= ttl,
                Err(_) => false, // mtime in the future (clock skew) — treat as stale
            },
            Err(_) => false,
        },
    )
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
    Raw tool results sit in the transcript and get re-read on every later turn. lens avoids that: it runs work in a subprocess (the "darkroom") and hands back only the finished answer. The habit to build: compute over data in code, not by pulling it into the conversation to read.
  </why>
  <loading_lens_tools>
    lens's tools are normally registered already — call them directly, don't spend a round on ToolSearch first. If a lens_* call errors not-found, register the schemas once and retry:
    ToolSearch(query: "select:lens_search,lens_symbol,lens_graph,lens_skeleton,lens_overview,lens_recall,lens_run,lens_grep_ast,lens_memory_query,lens_memory_record")
  </loading_lens_tools>
  <which_tool>
    - Code structure (callers, callees, definitions, imports, reachability): lens_graph(node) for neighborhood, lens_graph(node, to) for shortest path; lens_symbol for details. lens_recall expands compacted results.
    - Where a string or idea appears: lens_search(queries: [...]) — batch several for ranked snippets, not whole files.
    - Known symbol name: lens_symbol(name). Only know what it does: lens_symbol(query="...") for meaning-based fallback.
    - Turning data into an answer: lens_run(language, code) — only what you print returns; compose in-script via lens.search/symbol/callers/path/skeleton/grep_ast/overview/recall.
    - Recovering something offloaded or truncated: lens_recall(ref).
    - Whole-repo orientation: lens_overview (a digest is already pushed into context at session start — expand from it rather than re-running it).
    - One file's shape: lens_skeleton(path); include_bodies: ["the_fn"] for just the functions you need.
    - Syntax-shape patterns: lens_grep_ast(language, query|pattern) matches AST shape, not text. Counting "excluding tests"? prod_only=True, read the response's count.
  </which_tool>
  <when_plain_tools_win>"##;

const BULLET_BASH: &str = "\n    - Bash: for commands that change something or whose output is short. Piping output onward to count/grep/reshape it? Give it to lens_run instead.";

const BULLET_READ: &str = "\n    - Need to understand a file? lens_skeleton(path) first; include_bodies: [\"the_fn\"] for one body, not a second Read. Read is for when about to Edit. Already Read it this session? Use what you have.";

const BULLET_SEARCH: &str = "\n    - Finding or tracing something? Don't grep: where X appears — lens_search(queries: [...]) or lens_symbol(name); what calls X / what X calls — lens_graph(node, direction=\"callers\"/\"callees\"); how A reaches B — lens_graph(from, to). A multi-step structural question (\"every prod caller of X, three hops out, with call sites\") is still ONE call: compose it in the darkroom instead of firing lens_graph repeatedly — lens_run(language: \"python\", code: \"import lens; r = lens.callers('X', transitive=True, depth=3, prod_only=True); print(len(r['nodes'])); print(r['nodes'])\") prints the count and the full witnessed list in one round trip.";

const BULLET_WEBFETCH: &str = "\n    - WebFetch is off here: pull a URL with lens_run (python), keep only the part you need, print that. Retrievable via lens_recall.";

/// Close `</when_plain_tools_win>` through `</context_window_protection>`.
const BLOCK_TAIL: &str = r##"
  </when_plain_tools_win>
  <session_continuity>
    These directives stay active all session; don't drop them as context grows.
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
    fn parse_empty_is_off_unknown_is_full_fail_safe() {
        assert_eq!(Level::parse(""), Level::Off);
        assert_eq!(Level::parse("   "), Level::Off);
        assert_eq!(Level::parse("off"), Level::Off);
        assert_eq!(Level::parse("nudge"), Level::Nudge);
        // A typo must not silently disable routing: any non-empty
        // unrecognized value reads as Full.
        assert_eq!(Level::parse("nonsense"), Level::Full);
        assert_eq!(Level::parse("ful"), Level::Full);
        assert_eq!(Level::parse("Steer "), Level::Steer);
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
    fn mcp_not_ready_gates_redirects_and_denies_not_wrap() {
        // When the server is unreachable, every MCP-pointing decision
        // passthroughs (so the agent isn't sent to a dead tool). The wrap
        // rewrite shells the lens CLI, not the MCP server, so it still fires.
        let d = tempdir().unwrap();
        let ctx = rc(Level::Full, false, d.path());
        assert_eq!(
            route("WebFetch", &json!({"url": "http://x"}), &ctx),
            Decision::Passthrough,
            "WebFetch deny points at lens_run — suppressed when server down"
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
        assert!(
            matches!(
                route("Bash", &json!({"command": "find ."}), &ctx),
                Decision::Modify { .. }
            ),
            "wrap rewrite uses the lens CLI, not the MCP — fires regardless"
        );
        assert_eq!(
            route("Grep", &json!({"pattern": "x"}), &ctx),
            Decision::Passthrough,
            "no Grep decision fires when the server is down"
        );
        assert_eq!(
            route("Read", &json!({"file_path": "x"}), &ctx),
            Decision::Passthrough,
            "no Read decision fires when the server is down"
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

    #[test]
    fn webfetch_denies_once_per_session_and_not_when_mcp_down() {
        let d = tempdir().unwrap();
        let url = json!({"url": "http://x"});
        // MCP down: passthrough, and the one-shot is NOT spent (nudge_once
        // runs last), so the same session still denies once the server is up.
        let mut ctx = rc(Level::Full, false, d.path());
        assert_eq!(route("WebFetch", &url, &ctx), Decision::Passthrough);
        ctx.mcp_ready = true;
        assert_eq!(
            route("WebFetch", &url, &ctx),
            Decision::Deny(WEBFETCH_REASON.to_string()),
            "the gate-blocked one-shot survives to fire once the server is up"
        );
        // One-shot per session: the second WebFetch passes through.
        assert_eq!(
            route("WebFetch", &url, &ctx),
            Decision::Passthrough,
            "the WebFetch deny is once per session"
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
        // Genuinely stateful (export): blanket passthrough.
        assert_eq!(
            route(
                "Bash",
                &json!({"command": "export FOO=1 && find /"}),
                &rc(Level::Full, true, d.path())
            ),
            Decision::Passthrough
        );
        // A cd-led chain is classified (deny rails see `find /`) but never
        // REWRITTEN — wrapping it would drop the cwd persistence — so with no
        // deny applicable it passes through instead of wrapping.
        assert_eq!(
            route(
                "Bash",
                &json!({"command": "cd x && find /"}),
                &rc(Level::Full, true, d.path())
            ),
            Decision::Passthrough
        );
    }

    #[test]
    fn leading_cd_is_stripped_only_at_the_front() {
        // `cd x && find /` classifies `find /` (leading cd stripped, remainder
        // not stateful); `find / ; cd /tmp` keeps blanket passthrough (the cd
        // is mid-chain, so the line stays stateful).
        let segs = segments("cd x && find /");
        let stripped = strip_leading_cd(&segs);
        assert_eq!(stripped.len(), 1);
        assert!(!is_stateful_segs("cd x && find /", stripped));
        let segs2 = segments("find / ; cd /tmp");
        let stripped2 = strip_leading_cd(&segs2);
        assert_eq!(stripped2.len(), segs2.len(), "mid-chain cd is not stripped");
        assert!(is_stateful_segs("find / ; cd /tmp", stripped2));
        // A lone `cd` strips to nothing and routes to passthrough.
        let segs3 = segments("cd /tmp");
        assert!(strip_leading_cd(&segs3).is_empty());
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
    fn rtk_active_defers_rewrites_but_keeps_verdicts() {
        // H0: with RTK owning Bash rewriting, lens still issues every VERDICT
        // (the bash-grep deny fires) but never the wrap/redirect REWRITES.
        let _guard = READ_DENY_ENV_LOCK.lock().unwrap();
        let d = tempdir().unwrap();
        seed_index(d.path());
        let active = RouteCtx {
            level: Level::Full,
            mcp_ready: true,
            bin: "/path with space/lens",
            data_dir: d.path(),
            session_id: "sess-rtk-1",
            rtk_active: true,
            reads_since_map: 0,
        };
        // Wrappable command: NOT wrapped while RTK is active.
        assert_eq!(
            route("Bash", &json!({"command": "find . -type f"}), &active),
            Decision::Passthrough,
            "the wrap rewrite defers to RTK"
        );
        // curl: NOT redirected while RTK is active.
        assert_eq!(
            route(
                "Bash",
                &json!({"command": "curl https://api.example.com/data"}),
                &active
            ),
            Decision::Passthrough,
            "the net redirect defers to RTK"
        );
        // Broad shell grep with an armed prompt budget: the DENY still fires.
        throttle::bump(active.data_dir, active.session_id, "grep-scope");
        match route("Bash", &json!({"command": "grep -rn foo src/"}), &active) {
            Decision::Deny(reason) => assert!(
                reason.contains("lens_search"),
                "rtk_active must not silence the bash-grep deny: {reason}"
            ),
            other => panic!("expected the bash-grep deny under rtk, got {other:?}"),
        }
        // WebFetch is unaffected by rtk (still denied under steer/full).
        assert_eq!(
            route("WebFetch", &json!({"url": "http://x"}), &active),
            Decision::Deny(WEBFETCH_REASON.to_string())
        );
        // Same ctx but RTK inactive: the wrap behavior is unchanged.
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

    // ── route(): bash-grep deny (H1/H2) ────────────────────────────────────

    #[test]
    fn bash_grep_broad_denies_toward_lens_search_once_per_prompt() {
        // READ_DENY_ENV_LOCK serializes every test that reads or flips
        // LENS_BASH_GREP_DENY (see `rail_flags_default_on_kill_switch_polarity`).
        let _guard = READ_DENY_ENV_LOCK.lock().unwrap();
        let d = tempdir().unwrap();
        seed_index(d.path());
        let ctx = rc(Level::Full, true, d.path());
        let ti = json!({"command": "grep -rn foo src/"});
        // No armed prompt budget yet: passthrough-biased (never denied), and
        // at full the grep is wrapped instead.
        assert!(!matches!(route("Bash", &ti, &ctx), Decision::Deny(_)));
        // Armed budget: the deny fires with the pattern pre-filled.
        throttle::bump(ctx.data_dir, ctx.session_id, "grep-scope");
        match route("Bash", &ti, &ctx) {
            Decision::Deny(reason) => assert!(
                reason.contains("lens_search(queries: [\"foo\"])"),
                "broad bash grep denies toward a ready-to-paste lens_search: {reason}"
            ),
            other => panic!("expected the bash-grep deny, got {other:?}"),
        }
        // Budget consumed: the verbatim retry passes (one deny per prompt).
        assert!(
            !matches!(route("Bash", &ti, &ctx), Decision::Deny(_)),
            "the bash-grep deny is one-shot per prompt"
        );
        // ...and a broad Grep-tool call in the same prompt is NOT denied either:
        // the two arms share one budget.
        assert!(
            !matches!(
                route("Grep", &json!({"pattern": "foo"}), &ctx),
                Decision::Deny(_)
            ),
            "Bash-grep and Grep denies share the per-prompt budget"
        );
    }

    #[test]
    fn bash_grep_cd_chain_and_git_grep_deny() {
        let _guard = READ_DENY_ENV_LOCK.lock().unwrap();
        let d = tempdir().unwrap();
        seed_index(d.path());
        // `cd sub && grep -rn foo .`: the leading cd is stripped, the grep
        // classifies broad, the deny fires.
        let ctx = rc(Level::Full, true, d.path());
        throttle::bump(ctx.data_dir, ctx.session_id, "grep-scope");
        match route("Bash", &json!({"command": "cd sub && grep -rn foo ."}), &ctx) {
            Decision::Deny(reason) => assert!(reason.contains("lens_search"), "{reason}"),
            other => panic!("cd-led broad grep must deny, got {other:?}"),
        }
        // `git grep foo` denies too (fresh session for a fresh budget).
        let ctx2 = rc(Level::Full, true, d.path());
        throttle::bump(ctx2.data_dir, ctx2.session_id, "grep-first");
        match route("Bash", &json!({"command": "git grep foo"}), &ctx2) {
            Decision::Deny(reason) => assert!(reason.contains("lens_search"), "{reason}"),
            other => panic!("git grep must deny, got {other:?}"),
        }
    }

    #[test]
    fn bash_grep_single_file_escape_never_denies() {
        let _guard = READ_DENY_ENV_LOCK.lock().unwrap();
        let d = tempdir().unwrap();
        seed_index(d.path());
        let ctx = rc(Level::Full, true, d.path());
        throttle::bump(ctx.data_dir, ctx.session_id, "grep-scope");
        // Single concrete file: NarrowExact — the deliberate escape. Never
        // denied, and the armed budget is NOT consumed.
        assert!(
            !matches!(
                route("Bash", &json!({"command": "grep foo src/main.rs"}), &ctx),
                Decision::Deny(_)
            ),
            "a single-file grep is the scoped escape"
        );
        // The budget survived: a broad grep afterwards still denies.
        assert!(
            matches!(
                route("Bash", &json!({"command": "grep -rn foo src/"}), &ctx),
                Decision::Deny(_)
            ),
            "the narrow escape must not spend the prompt budget"
        );
        // A grep-as-filter mid-pipeline is never this arm's business.
        let ctx2 = rc(Level::Full, true, d.path());
        throttle::bump(ctx2.data_dir, ctx2.session_id, "grep-scope");
        assert!(
            !matches!(
                route("Bash", &json!({"command": "git log | grep fix"}), &ctx2),
                Decision::Deny(_)
            ),
            "a grep filtering stdin must never be denied"
        );
        // Kill-switch: LENS_BASH_GREP_DENY=0 disables the arm.
        std::env::set_var("LENS_BASH_GREP_DENY", "0");
        assert!(
            !matches!(
                route("Bash", &json!({"command": "grep -rn foo src/"}), &ctx2),
                Decision::Deny(_)
            ),
            "kill-switched bash-grep must never deny"
        );
        std::env::remove_var("LENS_BASH_GREP_DENY");
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
            "where is lens_symbol's handler? use lens_graph after", // names a lens tool: user is steering
        ] {
            assert!(!prompt_wants_find_trace(p), "should not match: {p}");
        }
    }

    #[test]
    fn greps_count_toward_the_same_deny_counter_as_code_reads() {
        // The measured drift signature is Grep → Read → Read → Read: mixed
        // lookups must share one counter, denying the 4th call. No index is
        // seeded, so every index-gated rail (scope/gsym/gast/rskel/runfile)
        // stays out of the way and the escalation counter is the only actor.
        let _guard = READ_DENY_ENV_LOCK.lock().unwrap();
        let d = tempdir().unwrap();
        let ctx = rc(Level::Steer, true, d.path());
        let grep = json!({"pattern": "include_bodies"});
        let code = json!({"file_path": "src/server.rs"});
        // Lookups 1-3 (Grep, Read, Read): silent — no priming Context step.
        assert_eq!(route("Grep", &grep, &ctx), Decision::Passthrough);
        assert_eq!(route("Read", &code, &ctx), Decision::Passthrough);
        assert_eq!(route("Read", &code, &ctx), Decision::Passthrough);
        // 4th (Read): deny; the reason maps every intent to its lens tool.
        match route("Read", &code, &ctx) {
            Decision::Deny(reason) => assert!(
                reason.contains("lens_search")
                    && reason.contains("lens_graph")
                    && reason.contains("lens_run"),
                "deny reason maps find/trace intents: {reason}"
            ),
            other => panic!("4th mixed lookup should deny, got {other:?}"),
        }
        // Deny reset the counter: a fresh Grep passes through.
        assert_eq!(route("Grep", &grep, &ctx), Decision::Passthrough);
    }

    #[test]
    fn read_non_code_files_never_escalate_to_graph() {
        let d = tempdir().unwrap();
        let ctx = rc(Level::Steer, true, d.path());
        let doc = json!({"file_path": "DECISIONS.md"});
        // Doc reads never count toward the deny counter — always silent.
        for _ in 0..7 {
            assert_eq!(route("Read", &doc, &ctx), Decision::Passthrough);
        }
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
    fn scope_deny_fires_once_per_arm_and_respects_kill_switch() {
        // grep-scope deny in isolation: ON by default (kill-switch polarity),
        // `=0` disables, one deny per arm, verbatim retry passes, re-arming
        // denies again.
        let _guard = SCOPE_ENV_LOCK.lock().unwrap();
        let d = tempdir().unwrap();
        seed_index(d.path());
        let ctx = rc(Level::Full, true, d.path());
        let broad = json!({"pattern": "x"});
        throttle::bump(ctx.data_dir, ctx.session_id, "grep-scope");

        // Kill-switched (`=0`) → never denies (marker untouched: the flag
        // check short-circuits before the throttle take).
        std::env::set_var("LENS_GREP_SCOPE_DENY", "0");
        assert!(
            !matches!(route("Grep", &broad, &ctx), Decision::Deny(_)),
            "kill-switched → no scope deny"
        );

        // Unset (default ON) → the armed grep-scope deny fires once with its
        // own reason.
        std::env::remove_var("LENS_GREP_SCOPE_DENY");
        match route("Grep", &broad, &ctx) {
            Decision::Deny(reason) => assert_eq!(reason, GREP_SCOPE_DENY_REASON),
            other => panic!("armed grep-scope deny should fire, got {other:?}"),
        }
        // Consumed → retry passes.
        assert!(
            !matches!(route("Grep", &broad, &ctx), Decision::Deny(_)),
            "grep-scope consumed → retry passes"
        );
        // Re-arm (explicit `=1` also enables) → denies again.
        std::env::set_var("LENS_GREP_SCOPE_DENY", "1");
        throttle::bump(ctx.data_dir, ctx.session_id, "grep-scope");
        assert!(
            matches!(route("Grep", &broad, &ctx), Decision::Deny(_)),
            "re-arming grep-scope denies again"
        );
        std::env::remove_var("LENS_GREP_SCOPE_DENY");
    }

    #[test]
    fn rail_flags_default_on_kill_switch_polarity() {
        // Every rail-arm flag shares grep-first's kill-switch polarity:
        // unset → enabled, `=0` → disabled, `=1` → enabled. Serialized with the
        // other env-mutating tests: LENS_GREP_FIRST_DENY is guarded by
        // READ_DENY_ENV_LOCK and LENS_GREP_SCOPE_DENY by SCOPE_ENV_LOCK
        // (acquired in that order, matching `grep_first_deny_consumes_scope_marker`).
        // The remaining flags are only read by route() paths whose other gates
        // (graph_resolves / index_present / classifier shape / reads_since_map)
        // are all false in the parallel tests, so flipping them here is benign.
        let _read_guard = READ_DENY_ENV_LOCK.lock().unwrap();
        let _scope_guard = SCOPE_ENV_LOCK.lock().unwrap();
        type FlagHelper = (&'static str, fn() -> bool);
        let helpers: &[FlagHelper] = &[
            ("LENS_GREP_FIRST_DENY", grep_first_deny_enabled),
            ("LENS_GREP_SCOPE_DENY", grep_scope_deny_enabled),
            ("LENS_GREP_SYMBOL_DENY", grep_symbol_deny_enabled),
            ("LENS_READ_SKELETON_DENY", read_skeleton_deny_enabled),
            ("LENS_GREP_AST_DENY", grep_ast_deny_enabled),
            ("LENS_BASH_AGG_DENY", bash_agg_deny_enabled),
            ("LENS_EDIT_LINKS_DENY", edit_links_deny_enabled),
            ("LENS_READ_OVERVIEW_DENY", read_overview_deny_enabled),
            ("LENS_BASH_GREP_DENY", bash_grep_deny_enabled),
            ("LENS_READ_RUNFILE_DENY", read_runfile_deny_enabled),
            ("LENS_GRAPH_REVERIFY", graph_reverify_enabled),
        ];
        for (var, enabled) in helpers {
            std::env::remove_var(var);
            assert!(enabled(), "{var}: unset must mean enabled (default ON)");
            std::env::set_var(var, "0");
            assert!(!enabled(), "{var}=0 must disable");
            std::env::set_var(var, "1");
            assert!(enabled(), "{var}=1 must enable");
            std::env::remove_var(var);
        }
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

    // ── default-ON rail arms: rovr deny, elink deny, rskel/runfile denies ──
    // READ_DENY_ENV_LOCK serializes these against
    // `rail_flags_default_on_kill_switch_polarity`, which flips the rail flags
    // these arms gate on.

    #[test]
    fn rovr_deny_fires_once_resets_counters_and_retry_passes() {
        let _guard = READ_DENY_ENV_LOCK.lock().unwrap();
        let d = tempdir().unwrap();
        seed_index(d.path());
        let mut ctx = rc(Level::Full, true, d.path());
        ctx.reads_since_map = reroute::read_overview::threshold();
        // Bounded Read (`limit`): keeps the rskel deny (default ON, index
        // seeded) out of the way so the rovr deny is the arm under test.
        let code = json!({"file_path": "src/server.rs", "limit": 40});
        // Prime the shared lookup counter to 3: the rovr deny must reset it so
        // the verbatim retry can't be caught by the escalation deny at 4.
        for _ in 0..3 {
            throttle::bump(ctx.data_dir, ctx.session_id, "read-code");
        }
        throttle::bump(ctx.data_dir, ctx.session_id, "reads-since-map");
        match route("Read", &code, &ctx) {
            Decision::Deny(reason) => assert!(
                reason.contains("lens_overview") && reason.contains("re-run it verbatim"),
                "rovr deny names lens_overview and promises the retry: {reason}"
            ),
            other => panic!("expected the rovr deny, got {other:?}"),
        }
        // The deny reset the hook-side reads-since-map counter (re-arm signal).
        assert_eq!(
            throttle::bump(ctx.data_dir, ctx.session_id, "reads-since-map"),
            1,
            "the rovr deny must reset reads-since-map"
        );
        // One-shot consumed + read-code reset + the runfile one-shot spent for
        // this (analysis-shaped) call → the verbatim retry passes even though
        // ctx still reports the read count as due. Without the runfile
        // neutralization the retry would be re-denied by the runfile arm.
        assert!(
            !matches!(route("Read", &code, &ctx), Decision::Deny(_)),
            "the verbatim retry must pass"
        );
    }

    #[test]
    fn rskel_deny_is_per_file_not_per_session() {
        // H3: each distinct file gets its own one-shot skeleton deny (the
        // `read-skeleton:{path}` key), instead of one deny per session forever.
        let _guard = READ_DENY_ENV_LOCK.lock().unwrap();
        let d = tempdir().unwrap();
        seed_index(d.path());
        let ctx = rc(Level::Full, true, d.path());
        let file_a = json!({"file_path": "src/widget_a.rs"});
        let file_b = json!({"file_path": "src/widget_b.rs"});
        match route("Read", &file_a, &ctx) {
            Decision::Deny(reason) => assert!(reason.contains("lens_skeleton"), "{reason}"),
            other => panic!("first whole-file Read of A should deny, got {other:?}"),
        }
        assert!(
            !matches!(route("Read", &file_a, &ctx), Decision::Deny(_)),
            "the retried Read of A passes (per-file one-shot)"
        );
        match route("Read", &file_b, &ctx) {
            Decision::Deny(reason) => assert!(
                reason.contains("lens_skeleton(path=\"src/widget_b.rs\")"),
                "file B gets its OWN skeleton deny in the same session: {reason}"
            ),
            other => panic!("first whole-file Read of B should deny, got {other:?}"),
        }
        assert!(
            !matches!(route("Read", &file_b, &ctx), Decision::Deny(_)),
            "the retried Read of B passes too"
        );
    }

    #[test]
    fn rskel_denies_regardless_of_file_size() {
        // The temporary RSKEL_MAX_FILE_BYTES guard is gone: lens_skeleton
        // budgets its own output now, so even a huge file is safely denied
        // toward it.
        let _guard = READ_DENY_ENV_LOCK.lock().unwrap();
        let d = tempdir().unwrap();
        seed_index(d.path());
        let big = d.path().join("big_module.rs");
        std::fs::write(&big, "x".repeat(200 * 1024)).unwrap();
        let ctx = rc(Level::Full, true, d.path());
        let ti = json!({"file_path": big.to_str().unwrap()});
        assert!(
            matches!(route("Read", &ti, &ctx), Decision::Deny(_)),
            "a whole-file Read is skeleton-denied at any size (budgeted skeleton)"
        );
    }

    #[test]
    fn runfile_deny_fires_once_per_file_then_passes() {
        // H5: an offset/limit Read of a code file denies once toward
        // lens_run; the second bounded Read of the SAME file passes —
        // never a hard wall.
        let _guard = READ_DENY_ENV_LOCK.lock().unwrap();
        let d = tempdir().unwrap();
        seed_index(d.path());
        let ctx = rc(Level::Full, true, d.path());
        let ti = json!({"file_path": "src/widget.rs", "offset": 10, "limit": 40});
        match route("Read", &ti, &ctx) {
            Decision::Deny(reason) => assert!(
                reason.contains("lens_run(path: \"src/widget.rs\"")
                    && reason.contains("language")
                    && reason.contains("code"),
                "runfile deny carries the pre-filled lens_run sketch: {reason}"
            ),
            other => panic!("expected the runfile deny, got {other:?}"),
        }
        assert!(
            !matches!(route("Read", &ti, &ctx), Decision::Deny(_)),
            "the second bounded Read of the same file passes through"
        );
        // A different file gets its own one-shot.
        let other_ti = json!({"file_path": "src/other_widget.rs", "limit": 20});
        assert!(
            matches!(route("Read", &other_ti, &ctx), Decision::Deny(_)),
            "a different file gets its own runfile one-shot"
        );
        // Kill-switch: LENS_READ_RUNFILE_DENY=0 disables the arm.
        std::env::set_var("LENS_READ_RUNFILE_DENY", "0");
        let ctx2 = rc(Level::Full, true, d.path());
        assert!(
            !matches!(route("Read", &ti, &ctx2), Decision::Deny(_)),
            "kill-switched runfile arm must never deny"
        );
        std::env::remove_var("LENS_READ_RUNFILE_DENY");
    }

    #[test]
    fn route_never_emits_context_for_routed_tools_at_full() {
        // Escalation-priming removal: at full routing level, route() must
        // never emit Decision::Context for Bash/Grep/Read/mcp__* — denies,
        // rewrites, and passthroughs only. Exercise every formerly-nudging
        // shape, repeatedly, with a seeded index so all rails are live.
        let _guard = READ_DENY_ENV_LOCK.lock().unwrap();
        let d = tempdir().unwrap();
        seed_index(d.path());
        let ctx = rc(Level::Full, true, d.path());
        throttle::bump(ctx.data_dir, ctx.session_id, "grep-first");
        throttle::bump(ctx.data_dir, ctx.session_id, "grep-scope");
        let calls: &[(&str, Value)] = &[
            ("Bash", json!({"command": "grep -rn foo src/"})),
            ("Bash", json!({"command": "find . -name '*.rs' | wc -l"})),
            ("Bash", json!({"command": "find . -type f"})),
            ("Bash", json!({"command": "curl https://api.example.com/data"})),
            ("Grep", json!({"pattern": "impl Forge"})),
            ("Grep", json!({"pattern": "fn handle_connection"})),
            ("Grep", json!({"pattern": "plain text"})),
            ("Read", json!({"file_path": "src/widget.rs"})),
            ("Read", json!({"file_path": "src/widget.rs", "limit": 40})),
            ("Read", json!({"file_path": "README.md"})),
            ("mcp__slack__search", json!({})),
            ("mcp__lens__lens_run", json!({})),
        ];
        for round in 0..3 {
            for (tool, ti) in calls {
                let decision = route(tool, ti, &ctx);
                assert!(
                    !matches!(decision, Decision::Context(_)),
                    "round {round}: {tool} {ti} must never yield Context, got {decision:?}"
                );
            }
        }
    }

    #[test]
    fn elink_deny_blocks_once_per_symbol_and_retry_passes() {
        use crate::discovery::graph::{Graph, Node};
        let _guard = READ_DENY_ENV_LOCK.lock().unwrap();
        let d = tempdir().unwrap();
        seed_index(d.path());
        // `foo` has 4 callers and `bar` 3 — both at/above the default
        // min_callers of 3.
        let mut g = Graph::new();
        let foo = g.add_node(Node::new("f.rs", "function", "foo", 1, "rust"));
        let bar = g.add_node(Node::new("f.rs", "function", "bar", 2, "rust"));
        for i in 0..4 {
            let c = g.add_node(Node::new(
                "c.rs",
                "function",
                &format!("caller{i}"),
                (i + 1) * 10,
                "rust",
            ));
            g.add_edge(&c, &foo, "calls");
            if i < 3 {
                g.add_edge(&c, &bar, "calls");
            }
        }
        g.save(&d.path().join("graph.json")).unwrap();
        let ctx = rc(Level::Full, true, d.path());
        let edit_foo = json!({"file_path": "f.rs", "old_string": "fn foo(a: i32)", "new_string": "fn foo(a: i64)"});
        match route("Edit", &edit_foo, &ctx) {
            Decision::Deny(reason) => assert!(
                reason.contains("lens_graph(node=\"foo\")") && reason.contains("re-run it verbatim"),
                "elink deny names lens_graph and promises the retry: {reason}"
            ),
            other => panic!("expected the elink deny, got {other:?}"),
        }
        // The elink:{sym} marker was set before the deny returned → the
        // verbatim retry (and any later Edit of `foo`) passes: an Edit is
        // blocked at most once per symbol per session.
        assert_eq!(route("Edit", &edit_foo, &ctx), Decision::Passthrough);
        // A different symbol gets its own (single) shot.
        let edit_bar =
            json!({"file_path": "f.rs", "old_string": "fn bar()", "new_string": "fn bar(x: u8)"});
        assert!(matches!(route("Edit", &edit_bar, &ctx), Decision::Deny(_)));
        assert_eq!(route("Edit", &edit_bar, &ctx), Decision::Passthrough);
        // A body-only edit never matches either arm.
        let body_edit = json!({"old_string": "let x = 1;", "new_string": "let x = 2;"});
        assert_eq!(route("Edit", &body_edit, &ctx), Decision::Passthrough);
    }

    #[test]
    fn edit_to_markdown_never_elink_denies() {
        use crate::discovery::graph::{Graph, Node};
        let _guard = READ_DENY_ENV_LOCK.lock().unwrap();
        let d = tempdir().unwrap();
        seed_index(d.path());
        // A graph where `name` has plenty of callers — the exact live false
        // positive (2026-07-19): a plan-file Edit whose old_string contained
        // `name` was matched as a decl symbol and denied.
        let mut g = Graph::new();
        let name = g.add_node(Node::new("f.rs", "function", "name", 1, "rust"));
        for i in 0..4 {
            let c = g.add_node(Node::new(
                "c.rs",
                "function",
                &format!("caller{i}"),
                (i + 1) * 10,
                "rust",
            ));
            g.add_edge(&c, &name, "calls");
        }
        g.save(&d.path().join("graph.json")).unwrap();
        let ctx = rc(Level::Full, true, d.path());
        let decl_ish = json!({
            "file_path": "plans/routing-closure.md",
            "old_string": "fn name(a: i32)",
            "new_string": "fn name(a: i64)",
        });
        assert_eq!(
            route("Edit", &decl_ish, &ctx),
            Decision::Passthrough,
            "a markdown/doc Edit must never trigger the elink deny"
        );
        // The same edit against a CODE path still denies (the guard is about
        // the file, not the text).
        let code_edit = json!({
            "file_path": "src/f.rs",
            "old_string": "fn name(a: i32)",
            "new_string": "fn name(a: i64)",
        });
        assert!(matches!(route("Edit", &code_edit, &ctx), Decision::Deny(_)));
    }

    // ── post_route(): retired to a passthrough seam ─────────────────────────

    #[test]
    fn post_route_always_passes_through() {
        let d = tempdir().unwrap();
        let big = "x".repeat(100_000);
        assert_eq!(
            post_route("Grep", &big, &rc(Level::Full, true, d.path())),
            Decision::Passthrough,
            "the grep-flood nudge is retired: PostToolUse never routes"
        );
    }

    #[test]
    fn post_route_marks_graph_surfaced_files_but_still_passes_through() {
        let d = tempdir().unwrap();
        let ctx = rc(Level::Full, true, d.path());
        let resp = json!({
            "nodes": [
                {"id": "n1", "name": "foo", "kind": "function", "file": "src/widget.rs", "line": 1, "language": "rust"},
            ],
            "edges": [],
            "truncated": false,
            "resolved": [],
        })
        .to_string();
        assert_eq!(
            post_route("mcp__lens__lens_graph", &resp, &ctx),
            Decision::Passthrough,
            "post_route never denies — it only arms the file marker"
        );
        assert!(
            throttle::fired(ctx.data_dir, ctx.session_id, "graphfile:src/widget.rs"),
            "the graph-surfaced file must be marked for the graph_reverify rail"
        );
        // A non-graph tool response never marks anything.
        let ctx2 = rc(Level::Full, true, d.path());
        post_route("Grep", &resp, &ctx2);
        assert!(!throttle::fired(
            ctx2.data_dir,
            ctx2.session_id,
            "graphfile:src/widget.rs"
        ));
    }

    #[test]
    fn to_post_hook_json_tags_posttooluse() {
        let v = to_post_hook_json(&Decision::Context("hi".into()));
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        assert_eq!(v["hookSpecificOutput"]["additionalContext"], "hi");
        assert_eq!(to_post_hook_json(&Decision::Passthrough), json!({}));
    }

    #[test]
    fn mcp_tools_pass_through_untouched() {
        // The periodic external-MCP nudge is retired: non-lens MCP tools and
        // composing lens calls always pass through — the achain rail below
        // only ever watches the atomic lens exploration calls.
        let d = tempdir().unwrap();
        let ctx = rc(Level::Full, true, d.path());
        let ti = json!({});
        assert_eq!(
            route("mcp__slack__search", &ti, &ctx),
            Decision::Passthrough
        );
        assert_eq!(
            route("mcp__lens__lens_run", &ti, &ctx),
            Decision::Passthrough
        );
    }

    // ── route(): atomic-chain (achain) compose deny ─────────────────────────

    #[test]
    fn achain_third_consecutive_atomic_call_denied_then_retry_passes() {
        let d = tempdir().unwrap();
        let ctx = rc(Level::Full, true, d.path());
        let ti = json!({});
        assert_eq!(
            route("mcp__lens__lens_skeleton", &ti, &ctx),
            Decision::Passthrough
        );
        assert_eq!(
            route("mcp__lens__lens_recall", &ti, &ctx),
            Decision::Passthrough
        );
        match route("mcp__lens__lens_recall", &ti, &ctx) {
            Decision::Deny(r) => {
                // The reason is tailored to the denied call (here lens_recall).
                assert!(r.contains("include_bodies"), "names the bodies escape");
                assert!(r.contains("lens_run"), "names the composed program");
                assert!(r.contains("verbatim"), "names the retry promise");
            }
            other => panic!("3rd consecutive atomic call must deny, got {other:?}"),
        }
        // The deny reset the counter: the verbatim retry passes.
        assert_eq!(
            route("mcp__lens__lens_recall", &ti, &ctx),
            Decision::Passthrough
        );
        // ONCE PER SESSION: the first deny taught the lesson; a later chain
        // passes instead of burning another round (2026-07-21 bad-set audit:
        // repeat denies measured not converting mid-chain).
        assert_eq!(
            route("mcp__lens__lens_graph", &ti, &ctx),
            Decision::Passthrough
        );
        assert_eq!(
            route("mcp__lens__lens_graph", &ti, &ctx),
            Decision::Passthrough,
            "a later chain must pass once the session's deny has fired"
        );
    }

    #[test]
    fn achain_chain_broken_by_composing_call_or_plain_tool() {
        let d = tempdir().unwrap();
        let ctx = rc(Level::Full, true, d.path());
        let ti = json!({});
        // Two atomic hops, then a composing lens call: the chain restarts.
        route("mcp__lens__lens_skeleton", &ti, &ctx);
        route("mcp__lens__lens_symbol", &ti, &ctx);
        assert_eq!(
            route("mcp__lens__lens_run", &ti, &ctx),
            Decision::Passthrough
        );
        assert_eq!(
            route("mcp__lens__lens_skeleton", &ti, &ctx),
            Decision::Passthrough,
            "a composing call must have restarted the count"
        );
        // Two hops again, then a plain tool: restarts again — the next atomic
        // calls are a fresh chain of 1 and 2, never a 3rd.
        route("mcp__lens__lens_symbol", &ti, &ctx);
        assert_eq!(route("Glob", &ti, &ctx), Decision::Passthrough);
        assert_eq!(
            route("mcp__lens__lens_recall", &ti, &ctx),
            Decision::Passthrough
        );
        assert_eq!(
            route("mcp__lens__lens_graph", &ti, &ctx),
            Decision::Passthrough
        );
    }

    #[test]
    fn achain_toolsearch_does_not_break_the_chain() {
        // Loading deferred schemas mid-chain is ceremony, not a change of
        // approach — the chain must survive it.
        let d = tempdir().unwrap();
        let ctx = rc(Level::Full, true, d.path());
        let ti = json!({});
        route("mcp__lens__lens_graph", &ti, &ctx);
        assert_eq!(route("ToolSearch", &ti, &ctx), Decision::Passthrough);
        route("mcp__lens__lens_graph", &ti, &ctx);
        assert!(
            matches!(route("mcp__lens__lens_graph", &ti, &ctx), Decision::Deny(_)),
            "ToolSearch between atomic calls must not reset the chain"
        );
    }

    #[test]
    fn achain_never_fires_at_nudge_level_or_before_mcp_ready() {
        let d = tempdir().unwrap();
        let ti = json!({});
        let ctx = rc(Level::Nudge, true, d.path());
        for _ in 0..4 {
            assert_eq!(
                route("mcp__lens__lens_skeleton", &ti, &ctx),
                Decision::Passthrough
            );
        }
        let ctx = rc(Level::Full, false, d.path());
        for _ in 0..4 {
            assert_eq!(
                route("mcp__lens__lens_skeleton", &ti, &ctx),
                Decision::Passthrough
            );
        }
    }

    #[test]
    fn achain_kill_switch_polarity() {
        // Route-level kill-switch runs would race the other achain tests on
        // the process-global env, so only the flag parse is asserted here.
        std::env::set_var("LENS_ATOMIC_CHAIN_DENY", "0");
        assert!(!atomic_chain_deny_enabled());
        std::env::remove_var("LENS_ATOMIC_CHAIN_DENY");
        assert!(atomic_chain_deny_enabled(), "on by default");
    }

    // ── route(): overview-rebuy (ovrb) unfocused re-buy deny ────────────────

    #[test]
    fn ovrb_fires_only_with_digest_marker() {
        let d = tempdir().unwrap();
        let ctx = rc(Level::Full, true, d.path());
        let ti = json!({});
        // No digest marker → passthrough.
        assert_eq!(
            route("mcp__lens__lens_overview", &ti, &ctx),
            Decision::Passthrough
        );
        // Digest armed → deny, naming the digest + expand path + retry.
        throttle::mark(ctx.data_dir, ctx.session_id, "ovrb:digest");
        match route("mcp__lens__lens_overview", &ti, &ctx) {
            Decision::Deny(r) => {
                assert!(
                    r.contains("session start") || r.contains("<repo_map>"),
                    "names the digest: {r}"
                );
                assert!(r.contains("lens_symbol"), "expand via symbol: {r}");
                assert!(r.contains("lens_graph"), "expand via graph: {r}");
                assert!(r.contains("verbatim"), "retry promise: {r}");
            }
            other => panic!("unfocused overview with digest must deny, got {other:?}"),
        }
    }

    #[test]
    fn ovrb_verbatim_retry_passes() {
        let d = tempdir().unwrap();
        let ctx = rc(Level::Full, true, d.path());
        let ti = json!({});
        throttle::mark(ctx.data_dir, ctx.session_id, "ovrb:digest");
        assert!(
            matches!(
                route("mcp__lens__lens_overview", &ti, &ctx),
                Decision::Deny(_)
            ),
            "first unfocused re-buy must deny"
        );
        // ovrb:done marked before Deny → verbatim retry always passes.
        assert_eq!(
            route("mcp__lens__lens_overview", &ti, &ctx),
            Decision::Passthrough
        );
    }

    #[test]
    fn ovrb_query_arg_call_passes() {
        let d = tempdir().unwrap();
        let ctx = rc(Level::Full, true, d.path());
        throttle::mark(ctx.data_dir, ctx.session_id, "ovrb:digest");
        // Focused overview is a different map, not a digest re-buy.
        assert_eq!(
            route(
                "mcp__lens__lens_overview",
                &json!({"query": "auth"}),
                &ctx
            ),
            Decision::Passthrough
        );
    }

    #[test]
    fn ovrb_stands_down_when_rovr_fired() {
        let d = tempdir().unwrap();
        let ctx = rc(Level::Full, true, d.path());
        throttle::mark(ctx.data_dir, ctx.session_id, "ovrb:digest");
        // rovr's one-shot key (nudge_once("read-overview")) already spent —
        // that rail pushed TOWARD lens_overview; do not fight it.
        throttle::mark(ctx.data_dir, ctx.session_id, "read-overview");
        assert_eq!(
            route("mcp__lens__lens_overview", &json!({}), &ctx),
            Decision::Passthrough
        );
    }

    #[test]
    fn ovrb_never_fires_at_nudge_level_or_before_mcp_ready() {
        let d = tempdir().unwrap();
        let ti = json!({});
        let ctx = rc(Level::Nudge, true, d.path());
        throttle::mark(ctx.data_dir, ctx.session_id, "ovrb:digest");
        assert_eq!(
            route("mcp__lens__lens_overview", &ti, &ctx),
            Decision::Passthrough
        );
        let ctx = rc(Level::Full, false, d.path());
        throttle::mark(ctx.data_dir, ctx.session_id, "ovrb:digest");
        assert_eq!(
            route("mcp__lens__lens_overview", &ti, &ctx),
            Decision::Passthrough
        );
    }

    #[test]
    fn ovrb_kill_switch_polarity() {
        // Route-level kill-switch runs would race other tests on the
        // process-global env, so only the flag parse is asserted here.
        std::env::set_var("LENS_OVERVIEW_REBUY_DENY", "0");
        assert!(!overview_rebuy_deny_enabled());
        std::env::remove_var("LENS_OVERVIEW_REBUY_DENY");
        assert!(overview_rebuy_deny_enabled(), "on by default");
    }

    #[test]
    fn ovrb_denied_call_does_not_advance_achain() {
        // A denied ovrb call must not bump achain-run — otherwise the next
        // two atomic hops would trip achain early.
        let d = tempdir().unwrap();
        let ctx = rc(Level::Full, true, d.path());
        throttle::mark(ctx.data_dir, ctx.session_id, "ovrb:digest");
        assert!(matches!(
            route("mcp__lens__lens_overview", &json!({}), &ctx),
            Decision::Deny(_)
        ));
        // Two atomic hops after the denied overview must still pass (count=2).
        assert_eq!(
            route("mcp__lens__lens_skeleton", &json!({}), &ctx),
            Decision::Passthrough
        );
        assert_eq!(
            route("mcp__lens__lens_symbol", &json!({}), &ctx),
            Decision::Passthrough
        );
        // Third atomic is the first achain deny.
        assert!(
            matches!(
                route("mcp__lens__lens_graph", &json!({}), &ctx),
                Decision::Deny(_)
            ),
            "achain must still trip on the 3rd consecutive atomic, not earlier"
        );
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
                assert!(p.contains("<lens_tools>"), "compact tool block appended");
                assert!(
                    p.contains("ToolSearch"),
                    "carries the deferred-tool bootstrap"
                );
                assert!(
                    !p.contains("<context_window_protection>"),
                    "the full SessionStart prose stays out of sub-agent prompts"
                );
                // The one-line map names every lens tool (10 current).
                for tool in [
                    "lens_run",
                    "lens_search",
                    "lens_symbol",
                    "lens_graph",
                    "lens_recall",
                    "lens_skeleton",
                    "lens_overview",
                    "lens_grep_ast",
                    "lens_memory_query",
                    "lens_memory_record",
                ] {
                    assert!(p.contains(tool), "sub-agent block names {tool}");
                }
                // Verify removed tools are NOT in the sub-agent block.
                for removed in [
                    "lens_run_file",
                    "lens_index",
                    "lens_map",
                    "lens_find",
                    "lens_links",
                    "lens_path",
                    "lens_stats",
                ] {
                    assert!(
                        !p.contains(removed),
                        "sub-agent block should not name removed tool {removed}"
                    );
                }
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

    // ── mcp_ready ──────────────────────────────────────────────────────────
    // NOTE: these touch LENS_ROUTING_MCP / LENS_MCP_TTL, so they are
    // grouped into one serialized test to avoid env races with other tests.

    #[test]
    fn mcp_ready_env_override_and_heartbeat() {
        let _guard = MCP_ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let d = tempdir().unwrap();
        // No heartbeats dir, no override → not ready.
        std::env::remove_var("LENS_ROUTING_MCP");
        std::env::remove_var("LENS_MCP_TTL");
        assert!(!mcp_ready(d.path()));

        // Override up/down wins regardless of heartbeat state.
        std::env::set_var("LENS_ROUTING_MCP", "up");
        assert!(mcp_ready(d.path()));
        std::env::set_var("LENS_ROUTING_MCP", "off");
        assert!(!mcp_ready(d.path()));
        std::env::remove_var("LENS_ROUTING_MCP");

        // Fresh heartbeat file within TTL → ready.
        let hb = d.path().join("heartbeats");
        std::fs::create_dir_all(&hb).unwrap();
        std::fs::write(hb.join("123.pid"), "123").unwrap();
        assert!(mcp_ready(d.path()));

        // A second session's server (different pid, own file) sharing the same
        // data dir: both fresh → still ready. Removing one (its clean shutdown)
        // must not affect the other — the whole point of per-pid files instead
        // of a single shared server.pid.
        std::fs::write(hb.join("456.pid"), "456").unwrap();
        assert!(mcp_ready(d.path()));
        std::fs::remove_file(hb.join("123.pid")).unwrap();
        assert!(
            mcp_ready(d.path()),
            "session B's heartbeat must still count as ready after session A's clean exit"
        );

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
        // Current 10-tool surface: check all are mentioned.
        for needle in [
            "lens_run",
            "lens_search",
            "lens_symbol",
            "lens_graph",
            "lens_recall",
            "lens_skeleton",
            "lens_overview",
            "lens_grep_ast",
            "lens_memory_query",
            "lens_memory_record",
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
        // Verify removed tools are NOT mentioned.
        for removed in [
            "lens_find",
            "lens_index",
            "lens_map",
            "lens_run_file",
            "lens_links",
            "lens_path",
            "lens_stats",
        ] {
            assert!(!b.contains(removed), "session_block still mentions removed tool {removed:?}");
        }
    }
}
