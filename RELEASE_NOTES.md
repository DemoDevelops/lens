The efficiency release: search overhauled end to end, the code graph made directed, and the first apples-to-apples numbers against vanilla Claude Code: same 89% accuracy at 35.7% fewer tokens and 8.5% faster time-to-answer, measured with tools live on the validity-gated agentic benchmark (see [BENCHMARKS.md](BENCHMARKS.md)). Covers everything merged since v0.8.1.

### Added

- **Match-line citations in search.** A hit's `line` now points at the line the query matched, and the snippet anchors on the densest window covering the most distinct query terms, instead of both citing the top of the chunk.
- **Per-hit definition names.** Multi-term queries get a `symbols` list on each hit (definitions the chunk contains that the snippet doesn't show), so "which function does X" is often answered by the hit itself. On the 8-task search suite, lens now matches vanilla accuracy (96% vs 96%) at 48% fewer tokens and 56% faster.
- **A prose-query ranking profile.** Natural-language queries (5+ words, no compound identifier) stand down the symbol-name field weight, definition boost, and graph def-injection, so bare words that collide with symbol names can't hijack rank 1. Identifier queries are byte-identical to before. Kill-switch: `LENS_PROSE_PROFILE=0`.
- **AST-boundary index chunks.** The full-text index chunks at tree-sitter node boundaries instead of fixed line windows, so a hit's chunk is a whole function, not a slice through two. The index format bump means the first index-touching call after updating rebuilds the full-text index once; the code graph is unaffected.
- **Directed graph traversal.** `lens_path` answers "does A reach B", not just "are A and B connected", and `lens_links` walks fan-in (`callers`) or fan-out (`callees`) without smearing into an undirected ball at depth >1. Scoped call resolution (`self.f()`, `Type::f()`, turbofish) and C/C++ call capture landed in the same wave. On path-tracing tasks: 76% fewer tokens, 77% faster at 100% accuracy.
- **Provenance labels on graph results.** When a result contains test or bench code, every node carries `origin` (prod/test/bench), so "production callers of X" no longer requires opening files to sort test callers out. All-production results are byte-identical to before.

### Fixed

- **A timed-out darkroom script can no longer wedge the handler.** Scripts run in their own process group: timeout kills the whole tree (backgrounded grandchildren included), and the output drain returns whatever was read after a 2s grace instead of blocking on a pipe an escaped process still holds.

### Improved

- **Routing rails: full deny coverage, nudge tier retired.** The reroute rails close their coverage gaps and drop nudges entirely (denies convert; nudges never did). All rails stay kill-switch default-ON (`LENS_*=0` to disable one). Measured on organic tool selection: lens engagement goes from 60% rails-off to 78% rails-on.
- **The benchmark suite is now release-grade.** The agentic accuracy suite grew from 9 to 15 tasks, each run under a validity gate that refuses to score a lens arm that never reached the lens server, and the numbers above come from it (claude-sonnet-5, 3 runs per arm per task, committed record in `benchmarks/accuracy/results/agentic/real-sonnet.json`).
