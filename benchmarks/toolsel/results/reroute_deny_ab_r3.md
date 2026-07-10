# gast + bagg deny A/B at runs=3 (T7)

Follow-up to the runs=1 `reroute_deny_ab.md` (which found **deny ≥ off > nudge** on task-success but at n=1 the deltas were within single-sample noise). This re-runs the two deny rails at **3 passes** with per-rail fired-task segmentation, to test the signal against a real noise floor.

## Setup

- **Model / effort:** claude-sonnet-5 / low (the grounding recipe).
- **Arms:** off vs **deny** (`LENS_GREP_AST_DENY=1` + `LENS_BASH_AGG_DENY=1` together, nudges off). Two env-only `LENS_TOOLSEL_SETTINGS` files, all other rail flags pinned `0`, `LENS_NO_GLOBAL_MIRROR=1` (ledger isolation).
- **Routing — CORRECTION to the plan.** The plan text said `LENS_ROUTING=nudge`, but the gast/bagg deny gates on `Level::steers()`, which is `true` only for `Steer | Full` (`src/routing/mod.rs:75`). At `nudge`, `steers()` is `false` and **neither deny can fire**. This run uses `LENS_ROUTING=steer` + `LENS_DEFER_BASH_TO_RTK=0` (so bagg's Bash reaches the router before the rtk wrap), matching the proven `reroute_deny_ab.md` recipe. Verified live: a grepast task fired gast (`gast_would_fire` +2), a bashagg task fired bagg (`bagg_would_fire` +4).
- **Runs:** 2 arms × 3 passes × 20 tasks = **120 sessions** (`bench_toolsel --runs 1`, per-task staged). Per-pass overall rate = mean over 20 tasks; `±` = sample stddev over the 3 passes.
- **Fire signal:** per-task delta of `gast_would_fire` / `bagg_would_fire` (`.lens/store.db`) — the classifier-hit counters the hook shadow-plane bumps. A task is **fired** for a rail when that rail's counter rose in the deny arm.

## Result 1 — overall (all 20 tasks), mean ± stddev over 3 passes

| arm | success | chain/lens_first |
|---|---|---|
| off | 0.567 ± 0.058 | 0.250 ± 0.050 |
| deny | 0.750 ± 0.050 | 0.350 ± 0.100 |
| **Δ (deny−off)** | +0.183 | +0.100 |

Fires: gast `would_fire` Σ30 over 6 tasks; bagg `would_fire` Σ82 over 5 tasks.

## Result 2 — fired-task segmentation (the signal under test)

Restricted to each rail's fired set (deny arm classifier hits). This is where the diluted aggregate would otherwise hide the effect.

| rail | fired tasks (n) | which | off success | deny success | Δ success | off chain | deny chain | Δ chain |
|---|---|---|---|---|---|---|---|---|
| gast | 6 | 0001, 0006, 0012, 0018, 0019, 0020 | 0.611 ± 0.096 | 0.889 ± 0.096 | +0.278 | 0.222 ± 0.096 | 0.278 ± 0.192 | +0.056 |
| bagg | 5 | 0014, 0015, 0016, 0017, 0020 | 0.133 ± 0.115 | 0.533 ± 0.115 | +0.400 | 0.067 ± 0.115 | 0.067 ± 0.115 | +0.000 |

## Recommendation (per flag)

Against the noise floor = the larger of the two arms' per-pass stddev on the rail's fired tasks. Promotion is the user's call; this is a read on the evidence only.

- **`LENS_GREP_AST_DENY`:** lean PROMOTE — Δsuccess +0.278 exceeds the ~0.096 noise floor; off 0.611 → deny 0.889 (n=6 fired tasks).
- **`LENS_BASH_AGG_DENY`:** lean PROMOTE — Δsuccess +0.400 exceeds the ~0.115 noise floor; off 0.133 → deny 0.533 (n=5 fired tasks).

_Raw per-(arm,pass,task) records with per-rail fire deltas: `scratchpad/t7/matrix.jsonl` (120 rows)._
