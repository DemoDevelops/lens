# Permissions

## The honest baseline

lens has **no roles, no accounts, no authentication, and no row-level
security**. It is a local tool whose security model is **inherited process
privilege**: every operation runs with the rights of whatever launched the
binary. Nothing here grants or checks access in the web-app sense.

So this document does not contain a role matrix. It maps the three real **actors**,
the capabilities each one exercises, and the **gate** (if any) that constrains the
capability. The gates are steering/safety controls, not authorization boundaries.

## Actors

| Actor | How it acts | Effective privilege |
| :- | :- | :- |
| **AI agent** | Calls MCP tools; its tool calls pass through the routing hook | Full host privilege via `lens_run` and Bash. lens does not raise or lower it. |
| **Claude Code harness** | Fires `lens hook claude <event>` on lifecycle events | Runs the hook subprocess with the session's environment |
| **Human operator** | Runs `lens <subcommand>` at a terminal | Their own shell privilege; the install subcommands edit their `~/.claude` config |

## Capability x actor x gate

| Capability | Agent | Claude Code | Operator | Gate that constrains it |
| :- | :-: | :-: | :-: | :- |
| Execute arbitrary code in a subprocess (`lens_run`) | yes | — | — | **None.** Only a timeout + supported-language check. Full FS + network (TB-1). |
| Run a shell command via `lens wrap` | yes (rewritten) | — | yes | Wrap allowlist decides *eligibility for rewriting*, not permission to run. |
| Read/index repo files (`lens_index`, `lens_search`, `lens_map`, graph) | yes | — | yes | Path resolves under repo dir; `.gitignore` is respected by indexing. |
| Retrieve an offloaded blob (`lens_recall`) | yes | — | yes | Must present a valid `retrieve_ref`; unknown ref → error. |
| Deny / rewrite / annotate a tool call (PreToolUse) | — | yes | — | `LENS_ROUTING` level (default `off` = no-op); `mcp_ready` for redirects. |
| Persist session events (prompts, files, errors) | — | yes | — | Skips system messages and empty prompts; startup clears prior events. |
| Edit `~/.claude/settings.json` | — | — | yes | `session install` refuses under Context Mode; changes scoped to lens entries. |
| Download + execute the RTK binary | — | — | yes | Version pin only; **no checksum** (TB-5). Opt-in. |
| Serve the dashboard | — | — | yes | Loopback bind by default; **no auth**; `--host` can widen it (TB-6). |

## Where "scope" is derived

- **Data dir scope.** All persisted state is per-project under `$LENS_DIR`
  (default `<project>/.lens`), resolved identically by the server
  (`server.rs::Forge::new`) and the hook (`session/mod.rs::resolve_data_dir`).
  Cross-project isolation is by directory, not by access control.
- **Path resolution.** `ctx_*` tools resolve relative paths against the repo
  working dir (`server.rs::resolve`); **absolute paths are honored as-is**, so a
  tool call can reference files outside the repo. There is no path jail.
- **Session identity.** Derived from the transcript path stem, else `session_id`,
  else `pid-<n>` (`hook.rs::HookInput::session_id`). Used to scope events and
  throttle markers; it is not an authentication token.

## Code-enforced vs. data-enforced

There is no database-enforced access control (no RLS, no row ownership). Every
constraint is **code-enforced** and advisory:

- The stateful-command and allowlist checks in `routing/mod.rs` exist to avoid
  *breaking* the agent's shell, not to *restrict* it.
- The `mcp_ready` gate prevents redirecting to a dead tool; it does not authorize.
- The Context Mode conflict refusal in `session/install.rs` prevents
  double-firing, not unauthorized installs.

## Reviewer takeaways

- Treat `lens_run` as equivalent to handing the agent a shell. The right
  question is not "what can the agent access" (everything the user can) but
  "should this agent be driving this binary on this machine at all."
- The only persisted-data exposure is `.lens/` (see `variables.md`). Anyone
  with read access to the project directory can read captured prompts and the
  offloaded blob store.
