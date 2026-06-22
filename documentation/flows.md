# Flows

The journeys where a trust boundary is crossed or a side effect lands. Feature
behavior that touches none of those (counting tokens, rendering the dashboard UI)
is out of scope here; see the README for usage.

There is no per-request authorization in this system (no remote principals). The
"authz check" column therefore records the **gate** that decides whether a step
runs and what it is allowed to do, not a role/claim check.

---

## F-1: Darkroom code execution (`lens_run` / `lens_run_file`)

**Actor:** the AI agent (via MCP). **Precondition:** MCP server running.
**Success:** the script's stdout/stderr returns; large stdout is offloaded to the
store and replaced with a head+tail preview plus a `retrieve_ref`.

| Step | Where | Gate / check | Side effects |
| :- | :- | :- | :- |
| 1. Agent calls `lens_run{language, code, timeout_secs, stdin}` | `server.rs::lens_run` | Language must resolve to a known runtime, else a clear error | Appends an `ops.log` start record |
| 2. Write `code` to a temp file (`lens_*.<ext>`) | `darkroom::run_with_args` | none | Temp file created (auto-deletes on drop) |
| 3. Spawn `runtime program <script> [file_arg]` with `cwd = repo_dir` | same | **none — TB-1 crossing.** Full FS + network, `kill_on_drop`, `stdin` piped | Arbitrary host-level effects the script performs |
| 4. Enforce timeout (`timeout_secs.max(1)`) | same | Timeout → `child.kill()`, `timed_out=true`, exit `-1` | Child process killed |
| 5. Capture stdout/stderr; if `stdout > max_inline` offload full to store | same | `LENS_MAX_INLINE` (8 KiB default) | Blob written to `store.db`; `retrieve_ref` minted |
| 6. Return preview/ref; bump savings counters | same | none | `ops.log` finish record; `stats` counters updated |

**Trust boundary:** TB-1 (agent-authored code → host). This is the dominant risk
surface. The step-3 child inherits the user's full privileges. `lens_run_file`
is identical except the target file path is injected as the script's first CLI
arg (`sys.argv[1]` / `process.argv[2]` / `$1`) and the file's byte size is credited
to the savings counters.

**Deny/negative case:** an unsupported language returns
`"unsupported language '<x>'..."`; a missing interpreter returns
`"interpreter '<p>' not found on PATH..."`. Neither runs anything.

---

## F-2: PreToolUse routing decision

**Actor:** Claude Code (fires the hook) on behalf of an agent tool call.
**Precondition:** hooks installed and `LENS_ROUTING != off`.
**Success:** the hook returns a decision JSON that passes through, denies,
rewrites, or annotates the tool call.

| Step | Where | Gate / check | Side effects |
| :- | :- | :- | :- |
| 1. `lens hook claude PreToolUse` reads payload on stdin | `session/hook.rs::handle` | `LENS_ROUTING == off` → return `{}` (no-op, store untouched) | none |
| 2. Build `RouteCtx` (level, `mcp_ready`, bin path, session, `rtk_active`) | same | `mcp_ready` reads `server.pid` freshness (`LENS_MCP_TTL`, default 90s) | none |
| 3. `routing::route(tool, input, ctx)` decides | `routing/mod.rs::route` | per-tool rules below | Throttle marker files under `.lens/throttle/` |
| 4. Serialize decision to PreToolUse hook JSON | `routing::to_hook_json` | none | stdout response to Claude Code |

Per-tool rules (`routing/mod.rs`):

- **WebFetch** → `deny` with guidance to fetch+process in `lens_run`
  (steer/full only; **gated on `mcp_ready`** so a dead server passes through). TB-3.
- **Bash** → if RTK active, passthrough (RTK owns Bash). Else: stateful commands
  (`cd`, `export`, assignments, function defs, backticks) always passthrough;
  structurally-bounded commands (e.g. `git status`, `ls`, `--version`) passthrough;
  curl/wget/build/inline-HTTP commands redirect into `lens_run` (steer, gated on
  `mcp_ready`); remaining read-only allowlisted commands get wrapped to `lens
  wrap -- <cmd>` (wrap/full) or a one-shot nudge (steer). TB-3.
- **Grep / Read** → one-shot `<context_guidance>` nudge per session (steer).
- **Agent / Task** → inject the tool-selection guide into the sub-agent prompt
  (every call; fresh context each time).
- **External MCP tools** (`mcp__<server>__*`, non-lens) → periodic nudge
  (every 10th call).

**Trust boundary:** TB-3. A deny/rewrite is authoritative for that call. The
`mcp_ready` rail (`routing::mcp_redirect`) ensures only redirect-to-MCP decisions
are suppressed when the server is down; nudges and wrap (which use the local CLI,
not the server) still fire.

**Deny/negative case:** level `off` and any unrecognized level → passthrough `{}`.
Empty/malformed stdin → the hook still prints a valid response and exits 0 (a hook
can never block the session).

---

## F-3: Session capture and resume

**Actor:** Claude Code lifecycle events. **Precondition:** hooks installed.
**Success:** working state is persisted and re-injected across a compaction or
resume boundary.

| Event | Where | What it captures / does | Side effects |
| :- | :- | :- | :- |
| `SessionStart` (startup) | `hook.rs::session_start` | Clears prior live events for the project; captures `CLAUDE.md`/`.claude/CLAUDE.md`/`AGENTS.md` as P1 rule events | Writes `session.db`; **clears project events** |
| `UserPromptSubmit` | `hook.rs` | Extracts intent from the prompt (skips system messages) | Inserts events (prompt text persisted) |
| `PostToolUse` | `hook.rs` | Extracts file/error/decision events from tool name+input+response | Inserts events (file paths persisted) |
| `PreCompact` | `hook.rs` | Builds a priority-tiered snapshot within `LENS_SNAPSHOT_BUDGET` (2 KB) | Upserts `session_resume`; bumps compact count |
| `SessionStart` (compact/resume) | `hook.rs::session_start` | Marks resume consumed; indexes events for `lens_search`; returns the snapshot as `additionalContext` | Writes FTS index; injects guide |

**Trust boundary:** none crossed at runtime, but **data-at-rest note:** prompts,
file paths, and error strings land in `session.db` in plaintext under the
project's `.lens/`. See `variables.md`.

**Privacy/side-effect call-out:** `SessionStart(startup)` *deletes* the project's
prior live events (clean-slate semantics). A snapshot persists across compaction
until consumed.

---

## F-4: Install session hooks (`lens session install`)

**Actor:** human operator. **Precondition:** none. **Success:** five hook groups
written to the settings file.

| Step | Where | Gate / check | Side effects |
| :- | :- | :- | :- |
| 1. Resolve settings path | `session/install.rs::settings_path` | `LENS_SETTINGS` else `~/.claude/settings.json` | none |
| 2. Refuse if Context Mode present | `install` → `context_mode_present` | Detects enabled `context-mode` plugin or any `context-mode` hook command | Aborts with an error, no write |
| 3. Strip stale lens entries, append 5 fresh groups | `strip_lens` + loop | Idempotent (re-install yields exactly one group/event) | **Edits `settings.json`** (TB-4) |

**Deny/negative case:** Context Mode detected → hard refuse (would double-fire).
`uninstall` removes only entries whose command contains both `lens` and
`hook claude`, leaving unrelated hooks intact.

---

## F-5: RTK install (`lens rtk install`)

**Actor:** human operator. **Precondition:** `curl` + `tar`/`unzip` present.
**Success:** pinned RTK binary at `~/.lens/bin/rtk`, hook registered in the
active Claude config dir.

| Step | Where | Gate / check | Side effects |
| :- | :- | :- | :- |
| 1. Resolve version + target triple | `rtk/install.rs` | `LENS_RTK_VERSION` / `LENS_RTK_TARGET` overrides; pin `v0.28.2` | none |
| 2. Skip if managed binary already matches | `install` | `rtk --version` contains the pinned digits (idempotent) | none |
| 3. `curl -fsSL <github asset>` → temp → extract → `chmod +x` | `download_and_extract` | **No checksum/signature** — TB-5 | Writes + executes `~/.lens/bin/rtk` |
| 4. Verify `rtk --version` matches the pin | `install` | Mismatch → fatal | none |
| 5. Generate hook script, inject PATH guards, patch settings | `register_hook` | Hook-registration failure is a warning, not fatal | Writes `hooks/lens-rtk-rewrite.sh`, edits settings (backs up `*.json.bak`) |

**Trust boundary:** TB-5 (network → executable) and TB-4 (settings edit). The
downloaded artifact runs with the user's privileges thereafter via the Bash hook.

**Deny/negative case:** unsupported platform → error before any download; `curl`
non-zero → abort and clean up the temp archive; version mismatch → abort.

---

## F-6: Dashboard serve (`lens dashboard`)

**Actor:** human operator. **Success:** a local web page polling `/api/stats`.

| Step | Where | Gate / check | Side effects |
| :- | :- | :- | :- |
| 1. Bind `host:port` (default `127.0.0.1:7878`) | `obs/dashboard.rs::run_cli` | `--host`/`--port` flags; **no auth** | Opens a listening socket (TB-6) |
| 2. Serve `/` (static HTML) and `/api/stats` (op-log aggregate) | `route` | Read-only; only `GET`; 404 otherwise | Reads `.lens/` op log + stores |

**Trust boundary:** TB-6. Loopback is safe; `--host 0.0.0.0` exposes op-log
stats (tool names, byte counts, session activity including captured file paths) to
the network with no authentication.
