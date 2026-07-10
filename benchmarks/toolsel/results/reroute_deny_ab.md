# Reroute rails A/B — off vs nudge vs **deny** (gast + bagg)

Follow-up to `reroute-rail-grounding`, which proved the four nudge rails **fire but do not convert**
(matched A/B chain rate flat `0.308 → 0.308`, 33 nudges vs 0 conversions). This run adds the third
arm — **deny** — for the two rails where a strictly-better lens alternative exists (`gast`, `bagg`),
after the T1 classifier recall fix made the gast deny reachable on the model's real (regex-escaped)
Grep patterns.

## Setup

- **Branch:** `feat/reroute-deny-and-gast-recall` (worktree). Deny rails wired in `route_inner` behind
  `LENS_GREP_AST_DENY` / `LENS_BASH_AGG_DENY`, both **default OFF** (dark-launch).
- **Model / effort:** `claude-sonnet-5` / `low` (the grounding baseline).
- **Harness:** `bench_toolsel`, arm flags injected via `LENS_TOOLSEL_SETTINGS` (env-only `--settings`,
  since settings.json env wins over process env). Ledger isolated with `LENS_NO_GLOBAL_MIRROR=1` so the
  bench cannot pollute the real usage ledger.
- **Six arms** = {gast, bagg} × {off, nudge, deny}. Each nudge arm sets `LENS_*_NUDGE=1`; each deny arm
  `LENS_*_DENY=1`. All bagg arms run at `LENS_ROUTING=steer` + `LENS_DEFER_BASH_TO_RTK=0` (the deny gates
  on `steers()`; `defer=0` disables the RTK wrap that would otherwise pre-empt bagg — the T3 deny is also
  placed before the wrap). gast arms run at `LENS_ROUTING=steer`.
- **Per-rail fire count** read from `routing_nudges.tsv`'s key column (`grep-ast` / `bash-agg`), not
  `routing.log`.

### Reductions from the plan (documented, not silent)

- **`--runs 1`, not `--runs 3`** — cost/quota. Each arm is ~20 live `claude -p` tasks (~4 min); six arms
  at runs=3 would be ~72 min of headless calls plus 3× quota. **Consequence: the chain-rate numbers are
  single-sample (each task is 0/1, ±0.05 per task); arm-to-arm rate deltas below ~0.15 are within noise.**
  The fire-count result is unaffected by this (it is a deterministic classifier signal).
- **20 tasks (17 mined), not 26** — this is the current master toolsel set; the grounding `0.308`
  baseline was measured on that run's 26-task set, so treat `0.308` as a reference point, not a
  same-corpus comparison.

## Result 1 — deny fires (the clean, deterministic result)

The T1 recall fix (normalize escaped/regex Grep patterns before the shape regexes) is confirmed **live**:
the gast deny fires on the model's actual patterns, which the pre-fix classifier missed entirely.

| rail | off | nudge | **deny** |
|------|----:|------:|---------:|
| gast (`grep-ast` fires) | 0 | 6 | **4** |
| bagg (`bash-agg` fires) | 0 | 6 | **4** |

Both deny arms fire > 0 — **predicate met.** (Fire-count variance between nudge=6 and deny=4 is just
which patterns the model happened to emit in a single run, not a classifier difference — deny and nudge
share the same classifier and one-shot key.)

## Result 2 — chain-conversion rates (the finding under test)

Rates are over all 20 tasks (`overall_*`) and the 17-task mined subset (`mined_*`). `lens_first` = the
run led with a lens tool (the grounding "chain rate", baseline `0.308`); `rate` = task passed using the
intended tool; `strict` = strict-match.

| arm | overall lens_first | overall rate | overall strict | mined lens_first | mined rate |
|-----|-------------------:|-------------:|---------------:|-----------------:|-----------:|
| gast-off   | 0.300 | 0.500 | 0.250 | 0.176 | 0.412 |
| gast-nudge | 0.250 | 0.350 | 0.200 | 0.176 | 0.294 |
| **gast-deny** | 0.250 | **0.550** | 0.250 | 0.118 | **0.471** |
| bagg-off   | 0.250 | 0.500 | 0.150 | 0.176 | 0.471 |
| bagg-nudge | 0.250 | 0.450 | 0.250 | 0.176 | 0.353 |
| **bagg-deny** | **0.300** | **0.650** | 0.250 | 0.176 | **0.588** |

### Reading

- **`lens_first` (the 0.308 grounding metric) stays noise-flat** across all arms (0.25–0.30). Nudge does
  not convert — reproducing the grounding finding — and at `--runs 1` deny does not move `lens_first`
  either.
- **On the `rate` (task-success) metric, deny is the top arm for both rails**, and consistently so on
  both the overall and the mined subset: gast `rate` deny 0.550 > off 0.500 > nudge 0.350; bagg `rate`
  deny 0.650 > off 0.500 > nudge 0.450 (mined: gast 0.471 > 0.412 > 0.294; bagg 0.588 > 0.471 > 0.353).
  The pattern — **deny ≥ off > nudge** — is directionally consistent across four independent measurements
  (2 rails × 2 subsets). This is more encouraging than the grounding nudge result, where nudge sat at or
  below off.
- **Interpretation:** a deny with a reason string that names the exact lens call and promises the retry
  passes appears to help the model reach a passing chain more than an advisory nudge does — but the
  effect shows up in task success, not in "lens is the *first* tool", and at `n=1` the per-arm deltas
  (0.05–0.20) are not statistically separable from single-sample noise.

## Verdict

- **Predicate: met.** All three arms measured for both rails (fire counts + chain rates); both deny arms
  fire > 0; the gast recall fix is confirmed to make the deny reachable on real patterns.
- **Conversion: promising but inconclusive at `--runs 1`.** Deny is the best arm on task-success for both
  rails on both subsets (deny ≥ off > nudge), but `lens_first` is flat and the sample is single-run.
- **Recommendation before any default-on flip:** re-run at `--runs ≥ 3` (ideally 5) to get mean ± stddev
  on the `rate` and `lens_first` metrics, and segment the rate on the tasks where the rail actually
  fired (join `routing_nudges.tsv` fire records to task ids) so the diluted aggregate does not hide the
  signal. Until then both flags remain **OFF** (dark-launch) per the plan; promotion is a separate
  decision.

## Appendix — per-task lens_first flips (off → nudge → deny)

Single-run, so individual flips are noise, but they show where each arm's fire landed:

```
gast:
  0003_search_quarantine                off=1.0 nudge=0.0 deny=1.0
  0005_readonly_extract_name_reuse       off=1.0 nudge=1.0 deny=0.0
  0009_nav_bench_toolsel_entrypoint      off=1.0 nudge=1.0 deny=0.0
  0010_grep_include_bodies_occurrences   off=0.0 nudge=0.0 deny=1.0
bagg:
  0001_skeleton_forge                    off=0.0 nudge=0.0 deny=1.0
  0005_readonly_extract_name_reuse       off=0.0 nudge=0.0 deny=1.0
  0009_nav_bench_toolsel_entrypoint      off=1.0 nudge=0.0 deny=0.0
  0010_grep_include_bodies_occurrences   off=0.0 nudge=1.0 deny=0.0
  0012_bigread_server_tool_surface       off=1.0 nudge=0.0 deny=1.0
  0013_bigread_mod_throttle_consts       off=0.0 nudge=1.0 deny=0.0
```

Raw snapshots (per-arm `nudges_*.tsv` + `toolsel_*.json`) live under the run's scratchpad
`reroute-deny-ab/snapshots/`.
