# lens

*Like the glass it's named for, lens focuses: your script runs in a darkroom, and only the developed image comes back, never the raw light.*

**A context optimizer for Claude Code: same answers, a third fewer tokens, faster.** lens gives your agent a *darkroom* (a subprocess where scripts run and only stdout returns), a local full-text index, and a directed code-symbol graph, so it stops re-paying for raw bytes on every turn and answers structure questions with lookups instead of file piles.

Measured end-to-end, tools live, agent free to work however it wants (15 repo-investigation tasks, 3 runs each, `claude-sonnet-5`, vs vanilla out-of-box Claude Code):

| | with lens | vanilla | delta |
| --- | ---: | ---: | ---: |
| **tokens per task** | 296k | 460k | **-36%** |
| **accuracy** | 89% | 89% | even |
| **time to answer** | 24.9s | 27.2s | **-8%** |

Where the work is hardest, the gap is widest: full-text search tasks run at **-48% tokens and -56% time** at equal accuracy, and call-path tracing at **-76% tokens and -77% time**. Full methodology, per-function tables, and scale curves in [BENCHMARKS.md](BENCHMARKS.md).

## Why it saves

**Bytes stay out of context.** Mechanism-level savings against a fixed corpus (byte counts, a close proxy for tokens):

| Workload | Mechanism | Before | After | Saved |
| --- | --- | ---: | ---: | ---: |
| Code search | full-text index | 160,230 | 10,020 | **94-99%** |
| Log debugging | darkroom | 7,210 | 517 | **93%** |
| Issue triage | compression | 94,195 | 31,323 | **~67%** |

Read each percentage as "what this mechanism does to a workload of this shape," not a guaranteed figure for your repo.

**Better context beats more context.** Given the same question and a fixed context budget, a model answering from lens-built context beats one answering from raw file slices by 38 to 62 accuracy points, across all four mechanisms (data analysis 75% vs 12%, code structure 92% vs 38%, search 69% vs 31%, file skeletons 75% vs 12%; `claude-sonnet-5`, headless, tools off).

**The agent actually uses it.** lens's routing layer steers organic tool choice: with rails on, agents engage the lens toolchain on 78% of eligible calls vs 60% with rails off, no prompt changes required.

## Install

One line downloads the binary, registers the MCP server, installs the session hooks and the `/dashboard` command, installs RTK shell compression, sets routing, and prints a verification report:

```sh
curl -fsSL https://raw.githubusercontent.com/DemoDevelops/lens/master/install.sh | sh
```

Restart Claude Code, then verify with the `lens_stats` tool. Supported: macOS (arm64, x64), Linux (x64, arm64).

Routing defaults to the aggressive `full` level: WebFetch and noisy commands are redirected into the darkroom, plus RTK shell compression. For nudges-only (encourages the lens tools, never denies WebFetch or rewrites commands), install with `… | LENS_ROUTING=nudge sh`, or change it anytime with `lens setup --routing <off|nudge|steer|wrap|full>`.

**From source** ([Rust](https://rustup.rs) stable; optional `python3`/`node`/`ruby`/`go`, only to run those languages through `lens_run`):

```sh
git clone https://github.com/DemoDevelops/lens && cd lens
cargo build --release
./target/release/lens setup
```

`lens setup` does the same wiring from a binary you built (copies it to `~/.local/bin`, registers the MCP server, installs the hooks + `/dashboard` + RTK, sets routing). Target a specific config dir with `lens setup --config-dir <dir>`.

Update later with `lens update`: it checks the public GitHub release (no auth), downloads the matching binary, and re-applies setup (preserving your routing level). lens also drops a one-line heads-up into a session when a newer release is out; silence it with `LENS_NO_UPDATE_CHECK=1`.

## Tools

| Tool | What it does |
| :- | :- |
| `lens_run` | Run a script in a darkroom; only stdout returns to context. Best for log parsing, data aggregation, large file analysis. |
| `lens_run_file` | Same as `lens_run` but receives a file path as its first argument. |
| `lens_skeleton` | Show a source file's structure: signatures + nesting with line numbers, bodies elided to `…`. Full text recoverable via `lens_recall`. |
| `lens_index` | Index a directory for full-text search. Run once per repo. |
| `lens_search` | BM25F search with a proximity rerank. Every hit cites its match line and lists the definitions its chunk contains, so "which function does X" is often answered by the hit itself. |
| `lens_grep_ast` | Structural search via a tree-sitter query: matches syntax, not text (real `.unwrap()` calls, not comments). |
| `lens_map` | Parse the repo into a symbol graph (functions, types, modules, relationships). Run once per repo. |
| `lens_overview` | Token-budgeted map (~2k tokens) of the repo's symbols: a knapsack packs the highest-importance subset that fits, so load-bearing hubs survive. Optional query focus. |
| `lens_symbol` | Find symbols by name and see their immediate connections, labeled prod/test/bench so test callers are excludable without opening files. |
| `lens_find` | Find symbols by natural-language description. |
| `lens_links` | Expand a symbol's neighborhood N hops out, directed: fan-in (callers), fan-out (callees), or both. |
| `lens_path` | Shortest call/import path between two symbols over directed edges: answers "does A reach B", not just "are they connected". |
| `lens_memory_record` | Record durable project memory (decisions, constraints, rules) that survives across sessions. |
| `lens_memory_query` | Query that memory, ranked by relevance. |
| `lens_recall` | Recover the full content behind a `retrieve_ref` from any other tool. |
| `lens_stats` | Show token savings and index/graph sizes for this session. |

### Examples

**Darkroom.** Run code; the data stays out of context:

```python
# lens_run: count log levels in a 50k-line build log; only the dict returns
import collections, re
c = collections.Counter()
for line in open("build.log"):
    m = re.search(r"\b(ERROR|WARN|INFO)\b", line)
    if m: c[m.group(1)] += 1
print(dict(c))                      # → {'ERROR': 12, 'WARN': 73, 'INFO': 4120}
```

```python
# lens_run_file: the file path arrives as argv[1]; print only the shape
import sys, csv
rows = list(csv.DictReader(open(sys.argv[1])))
print(len(rows), "rows;", "cols:", list(rows[0])[:5])   # the CSV never enters context
```

**Search.** Full-text over the repo, ranked snippets instead of whole files:

```text
lens_index(path=".")                            # once per repo
lens_search(queries=["where is the routing level parsed",
                     "deny WebFetch under steering"])
# → src/routing/mod.rs:52    Level::parse(s) { "nudge" => …, "full" => … }
#   src/routing/mod.rs:193   WEBFETCH_REASON: "fetch+process web content in the darkroom…"
```

**Graph.** Structure and relationships without reading files:

```text
lens_map(path=".")                              # once per repo → .lens/graph.json
lens_symbol(name="install")                     # find a symbol + its callers/callees
lens_find(query="dedup rtk hooks")              # NL → rtk::install::dedup_rtk_hooks
lens_path(from="run_cli", to="purge_context_mode")   # does run_cli reach it? (directed)
lens_links(node_id="<id>", depth=2, direction="callers")   # transitive fan-in
```

**Recover & observe.**

```text
lens_recall(ref="<retrieve_ref from a truncated result>")   # full content, losslessly
lens_stats()                                    # tokens saved + index/graph sizes this session
```

## Dashboard

A local, read-only view of what lens is saving you: the op log, token savings, applied value, and session activity, rendered live. Two front-ends over the same snapshot.

**In Claude Code:** `/dashboard` launches the web view for the current repo as a background process and prints its URL. It reads `<cwd>/.lens`, so run it from the repo whose savings you want to see.

**Web** (`lens dashboard`): serves on `http://127.0.0.1:7878` (`--port` to change). Live `$` saved and tokens, throughput sparklines, a per-tool table, by-mechanism and RTK shell savings, an applied-value panel (benchmark rates × your live ops → estimated tokens and time saved), and session activity. Header controls, all remembered in the browser:

- **time window**: live, last 15m/1h/3h, today, since a clock time, or all
- **scope**: this repo, or all repos (every repo + launch profile)
- **theme**: dark (default) or retro 70s
- **mini / full**: a compact pane vs the expansive charts

<img width="1206" height="868" alt="image" src="https://github.com/user-attachments/assets/a651e84b-4087-4e6f-9e43-21ceb68b9bdb" />

**Terminal** (`lens dashboard --tui`, alias `lens top`): the same snapshot in the terminal, no browser or socket. Zero-dependency ANSI (box panels, block sparklines, `NO_COLOR`-aware), auto mini/full by width.

```sh
lens top                       # this repo, auto layout
lens dashboard --tui --global  # every repo + launch profile
lens top --today               # scope to since local midnight
lens top --since 1h            # ...or a sliding window (15m|1h|3h|2d|all)
lens top --theme 70s           # retro palette (dark is the default)
lens top --full --interval 2   # framed layout, refresh every 2s
```

The `$` headline prices the measured tokens-saved at the model input rate (`--rate <$/M>` or `--model opus|sonnet|haiku`). Applied-value figures (tokens plus time, at `--rt-seconds` per avoided round-trip, default 4s) are estimates and never enter that headline.

## How it works

lens is one Rust binary that attaches to Claude Code two ways: as an **MCP stdio server** (the `lens_*` tools, `src/server.rs`) and as **hook handlers** the same binary runs on Claude Code's PreToolUse, PostToolUse, UserPromptSubmit, PreCompact, and SessionStart events. Per-repo state lives in `.lens/` (the symbol graph, the FTS index, and the reversible blob store); the managed RTK binary lives in `~/.lens/bin`.

**Darkroom (`lens_run` / `lens_run_file`).** Your script runs in a subprocess; lens captures only its stdout/stderr. The raw data the script reads never enters the model's context. Anything large that lens would otherwise truncate is first written to a content-addressed store (blobs keyed by blake3 hash), so `lens_recall` can reverse any truncation losslessly. The subprocess gives you process isolation and a timeout, not an OS sandbox (see [Security](#security)).

**Search (`lens_index` / `lens_search`).** `lens_index` builds a full-text index over the repo, chunked at tree-sitter AST boundaries so a hit's chunk is a whole function, not a slice through two. `lens_search` ranks with BM25F fused with graph importance, re-ranks by term proximity, and returns snippets anchored on the match line with the chunk's definition names attached. Natural-language queries get their own ranking profile so prose words that collide with symbol names can't hijack the results. Batch several questions in one call to save round-trips.

**Graph (`lens_map` / `lens_symbol` / `lens_find` / `lens_links` / `lens_path`).** `lens_map` parses [supported files](SUPPORTED.md) with tree-sitter and builds a deterministic structural graph (functions, types, modules, and their calls/imports/contains edges) in `.lens/graph.json`. Edges are directed, so "who calls X", "what does X reach", and "how does A get to B" are graph lookups instead of file reads, walkable in either direction. Every node is labeled prod/test/bench, so "production callers of X" never requires opening files to sort test code out.

**Warmup (`lens warmup` / `lens watch`).** The graph and index build lazily on the first tool call that needs them. To pay that cost up front, run `lens warmup [path]` before you start; to keep both fresh as you edit, run `lens watch [path]` (debounced re-index on file changes). Both write into the same `.lens/` dir the server reads, so a running server picks up the changes on its next query with no restart.

**Session continuity.** The lifecycle hooks capture events into a store, each tagged priority 1 (critical) to 4 (low). At `PreCompact`, lens builds a priority-tiered resume snapshot within a small byte budget; at `SessionStart` it re-injects a Session Guide, so long runs keep their thread across compaction. Durable decisions and constraints live in cross-session memory (`lens_memory_record` / `lens_memory_query`).

**Routing.** A PreToolUse policy, gated by `LENS_ROUTING`, decides whether to pass, nudge, rewrite, or deny each tool call:

- `off`: tools available, no steering.
- `nudge`: one-shot nudges toward the lens tools plus the SessionStart guide; never denies or rewrites.
- `steer`: nudge, plus deny `WebFetch` and redirect `curl`/`wget`/build commands into the darkroom.
- `wrap`: transparently rewrite a read-only, high-output `Bash` command into `lens wrap -- <cmd>` so its output is offloaded losslessly.
- `full`: steer and wrap together.

Above the levels sit per-pattern rails (broad greps, whole-file reads for structure, aggregation pipelines) that redirect specific wasteful call shapes to the equivalent lens tool. Every rail is individually kill-switchable (`LENS_<RAIL>=0`), and all of them stand down automatically when the lens server is unreachable.

**RTK (optional).** lens ships and installs a pinned RTK binary and surfaces RTK's own measured shell-command savings. RTK owns Bash rewriting via its own hook; when it is active, lens defers Bash to it so the two never double-wrap. lens is additive to whatever else your setup runs: it keeps byte-floods out of context and stays out of the way otherwise.

## Development

```sh
cargo test
cargo clippy -- -D warnings
```

```sh
cargo run --bin bench_savings    # mechanism savings table (no credentials needed)
cargo run --bin bench_accuracy   # accuracy harness (LENS_BENCH_BACKEND=agentic|headless|claude-pty, or mock)
cargo run --bin bench_toolsel    # organic tool-selection A/B (rails on/off)
cargo run --bin bench_report     # regenerate BENCHMARKS.md + BENCHMARKS_APPENDIX.md
```

The headline numbers come from `bench_accuracy` with `LENS_BENCH_BACKEND=agentic`: both arms are real Claude Code sessions with tools live, and a validity gate refuses to score any lens-arm run that never reached the lens server.

## Security

`lens_run` executes the script you or the agent supply in a subprocess: real process isolation and a timeout (30s default), but not an OS sandbox. The script runs as your user with your normal filesystem access, so treat a `lens_run` script like any code you'd run locally. Routing's `steer`/`full` levels redirect `WebFetch` and `curl`/`wget`/build commands into the darkroom; drop to `nudge` if you'd rather lens never rewrite a command. Report vulnerabilities via [SECURITY.md](SECURITY.md).

## License

MIT License. See [LICENSE](LICENSE).
