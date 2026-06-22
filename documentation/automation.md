# Automation

ctxforge does not *embed* an AI agent, but it **inserts automation into another
agent's loop**: the Claude Code lifecycle hooks observe, annotate, and can
deny/rewrite the agent's tool calls, and the MCP sandbox is an agent-driven
execution surface. That is exactly the surface this document exists to make
visible: what the automation may touch, where steering ends and hard guardrails
begin, and the line between what it *suggests* and what it *enforces*.

There are three automation paths. None calls an LLM itself; they shape what the
host LLM sees and does.

---

## A-1: PreToolUse routing (the interception path)

| Aspect | Detail |
| :- | :- |
| **Trigger** | Claude Code fires `ctxforge hook claude PreToolUse` before every tool call. Automatic. |
| **Owner** | `src/routing/mod.rs`, driven by `src/session/hook.rs`. |
| **Runs automatically vs. on approval** | Automatic, but **gated by `CTXFORGE_ROUTING`** (default `off` = a true no-op returning `{}`). Levels: `steer`, `wrap`, `full`. |
| **Inputs it may read** | The tool name and tool input JSON, the session id, the `server.pid` heartbeat, and throttle markers. It does **not** read file contents or network. |
| **Tools/APIs it may call** | None outbound. Its only "action" is returning a decision JSON. The *effect* of a wrap decision is to rewrite a Bash command to invoke the local `ctxforge wrap` CLI. |
| **Steering (soft, prompt-level)** | `<context_guidance>` nudges for Bash/Grep/Read; the sub-agent prompt injection; the periodic external-MCP nudge; the WebFetch/curl/build redirect messages. The model can ignore all of these. |
| **Hard guardrails (non-prompt)** | (1) `off` short-circuits everything. (2) **Stateful commands are never rewritten** (`cd`, `export`, assignments, function defs, backticks) — protects the persistent shell. (3) Only an explicit **read-only allowlist** of programs/subcommands is wrappable. (4) **Structurally-bounded** commands are skipped. (5) `mcp_ready` gates redirect-to-MCP decisions so the agent is never sent to a dead tool. (6) RTK-active defers Bash entirely. |
| **Output contract** | A PreToolUse hook JSON: `permissionDecision: deny`/`allow` (+ optional `updatedInput`), or `additionalContext` for a soft nudge, or `{}` for passthrough. Built by `to_hook_json`; shape is unit-tested with golden payloads. |
| **App-owned side effects vs. suggestions** | **Enforced by ctxforge:** the deny, and the command rewrite (`updatedInput`) — Claude Code honors these. **Suggestions only:** every `additionalContext` nudge and every redirect that replaces the command with an `echo "...guidance..."`. |
| **Controls** | Kill switch: `CTXFORGE_ROUTING=off`. Throttling: one-shot (`guidance_once`) and periodic (`throttle_periodic`) markers under `.ctxforge/throttle/`. Audit: every wrap writes one `ops.log` record. No rate limit beyond throttling; no approval gate (the hook is synchronous). |

**Failure handling:** any error in the hook is swallowed and a valid passthrough
response is printed (a hook can never block the session). Malformed stdin →
default response, exit 0.

---

## A-2: The sandbox (agent-driven execution surface)

| Aspect | Detail |
| :- | :- |
| **Trigger** | The agent calls `ctx_execute` / `ctx_execute_file`. Never automatic; the model chooses it. |
| **Owner** | `src/sandbox/mod.rs`. |
| **Inputs it may read** | Whatever the agent-authored script reads: full filesystem, stdin (if provided), network. For `ctx_execute_file`, a target path is injected as the script's first CLI arg. |
| **Tools/APIs it may call** | A real language interpreter (python/node/tsx/bash/ruby/go) as a subprocess with `cwd = repo`. **This is the tool surface, and it is unbounded** — there is no syscall, FS, or network restriction. |
| **Steering vs. hard guardrails** | Steering: the MCP `instructions` and SessionStart guide tell the model to "print only the answer." **Hard guardrails: only a per-call timeout and `kill_on_drop`.** Output above `CTXFORGE_MAX_INLINE` is offloaded, but that bounds *context*, not *what the code can do*. |
| **Output contract** | `ExecuteResponse { stdout, stderr, exit_code, timed_out, stdout_bytes, truncated, retrieve_ref }`. Large stdout is replaced by a head+tail preview + a `retrieve_ref`; the full blob is recoverable via `ctx_retrieve`. |
| **App-owned side effects vs. suggestions** | Everything the script does is a **real, app-owned side effect** on the host. There is no proposal/approval split. |
| **Controls** | Timeout (`timeout_secs`, min 1s); supported-language check; missing-interpreter error. Audit: one `ops.log` record per call (logs `{language, code_bytes}`, **not the code**). No kill switch short of not exposing the tool. |

**Reviewer note:** this is the highest-risk automation surface (TB-1). It is
deliberately unconfined because the agent already has a Bash tool; ctxforge adds
output capture, not a security boundary. See `flows.md` F-1.

---

## A-3: Session capture and resume injection

| Aspect | Detail |
| :- | :- |
| **Trigger** | `UserPromptSubmit`, `PostToolUse`, `PreCompact`, `SessionStart`. Automatic. |
| **Owner** | `src/session/hook.rs`, `src/session/extract.rs`, `src/session/snapshot.rs`. |
| **Inputs it may read** | Prompt text, tool input/response, and the project rule files (`CLAUDE.md`/`AGENTS.md`) on startup. |
| **Tools/APIs it may call** | None outbound. Writes to `session.db` and the FTS index. |
| **Steering vs. hard guardrails** | Hard: system messages and empty prompts are skipped (`is_system_message`); startup clears prior project events; snapshots are bounded by `CTXFORGE_SNAPSHOT_BUDGET`. |
| **Output contract** | At `SessionStart(compact/resume)`, returns a Session Guide string as `additionalContext` (re-injected into the conversation). Otherwise `{}`. |
| **App-owned side effects vs. suggestions** | **Enforced:** the persisted events and the consumed-resume bookkeeping. **Suggestion:** the re-injected Session Guide is context the model reads, not an action. |
| **Controls** | Uninstall the hooks to disable. Data is local to `.ctxforge/`. No rate limit (events are cheap appends). |

**Privacy:** captured prompts and file paths are stored in plaintext; there is no
redaction. See `variables.md`.

---

## Cross-cutting guardrails

- **Default-off for interception.** The one automation that changes agent
  behavior (A-1) is a no-op until explicitly enabled.
- **Hooks can't block the session.** All hook errors degrade to passthrough.
- **Audit trail.** `ops.log` records every MCP tool op and every `bash_wrap`,
  feeding `ctxforge stats` and the dashboard.
- **No autonomous outbound calls.** Nothing here reaches the network on its own;
  the only outbound traffic is RTK's opt-in installer and whatever an agent script
  chooses to fetch inside the sandbox.
