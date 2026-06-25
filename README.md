# lens

AI agents waste tokens reading raw data into context: file dumps, grep floods, build logs, web pages. lens is a set of MCP tools and hooks for Claude Code that does that work without the bytes. Like the glass it's named for, it focuses: your script runs in a darkroom (a subprocess), and only the developed image comes back, never the raw light. The same idea powers full-text search, a code symbol graph, and lossless recall; a continuity layer carries session state across compaction so long runs keep their thread.

## Results

Token savings at realistic session scale:

| Workload | Mechanism | Tokens before | Tokens after | Saved |
| --- | --- | ---: | ---: | ---: |
| Code search | FTS5 index | 160,230 | 9,825 | **94–99%** |
| Log debugging | darkroom | 7,210 | 517 | **93%** |
| Issue triage | compression | 94,195 | 31,323 | **~67%** |

Accuracy on real tasks (`claude-opus-4-8`):

| Task type | Without lens | With lens |
| --- | ---: | ---: |
| Darkroom (data analysis) | 33% | **100%** |
| Discovery (code structure) | 100% | 100% |
| Search | 100% | 100% |

Session recovery vs Context Mode:

| Scenario | Context Mode | lens | CM tokens | lens tokens |
| --- | ---: | ---: | ---: | ---: |
| File/task recovery | 75% | **100%** | 4,622 | 205 |
| Error/decision recovery | 75% | **100%** | 4,677 | 291 |

Full methodology: [BENCHMARKS.md](BENCHMARKS.md)

## Install

lens is one binary. Get it onto your machine, then run `lens setup` to wire it into Claude Code.

**From a binary you were sent** (no GitHub access needed). Pick the file matching your platform (`uname -sm`):

```sh
chmod +x lens-aarch64-apple-darwin     # or x86_64-apple-darwin / x86_64-unknown-linux-gnu
./lens-aarch64-apple-darwin setup --full
```

**As a collaborator** (repo access + `gh auth login`):

```sh
# target: aarch64-apple-darwin | x86_64-apple-darwin | x86_64-unknown-linux-gnu
gh release download --repo DemoDevelops/lens --pattern "lens-<target>" --output lens
chmod +x lens && ./lens setup --full
```

**From source** ([Rust](https://rustup.rs) stable; optional `python3`/`node`/`ruby`/`go`, only to run those languages through `lens_run`):

```sh
git clone https://github.com/DemoDevelops/lens && cd lens
cargo build --release
./target/release/lens setup --full
```

`lens setup` copies the binary to `~/.local/bin`, registers the MCP server, installs the session hooks and the `/dashboard` command, installs RTK shell compression, sets the routing level, and prints a verification report. Restart Claude Code, then verify with the `lens_stats` tool.

Routing defaults to the safe `nudge` level (encourages the lens tools, never denies WebFetch or rewrites commands). `--full` turns on the aggressive routing (WebFetch and noisy commands redirected into the darkroom) plus RTK. Change it anytime with `lens setup --routing <off|nudge|steer|wrap|full>`.

For multiple Claude accounts, target a config dir: `lens setup --config-dir ~/.claude-personal`. Supported: macOS (arm64, x64), Linux (x64).

Update later with `lens update`: it checks for a newer release, downloads the matching binary, and re-applies setup (preserving your routing level). It uses `gh` for the private repo, so run `gh auth login` once. Without `gh`, re-run `setup` with a newer binary instead.

## Tools

| Tool | What it does |
| :- | :- |
| `lens_run` | Run a script in a darkroom; only stdout returns to context. Best for log parsing, data aggregation, large file analysis. |
| `lens_run_file` | Same as `lens_run` but receives a file path as its first argument. |
| `lens_index` | Index a directory for full-text search. Run once per repo. |
| `lens_search` | BM25-ranked search across the index. Answers "where is X mentioned". |
| `lens_map` | Parse the repo into a symbol graph (functions, types, modules, relationships). Run once per repo. |
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

## How it works

lens is one Rust binary that attaches to Claude Code two ways: as an **MCP stdio server** (the `lens_*` tools, `src/server.rs`) and as **hook handlers** the same binary runs on Claude Code's PreToolUse, PostToolUse, UserPromptSubmit, PreCompact, and SessionStart events. Per-repo state lives in `.lens/` (the symbol graph, the FTS index, and the reversible blob store); the managed RTK binary lives in `~/.lens/bin`.

**Darkroom (`lens_run` / `lens_run_file`).** Your script runs in a subprocess; lens captures only its stdout/stderr. The raw data the script reads never enters the model's context. Anything large that lens would otherwise truncate is first written to a content-addressed store (blobs keyed by blake3 hash), so `lens_recall` can reverse any truncation losslessly.

**Search (`lens_index` / `lens_search`).** `lens_index` builds a SQLite FTS5 full-text index over the repo. `lens_search` runs BM25-ranked queries and returns the top snippets with `path:line`, not whole files. Batch several questions in one call to save round-trips.

**Graph (`lens_map` / `lens_symbol` / `lens_find` / `lens_links` / `lens_path`).** `lens_map` parses supported files with tree-sitter and builds a deterministic structural graph (functions, types, modules, and their calls/imports/contains edges) in `.lens/graph.json`. The query tools walk that graph, so "who calls X" or "how does A reach B" is a graph lookup instead of a pile of file reads.

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

## License

Elastic License 2.0 (ELv2). See [LICENSE](LICENSE).
