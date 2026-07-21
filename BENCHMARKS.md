# lens benchmarks

lens is an MCP tool provider that keeps work **out** of the agent's context window: it indexes, darkroomes, compresses, and graphs data so the bytes a naive agent would read never enter context. The tables below are the measured results.

_Full scale curves, mechanism classifications, and methodology are in [BENCHMARKS_APPENDIX.md](BENCHMARKS_APPENDIX.md)._

## End to end (agentic, tools live)

The primary product claim. Both arms are real Claude Code sessions with tools live and the agent free to work however it wants; the lens arm additionally has the lens MCP server and routing installed. The `0.11-dev` set: 26 repo-investigation tasks over this repo's `src/` (the 22 frozen `0.10` ids plus 6 composed multi-step tasks), 3 runs per arm per task, `claude-sonnet-5` at effort `medium`. A per-arm canary proves each config reaches (or, for the baseline, never reaches) the lens server before any task is scored; a zero-lens lens-arm run then scores as an adoption miss instead of being dropped, so the record carries every cell rather than only the lens-engaging ones.

| | lens | vanilla | delta |
| --- | ---: | ---: | ---: |
| tokens per task | 452k | 526k | **-14.0%** |
| accuracy | 82±36% | 85±34% | -3pp |
| time to answer | 27.4s | 40.2s | **-31.8%** |

Per function (mean over K=3, functions with tagged tasks):

| fn | n | tokens (lens/vanilla) | accuracy | time (lens/vanilla) |
| :- | -: | :- | :- | :- |
| `lens_search` | 8 | 229k / 313k (**-27%**) | 100% / 100% | 7.7s / 13.5s (**-43%**) |
| `lens_run` | 8 | 749k / 873k (**-14%**) | 54% / 58% | 53.7s / 75.0s (**-28%**) |
| `lens_run_file` | 2 | 327k / 480k (**-32%**) | 100% / 100% | 15.7s / 22.9s (**-32%**) |
| `lens_links` | 2 | 967k / 1000k (-3%) | 100% / 100% | 69.0s / 116.2s (**-41%**) |
| `lens_path` | 1 | 171k / 389k (**-56%**) | 100% / 100% | 8.1s / 26.5s (**-69%**) |
| `lens_map` | 1 | 169k / 182k (-7%) | 100% / 100% | 5.6s / 7.0s (-19%) |
| `lens_overview` | 1 | 355k / 224k (**+58%**) | **0% / 33%** | 14.8s / 9.4s (+57%) |
| `lens_symbol` | 1 | 145k / 121k (+20%) | 100% / 100% | 5.7s / 4.1s (+39%) |
| `lens_grep_ast` | 1 | 344k / 183k (**+88%**) | 100% / 100% | 11.8s / 8.0s (+46%) |
| `lens_skeleton` | 1 | 149k / 121k (+23%) | 100% / 100% | 5.5s / 3.4s (+60%) |

Honest reading: net **-14% tokens** and **-32% time** at near-parity accuracy (82% vs 85%, inside the ±36/34pp across-task spread). Wins concentrate in `lens_search` (-27% tok, -43% time), the directed graph paths `lens_path`/`lens_links` (-56%/-3% tok, -69%/-41% time), and `lens_run_file` (-32%). The losses are the single-task skeleton-family rows: `lens_grep_ast` (+88%), `lens_overview` (+58%, and a real accuracy drop, 0% vs 33%), `lens_skeleton` (+23%), `lens_symbol` (+20%), where the model spends more context reaching for the tool than a targeted read would. The 8 composed `lens_run` tasks reach lens on 91% of runs but the model iterates many small programs instead of composing one, saving only -14% tokens at 54% vs 58% accuracy. This is a wider, harder set than earlier records (it keeps the previously-hard cells instead of dropping them), so its deltas are more conservative by construction.

Three within-run gates (adoption, economics, reliability) with explicit PASS/FAIL and the numbers behind each are in `benchmarks/accuracy/results/agentic/gates-0.11-dev.md`. Committed record: `benchmarks/accuracy/results/agentic/real-sonnet.json` (this table, `0.11-dev` set), `benchmarks/accuracy/results/agentic/real.json` (the earlier haiku dev-tier record, not part of this set).

## Savings

Headline savings are at **realistic session scale**, not the 1× diagnostic fixtures. Each row stays segmented by the lens mechanism that produced it — never a single blended percentage.

| Workload | Mechanism | Before (bytes) | After (bytes) | Savings |
| --- | --- | ---: | ---: | ---: |
| Code search | index | 160,230 | 9,515 | **94–99%** |
| Log debugging | darkroom | 7,210 | 517 | **93%** |
| Issue triage | compression | 94,195 | 31,323 | **~67%** |
| Codebase exploration | discovery | 2,606 | 2,163 | see note |

Code search and issue triage are shown at 10× the committed fixture (code search reaches 99% at 50×); log debugging is size-insensitive and shown at the committed fixture. The full 1×/10×/50× curve and the artifact-vs-real classification are in the appendix.

_Codebase exploration has no single honest representative number: discovery saves 17% on the committed fixture, the scaled replication is a known-pessimistic O(N²) lower bound (appendix), and the production case is bounded by `Forge::maybe_compact`. Discovery replaces multi-file reads with a scoped subgraph; we state that bound rather than headline a flattering extreme._

## Accuracy (context quality, tools off)

Given the same question and a fixed context budget, does a model answer better from lens-built context than from raw file slices?

Model: `claude-sonnet-5 (via claude-headless)`

| Task set | N | Control acc | lens acc | Δ acc | Control tokens | lens tokens | Token Δ |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Darkroom tasks | 8 | 12% | 75% | +62pp | 4029 | 497 | -3532 |
| Discovery tasks | 24 | 38% | 92% | +54pp | 12321 | 29313 | +16992 |
| Search tasks | 13 | 31% | 69% | +38pp | 5801 | 9149 | +3348 |
| Skeleton tasks | 8 | 12% | 75% | +62pp | 4045 | 14055 | +10010 |

> Run method: real model via headless `claude -p`, tools disabled, context-only isolation — each arm answers only from its given context, exactly like a direct API call. Token columns are the context handed to each arm: lens sometimes spends *more* context (a scoped subgraph vs one truncated file slice) and converts it into +38 to +62pp accuracy; the darkroom row shows the inverse, 8x less context and +62pp. Context quality, not just context size, is what moves accuracy.
>
> The end-to-end cost story (tools live, agent chooses its own reads) is the agentic section above, where lens is a net -35% tokens.

## Session recovery

Proves the Context Mode replacement: each scenario builds a working state, forces a compaction boundary, then asks a question only answerable if the state survived. The bar is **Context Mode**, not lens's own sense of working — the swap is only safe when **lens ≥ Context Mode** at comparable token cost.

Model: `claude-opus-4-8 (via claude-pty)`. Survival = % of scenarios whose working state was recoverable from the post-compaction context.

| Scenario set | N | No-continuity | Context Mode | lens | Δ (lens − CM) | CM tokens | lens tokens |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| File/task recovery | 4 | 0% | 75% | 100% | +25pp | 4622 | 205 |
| Error/decision recovery | 4 | 0% | 75% | 100% | +25pp | 4677 | 291 |

✅ **lens ≥ Context Mode** on every scenario set above — the swap is safe on recovery fidelity.

_Samples are small (N = 4 / 4); directional confirmations, not statistically powered rates._

## Tool-selection enforcement (lens-only agent)

Does restricting an agent to lens tools only (no `Read`/`Grep`/`Glob`/`Bash`) hold up on real work, not just synthetic tasks? 10 real historical investigation tasks (mined from this project's own session history: tool-surface audits, architecture maps, adversarial concurrency/integrity/ranking-parity audits) were replayed through `code-analyst` (full tools + lens) and `code-analyst-lens-only` (lens tools only), then judged for quality against the full-tools baseline.

**Net result: fewer round-trips to the same answer.** Summed across all 10 tasks, lens-only used **29% fewer agent turns**, **24% fewer tool calls**, **28% less wall-clock time**, and **28% fewer tokens** than the full-tools baseline, and on 2 of the 10 tasks lens-only caught a real defect (a Tantivy schema-drift/version-gate gap; an unbounded ranking scan) that the full-tools agent missed entirely.

| Task | Turns Δ | Tool calls Δ | Duration Δ | Tokens Δ |
| --- | ---: | ---: | ---: | ---: |
| telemetry-mining | -50% | -51% | -58% | -56% |
| tool-surface-audit | -38% | -35% | -34% | -55% |
| t11-verification | -38% | -36% | -33% | -43% |
| dashboard-seams-map | +4% | +20% | -23% | +25% |
| accuracy-bench-map | +30% | +42% | +20% | -6% |
| server-tools-map | +3% | +5% | +7% | +120% |
| routing-machinery-map | -41% | -44% | -27% | -19% |
| tantivy-concurrency-audit | -40% | -26% | -35% | -41% |
| tantivy-integrity-audit | -9% | +12% | -13% | -5% |
| tantivy-ranking-parity-audit | -37% | -28% | -39% | -48% |
| **Total (sum)** | **-29%** | **-24%** | **-28%** | **-28%** |

> Model: `claude-sonnet-5`, live session (real `Agent`/`Task` tool dispatch, not headless). N=10, single run per task per arm. Quality judged by an independent LLM judge per task against a full-tools AND a plain-tools (`Read`/`Grep`/`Glob`/`Bash`, no lens) control; a sample of citations was spot-verified directly against source. lens-only was equal-or-better on 6/10 tasks and best-of-all-three on 2/10 (the defect finds above); `server-tools-map` is the one clear loss, worse output at 2.2x the tokens, and is analyzed in the appendix. Full per-task breakdown, the three-way control, and the loss's failure mode are in the appendix.

## Routing (hooks): does steering toward lens tools work?

`LENS_ROUTING=full` (the default as of this release, see [Install](README.md#install)) wires PreToolUse/PostToolUse hooks that nudge or deny a raw `Bash`/`Grep`/`Read` call toward the matching `lens_*` tool. Unlike the tool-selection section above (`Agent`-tool dispatch, which never fires hooks at all, see the appendix), this is measured through real headless `claude -p` sessions with the hooks genuinely wired via `--settings`.

**Denying converts to a measurable accuracy gain.** An A/B on two of the newer deny rails (`lens_grep_ast` in place of a syntax-shaped `Grep`; `lens_run` in place of a repeated aggregate `Bash` call), scored on task success rate on the subset of tasks where the rail actually fired:

| Rail | Off | On (deny) | Δ |
| --- | ---: | ---: | ---: |
| grep-ast deny | 61.1% | 88.9% | **+27.8pp** |
| bash-aggregate deny | 13.3% | 53.3% | **+40.0pp** |
| Combined (fired tasks) | 56.7% | 75.0% | **+18.3pp** |

> `claude-sonnet-5`, 3 runs per arm, real headless sessions via `bench_toolsel` with hooks wired through `--settings`. This result promoted both rails from dark-launch to the default-ON, kill-switch polarity all 13 routing rails now ship with.

The oldest deny rail (a broad `Grep` redirected to `lens_search`/`lens_symbol`) has a full release of live production data behind it: **71% of denied attempts convert** to the correct lens call rather than a retry or an abandon, the number the newer rails' promotion was calibrated against.

Not every rail moves accuracy further once a model already reaches for lens under `full` routing: an isolated per-model A/B on the grep-scope rail alone (3 models, 234 sessions) found no additional lift beyond baseline (sonnet -7.7pp, haiku 0.0pp, opus +5.1pp, all inside per-arm noise), reported honestly rather than folded into the headline above. That rail's measured value is the 71% conversion rate, not a further accuracy bump on models already reaching for lens.

## Notes

- Context Mode has no JSON-compactor or code-graph equivalent, so three of the four savings workloads have no faithful Context Mode head-to-head (full per-cell reasoning in the appendix); the one faithful Context Mode comparison is **session recovery**, above.
- The real-model runs were obtained via Claude Code on plan quota; the supported path for reproduction is a direct `ANTHROPIC_API_KEY` run (see the appendix and [benchmarks/README.md](benchmarks/README.md)).
