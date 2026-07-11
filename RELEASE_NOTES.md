Smarter search ranking, tool-selection routing that steers Claude toward lens by default, markdown as a first-class graph language, and cross-session memory. Covers everything merged since v0.6.1, including the v0.7.0 cut (whose own release notes were missed).

### Added
- **Tool-selection routing, on by default.** `LENS_ROUTING` now defaults to `full`: nudges toward lens tools for `Bash`/`Grep`/`Read`, a first-time broad `Grep` is denied once per prompt in favor of a lens call, and high-output read-only `Bash` commands are transparently offloaded through `lens_run`. Six additional rails (grep-by-symbol-name, pre-edit reads, aggregated bash output, markdown links, AST-shaped grep, overview-worthy reads) each ship a nudge arm and a deny arm and are on by default (kill-switch polarity: set the matching `LENS_*_DENY`/`LENS_*_NUDGE` to `0` to disable one), promoted after a live A/B showed real adoption gains (see [BENCHMARKS.md](BENCHMARKS.md)).
- **Identifier-rarity rerank.** A query naming a specific identifier now reranks the file that defines it to the top of results, even when a prose-heavy file scores higher on raw term frequency. On by default; a no-op on queries with no strong identifier.
- **Graph-aware search fusion (RRF).** Search fuses lexical rank with graph centrality, so a structurally important file (imported and called from everywhere) surfaces even when its own text match is weak. On by default.
- **`$META` structural pattern language.** Structural search accepts `$X`-style meta-patterns (e.g. `$X.unwrap()`) that compile to tree-sitter queries, verified against independent hand-written oracles.
- **Cross-session memory.** New `record_memory` / `query_memory` tools store durable, project-scoped notes that a later session can recall by query.
- **Markdown as a first-class graph language.** Headings, sections, and skeleton views now work on `.md` files like any other language; links resolve to real graph nodes instead of stub placeholders.
- **Richer skeletons.** `lens_skeleton` now shows struct/enum fields instead of eliding them like function bodies, with optional line-number citations.
- **Bundled `/warmup` command.** Builds the lens index up front instead of paying the cost on the first real query.
- **Dashboard: real per-model usage.** Savings can be priced against the actually-used model mix ("Actual Usage" mode) instead of one fixed model, and a deterministic classifier attributes savings credit instead of a heuristic.

### Fixed
- Stale file snapshots are now flagged on recall and edit instead of silently serving out-of-date content.
- `bench_fitness`'s tool-surface count no longer miscounts `#[cfg(test)]` functions as tools.
- The `rtk` hook no longer double-registers when its path is spelled two different ways in config.
- A recall regression in `lens_grep_ast` match results.

### Improved
- Setup now allow-lists every lens tool and the CLI up front, including in plan mode, so nothing prompts for permission on first use.
