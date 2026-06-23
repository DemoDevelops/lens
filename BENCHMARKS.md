# lens benchmarks

lens is an MCP tool provider that keeps work **out** of the agent's context window: it indexes, darkroomes, compresses, and graphs data so the bytes a naive agent would read never enter context. The tables below are the measured results.

_Full scale curves, mechanism classifications, the discovery-regression investigation, and methodology are in [BENCHMARKS_APPENDIX.md](BENCHMARKS_APPENDIX.md)._

## Feature suite: value × adoption

Two questions, two planes. **Plane A** (deterministic, no model): if a feature is used, how much smaller is what comes back. **Plane B** (real agent): does Claude actually fire it when it should. A feature only pays off when both hold.

### Plane A: value across codebase scale (deterministic, % bytes saved)

Savings move with size, and sometimes the conclusion flips, so each feature is measured at Small / Medium / Large / Huge (1× / 10× / 50× / 200× the committed fixture). Run: `cargo run --bin bench_value`.

| What it does for you | Small | Medium | Large | Huge | scale effect |
| --- | ---: | ---: | ---: | ---: | --- |
| Crunch a big file/log, return the answer | 93% | 93% | 93% | 93% | flat |
| Find where something is across the repo (vs grep) | 3% | 91% | 98% | 100% | **flips** |
| Shrink repetitive structured data (big JSON) | 63% | 67% | 67% | 67% | flat |
| Run a noisy command, keep a preview | 54% | 95% | 99% | 100% | grows |
| Stop a web page / build log flooding the chat | 100% | 100% | 100% | 100% | flat |
| Map code structure (round-trips: read each file vs 1 graph call) | 20→4 | 200→4 | 1000→4 | 4000→4 | grows |

- **flat**: size-insensitive, one number is honest.
- **flips**: the conclusion changes with size. grep is as lean as lens_search on a small repo but floods on a big one. This is why search steering is scale-aware (below), not always-on.
- **grows**: the saving widens with size (bounded preview / fixed graph call vs ever-growing raw output / file reads).
- Graph is on the round-trip axis: its byte savings at scale are an O(N²) artifact of the duplicate-symbol test fixture (appendix); the round-trip win scales cleanly. Baselines are the realistic alternative (search vs grep, not vs full-file reads).

### Plane B: adoption (does Claude fire it?)

Real agent under normal steering, on a task that should trigger each feature; **fired** = it used the lens tool instead of falling back to Read/Grep. N = 3 per feature. Run: `bash benchmarks/adoption/run_adoption.sh --runs 3`.

| Feature | Fires when it should | What it did |
| --- | ---: | --- |
| Crunch a big file/log (darkroom) | **3/3 (100%)** | used lens_run every run |
| Map code structure (graph) | **3/3 (100%)** | used lens_symbol / path / neighbors |
| Find across the repo (search) | **0/3 (0%)** | fell back to grep |

- Darkroom and graph are high value **and** high adoption: they work end to end.
- Search's 0% is **correct on a small repo** (Plane A shows grep ties lens_search there). lens_search only wins at scale, so a scale-aware PostToolUse nudge now fires **only when a grep result floods** (>16KB, `LENS_GREP_FLOOD_BYTES`). The nudge is verified to fire; whether it lifts adoption on large-hit tasks is not yet measured.
- Compression and wrap fire automatically (not agent choices); recovery is the **Session recovery** section below.

The honest headline: lens's value is real and largest at scale, adoption is solid where the tool genuinely beats the built-in (darkroom, graph), and the one gap (search) is a scale-conditioned steering problem now addressed but not yet re-measured.

## Savings

Headline savings are at **realistic session scale**, not the 1× diagnostic fixtures. Each row stays segmented by the lens mechanism that produced it — never a single blended percentage.

| Workload | Mechanism | Before (bytes) | After (bytes) | Savings |
| --- | --- | ---: | ---: | ---: |
| Code search | index | 160,230 | 9,825 | **94–99%** |
| Log debugging | darkroom | 7,210 | 517 | **93%** |
| Issue triage | compression | 94,195 | 31,323 | **~67%** |
| Codebase exploration | discovery | 2,606 | 2,094 | see note |

Code search and issue triage are shown at 10× the committed fixture (code search reaches 99% at 50×); log debugging is size-insensitive and shown at the committed fixture. The full 1×/10×/50× curve and the artifact-vs-real classification are in the appendix.

_Codebase exploration has no single honest representative number: discovery saves 20% on the committed fixture, the scaled replication is a known-pessimistic O(N²) lower bound (appendix), and the production case is bounded by `Forge::maybe_compact`. Discovery replaces multi-file reads with a scoped subgraph; we state that bound rather than headline a flattering extreme._

## Accuracy

Model: `claude-opus-4-8 (via claude-pty)`

| Task set | N | Control acc | lens acc | Δ acc | Control tokens | lens tokens | Token Δ |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Darkroom tasks | 6 | 17% | 100% | +83pp | 1847 | 103 | -1744 |
| Discovery tasks | 3 | 67% | 100% | +33pp | 987 | 467 | -520 |
| Search tasks | 2 | 100% | 100% | +0pp | 432 | 314 | -118 |

> Run method: real model via `claude-pty`, tools disabled, context-only isolation — each arm answers only from its given context, exactly like a direct API call.
>
> Samples are small (N = 6 / 3 / 2); these are directional confirmations consistent with the mechanism analysis, not statistically powered rates.

## Session recovery

Proves the Context Mode replacement: each scenario builds a working state, forces a compaction boundary, then asks a question only answerable if the state survived. The bar is **Context Mode**, not lens's own sense of working — the swap is only safe when **lens ≥ Context Mode** at comparable token cost.

Model: `claude-opus-4-8 (via claude-pty)`. Survival = % of scenarios whose working state was recoverable from the post-compaction context.

| Scenario set | N | No-continuity | Context Mode | lens | Δ (lens − CM) | CM tokens | lens tokens |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| File/task recovery | 4 | 0% | 75% | 100% | +25pp | 5070 | 202 |
| Error/decision recovery | 4 | 0% | 75% | 100% | +25pp | 5136 | 302 |

✅ **lens ≥ Context Mode** on every scenario set above — the swap is safe on recovery fidelity.

_Samples are small (N = 4 / 4); directional confirmations, not statistically powered rates._

## Notes

- Context Mode has no JSON-compactor or code-graph equivalent, so three of the four savings workloads have no faithful Context Mode head-to-head (full per-cell reasoning in the appendix); the one faithful Context Mode comparison is **session recovery**, above.
- The real-model runs were obtained via `claude-pty` on plan quota; the supported path for reproduction is a direct `ANTHROPIC_API_KEY` run (see the appendix and [benchmarks/README.md](benchmarks/README.md)).
