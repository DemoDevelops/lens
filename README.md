# lens

**Keep raw data out of your agent's context window.** When Claude Code reads a 50k-line log, greps a large repo, or fetches a web page, every byte enters the conversation and keeps re-costing tokens on every later turn. Like the glass it's named for, lens focuses: your script runs in a *darkroom* (a subprocess), and only the developed image comes back, never the raw light.

Concretely: counting the log levels in a 50k-line build log costs **7,210 bytes** of context read inline, and **517** through `lens_run`, because the script runs in the darkroom and only the answer comes back. The same move powers full-text search, a code-symbol graph, and lossless recall, with a continuity layer that carries session state across compaction so long runs keep their thread.

## Results

Measured savings at realistic session scale. Full methodology and scale curves in [BENCHMARKS.md](BENCHMARKS.md).

| Workload | Mechanism | Before | After | Saved |
| --- | --- | ---: | ---: | ---: |
| Code search | full-text index | 160,230 | 10,020 | **94–99%** |
| Log debugging | darkroom | 7,210 | 517 | **93%** |
| Issue triage | compression | 94,195 | 31,323 | **~67%** |

Numbers are byte counts against a fixed corpus, a close proxy for tokens (token savings run a little lower for already-compact outputs). Code search and issue triage are shown at 10× the committed test fixture (code search reaches 99% at 50×); log debugging is size-insensitive, shown at 1×. Read each percentage as "what this mechanism does to a workload of this shape," not a guaranteed figure for your repo.

Accuracy on small real-model task sets (`claude-opus-4-8`, headless, context-only):

| Task type | N | Without lens | With lens |
| --- | ---: | ---: | ---: |
| Darkroom (data analysis) | 6 | 67% | **100%** |
| Discovery (code structure) | 4 | 75% | 100% |
| Search | 3 | 67% | 100% |
| File skeleton (read a file's API) | 2 | 0% | **100%** |

N is tiny (2–6 tasks, each run once), so these are directional, not powered rates. The control answers from one truncated slice of the file, which is part of why skeleton reads go 0%→100% (the answer sits past the cut). The signal points the right way; don't quote these as headline percentages.

Session recovery is the one head-to-head against a real comparator, Context Mode (N=4 per set). lens recovers 100% of post-compaction state vs Context Mode's 75%, at roughly 20× lower token cost:

| Scenario | Context Mode | lens | CM tokens | lens tokens |
| --- | ---: | ---: | ---: | ---: |
| File/task recovery | 75% | **100%** | 4,622 | 205 |
| Error/decision recovery | 75% | **100%** | 4,677 | 291 |

## How it compares

Most tools here either compress data that still lands in context, or pull more data in. lens does neither: the bytes stay in the darkroom and only the result comes back.

- **vs output compressors (RTK, headroom, squeez):** they shrink tool output, but the smaller data still enters the transcript and re-costs on later turns. lens keeps it out entirely, and anything it does surface is recoverable in full by reference. lens bundles RTK and defers Bash to it, so you can run both.
- **vs code-search / context servers (Serena, Zilliz claude-context, repomix):** these retrieve code *into* context, and several need a vector DB, an embedding API key, or a cloud account. lens answers structure questions from a local tree-sitter graph and returns snippets, not whole files, with zero external dependencies.
- **vs context-mode:** the closest design. Its `ctx_execute` is the same darkroom idea and it has session continuity too, but no code-symbol graph, no lossless recall, and an ELv2 (source-available) license. lens adds those and is MIT.

Run lens alongside any of them. It is not trying to replace your agent or your search tool, just keep their byte-floods out of context.

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
| `lens_skeleton` | Show a source file's structure: signatures + nesting, bodies elided to `…`. Full text recoverable via `lens_recall`. |
| `lens_index` | Index a directory for full-text search. Run once per repo. |
| `lens_search` | BM25F search with a proximity rerank: chunks whose query terms sit close together rank higher. Answers "where is X mentioned". |
| `lens_grep_ast` | Structural search via a tree-sitter query: matches syntax, not text (real `.unwrap()` calls, not comments). |
| `lens_map` | Parse the repo into a symbol graph (functions, types, modules, relationships). Run once per repo. |
| `lens_overview` | Token-budgeted map (~2k tokens) of the repo's symbols: a knapsack packs the highest-importance subset that fits the budget, so load-bearing hubs survive. |
| `lens_symbol` | Find symbols by name and see their immediate connections. |
| `lens_find` | Find symbols by natural-language description. |
| `lens_links` | Expand a symbol's neighborhood N hops out. |
| `lens_path` | Shortest call/import path between two symbols. |
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
lens_path(from="run_cli", to="purge_context_mode")   # shortest call path between two symbols
lens_links(node_id="<id from a lens_symbol result>", depth=1)   # neighborhood, 1 hop
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

**Search (`lens_index` / `lens_search`).** `lens_index` builds a full-text index over the repo. `lens_search` ranks with BM25F, then over-fetches a deeper candidate pool and re-ranks it by term proximity (a chunk where the query terms sit in a tight window outranks one where they are scattered) before returning the top snippets with `path:line`, not whole files. Batch several questions in one call to save round-trips.

**Graph (`lens_map` / `lens_symbol` / `lens_find` / `lens_links` / `lens_path`).** `lens_map` parses [supported files](SUPPORTED.md) with tree-sitter and builds a deterministic structural graph (functions, types, modules, and their calls/imports/contains edges) in `.lens/graph.json`. The query tools walk that graph, so "who calls X" or "how does A reach B" is a graph lookup instead of a pile of file reads.

**Session continuity.** The lifecycle hooks capture events into a store, each tagged priority 1 (critical) to 4 (low). At `PreCompact`, lens builds a priority-tiered resume snapshot within a small byte budget; at `SessionStart` it re-injects a Session Guide. This survives compaction at a fraction of the Context Mode plugin's token cost.

**Routing.** A PreToolUse policy, gated by `LENS_ROUTING`, decides whether to pass, nudge, rewrite, or deny each tool call:

- `off`: tools available, no steering.
- `nudge`: one-shot nudges toward the lens tools plus the SessionStart guide; never denies or rewrites.
- `steer`: nudge, plus deny `WebFetch` and redirect `curl`/`wget`/build commands into the darkroom.
- `wrap`: transparently rewrite a read-only, high-output `Bash` command into `lens wrap -- <cmd>` so its output is offloaded losslessly.
- `full`: steer and wrap together.

**RTK (optional).** lens ships and installs a pinned RTK binary (the "headroom pattern") and surfaces RTK's own measured shell-command savings. RTK owns Bash rewriting via its own hook; when it is active, lens defers Bash to it so the two never double-wrap.

## Development

```sh
cargo test
cargo clippy -- -D warnings
```

```sh
cargo run --bin bench_savings    # savings table (no credentials needed)
cargo run --bin bench_accuracy   # accuracy harness (LENS_BENCH_BACKEND=claude-pty, ANTHROPIC_API_KEY, or mock)
cargo run --bin bench_recovery   # session-recovery head-to-head vs Context Mode
cargo run --bin bench_report     # regenerate BENCHMARKS.md + BENCHMARKS_APPENDIX.md
```

## Security

`lens_run` executes the script you or the agent supply in a subprocess: real process isolation and a timeout (30s default), but not an OS sandbox. The script runs as your user with your normal filesystem access, so treat a `lens_run` script like any code you'd run locally. Routing's `steer`/`full` levels redirect `WebFetch` and `curl`/`wget`/build commands into the darkroom; drop to `nudge` if you'd rather lens never rewrite a command. Report vulnerabilities via [SECURITY.md](SECURITY.md).

## License

MIT License. See [LICENSE](LICENSE).
