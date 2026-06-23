# lens

A Rust tool for Claude Code that cuts an AI coding agent's token use and
survives context compaction. It has two halves, installed independently:

1. **An MCP server** (token savings) that fuses four deterministic primitives: a
   **darkroom** that runs code in a subprocess and returns only what the script
   prints (not the raw data it processed), an **FTS5 search index** over your
   files, a **tree-sitter code graph** of symbols and relationships, and a
   **reversible compression store** that offloads large results and hands back a
   compact view plus a retrieval ref. The darkroom is where most of the savings
   come from; the other layers are additive.

2. **Session-continuity hooks** (recovery) — a drop-in replacement for the
   **Context Mode** Claude Code plugin. It captures lifecycle events, builds a
   priority-tiered resume snapshot when Claude Code compacts the conversation,
   and re-injects a Session Guide on resume so working state (open files, the
   task, the last error, decisions) survives the boundary. In the benchmarks it
   recovers **≥ Context Mode at ~20× lower token cost** (see
   [BENCHMARKS.md](BENCHMARKS.md)).

You can install either half alone. The savings half is a passive MCP server the
agent calls; the recovery half is active hooks Claude Code fires on its own.

## Prerequisites

- **Rust** stable, via [rustup](https://rustup.rs).
- Optional language runtimes — only needed if you run that language through
  `lens_run`:
  - `python3` (python)
  - `node` and `npx` (javascript / typescript; TypeScript runs via `tsx`)
  - `bash` (bash)
  - `ruby` (ruby)
  - `go` (go)

If a runtime is missing, `lens_run` returns a clear "install X" error rather
than failing silently.

## Build

```
git clone <repo> && cd lens
cargo build --release
```

The binary is at `target/release/lens`.

## Install into Claude Code

```
claude mcp add lens -- /absolute/path/to/target/release/lens
```

…or add it manually to your `.mcp.json` / `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "lens": {
      "command": "/absolute/path/to/target/release/lens"
    }
  }
}
```

After adding, restart Claude Code and verify with a quick `lens_stats` call.

## Install session continuity (optional)

The recovery half is separate from the MCP server: it registers five lifecycle
hooks in Claude Code's `settings.json`, each invoking the **same** binary as
`lens hook claude <event>`. Run it from the built binary so the absolute
path is embedded correctly:

```
./target/release/lens session install
```

This adds hooks for `PreToolUse`, `PostToolUse`, `UserPromptSubmit`,
`PreCompact`, and `SessionStart` to `~/.claude/settings.json`. Then:

```
lens session status      # show installed hooks + backing-store health
lens session uninstall   # remove only lens's entries, leave others intact
```

**Conflict guard.** lens and Context Mode both fire on the same lifecycle
events, so `install` **refuses** to run while Context Mode is enabled — uninstall
it first (`/plugin uninstall context-mode`). If you also run RTK on these events,
disable it too (it isn't auto-detected, but it will double-fire). `install` is
idempotent and `uninstall` removes only lens-owned entries, so unrelated
hooks are never touched.

**Choosing the config folder.** Install targets, in precedence order:
`--config-dir <dir>` (or `--settings <file>`), then `LENS_SETTINGS`, then
`$CLAUDE_CONFIG_DIR` (the dir the running Claude Code reads), then `~/.claude`.
So with separate accounts (e.g. a personal `~/.claude-personal` and a work
`~/.claude`):

```
lens session install --config-dir ~/.claude-personal   # personal
lens session install --config-dir ~/.claude            # work
```

`status`/`uninstall` take the same flag, so point them at the same folder you
installed into. Run from inside a Claude session and `$CLAUDE_CONFIG_DIR`
already selects the right account, no flag needed.

## Tool reference

| Tool | Description | Example input |
| :- | :- | :- |
| `lens_run` | Run code (python/js/ts/bash/ruby/go) in a darkroom subprocess; only stdout/stderr returns to context, not the data the script read. Large output is offloaded with a recall ref. | `{"language":"python","code":"print(sum(len(l) for l in open('big.log')))"}` |
| `lens_run_file` | Analyze one file in the darkroom; your `code` receives the file path as its first CLI arg (`sys.argv[1]` / `process.argv[2]` / `$1`). Only what it prints returns; the file's bytes stay out of context. | `{"path":"big.log","language":"python","code":"import sys;print(sum(1 for _ in open(sys.argv[1])))"}` |
| `lens_index` | Build an FTS5 full-text index over a file or directory (respects `.gitignore`). Returns files indexed and chunk count; prerequisite for `lens_search`. | `{"path":"src","recursive":true}` |
| `lens_search` | Run one or more BM25-ranked full-text queries in a single call. Returns the top snippets per query with path and relevance score; answers "where is X mentioned". | `{"queries":["auth","retry"],"limit_per_query":5}` |
| `lens_map` | Parse the whole repo with tree-sitter into a symbol graph (functions, types, modules) and their relationships (calls, imports, contains). Run once before the other graph tools. | `{"path":".","languages":["rust"]}` |
| `lens_symbol` | Find graph symbols by name substring (+ optional kind) and return each with its immediate connections: where a symbol lives and what directly touches it. | `{"name":"handle","kind":"function"}` |
| `lens_find` | Find symbols by a natural-language query, ranked lexically by word overlap with symbol names. Use when you know what a symbol does but not its exact name. | `{"query":"retry with backoff","limit":20}` |
| `lens_links` | Return the local subgraph within N hops of a node id: a symbol's neighborhood or blast radius at a chosen depth. | `{"node_id":"<id>","depth":2}` |
| `lens_path` | Find the shortest path between two symbols via BFS over graph edges: how A reaches B through the call/import chain. | `{"from":"main","to":"helper"}` |
| `lens_recall` | Recover the full blob behind a `retrieve_ref` returned by another tool, reversing any truncation or offloading. | `{"ref":"<hash>"}` |
| `lens_stats` | Report darkroom usage, estimated tokens saved, and current index/graph sizes for this repo. | `{}` |

## Recommended workflow

1. Run `lens_map` **once** per repo to build the structural graph.
2. Lean on `lens_run` for anything data-heavy — log parsing, scanning large
   files, transforming data, computing aggregates. The script reads the data;
   only what it prints comes back to context. This is the biggest saver.
3. Use `lens_index` + `lens_search` for lookup ("where is X mentioned").
4. Use `lens_symbol` / `lens_links` / `lens_path` for structure ("what
   calls X", "how does A reach B") instead of reading many files.
5. When a tool returns a `retrieve_ref` (large output / large subgraph), call
   `lens_recall` only if you actually need the full version.
6. Check `lens_stats` to see measured savings.

## Auto-routing (opt-in)

The MCP tools only save tokens when the model *chooses* to call them. Auto-routing
adds **interception at the hook layer** so savings happen automatically — the
`PreToolUse` hook can deny, transparently rewrite, or nudge a built-in tool call,
and `SessionStart` injects a short tool-selection guide.

It is gated by `LENS_ROUTING` and **defaults to `off` (a true no-op:
`PreToolUse` returns `{}`, identical to having no routing at all).** The four
levels are the rollout — flip the level to widen the behavior:

| `LENS_ROUTING` | Behavior |
| :- | :- |
| `off` (default) | Nothing. `PreToolUse` returns `{}`. |
| `steer` | `WebFetch` → **deny** (fetch+process via `lens_run` instead); periodic per-tool guidance for `Bash`/`Grep`; inject the authoritative `SessionStart` tool-selection directive (`<context_window_protection>`: the *why*, a graph-first hierarchy, a nuanced when-not-to-use, and a deferred-tool `ToolSearch` bootstrap). No rewriting. |
| `wrap` | Transparently rewrite allowlisted read-only `Bash` commands to `lens wrap -- <cmd>` (deterministic savings, no reliance on model compliance). No deny/nudges. |
| `full` | `steer` + `wrap` together, plus periodic `Read`→`lens_run_file` guidance. |

**Bash wrap allowlist (read-only, high-output only).** Wrapping is restricted to
commands whose every pipeline segment leads with an allowlisted program:
`find`, `cat`, `ls`, `tree`, `rg`/`grep`/`egrep`/`fgrep`, `tail`, `head`, `wc`,
`sort`, `uniq`, `nl`, `curl`, `wget`, `gradle`/`gradlew`/`mvn`/`sbt`,
`pytest`/`jest`/`vitest`, and **subcommand-aware** `git` (`log`/`diff`/`show`/
`status`/`blame`/…), `cargo` (`test`/`build`/`check`/…), `go` (`test`/`build`/…),
`npm`/`yarn`/`pnpm` (`test`/`run`/`build`/…). So `git log` is wrapped but
`git commit` is not.

**Stateful commands are never wrapped.** Anything that mutates persistent shell
state — `cd`, `export`, `source`, assignments (`FOO=bar …`), `alias`, `eval`,
function defs — or any `&&`/`||`/`;`/`|` chain containing such a segment passes
through untouched. Wrapping them in a subshell would silently break the
persistent-shell cwd/env that Claude Code relies on.

**Lossless + observable.** `lens wrap` runs the command via `sh -c`
(preserving exit code and streaming stderr), offloads large stdout to the
reversible store, and returns a head+tail preview + a `retrieve_ref` — recover
the full output byte-for-byte with `lens_recall` (or `lens verify --roundtrip
<ref>`). Every wrap writes one `ops.log` record (`tool: bash_wrap`), so the
savings show up in `lens stats` and on the dashboard.

**Safety rails.** Routing engages only while the MCP server is reachable (a
liveness heartbeat at `<data_dir>/server.pid`); if it is down, every decision
falls through to passthrough. Per-tool guidance re-injects on a **periodic cadence**
(every Nth call per session per tool), so the directive stays live as context grows
rather than firing once and being forgotten.
You can run the wrapper directly: `lens wrap -- find . -name '*.rs'`.

## RTK shell savings (opt-in)

lens's MCP tools and the Bash wrap above only compress what gets routed
through lens. The shell commands Claude Code runs most (every `cd "<proj>" &&
…` chain) slip past, because wrapping a stateful chain would break the persistent
shell. [RTK](https://github.com/rtk-ai/rtk) (Rust Token Killer, Apache-2.0) is
built for exactly that case: it rewrites commands *per segment* through its own
Claude Code hook and ships per-command compactors, so it fires constantly where
lens's wrap cannot.

Rather than re-author RTK, lens adopts the **headroom pattern**
(`chopratejas/headroom`): ship the prebuilt RTK binary, let RTK own Bash, and
surface RTK's *own* measured savings. The division of labor:

- **RTK owns Bash command rewriting.** lens installs the pinned RTK binary and
  lets RTK's hook rewrite shell commands. While RTK is active, lens's
  `PreToolUse` passes `Bash` straight through, so the two hooks never double-wrap.
  Non-Bash routing (WebFetch-deny, Read/Grep nudges) is unaffected.
- **lens surfaces RTK's savings.** `lens rtk sync` reads `rtk gain --format
  json` and appends the delta to `ops.log` as an `rtk_shell` op whose
  `tokens_saved_est` is RTK's own measured `total_saved` (never a lens
  re-estimate). `lens stats` and the dashboard then show an **RTK shell
  savings** plane next to lens's MCP-tool savings.
- **lens keeps its lane.** Darkroom execution (`lens_run`), FTS5 search,
  the code graph, session continuity, and reversible compression stay the
  downstream context tools, unchanged.

Fully opt-in and additive: with no RTK installed every path here is a no-op and
existing behavior is identical.

```sh
lens rtk install     # download the pinned RTK binary to ~/.lens/bin/rtk
                         #   and register RTK's hook in $CLAUDE_CONFIG_DIR (else ~/.claude)
lens rtk status      # installed? which version? hook registered? + rtk gain summary
lens rtk sync        # fold RTK's measured savings delta into ops.log (rtk_shell op)
lens rtk uninstall   # remove RTK's Claude hook (rtk init --global --uninstall)
```

`install` is version-pinned (RTK `v0.28.2`, the headroom pin) and idempotent;
re-running it re-registers the hook without re-downloading. The hook is registered
in the dir your Claude Code actually reads, `$CLAUDE_CONFIG_DIR` (else `~/.claude`):
since `rtk init` itself only writes `~/.claude`, lens patches that dir's
`settings.json` and copies the hook script into its `hooks/` so the hook is
self-contained. Run `lens rtk sync` periodically to keep the dashboard current. The dashboard (`lens
dashboard`) then renders three planes: lens MCP tool savings, **RTK shell
savings**, and session activity.

**Activating live rewriting.** `lens rtk install` lands the binary at
`~/.lens/bin/rtk` and registers RTK's hook, but RTK's `rtk-rewrite.sh` finds
`rtk` via `PATH` and needs `jq`. So to have RTK actually rewrite shell commands
going forward, add `~/.lens/bin` to your `PATH` and install `jq`. `lens
rtk status` reports whether live rewriting is active or what is missing.
(`lens rtk sync` and the dashboard work regardless, since lens calls the
binary by absolute path.)

> Tests are network-free: a stub `rtk` placed on `LENS_HOME/bin` answers
> `--version` / `gain --format json`, so `cargo test` never downloads. The real
> download is exercised on-machine only.

## Configuration

| Env var | Default | Meaning |
| :- | :- | :- |
| `LENS_DIR` | `<project>/.lens` | Where `index.db`, `store.db`, and `graph.json` live. |
| `LENS_MAX_INLINE` | `8192` | Stdout/subgraph byte threshold before offloading to the store. |
| `LENS_ROUTING` | `off` | Auto-routing level: `off` \| `steer` \| `wrap` \| `full` (see above). |
| `LENS_ROUTING_MCP` | *(auto)* | Override the MCP-ready guard: `up` forces routing on, `down` forces passthrough. Default reads the `server.pid` heartbeat. |
| `LENS_MCP_TTL` | `90` | Seconds the `server.pid` heartbeat stays "fresh" for the routing guard. |
| `LENS_SNAPSHOT_BUDGET` | `2048` | Byte budget for the session-resume snapshot (recovery half). |
| `LENS_SETTINGS` | `~/.claude/settings.json` | Settings file `lens session install` writes its hooks into. |
| `LENS_HOME` | `~/.lens` | Global home for the managed RTK binary (`<home>/bin/rtk`). Distinct from the per-project `LENS_DIR`. |
| `LENS_DEFER_BASH_TO_RTK` | *(auto)* | Override the "RTK owns Bash" gate: `1` forces lens to defer `Bash`, `0` forces normal routing. Default detects the RTK binary + its registered hook. |
| `LENS_RTK_VERSION` | `v0.28.2` | RTK release that `lens rtk install` downloads. |
| `RUST_LOG` | `info` | Log level (logs go to **stderr**; stdout is the MCP channel). |

## Development

```
cargo test          # unit + integration + e2e (spawns the real binary)
cargo clippy -- -D warnings
```

To add a language to discovery, add one `LangSpec` (grammar + three queries)
plus a scope-kinds entry in `src/discovery/extract.rs`. See **DECISIONS.md**
("Adding a language") for the exact steps.

## Benchmarks

[BENCHMARKS.md](BENCHMARKS.md) is the results-first headline doc (realistic-scale
savings, accuracy, and the session-recovery head-to-head vs Context Mode);
[BENCHMARKS_APPENDIX.md](BENCHMARKS_APPENDIX.md) is the full audit trail (scale
curves, mechanism classification, the discovery-regression investigation), and
[benchmarks/README.md](benchmarks/README.md) covers methodology. Savings are
measured headroom-style (token-in vs token-out, segmented by mechanism); the
accuracy method is **task-based rather than GSM8K-style** because lens sits
beside the prompt path (as an MCP tool the agent chooses to call), not inside it,
so the faithful question is whether tasks stay correct when the agent uses the
tools instead of reading raw files. Both docs are generated — never hand-edited.

```sh
cargo run --bin bench_savings    # savings table (no credentials needed)
cargo run --bin bench_accuracy   # accuracy harness (real model if ANTHROPIC_API_KEY set, else mock)
cargo run --bin bench_recovery   # session-recovery head-to-head vs Context Mode
cargo run --bin bench_report     # regenerate BENCHMARKS.md + BENCHMARKS_APPENDIX.md
```

## License

Apache-2.0. See [LICENSE](LICENSE).
