# Stage-3 rail counter readout — 2026-07-09

Live snapshot of the raw cumulative rail counters across every store the dashboard's
**global** scope enumerates. No epoch exists yet (that's T4) — these are the
lifetime-cumulative `stats` table values, not a windowed delta. This file is a
read-only report; it changes no code and no counters.

## Method

- **Store-dir enumeration** mirrors `src/obs/dashboard.rs`'s `scope=global` branch
  (`:184-225`): `crate::rtk::home_root()` (`~/.lens`, or `$LENS_HOME`) opens the
  machine-global `SessionStore` (`session.db`), whose `distinct_projects()`
  (`src/session/store.rs:333-341`) lists every project path that has ever logged a
  session event, newest-first. The dashboard filters that list to `<project>/.lens`
  dirs that `is_dir()` and are not the home dir itself, takes the first 30, and sums
  `stats` keys across all of them via `Store::open(dir).get_stat(k)` — the exact
  per-dir sum `grep_scope_aggregate`/`reroute_aggregate` (`src/obs/stats.rs:319,390`)
  perform. This snapshot reproduces that enumeration and that summation by hand with
  `sqlite3 <dir>/.lens/store.db "SELECT key,value FROM stats"` per dir.
- **Key lists** are `GREP_SCOPE_KEYS` (`stats.rs:298-309`), `REROUTE_PREFIXES = [gsym,
  rskel, bagg, elink, gast, rovr]` (`:348`), `REROUTE_CLASSES = [lens, grep, read,
  bash, edit, other]` (`:352`).
- **Adoption / guard denominator.** Not every `{p}_would_fire` has an observed
  follow-up tool call (session can end first), so the percentage denominator used
  here is the **next-class sum** (`Σ next_{class}`), not `would_fire` — the same
  convention the plan's own worked example uses (`gsym 28/52`, where 52 is the
  next-sum, not the 55 would-fires). Both `would_fire` (n) and the next-sum (the CI
  trial count) are reported so the gap is visible.
- **CI** is the Wilson score interval (a normal-approximation interval, better
  behaved than Wald at small n or when a class count is 0), 95% (`z=1.96`).
- Real store dirs found via the global-scope filter (8 candidates; only 6 have any
  `stats` rows at all):

  ```
  /Users/gene/Documents/AI Stuff/lens
  /Users/gene/Documents/AI Stuff/Meridian/app
  /Users/gene/Documents/Projects
  /Users/gene/Documents/Fubo/growth-landing
  /Users/gene/Documents/AI Stuff/Meridian/app-eval-receipts   (store.db present, 0 rows)
  /Users/gene/.lens-loop
  /Users/gene/Documents/AI Stuff/dotfiles                     (store.db present, 0 rows)
  /Users/gene/Documents/Projects/refinery
  ```

## Full table — all seven rails

| rail | mech | would_fire (n) | next breakdown (lens/grep/read/bash/edit/other or lens/grep/shellgrep/other) | n (next-sum, CI trials) | adoption % (k/n) | 95% CI adoption | guard % (k/n) | 95% CI guard | bar | verdict |
|---|---|---:|---|---:|---:|---|---:|---|---|---|
| **grep-scope** | deny | 19 | lens 12 / grep 5 / shellgrep 0 / other 0 | 17 | 70.6% (12/17) | [46.9, 86.7] | shellgrep 0.0% (0/17) | [0.0, 18.4] | adoption ≥55%, guard <25% | **PASS** |
| **gsym** | deny | 55 | lens 28 / grep 13 / read 9 / bash 2 / edit 0 / other 0 | 52 | 53.8% (28/52) | [40.5, 66.7] | grep 25.0% (13/52) | [15.2, 38.2] | adoption ≥55%, guard <25% | **AT-BAR** |
| **rskel** | deny | 25 | lens 7 / grep 0 / read 7 / bash 3 / edit 5 / other 3 | 25 | 28.0% (7/25) | [14.3, 47.6] | read 28.0% (7/25) | [14.3, 47.6] | adoption ≥55%, guard <25% | **FAIL** |
| **gast** | nudge | 3 | lens 2 / grep 1 / read 0 / bash 0 / edit 0 / other 0 | 3 | 66.7% (2/3) | [20.8, 93.9] | n/a (nudge, no guard bar) | — | adoption ≥55% | **N-TOO-SMALL** |
| **elink** | nudge | 8 | lens 2 / grep 0 / read 0 / bash 1 / edit 5 / other 0 | 8 | 25.0% (2/8) | [7.1, 59.1] | n/a | — | adoption ≥55% | **FAIL** |
| **bagg** | nudge | 27 | lens 2 / grep 0 / read 4 / bash 14 / edit 2 / other 4 | 26 | 7.7% (2/26) | [2.1, 24.1] | n/a | — | adoption ≥55% | **FAIL** |
| **rovr** | nudge | 180 | lens 19 / grep 0 / read 63 / bash 46 / edit 45 / other 6 | 179 | 10.6% (19/179) | [6.9, 16.0] | n/a | — | adoption ≥55% | **FAIL** |

`shadow_next_*` for all seven rails, summed across every populated dir, are **all
zero** right now — expected, since every rail's live flag is currently `1`
(deny/nudge), not shadow-only, per `~/.claude-personal/settings.json`; the shadow
counters have simply never had a shadow-only window to accumulate in.

## Per-dir breakdown

Only two of the eight candidate dirs contributed any rail-relevant counters; the
rest logged only unrelated stats (`darkroom_calls`, `graph_nodes`, `index_chunks`,
...) or nothing at all.

**`/Users/gene/Documents/AI Stuff/lens`** (this repo's main checkout — carries the
overwhelming majority of every rail's counters):

| key | value | key | value | key | value |
|---|---:|---|---:|---|---:|
| grep_scope_single | 56 | grep_scope_broad | 19 | grep_scope_would_deny | 19 |
| deny_next_lens | 12 | deny_next_grep | 5 | deny_next_shellgrep/other | 0 / 0 |
| gsym_would_fire | 55 | gsym_next_lens/grep/read/bash | 28 / 13 / 9 / 2 | — | — |
| rskel_would_fire | 25 | rskel_next_lens/read/bash/edit/other | 7 / 7 / 3 / 5 / 3 | — | — |
| bagg_would_fire | 26 | bagg_next_read/bash/edit/other | 4 / 13 / 2 / 4 | bagg_shadow_next_lens | 1 |
| elink_would_fire | 8 | elink_next_lens/bash/edit | 2 / 1 / 5 | — | — |
| gast_would_fire | 3 | gast_next_lens/grep | 2 / 1 | — | — |
| rovr_would_fire | 173 | rovr_next_lens/read/bash/edit/other | 19 / 62 / 43 / 42 / 6 | rovr_shadow_next_bash | 1 |

**`/Users/gene/Documents/AI Stuff/Meridian/app`** (adds only to `bagg` and `rovr`):

| key | value | key | value |
|---|---:|---|---:|
| rovr_would_fire | 7 | rovr_next_edit/bash/read | 3 / 3 / 1 |
| bagg_would_fire | 1 | bagg_next_bash | 1 |

**`/Users/gene/Documents/Projects`, `/Users/gene/Documents/Fubo/growth-landing`,
`/Users/gene/.lens-loop`, `/Users/gene/Documents/Projects/refinery`** — `stats`
rows present but none of them are `grep_scope_*`/rail-prefix keys (only
`darkroom_calls`, `graph_nodes`, `graph_edges`, `index_chunks`,
`raw_bytes_processed`, `bytes_returned_to_context`). Contribute 0 to every rail
counter above.

**`/Users/gene/Documents/AI Stuff/Meridian/app-eval-receipts`,
`/Users/gene/Documents/AI Stuff/dotfiles`** — `.lens/store.db` exists (passes the
dashboard's `is_dir()` filter) but the `stats` table has 0 rows. Contribute 0.

## Caveats

- **This repo's `grep-scope` counters may include build-time demo pokes from
  2026-07-08** (the rail's launch day) that are indistinguishable from organic
  routing in the raw cumulative keys — there is no epoch marker yet to exclude
  them. The 71%/0% numbers above match the plan's Context-table prior reading
  exactly (12/17 and 0/17), so if demo noise is present it has been present
  unchanged since that reading; it is not possible to separate it out further from
  these keys alone.
- **The `gast` counts straddle the `9c84d6f` recall fix** (grep-ast classifier
  regex-escape handling, merged after the rail went live) — `gast_would_fire=3`
  mixes fires from before and after that fix, so the n=3 sample is not a clean
  read of post-fix classifier behavior on top of already being too small to trust.
- **The epoch mechanism (T4) closes this class of ambiguity going forward**: once
  `lens stats --epoch` stamps a baseline, `grep_scope_aggregate`/
  `reroute_rail_aggregate` will report only counts accumulated *since* the stamp,
  so future readouts won't need this caveat.
- **Drift vs. the plan's Context-table prior reading (2026-07-09):** grep-scope
  (19 would-deny, 12/17=70.6%) and gsym (55 would-fire, 28/52=53.8%, guard
  13/52=25.0%) and gast (3 would-fire, 2/3=66.7%) reproduce the prior numbers
  exactly — no live traffic has touched those counters since. `rskel` drifted
  (n 16→25 next-observations, adoption 25%→28.0%, guard/read 31%→28.0%), `bagg`
  drifted (n 15→26, adoption 0%→7.7%), `elink` drifted (n 6→8, adoption 33%→25.0%),
  and `rovr` grew in volume but held its verdict (n 93→179, adoption ~11%→10.6%).
  None of the drift changes any rail's PASS/FAIL/AT-BAR/N-TOO-SMALL call.

## Verdicts

- **grep-scope: PASS** — adoption 70.6% (12/17, CI [46.9, 86.7]) clears the ≥55%
  bar; guard (shellgrep) 0.0% (0/17, CI [0.0, 18.4]) clears the <25% bar with room
  to spare. Matches the plan's stated PASS (71%/0%).
- **gsym: AT-BAR** — adoption 53.8% (28/52) sits just under the 55% bar but well
  within its own CI [40.5, 66.7], which spans the bar; guard 25.0% (13/52) sits
  **exactly** at the <25% cutoff — a breach by definition, but its CI
  [15.2, 38.2] comfortably spans both sides of 25%, so this is not distinguishable
  from a pass at current n. Matches the plan's stated "fails guard at exactly
  25.0%, within CI."
- **rskel: FAIL** — adoption 28.0% (7/25) is far below the 55% bar (CI
  [14.3, 47.6] does not reach it); guard (read) 28.0% (7/25) is above the <25%
  cutoff, also outside CI-noise territory relative to the bar being this far off.
- **gast: N-TOO-SMALL** — would_fire=3, next-sum=3; adoption 66.7% (2/3) would
  clear the bar at face value but a 3-trial CI of [20.8, 93.9] spans nearly the
  entire probability space and straddles the recall-fix boundary (see caveats) —
  not a usable verdict either way yet.
- **elink: FAIL** — adoption 25.0% (2/8, CI [7.1, 59.1]) is below the 55% bar; CI
  is wide (small n) but its upper bound (59.1%) barely clears the bar while the
  point estimate sits well under it — treat as fail pending more volume, not a
  pass-in-waiting.
- **bagg: FAIL** — adoption 7.7% (2/26, CI [2.1, 24.1]) is far below the 55% bar
  with a CI that doesn't come close to reaching it.
- **rovr: FAIL** — adoption 10.6% (19/179, CI [6.9, 16.0]) is far below the 55%
  bar; this is the largest-n rail of the seven, so this is the most
  statistically confident FAIL in the set.
