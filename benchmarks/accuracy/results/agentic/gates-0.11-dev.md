# Gate readout: `0.11-dev` set, agentic A/B

Within-run gate check for a future v0.10, computed by T8 of the darkroom-first
hardening plan. **No cross-version deltas** (the plan forbids comparing against
v0.9.0 or any prior release): every number below is lens-arm vs the same-run
baseline arm on the `0.11-dev` set.

## Run scope

- **Set:** `sets/0.11-dev.json` (26 tasks: 0060-0085, the 22 frozen `0.10` ids +
  6 composed tasks 0080-0085).
- **Model:** `claude-sonnet-5`, effort `medium`, K=3, both arms real `claude -p`
  agentic sessions, canary-gated. **Sonnet-medium only** per the standing
  release-gate policy (sonnet-medium is the benched configuration; haiku is a
  dev-only assertion and was dropped from this run). A one-task haiku probe was
  run only to validate the pipeline end-to-end and is **discarded** from these
  numbers.
- **Committed record:** `benchmarks/accuracy/results/agentic/real-sonnet.json`.

## Predicate evidence

- **Cells:** 26 of 26 (task x model) scored. **Zero dropped cells.** 7 adoption
  misses were scored (not dropped), as designed.
- **Canary:** PASS for both sonnet arm configs (lens-arm reaches lens; baseline
  does not). The baseline canary is adversarial (it explicitly instructs the
  model to call lens); on sonnet it leaked ~40% of the time and one group (g1)
  needed 2 canary attempts to pass. The other three groups passed first try.
- **Control-arm leakage audit:** **0 of 78 scored baseline runs reached a lens
  tool** (`control_stats.lens_runs = 0` on every task). The adversarial-canary
  leak did **not** translate into scored-task contamination: on real questions
  (where the model is not told to call lens) the baseline isolation held.

## Gate 1 - Adoption: **FAIL**

Criterion (plan): the composed tasks (0080-0085) reach **one `lens_run` program**
(allow `+lens_recall`), not atomic chains, for the majority of lens-arm runs.

- **Composed one-program shape: 0 of 6.** Every composed task's lens-arm run was
  a multi-call chain, not a single composed program:
  - `0080` 5x `lens_run` + `lens_skeleton` + `Read` (+ToolSearch)
  - `0081` 14x `lens_run` (+ToolSearch, +ReportFindings)
  - `0082` `lens_graph` + 6x `lens_skeleton` + 2x `Read` + 2x `lens_run`
  - `0083` 6x `lens_skeleton` + 3x `lens_recall` + `lens_run`
  - `0084` `lens_grep_ast` + `lens_run` + 2x `lens_skeleton` + 2x `lens_recall`
  - `0085` `lens_graph` + `lens_skeleton` (no `lens_run` at all)
- The composed **front door is reached** (`lens_run` appears in 5 of 6), but the
  model iterates many small calls rather than composing one program; 2 of 6
  (`0080`, `0082`) also mix in raw `Read`.
- **Overall adoption_rate: 91.0% (71/78 lens-arm runs reached a lens tool);** all
  6 composed tasks reached lens on all 3 runs (`lens_runs = 3/3` each).

**Verdict FAIL** on the composed-one-program criterion. Tool *reach* is strong
(91%), but the composed-program *shape* did not materialize on sonnet-medium.
This is the evidence the plan reserved for deciding whether the atomic front door
gets demoted later. Note: the folded record retains only each cell's first-run
tool sequence, so the shape judgment is over 6 first-run sequences; `lens_runs =
3/3` confirms all three runs of each composed task reached lens.

## Gate 2 - Economics: **FAIL**

Criterion (plan): graph/skeleton mechanism lens tokens `<=` same-run control;
search `<= -40%`. Tokens are the K=3 mean per task, summed over the mechanism's
tasks.

| Mechanism | n | Control tokens | lens tokens | Delta | Target | Result |
| --- | ---: | ---: | ---: | ---: | --- | --- |
| graph (discovery) | 5 | 3,348,596 | 2,759,560 | **-17.6%** | `<= control` | **PASS** |
| skeleton | 5 | 831,668 | 1,161,723 | **+39.7%** | `<= control` | **FAIL** |
| search | 8 | 2,501,626 | 1,834,324 | **-26.7%** | `<= -40%` | **FAIL** |
| darkroom (composed) | 8 | 6,981,604 | 5,988,872 | -14.2% | (not gated) | context |

- **graph** clears its bar (lens is 17.6% cheaper than control).
- **skeleton** is 39.7% *more* expensive on lens, driven by `lens_skeleton` on
  0070 (+58%; retagged from overview, and a stale-GT accuracy artifact — see
  Post-run corrections) and `lens_grep_ast` (+88%); only `lens_map` (-7%) beats
  control.
- **search** is cheaper (-26.7%) but misses the -40% bar.

**Verdict FAIL** (graph passes; skeleton and search miss their targets).

## Gate 3 - Reliability: **PASS**

- **T5 build lock** `cargo test --test index_concurrency`: **4 passed, 0 failed**
  (`concurrent_cold_sessions_build_the_index_once`,
  `concurrent_cold_sessions_build_the_graph_once`,
  `stale_lock_with_dead_pid_is_reclaimed`, + 1).
- **T6 nested auto-build** `cargo test --test e2e_tests nested`: **3 passed, 0
  failed** (`nested_repo_auto_builds_on_federation_miss`,
  `nested_autobuild_off_skips_with_note`,
  `nested_autobuild_max_files_skips_oversized_nested_repo`).

**Verdict PASS.**

## Summary

| Gate | Verdict | Headline number |
| --- | --- | --- |
| Adoption | **FAIL** | 0/6 composed = one program; 91.0% overall reach |
| Economics | **FAIL** | graph -17.6% (pass); skeleton +39.7%, search -26.7% (both miss) |
| Reliability | **PASS** | T5 4/4, T6 3/3 |

Overall end-to-end on the 26-task set: lens **452k vs 526k tokens (-14.0%)**,
accuracy **82% vs 85%**, time **27.4s vs 40.2s (-31.8%)**. Two of three gates
fail: the composed-program front door is reached but not composed into one
program, and the skeleton/search token targets are not met on sonnet-medium.

## Post-run corrections (2026-07-20)

- **0070 ground truth was stale** (`fn_count: 10` vs the correct 11; both
  arms' stored first-run answers were 11). The stored success rates (control
  1/3, lens 0/3) graded against the stale value and cannot be recomputed
  offline, because the folded record kept only first-run answers. The
  "`lens_overview` +58% tokens with a real accuracy loss 0% vs 33%" line in
  Gate 2 is therefore an artifact of a stale GT, not an overview defect. GT
  is now fixed for future runs; the harness now retains per-run answers.
- **0072** (the `lens_grep_ast` +88% row): the lens arm's stored sequence is
  `Grep, Grep, Grep`, zero lens calls (one of the 7 scored adoption misses).
  The +88% is lens-arm fixed context overhead plus one extra round, not
  grep_ast cost.
- The lens arm carries a measurable fixed floor: on 1-round tasks (0074: one
  `lens_search` vs one `Grep`) lens 147.0k vs control 121.1k tokens (+21%),
  i.e. static per-round context (schemas + guide), which bounds how far
  short-task buckets can drop.
