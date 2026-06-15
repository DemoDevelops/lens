# ctxforge

A Rust tool for Claude Code that cuts an AI coding agent's token use and
survives context compaction. It has two halves, installed independently:

1. **An MCP server** (token savings) that fuses four deterministic primitives: a
   **sandbox** that runs code in a subprocess and returns only what the script
   prints (not the raw data it processed), an **FTS5 search index** over your
   files, a **tree-sitter code graph** of symbols and relationships, and a
   **reversible compression store** that offloads large results and hands back a
   compact view plus a retrieval ref. The sandbox is where most of the savings
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
  `ctx_execute`:
  - `python3` (python)
  - `node` and `npx` (javascript / typescript; TypeScript runs via `tsx`)
  - `bash` (bash)
  - `ruby` (ruby)
  - `go` (go)

If a runtime is missing, `ctx_execute` returns a clear "install X" error rather
than failing silently.

## Build

```
git clone <repo> && cd ctxforge
cargo build --release
```

The binary is at `target/release/ctxforge`.

## Install into Claude Code

```
claude mcp add ctxforge -- /absolute/path/to/target/release/ctxforge
```

…or add it manually to your `.mcp.json` / `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "ctxforge": {
      "command": "/absolute/path/to/target/release/ctxforge"
    }
  }
}
```

After adding, restart Claude Code and verify with a quick `ctx_stats` call.

## Install session continuity (optional)

The recovery half is separate from the MCP server: it registers five lifecycle
hooks in Claude Code's `settings.json`, each invoking the **same** binary as
`ctxforge hook claude <event>`. Run it from the built binary so the absolute
path is embedded correctly:

```
./target/release/ctxforge session install
```

This adds hooks for `PreToolUse`, `PostToolUse`, `UserPromptSubmit`,
`PreCompact`, and `SessionStart` to `~/.claude/settings.json`. Then:

```
ctxforge session status      # show installed hooks + backing-store health
ctxforge session uninstall   # remove only ctxforge's entries, leave others intact
```

**Conflict guard.** ctxforge and Context Mode both fire on the same lifecycle
events, so `install` **refuses** to run while Context Mode is enabled — uninstall
it first (`/plugin uninstall context-mode`). If you also run RTK on these events,
disable it too (it isn't auto-detected, but it will double-fire). `install` is
idempotent and `uninstall` removes only ctxforge-owned entries, so unrelated
hooks are never touched.

Install is per-user by default (`~/.claude/settings.json`); point it elsewhere
with `CTXFORGE_SETTINGS=/path/to/settings.json`.

## Tool reference

| Tool | Description | Example input |
| :- | :- | :- |
| `ctx_execute` | Run code in a sandbox; only stdout/stderr return, not the data the script read. Large output is offloaded. | `{"language":"python","code":"print(sum(len(l) for l in open('big.log')))"}` |
| `ctx_index` | Index a file/dir into FTS5 (respects `.gitignore`). | `{"path":"src","recursive":true}` |
| `ctx_search` | BM25 search, multiple queries per call. | `{"queries":["auth","retry"],"limit_per_query":5}` |
| `ctx_discover` | Parse the repo into a symbol/relationship graph. | `{"path":".","languages":["rust"]}` |
| `graph_query` | Find symbols by name (+ optional kind) with their connections. | `{"name":"handle","kind":"function"}` |
| `graph_neighbors` | Local subgraph around a node id. | `{"node_id":"<id>","depth":2}` |
| `graph_path` | Shortest path between two symbols. | `{"from":"main","to":"helper"}` |
| `ctx_retrieve` | Recover a full blob from a `retrieve_ref`. | `{"ref":"<hash>"}` |
| `ctx_stats` | Token-savings counters and index/graph sizes. | `{}` |

## Recommended workflow

1. Run `ctx_discover` **once** per repo to build the structural graph.
2. Lean on `ctx_execute` for anything data-heavy — log parsing, scanning large
   files, transforming data, computing aggregates. The script reads the data;
   only what it prints comes back to context. This is the biggest saver.
3. Use `ctx_index` + `ctx_search` for lookup ("where is X mentioned").
4. Use `graph_query` / `graph_neighbors` / `graph_path` for structure ("what
   calls X", "how does A reach B") instead of reading many files.
5. When a tool returns a `retrieve_ref` (large output / large subgraph), call
   `ctx_retrieve` only if you actually need the full version.
6. Check `ctx_stats` to see measured savings.

## Configuration

| Env var | Default | Meaning |
| :- | :- | :- |
| `CTXFORGE_DIR` | `<project>/.ctxforge` | Where `index.db`, `store.db`, and `graph.json` live. |
| `CTXFORGE_MAX_INLINE` | `8192` | Stdout/subgraph byte threshold before offloading to the store. |
| `CTXFORGE_SNAPSHOT_BUDGET` | `2048` | Byte budget for the session-resume snapshot (recovery half). |
| `CTXFORGE_SETTINGS` | `~/.claude/settings.json` | Settings file `ctxforge session install` writes its hooks into. |
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
accuracy method is **task-based rather than GSM8K-style** because ctxforge sits
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
