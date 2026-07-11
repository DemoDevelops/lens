Smarter search ranking, markdown as a first-class graph language, cross-session memory, and six more routing rails on by default building on v0.7.0's tool-selection steering. Covers everything merged since v0.7.0.

### Added
- **Six more routing rails, on by default.** v0.7.0 shipped the base tool-selection routing (nudge toward lens tools, deny a first broad `Grep`). `LENS_ROUTING` now defaults to `full`, and six additional rails (grep-by-symbol-name, pre-edit reads, aggregated bash output, markdown links, AST-shaped grep, overview-worthy reads) each ship a nudge arm and a deny arm and are on by default (kill-switch polarity: set the matching `LENS_*_DENY`/`LENS_*_NUDGE` to `0` to disable one), promoted after a live A/B showed real adoption gains (see [BENCHMARKS.md](BENCHMARKS.md)). High-output read-only `Bash` commands are also now transparently offloaded through `lens_run`.
- **Identifier-rarity rerank.** A query naming a specific identifier now reranks the file that defines it to the top of results, even when a prose-heavy file scores higher on raw term frequency. On by default; a no-op on queries with no strong identifier.
- **Graph-aware search fusion (RRF).** Search fuses lexical rank with graph centrality, so a structurally important file (imported and called from everywhere) surfaces even when its own text match is weak. On by default.
- **`$META` structural pattern language.** Structural search accepts `$X`-style meta-patterns (e.g. `$X.unwrap()`) that compile to tree-sitter queries, verified against independent hand-written oracles.
- **Cross-session memory.** New `record_memory` / `query_memory` tools store durable, project-scoped notes that a later session can recall by query.
- **Markdown as a first-class graph language.** Headings, sections, and skeleton views now work on `.md` files like any other language; links resolve to real graph nodes instead of stub placeholders.
- **Richer skeletons.** `lens_skeleton` now shows struct/enum fields instead of eliding them like function bodies, with optional line-number citations.
- **Dashboard: real per-model usage.** Savings can be priced against the actually-used model mix ("Actual Usage" mode) instead of one fixed model, and a deterministic classifier attributes savings credit instead of a heuristic.

### Fixed
- `bench_fitness`'s tool-surface count no longer miscounts `#[cfg(test)]` functions as tools.
- A recall regression in `lens_grep_ast` match results.

### Improved
- Setup now allow-lists every lens tool and the CLI up front, including in plan mode, so nothing prompts for permission on first use.
