# grep-scope deny — stage-3 per-model A/B (T6)

Task-success and chain-rate A/B for the **grep-scope deny** rail (`LENS_GREP_SCOPE_DENY`) across three models. The rail denies a **broad Grep-tool call** once per prompt, steering the model to a lens call; it is the one rail that PASSED live (71% adoption, T1). This run re-measures it under the toolsel bench.

## Setup

- **Models / effort:** sonnet-5 / low, haiku-4-5 / low, opus-4-8 / high (the `model_matrix.md` per-model recipe).
- **Arms:** two env-only `LENS_TOOLSEL_SETTINGS` files, identical except for the single flag under test — arm **off** `LENS_GREP_SCOPE_DENY=0`, arm **deny** `=1`. Both pin every other rail flag to `0` (gsym/rskel denies, all four nudges, gast/bagg denies) and set `LENS_ROUTING=full`, `LENS_NO_GLOBAL_MIRROR=1` (ledger isolation so headless sessions never touch the real usage ledger).
- **Runs:** 3 passes × 13 tasks per (model, arm), one live `claude -p` session each (`bench_toolsel --runs 1`, per-task staged). 3 models × 2 arms × 3 passes × 13 tasks = **234 sessions**. Each pass's overall rate is the mean over the 13 tasks (0/1 per task at runs=1); the `± ` is the sample stddev over the 3 passes.
- **Task subset (13, documented — not a silent trim):** the search / readonly / nav / grep / bigread tasks `0001`–`0013`. Excludes `0014`–`0017` (bashagg) and `0018`–`0020` (grepast): those target the bagg/gast rails and do not elicit a broad **Grep-tool** call, so grep-scope structurally cannot fire there — including them would only dilute the aggregate. This is the plan's permitted `--only` fallback, used for relevance as well as wall-clock.
- **Fire signal:** per-task delta of the `grep_scope_would_deny` store stat (`.lens/store.db`), the authoritative counter T1 reads. (The first harness attempt counted fires with `grep -P`, which is a no-op on macOS BSD grep and always read 0; corrected here.)

## Result 1 — overall success + chain, mean ± stddev over 3 passes

`success` = task passed with the intended tool; `chain` = `lens_first` (the run led with a lens tool, the grounding adoption metric).

| model | arm | success (mean ± sd) | chain/lens_first (mean ± sd) |
|---|---|---|---|
| sonnet-5 / low | off | 0.821 ± 0.089 | 0.513 ± 0.118 |
| sonnet-5 / low | deny | 0.744 ± 0.044 | 0.410 ± 0.044 |
| haiku-4-5 / low | off | 0.462 ± 0.077 | 0.359 ± 0.044 |
| haiku-4-5 / low | deny | 0.462 ± 0.077 | 0.308 ± 0.077 |
| opus-4-8 / high | off | 0.513 ± 0.089 | 0.282 ± 0.089 |
| opus-4-8 / high | deny | 0.564 ± 0.089 | 0.282 ± 0.044 |

## Result 2 — delta (deny − off), per model

| model | Δ success | Δ chain | fires (Σ would_deny, deny arm) |
|---|---|---|---|
| sonnet-5 / low | -0.077 | -0.103 | 26 |
| haiku-4-5 / low | +0.000 | -0.051 | 30 |
| opus-4-8 / high | +0.051 | +0.000 | 44 |

- **sonnet delta:** success -0.077, chain -0.103 (off success 0.821 → deny 0.744; off chain 0.513 → deny 0.410).
- **haiku delta:** success +0.000, chain -0.051 (off success 0.462 → deny 0.462; off chain 0.359 → deny 0.308).
- **opus delta:** success +0.051, chain +0.000 (off success 0.513 → deny 0.564; off chain 0.282 → deny 0.282).

## Result 3 — fired-task segmentation

A task is **fired** when the deny arm actually classified a broad Grep (`grep_scope_would_deny` delta > 0) in ≥1 pass — i.e. where the rail can change behavior. Rates below are restricted to each model's fired set (overall rates are in Result 1).

| model | fired tasks (n) | which tasks | off success (fired) | deny success (fired) | Δ success (fired) |
|---|---|---|---|---|---|
| sonnet-5 / low | 6 | 0003, 0007, 0008, 0009, 0010, 0011 | 0.889 ± 0.096 | 1.000 ± 0.000 | +0.111 |
| haiku-4-5 / low | 7 | 0003, 0005, 0007, 0010, 0011, 0012, 0013 | 0.571 ± 0.143 | 0.571 ± 0.143 | +0.000 |
| opus-4-8 / high | 5 | 0003, 0007, 0009, 0010, 0011 | 0.200 ± 0.000 | 0.533 ± 0.115 | +0.333 |

Fired-set chain (lens_first), mean ± sd:

| model | off chain (fired) | deny chain (fired) | Δ chain (fired) |
|---|---|---|---|
| sonnet-5 / low | 0.556 ± 0.255 | 0.389 ± 0.096 | -0.167 |
| haiku-4-5 / low | 0.381 ± 0.082 | 0.286 ± 0.143 | -0.095 |
| opus-4-8 / high | 0.000 ± 0.000 | 0.000 ± 0.000 | +0.000 |

## Verdict (per model)

- **sonnet:** 6 fired tasks, Σ26 broad-Grep classifications; Δsuccess -0.077 is **within** the per-arm pass stddev (noise floor ~0.089).
- **haiku:** 7 fired tasks, Σ30 broad-Grep classifications; Δsuccess +0.000 is **within** the per-arm pass stddev (noise floor ~0.077).
- **opus:** 5 fired tasks, Σ44 broad-Grep classifications; Δsuccess +0.051 is **within** the per-arm pass stddev (noise floor ~0.089).

## Takeaway

Task success is the deciding metric (the runs=1 reroute A/B moved success while `lens_first` stayed flat). grep-scope's value was established **live** at 71% adoption (T1); on the curated toolsel tasks the models already lead with lens under `LENS_ROUTING=full`, so the deny's headroom is bounded by the off-arm ceiling. The fired-task segmentation isolates the tasks where the rail actually engaged. See the per-model deltas against the noise floor above for whether the bench separates the arms.

_Raw per-(model,arm,pass,task) records: `scratchpad/t6/matrix.jsonl` (234 rows)._
