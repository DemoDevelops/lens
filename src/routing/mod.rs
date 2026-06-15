//! PreToolUse routing policy — pass through, deny, rewrite, or nudge a tool call.
//!
//! Gated by `CTXFORGE_ROUTING` (off|steer|wrap|full); default `off` is a true
//! no-op (PreToolUse returns `{}`). Three concerns layer on top of `off`:
//!
//!   * **steer** — deny `WebFetch` (fetch+process in the sandbox instead), and
//!     emit one-shot non-blocking nudges toward ctxforge tools for `Bash` /
//!     `Grep` / (at full) `Read`. Also injects a tool-selection guide at
//!     `SessionStart`.
//!   * **wrap** — transparently rewrite a read-only, high-output `Bash` command
//!     into `ctxforge wrap -- <cmd>` so its output is offloaded losslessly.
//!   * **full** — both of the above.
//!
//! Two hard safety rails: routing never engages unless the MCP server is
//! reachable ([`mcp_ready`]), and stateful shell commands (anything that mutates
//! shell state — `cd`, `export`, assignments, …) are always passed through
//! untouched, because rewriting them would silently change session behavior.

use std::fs::OpenOptions;
use std::path::Path;

use serde_json::{json, Value};

/// Active routing level, parsed from `CTXFORGE_ROUTING`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// True no-op: PreToolUse returns `{}`, SessionStart unchanged.
    Off,
    /// Deny WebFetch + emit one-shot nudges + inject the SessionStart guide.
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
            "steer" => Level::Steer,
            "wrap" => Level::Wrap,
            "full" => Level::Full,
            _ => Level::Off,
        }
    }

    /// Read the level from `CTXFORGE_ROUTING` (unset => `off`).
    pub fn from_env() -> Level {
        Level::parse(&std::env::var("CTXFORGE_ROUTING").unwrap_or_default())
    }

    /// Whether this level steers: WebFetch deny, Bash/Grep/Read nudges, and the
    /// SessionStart guide block.
    pub(crate) fn steers(self) -> bool {
        matches!(self, Level::Steer | Level::Full)
    }

    /// Whether this level rewrites read-only Bash commands into `ctxforge wrap`.
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

/// Everything [`route`] needs that isn't the tool call itself.
pub struct RouteCtx<'a> {
    /// Active routing level.
    pub level: Level,
    /// Whether the MCP server is currently reachable (the master safety gate).
    pub mcp_ready: bool,
    /// Absolute path to the `ctxforge` binary (for the wrap rewrite).
    pub bin: &'a str,
    /// ctxforge data dir (holds throttle markers + `server.pid`).
    pub data_dir: &'a Path,
    /// Current session id (scopes one-shot nudge throttling).
    pub session_id: &'a str,
}

// ── Reason / nudge prose (original wording) ────────────────────────────────

/// Shown when a WebFetch is denied under steering.
pub const WEBFETCH_REASON: &str = "ctxforge routing: fetch+process web content in the sandbox instead — use ctx_execute (python) to fetch the URL and print only what you need; the full response stays out of context and is recoverable via ctx_retrieve.";

/// Shown when a read-only Bash command is wrapped under `wrap`/`full`.
pub const WRAP_REASON: &str = "ctxforge: wrapped a read-only command to offload large output losslessly (recover full output via ctx_retrieve).";

/// One-shot nudge for non-wrapping steering on a wrappable Bash command.
pub const BASH_NUDGE: &str = "ctxforge tip: for large read-only shell output, run it through ctx_execute (bash) — only the printed lines return to context and the full output stays recoverable via ctx_retrieve.";

/// One-shot nudge steering Grep toward the indexed search tool.
pub const GREP_NUDGE: &str = "ctxforge tip: prefer ctx_search over Grep — it queries the FTS5 index for ranked snippets without spilling whole files into context (run ctx_index first if needed).";

/// One-shot nudge (full only) steering large reads into the sandbox.
pub const READ_NUDGE: &str = "ctxforge tip: for large files, ctx_execute_file runs a script over the file in the sandbox and returns only what you print — keeping the raw bytes out of context.";

/// Route one PreToolUse call to a [`Decision`].
///
/// Order matters: `Off` and `!mcp_ready` short-circuit to [`Decision::Passthrough`]
/// before any per-tool logic, so routing is inert whenever it should be.
pub fn route(tool: &str, tool_input: &Value, ctx: &RouteCtx) -> Decision {
    if ctx.level == Level::Off {
        return Decision::Passthrough;
    }
    // Master safety gate: never wedge a tool call when the server that backs the
    // ctxforge tools is unreachable.
    if !ctx.mcp_ready {
        return Decision::Passthrough;
    }

    match tool {
        "WebFetch" => {
            if ctx.level.steers() {
                Decision::Deny(WEBFETCH_REASON.to_string())
            } else {
                Decision::Passthrough
            }
        }
        "Bash" => bash_decision(tool_input, ctx),
        "Grep" => {
            if ctx.level.steers() && throttle_once(ctx, "grep") {
                Decision::Context(GREP_NUDGE.to_string())
            } else {
                Decision::Passthrough
            }
        }
        "Read" => {
            if ctx.level == Level::Full && throttle_once(ctx, "read") {
                Decision::Context(READ_NUDGE.to_string())
            } else {
                Decision::Passthrough
            }
        }
        _ => Decision::Passthrough,
    }
}

/// Bash-specific routing: wrap or nudge read-only high-output commands, but
/// never touch stateful or non-allowlisted ones.
fn bash_decision(tool_input: &Value, ctx: &RouteCtx) -> Decision {
    let cmd = tool_input["command"].as_str().unwrap_or("");
    if cmd.is_empty() {
        return Decision::Passthrough;
    }
    // Stateful commands mutate shell state; rewriting them would change behavior.
    if is_stateful(cmd) {
        return Decision::Passthrough;
    }
    if is_wrappable(cmd) {
        if ctx.level.wraps() {
            let mut updated = tool_input.clone();
            let rewritten = format!("{} wrap -- {}", q(ctx.bin), q(cmd));
            updated["command"] = Value::String(rewritten);
            Decision::Modify {
                reason: WRAP_REASON.to_string(),
                updated_input: updated,
            }
        } else if ctx.level.steers() {
            if throttle_once(ctx, "bash") {
                Decision::Context(BASH_NUDGE.to_string())
            } else {
                Decision::Passthrough
            }
        } else {
            Decision::Passthrough
        }
    } else {
        Decision::Passthrough
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
fn is_stateful(cmd: &str) -> bool {
    // Backtick command substitution and function definitions are hard to reason
    // about; treat the whole command as stateful.
    if cmd.contains('`') {
        return true;
    }
    const STATEFUL: &[&str] = &[
        "cd", "export", "source", ".", "alias", "unalias", "set", "unset",
        "pushd", "popd", "eval", "trap",
    ];
    for seg in segments(cmd) {
        let tok = first_token(&seg);
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
        if contains_fn_def(&seg) {
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

/// Programs that are read-only regardless of their arguments.
const PLAIN_ALLOW: &[&str] = &[
    "find", "cat", "ls", "tree", "rg", "grep", "egrep", "fgrep", "tail", "head",
    "wc", "sort", "uniq", "nl", "curl", "wget", "gradle", "gradlew", "mvn",
    "sbt", "pytest", "jest", "vitest",
];

/// For tools that mix read-only and mutating subcommands, the subcommand
/// (token[1]) must be in the read-only set for the segment to be allowlisted.
fn subcommand_ok(prog: &str, sub: &str) -> Option<bool> {
    let set: &[&str] = match prog {
        "git" => &[
            "log", "diff", "show", "status", "blame", "shortlog", "reflog",
            "whatchanged", "ls-files", "ls-tree", "rev-parse", "describe", "grep",
        ],
        "cargo" => &["test", "build", "check", "clippy", "bench", "tree", "doc"],
        "go" => &["test", "build", "vet", "list"],
        "npm" | "yarn" | "pnpm" => {
            &["test", "run", "build", "ci", "audit", "outdated", "list", "ls"]
        }
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
    PLAIN_ALLOW.contains(&prog)
}

/// Is the whole command line safe to wrap? Every pipeline/chain segment's
/// leading program must be read-only and allowlisted, so a single mutating stage
/// (`find … | xargs rm`) disqualifies the line.
fn is_wrappable(cmd: &str) -> bool {
    let mut any = false;
    for seg in segments(cmd) {
        if seg.trim().is_empty() {
            continue;
        }
        any = true;
        if !segment_allowlisted(&seg) {
            return false;
        }
    }
    any
}

/// Fire a one-shot nudge per `(session, key)`. Returns `true` exactly once: the
/// first call creates a marker file with `O_EXCL` semantics; later calls see
/// `AlreadyExists` and return `false`. Any other error also returns `false`, so
/// a flaky filesystem suppresses the nudge rather than spamming it.
fn throttle_once(ctx: &RouteCtx, key: &str) -> bool {
    let dir = ctx.data_dir.join("throttle");
    let _ = std::fs::create_dir_all(&dir); // best effort
    let marker = dir.join(format!("{}.{}", sanitize(ctx.session_id), key));
    // `create_new` is O_EXCL: Ok only on the first creation. Any error
    // (AlreadyExists, or a filesystem failure) means "don't fire" — suppress
    // rather than risk spamming the nudge.
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
        .is_ok()
}

/// Replace every non-alphanumeric byte with `_` so a session id is filesystem
/// safe.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Is the MCP server reachable right now?
///
/// `CTXFORGE_ROUTING_MCP` forces the answer when set (`up`/`1`/`on`/`true` =>
/// reachable; `down`/`0`/`off`/`false` => not). Otherwise the server's
/// heartbeat file `<data_dir>/server.pid` is consulted: it counts as reachable
/// only while its mtime is within the TTL (`CTXFORGE_MCP_TTL` seconds, default
/// 90 — three heartbeat intervals). Missing, stale, or unreadable => not ready.
pub fn mcp_ready(data_dir: &Path) -> bool {
    if let Ok(v) = std::env::var("CTXFORGE_ROUTING_MCP") {
        match v.trim().to_ascii_lowercase().as_str() {
            "up" | "1" | "on" | "true" => return true,
            "down" | "0" | "off" | "false" => return false,
            _ => {} // fall through to the heartbeat check
        }
    }
    let ttl = std::env::var("CTXFORGE_MCP_TTL")
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

/// The tool-selection guide injected at `SessionStart` while steering is active.
///
/// Original prose: a small `<ctxforge_routing>` block giving the model a
/// hierarchy for choosing ctxforge tools over the raw ones, plus the two routing
/// behaviors it will observe (WebFetch denied, read-only shell wrapped). Kept
/// well under ~500 tokens.
pub fn session_block(level: Level) -> String {
    let lvl = match level {
        Level::Off => "off",
        Level::Steer => "steer",
        Level::Wrap => "wrap",
        Level::Full => "full",
    };
    format!(
        "<ctxforge_routing>\n\
ctxforge routing is active (level: {lvl}). It keeps bulky data out of context by \
preferring sandboxed/indexed tools over raw ones. Follow this tool-selection \
hierarchy:\n\
1. Process or transform data (parse, filter, fetch a URL, crunch a file): \
ctx_execute runs a script in a sandbox and returns only what you print; \
ctx_execute_file does the same scoped to one file. The raw data never enters \
context and is recoverable with ctx_retrieve.\n\
2. Search code or notes: ctx_index then ctx_search for ranked FTS5 snippets, \
instead of reading whole files.\n\
3. Understand structure (who-calls-what, imports, paths): ctx_discover once, \
then graph_query to locate symbols and ctx_retrieve to expand any compacted \
result.\n\
4. Only drop to raw Read/Grep/Bash when no ctxforge tool fits.\n\
Two behaviors you will observe: WebFetch is denied — fetch and reduce web \
content via ctx_execute instead; and allowlisted read-only shell commands are \
transparently wrapped via `ctxforge wrap` so their full output is offloaded \
losslessly (recover it with ctx_retrieve).\n\
</ctxforge_routing>"
    )
}

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
        assert_eq!(Level::parse("nonsense"), Level::Off);
    }

    #[test]
    fn steers_and_wraps_flags() {
        assert!(!Level::Off.steers() && !Level::Off.wraps());
        assert!(Level::Steer.steers() && !Level::Steer.wraps());
        assert!(!Level::Wrap.steers() && Level::Wrap.wraps());
        assert!(Level::Full.steers() && Level::Full.wraps());
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
        assert_eq!(q("/p ath/ctxforge"), "'/p ath/ctxforge'");
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

    // ── route(): MCP-ready gate ────────────────────────────────────────────

    fn rc<'a>(level: Level, mcp_ready: bool, dir: &'a Path) -> RouteCtx<'a> {
        RouteCtx {
            level,
            mcp_ready,
            bin: "/path with space/ctxforge",
            data_dir: dir,
            session_id: "sess-1",
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
    fn mcp_not_ready_forces_passthrough_for_all_tools() {
        let d = tempdir().unwrap();
        let ctx = rc(Level::Full, false, d.path());
        assert_eq!(
            route("WebFetch", &json!({"url": "http://x"}), &ctx),
            Decision::Passthrough
        );
        assert_eq!(
            route("Bash", &json!({"command": "find ."}), &ctx),
            Decision::Passthrough
        );
        assert_eq!(
            route("Grep", &json!({"pattern": "x"}), &ctx),
            Decision::Passthrough
        );
        assert_eq!(
            route("Read", &json!({"file_path": "x"}), &ctx),
            Decision::Passthrough
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
                    q("/path with space/ctxforge"),
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
    fn bash_empty_command_passthrough() {
        let d = tempdir().unwrap();
        assert_eq!(
            route("Bash", &json!({}), &rc(Level::Full, true, d.path())),
            Decision::Passthrough
        );
        assert_eq!(
            route("Bash", &json!({"command": ""}), &rc(Level::Full, true, d.path())),
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
    fn read_nudges_only_at_full() {
        let d = tempdir().unwrap();
        let ti = json!({"file_path": "big.rs"});
        // steer alone does NOT nudge Read
        assert_eq!(
            route("Read", &ti, &rc(Level::Steer, true, d.path())),
            Decision::Passthrough
        );
        // full nudges once
        let d2 = tempdir().unwrap();
        let ctx = rc(Level::Full, true, d2.path());
        assert_eq!(
            route("Read", &ti, &ctx),
            Decision::Context(READ_NUDGE.to_string())
        );
        assert_eq!(route("Read", &ti, &ctx), Decision::Passthrough);
    }

    #[test]
    fn unknown_tool_passthrough() {
        let d = tempdir().unwrap();
        assert_eq!(
            route("Edit", &json!({"file_path": "x"}), &rc(Level::Full, true, d.path())),
            Decision::Passthrough
        );
    }

    // ── throttle_once ──────────────────────────────────────────────────────

    #[test]
    fn throttle_fires_exactly_once_per_key() {
        let d = tempdir().unwrap();
        let ctx = rc(Level::Steer, true, d.path());
        assert!(throttle_once(&ctx, "k"));
        assert!(!throttle_once(&ctx, "k"));
        // a different key is independent
        assert!(throttle_once(&ctx, "other"));
        assert!(!throttle_once(&ctx, "other"));
    }

    #[test]
    fn sanitize_replaces_non_alphanumeric() {
        assert_eq!(sanitize("a-b/c.d 1"), "a_b_c_d_1");
        assert_eq!(sanitize("abc123"), "abc123");
    }

    // ── mcp_ready ──────────────────────────────────────────────────────────
    // NOTE: these touch CTXFORGE_ROUTING_MCP / CTXFORGE_MCP_TTL, so they are
    // grouped into one serialized test to avoid env races with other tests.

    #[test]
    fn mcp_ready_env_override_and_heartbeat() {
        let d = tempdir().unwrap();
        // No pidfile, no override → not ready.
        std::env::remove_var("CTXFORGE_ROUTING_MCP");
        std::env::remove_var("CTXFORGE_MCP_TTL");
        assert!(!mcp_ready(d.path()));

        // Override up/down wins regardless of pidfile.
        std::env::set_var("CTXFORGE_ROUTING_MCP", "up");
        assert!(mcp_ready(d.path()));
        std::env::set_var("CTXFORGE_ROUTING_MCP", "off");
        assert!(!mcp_ready(d.path()));
        std::env::remove_var("CTXFORGE_ROUTING_MCP");

        // Fresh pidfile within TTL → ready.
        std::fs::write(d.path().join("server.pid"), "123").unwrap();
        assert!(mcp_ready(d.path()));

        // TTL of 0 makes any nonzero age stale (sleep a moment to be safe).
        std::env::set_var("CTXFORGE_MCP_TTL", "0");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert!(!mcp_ready(d.path()));
        std::env::remove_var("CTXFORGE_MCP_TTL");
    }

    // ── session_block ──────────────────────────────────────────────────────

    #[test]
    fn session_block_mentions_tools_and_behaviors() {
        let b = session_block(Level::Full);
        assert!(b.starts_with("<ctxforge_routing>"));
        assert!(b.contains("</ctxforge_routing>"));
        for needle in [
            "ctx_execute",
            "ctx_index",
            "ctx_search",
            "ctx_execute_file",
            "ctx_discover",
            "graph_query",
            "ctx_retrieve",
            "hierarchy",
            "WebFetch is denied",
            "ctxforge wrap",
            "level: full",
        ] {
            assert!(b.contains(needle), "session_block missing {needle:?}");
        }
    }
}
