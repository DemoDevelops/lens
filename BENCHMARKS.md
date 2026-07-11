# lens benchmarks

lens is an MCP tool provider that keeps work **out** of the agent's context window: it indexes, darkroomes, compresses, and graphs data so the bytes a naive agent would read never enter context. The tables below are the measured results.

_Full scale curves, mechanism classifications, and methodology are in [BENCHMARKS_APPENDIX.md](BENCHMARKS_APPENDIX.md)._

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

## Accuracy

Model: `claude-opus-4-8 (via claude-headless)`

| Task set | N | Control acc | lens acc | Δ acc | Control tokens | lens tokens | Token Δ |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Darkroom tasks | 6 | 67% | 100% | +33pp | 2999 | 111 | -2888 |
| Discovery tasks | 4 | 75% | 100% | +25pp | 1708 | 2418 | +710 |
| Search tasks | 3 | 67% | 100% | +33pp | 828 | 1160 | +332 |
| Skeleton tasks | 2 | 0% | 100% | +100pp | 980 | 920 | -60 |

> Run method: real model via headless `claude -p`, tools disabled, context-only isolation — each arm answers only from its given context, exactly like a direct API call.
>
> Samples are small (N = 6 / 4 / 3 / 2) and each task runs once. Directional confirmations, not statistically powered rates.

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
