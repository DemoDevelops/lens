# Variables: configuration, persisted data, and secrets

## Secrets surface: essentially none

lens has **no API keys, tokens, or credentials in its runtime**. There is no
`.env`, no secret manager, nothing bundled client-side (there is no client). The
only credential that appears anywhere in the repo is `ANTHROPIC_API_KEY`, read
*optionally* by the benchmark harness (`bench_accuracy`) to hit the real model;
unset, it falls back to a mock. That path is developer tooling, not the shipped
binary.

Confirmation: no secret is shipped, embedded, or required to run the MCP server,
the hooks, the wrapper, or the dashboard.

## Environment configuration

All behavior is driven by environment variables; none holds a secret. Scope is
"process" (read by the binary at startup) throughout, since there is no client.

| Name | Default | Used by | Source | Risk if mis-set |
| :- | :- | :- | :- | :- |
| `LENS_DIR` | `<project>/.lens` | server + hooks | operator/launcher | Wrong dir splits state or points at a shared location (cross-project leakage of captured data). |
| `LENS_MAX_INLINE` | `8192` | darkroom / graph compaction | operator | Too high = large raw output floods context (the thing this tool prevents); too low = excess offloading. Not a safety risk. |
| `LENS_ROUTING` | `off` | PreToolUse hook | operator | `steer`/`wrap`/`full` let the hook deny/rewrite tool calls. `off` is a guaranteed no-op. Behavioral, not a secret. |
| `LENS_ROUTING_MCP` | *(auto)* | routing gate | operator | Forces the MCP-ready gate `up`/`down`, overriding the heartbeat. `up` while the server is down sends the agent to a dead tool. |
| `LENS_MCP_TTL` | `90` (s) | routing gate | operator | Freshness window for `server.pid`. Too low = routing flaps to passthrough. |
| `LENS_SNAPSHOT_BUDGET` | `2048` (bytes) | PreCompact snapshot | operator | Larger snapshots cost more context on resume; smaller drop recovery detail. |
| `LENS_SETTINGS` | `~/.claude/settings.json` | `session install` | operator | Points the hook installer at a different settings file. Mis-set could write hooks to the wrong place. |
| `LENS_HOME` | `~/.lens` | RTK manage | operator | Home for the managed RTK binary (`<home>/bin/rtk`). |
| `LENS_DEFER_BASH_TO_RTK` | *(auto)* | routing | operator | `1` forces lens to defer Bash to RTK; `0` forces normal routing. Wrong value can double-wrap or un-wrap Bash. |
| `LENS_RTK_VERSION` | `v0.28.2` | `rtk install` | operator | Which RTK release is downloaded. Pointing at an untrusted version widens TB-5. |
| `LENS_RTK_TARGET` | *(auto)* | `rtk install` | operator | Override the platform target triple. |
| `LENS_EXPLAIN` / `--explain` | unset | op log | operator | Enables a per-op `explain.log` trace. Verbose, local only. |
| `RUST_LOG` | `info` | logging | operator | Log level. **Logs go to stderr; stdout is the MCP channel** (`src/main.rs`). |
| `ANTHROPIC_API_KEY` | unset | `bench_accuracy` only | operator | The one real credential; benchmark-only, never read by the runtime binary. Treat as a normal secret if you run real-model benchmarks. |
| `CLAUDE_PROJECT_DIR` | unset | hook project resolution | Claude Code | Fallback for the project path when `cwd` is absent. |
| `CLAUDE_CONFIG_DIR` | `~/.claude` | RTK hook registration | Claude Code | Which config dir the RTK hook is written into. |

## Persisted data (the real "PII/leak surface")

Everything lives under the per-project data dir (`LENS_DIR`, default
`<project>/.lens/`). No remote storage. Anyone with filesystem read access to
the project can read all of it.

| File | Engine | Contents | Sensitivity |
| :- | :- | :- | :- |
| `index.db` | SQLite FTS5 (`chunks`) | Indexed snippets of repo file content (respects `.gitignore`) + indexed session events | Mirror of source you already have; session-event chunks include prompt/file text. |
| `store.db` | SQLite (`blobs`, `stats`) | **Offloaded tool/command output** keyed by blake3 ref, plus savings counters | Can hold arbitrary file content the agent processed (whatever a script printed or a wrapped command emitted). Reversible via `lens_recall`. |
| `session.db` | SQLite (`session_events`, `session_meta`, `session_resume`) | Captured **user prompts**, edited **file paths**, **error strings**, decisions, and resume snapshots, in plaintext | Highest-sensitivity store. Prompts can contain anything the user typed. |
| `ops.log` (+ `ops.log.1`) | JSONL, append-only, ~10 MB rotation | One record per op: timestamp, session id, pid, tool, `input_summary`, byte counts, `tokens_saved_est`, `store_ref`, outcome, note | Metadata only. **`input_summary` for the darkroom is `{language, code_bytes}` — the code itself is not logged.** `bash_wrap` logs a truncated command summary. |
| `explain.log` | text (opt-in) | Per-op human-readable trace when `LENS_EXPLAIN`/`--explain` is set | Local diagnostics. |
| `graph.json` | JSON | Tree-sitter symbol/relationship graph for the repo | Structural metadata about your code. |
| `server.pid` | text | Current server PID; mtime is the routing heartbeat | Not sensitive. |
| `throttle/*.count`, `throttle/*.once` | text markers | Per-session nudge throttle state | Not sensitive. |

Outside the data dir, `rtk install` writes the managed binary to
`~/.lens/bin/rtk` and a hook script to the Claude config dir's `hooks/`, and
backs up the edited `settings.json` to `*.json.bak`.

## Pre-go-live checklist

- [ ] Confirm `.lens/` is git-ignored (it holds prompts and offloaded output;
      do not commit it).
- [ ] Decide whether `session.db` plaintext prompt capture is acceptable for the
      environment; there is no redaction layer.
- [ ] Keep `LENS_ROUTING` at `off` unless you have validated the routing
      behavior, and never set `LENS_ROUTING_MCP=up` blindly.
- [ ] Run the dashboard on loopback only; never pass `--host 0.0.0.0` on an
      untrusted network (no auth on `/api/stats`).
- [ ] If using RTK, pin and trust `LENS_RTK_VERSION`; the download is
      unverified (TB-5).
- [ ] Treat `ANTHROPIC_API_KEY` as a real secret if you run real-model benchmarks;
      it is the only credential in the repo.
