# Architecture

## What this is

lens is a single Rust binary (`lens`) that helps a Claude Code agent use
fewer context tokens and survive conversation compaction. It is a **local
developer tool**, not a networked service. There are no end users, no accounts,
no login. The "callers" are an AI agent, the Claude Code harness, and the human
who runs the CLI on their own machine.

The one binary runs in several modes, dispatched by `argv[1]` in `src/main.rs`:

| Mode | Invocation | Process role |
| :- | :- | :- |
| MCP server (default) | `lens` (no subcommand) | Long-lived; stdout is a JSON-RPC channel |
| Lifecycle hook | `lens hook claude <event>` | Short-lived; Claude Code fires it per event |
| Hook install/remove | `lens session <install\|uninstall\|status>` | Edits `~/.claude/settings.json` |
| RTK manage | `lens rtk <install\|status\|uninstall\|sync>` | Downloads/registers the RTK binary |
| Bash wrapper | `lens wrap -- <cmd>` | Runs a shell command, offloads big output |
| Observability | `lens stats \| verify \| dashboard` | Read-only views over `.lens/` state |
| Warmup | `lens warmup [path]` | Prebuilds graph + index |

The two halves are installed and used independently:

1. **Savings half (passive MCP server).** Exposes nine tools (`lens_run`,
   `lens_run_file`, `lens_index`, `lens_search`, `lens_map`, `lens_symbol`,
   `lens_links`, `lens_path`, `lens_recall`, `lens_stats`). The agent
   *chooses* to call them. The headline primitive is a **darkroom** that runs
   agent-supplied code in a subprocess and returns only what the script prints.

2. **Recovery half (active hooks).** Claude Code fires `lens hook claude
   <event>` on five lifecycle events. The hooks capture working state, build a
   resume snapshot at compaction, re-inject it on resume, and (optionally) route
   tool calls toward the savings tools.

## Stack

- **Language:** Rust (edition 2021), `tokio` async runtime for the server/darkroom.
- **MCP:** `rmcp` 1.7 over stdio (`transport-io`).
- **Storage:** SQLite via `rusqlite` (bundled). Three DBs + two flat files under
  the per-project data dir (see `variables.md`). No external database.
- **Parsing:** `tree-sitter` (rust/python/javascript/typescript/go grammars) for
  the code graph; SQLite FTS5 for search.
- **Dashboard:** a dependency-free, hand-rolled HTTP/1.1 server bound to loopback.
- **No network dependency in the core.** The only outbound network calls are: the
  RTK installer (`curl` to GitHub releases), and any fetch the *agent* writes
  inside a darkroom script. The benchmark harness optionally shells `curl` to the
  Anthropic API if `ANTHROPIC_API_KEY` is set; that path is not in the normal
  runtime.

## "Auth" / privilege model

There is no authentication or authorization layer, because there are no remote
principals. The security model is **inherited process privilege**: lens runs
with exactly the rights of whatever launched it (the agent's shell session, or
the user at a terminal). It can read and write anything that user can.

The closest things to authorization decisions are:

- The **PreToolUse routing gate** (`LENS_ROUTING`, default `off`), which can
  deny or rewrite the agent's tool calls. This is a steering control, not a
  security boundary (the agent can ignore nudges; deny only applies to the gated
  tools at the configured level).
- The **wrap/route allowlists** that decide which Bash commands are eligible for
  transparent rewriting. These exist to avoid breaking stateful shells, not to
  restrict the agent.

See `permissions.md` for the full actor x capability matrix.

## Trust boundaries

| # | Boundary | Crossing | Notes |
| :- | :- | :- | :- |
| TB-1 | Agent → host process | `lens_run` / `lens_run_file` run agent-authored code | **No isolation.** Subprocess with `cwd = repo`, full FS + network, only a timeout + `kill_on_drop`. The "darkroom" captures output; it does not contain the code. This is the highest-risk surface. |
| TB-2 | Agent → host shell | `lens wrap -- <cmd>` runs the command via `sh -c` | Same privilege as the agent's own Bash tool; lens adds output offloading, not confinement. |
| TB-3 | Hook → Claude Code | PreToolUse routing returns an authoritative `permissionDecision` (deny/allow) | Can block or transparently rewrite tool calls. Gated by `LENS_ROUTING`; default `off` is a true no-op. |
| TB-4 | Installer → user config | `session install` / `rtk install` edit `~/.claude/settings.json` | Both are scoped to lens-owned entries and reversible; `rtk install` backs up to `*.json.bak`. |
| TB-5 | Installer → network | `rtk install` `curl`s a pinned RTK release and `chmod +x`'s it | Supply-chain surface: pinned by version, **no checksum/signature verification**. See `flows.md` RTK flow. |
| TB-6 | Dashboard → network | `lens dashboard` binds an HTTP server | Loopback (`127.0.0.1:7878`) by default, read-only, **no auth**. `--host` can widen the bind address. |
| TB-7 | Server stdout | MCP JSON-RPC owns stdout; all logs go to stderr | A stray stdout write corrupts the protocol; enforced by convention across the codebase. |

## Known risks / assumptions

- **The darkroom is not a security darkroom** (`src/darkroom/mod.rs`). It spawns a
  real interpreter with full host access. Anything the agent can write, it can
  run. Acceptable because the agent already has Bash; lens does not raise
  privilege, but reviewers must not mistake "darkroom" for "isolation."
- **RTK download is unverified** (`src/rtk/install.rs`): `curl -fsSL` of a
  version-pinned GitHub asset, then `chmod +x`. No hash check. Trust rests on
  GitHub + the pin (`v0.28.2`). Opt-in; a clean install never downloads.
- **Dashboard has no authentication** (`src/obs/dashboard.rs`). Safe on loopback;
  `--host 0.0.0.0` would expose op-log stats (byte counts, tool names, file paths
  in session activity) to the LAN.
- **Session capture stores prompt text and file paths** (`session.db`). User
  prompts, edited file paths, and error strings are persisted in plaintext under
  the project's `.lens/`. No secrets are deliberately captured, but prompts
  can contain anything. See `variables.md` and `flows.md`.
- **Conflict with other lifecycle-hook tools.** lens, Context Mode, and RTK
  all fire on the same events. `session install` refuses to run alongside Context
  Mode; RTK is deferred-to (not auto-disabled) via the `LENS_DEFER_BASH_TO_RTK`
  gate. Running RTK *and* lens wrap without that gate would double-wrap Bash.
- **Routing decisions assume a live server for redirects** (`mcp_ready`). If the
  MCP server's heartbeat (`.lens/server.pid`) is stale, redirect decisions
  fall through to passthrough so the agent is never sent to a dead tool.

## Background work

The MCP server spawns one `tokio` task that re-touches `.lens/server.pid`
every 30 seconds as a liveness heartbeat for the routing gate (`src/main.rs`).
This is the only background loop. There are **no scheduled jobs / cron**, so there
is no `cron.md` (see below).

## Related Documents

- `flows.md` — the security/operations-relevant journeys (darkroom exec, routing,
  session capture, installs) with their trust-boundary crossings and side effects.
- `permissions.md` — actors x capabilities, and what gates each one (there are no
  roles or RLS; this maps the inherited-privilege model honestly).
- `variables.md` — environment configuration, persisted data, and the (near-empty)
  secrets surface, mapped to risk.
- `automation.md` — the routing/hook automation embedded in the agent loop: tool
  surface, steering vs. hard guardrails, output contract, and side effects.
- `tests.md` — verification map (produced separately by `/derive-tests`).

**Conditional docs not produced (capability absent):**

- **emails.md** — No email/notification path exists. lens sends no mail.
- **cron.md** — No scheduled or background jobs beyond the in-process heartbeat
  loop noted above. Nothing to operate, idempotency or auth a scheduler.
- **seo.md** — No public, indexable, or bot-facing routes. The only HTTP surface
  is the loopback-only dashboard.
