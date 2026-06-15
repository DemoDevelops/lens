# ctxforge benchmarks

ctxforge is an MCP tool provider that keeps work **out** of the agent's context window: it indexes, sandboxes, compresses, and graphs data so the bytes a naive agent would read never enter context. The tables below are the measured results.

_Full scale curves, mechanism classifications, the discovery-regression investigation, and methodology are in [BENCHMARKS_APPENDIX.md](BENCHMARKS_APPENDIX.md)._

## Savings

Headline savings are at **realistic session scale**, not the 1× diagnostic fixtures. Each row stays segmented by the ctxforge mechanism that produced it — never a single blended percentage.

| Workload | Mechanism | Before (bytes) | After (bytes) | Savings |
| --- | --- | ---: | ---: | ---: |
| Code search | index | 160,230 | 9,825 | **94–99%** |
| Log debugging | sandbox | 7,210 | 517 | **93%** |
| Issue triage | compression | 94,195 | 36,963 | **~61%** |
| Codebase exploration | discovery | 2,606 | 2,094 | see note |

Code search and issue triage are shown at 10× the committed fixture (code search reaches 99% at 50×); log debugging is size-insensitive and shown at the committed fixture. The full 1×/10×/50× curve and the artifact-vs-real classification are in the appendix.

_Codebase exploration has no single honest representative number: discovery saves 20% on the committed fixture, the scaled replication is a known-pessimistic O(N²) lower bound (appendix), and the production case is bounded by `Forge::maybe_compact`. Discovery replaces multi-file reads with a scoped subgraph; we state that bound rather than headline a flattering extreme._

## Accuracy

Model: `claude-opus-4-8 (via claude-pty)`

| Task set | N | Control acc | ctxforge acc | Δ acc | Control tokens | ctxforge tokens | Token Δ |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Sandbox tasks | 6 | 17% | 100% | +83pp | 1847 | 103 | -1744 |
| Discovery tasks | 3 | 67% | 100% | +33pp | 987 | 467 | -520 |
| Search tasks | 2 | 100% | 100% | +0pp | 432 | 314 | -118 |

> Run method: real model via `claude-pty`, tools disabled, context-only isolation — each arm answers only from its given context, exactly like a direct API call.
>
> Samples are small (N = 6 / 3 / 2); these are directional confirmations consistent with the mechanism analysis, not statistically powered rates.

## Session recovery

Proves the Context Mode replacement: each scenario builds a working state, forces a compaction boundary, then asks a question only answerable if the state survived. The bar is **Context Mode**, not ctxforge's own sense of working — the swap is only safe when **ctxforge ≥ Context Mode** at comparable token cost.

Model: `claude-opus-4-8 (via claude-pty)`. Survival = % of scenarios whose working state was recoverable from the post-compaction context.

| Scenario set | N | No-continuity | Context Mode | ctxforge | Δ (ctxforge − CM) | CM tokens | ctxforge tokens |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| File/task recovery | 4 | 0% | 75% | 100% | +25pp | 5070 | 202 |
| Error/decision recovery | 4 | 0% | 75% | 100% | +25pp | 5136 | 302 |

✅ **ctxforge ≥ Context Mode** on every scenario set above — the swap is safe on recovery fidelity.

_Samples are small (N = 4 / 4); directional confirmations, not statistically powered rates._

## Notes

- Context Mode has no JSON-compactor or code-graph equivalent, so three of the four savings workloads have no faithful Context Mode head-to-head (full per-cell reasoning in the appendix); the one faithful Context Mode comparison is **session recovery**, above.
- The real-model runs were obtained via `claude-pty` on plan quota; the supported path for reproduction is a direct `ANTHROPIC_API_KEY` run (see the appendix and [benchmarks/README.md](benchmarks/README.md)).
