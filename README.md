# lens

AI agents waste tokens reading raw data into context: file dumps, grep floods, build logs, web pages. lens keeps that data out — your script runs in a subprocess and only what it prints comes back. It also replaces the Context Mode plugin with a lighter session-continuity layer that survives compaction at a fraction of the token cost.

## Results

Token savings at realistic session scale:

| Workload | Mechanism | Tokens before | Tokens after | Saved |
| --- | --- | ---: | ---: | ---: |
| Code search | FTS5 index | 160,230 | 9,825 | **94–99%** |
| Log debugging | darkroom | 7,210 | 517 | **93%** |
| Issue triage | compression | 94,195 | 36,963 | **~61%** |

Accuracy on real tasks (`claude-opus-4-8`):

| Task type | Without lens | With lens |
| --- | ---: | ---: |
| Darkroom (data analysis) | 17% | **100%** |
| Discovery (code structure) | 67% | **100%** |
| Search | 100% | 100% |

Session recovery vs Context Mode:

| Scenario | Context Mode | lens | CM tokens | lens tokens |
| --- | ---: | ---: | ---: | ---: |
| File/task recovery | 75% | **100%** | 5,070 | 202 |
| Error/decision recovery | 75% | **100%** | 5,136 | 302 |

Full methodology: [BENCHMARKS.md](BENCHMARKS.md)

## Install

**Prerequisites:** [Rust](https://rustup.rs) stable. Optional: `python3`, `node`/`npx`, `ruby`, `go` — only needed if you run those languages through `lens_run`.

```sh
git clone https://github.com/DemoDevelops/lens && cd lens
cargo build --release
```

**MCP server** (token savings):
```sh
claude mcp add lens -- /absolute/path/to/target/release/lens
```
Restart Claude Code. Verify with `lens_stats`.

**Session continuity** (optional, replaces Context Mode):
```sh
# Uninstall Context Mode first if you have it:
# /plugin uninstall context-mode

./target/release/lens session install
```
This registers lifecycle hooks and installs RTK for shell-command compression. Verify with `lens session status`.

For multiple Claude accounts, target the right config:
```sh
lens session install --config-dir ~/.claude-personal
```

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

## Development

```sh
cargo test
cargo clippy -- -D warnings
```

```sh
cargo run --bin bench_savings    # savings table (no credentials needed)
cargo run --bin bench_accuracy   # accuracy harness (ANTHROPIC_API_KEY or mock)
cargo run --bin bench_recovery   # session-recovery head-to-head vs Context Mode
cargo run --bin bench_report     # regenerate BENCHMARKS.md + BENCHMARKS_APPENDIX.md
```

## License

Apache-2.0. See [LICENSE](LICENSE).
