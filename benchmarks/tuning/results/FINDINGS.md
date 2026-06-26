# bench_tuning findings (T2): where the SQLite/FTS5 latency actually goes

Source: `cargo run --release --bin bench_tuning`. Committed snapshot: `results/baseline.json`
(run C below). Machine: macOS / aarch64 / 12 logical CPUs, **warm-only** (cold-cache
`sudo purge` not run; production is a long-lived warm server, so warm is the honest
headline). All figures are **median microseconds**. Where a signal is small I give the
spread across 6 runs (A,B,C clean + run1, run3, plus the loaded-machine outlier run4) so
robustness is visible. We gate on **within-run A/B vs the per-scale A-vs-A noise floor**,
never absolute ms.

## Headline

The per-query **repo walk dominates**, scales with file count, and is pure overhead when
the repo is unchanged. **Connection-open is negligible.** The one surprise the attribution
caught is that `configure_conn` (the per-op WAL setup), not the open, is the real
connection cost.

Per-stage attribution, one search call (run C / baseline.json):

| stage | 1x (12 files) | 10x (120) | 50x (600) |
|---|---:|---:|---:|
| `repo_walk` (`file_manifest`) | 129 | 361 | 1392 |
| `conn_open` | 28 | 28 | 28 |
| `configure_conn` (WAL) | 233 | 226 | 222 |
| `prepare` | 4 | 4 | 4 |
| `execute` (8-query mix¹) | 78 | 210 | 514 |
| `serialize` | 1 | 1 | 1 |
| isolated `Index::search` (mix) | 893 | 1799 | 3669 |
| through-handler (walk+search+ser) | 1070 | 2353 | 5398 |

`walk / conn_open` = **4.4-4.6x (1x), 12.5-12.8x (10x), 49-50x (50x)**, identical every
run. The walk grows with file count without bound on real repos; `conn_open` is flat ~28us.

¹ the mix includes two short-operator LIKE full-scans (`->`, `::`) that dominate `execute`
at scale and over-represent scan cost; see caveats.

## The gate question: is conn-open + configure + prepare a material end-to-end slice?

| as share of the through-handler call | 1x | 10x | 50x |
|---|---:|---:|---:|
| `conn_open` alone | 2.6% | 1.2% | 0.5% |
| `conn_open + configure + prepare` | **24.8%** | **11.0%** | 4.7% |

- **`conn_open` alone is negligible at every scale** (the red-team is correct; this is the
  T4 gate condition in the goal directive, and it is met -> T4 skips with evidence).
- The *combined* open+configure+prepare slice is material at small scale (25% at 1x),
  borderline at lens's own scale (~11% at 10x; lens indexes its ~112-file repo), and not
  material at 50x. It is addressable **only by connection reuse** (T4): a probe that skips
  the `journal_mode=WAL` pragma saves just **20-53us**; the remaining ~200us is the
  intrinsic first-touch WAL-index attach + first-MATCH FTS5 vtable init, which a
  `configure_conn` guard cannot remove (it just moves to the first query) but connection
  reuse amortizes.

## Ranking by measured, attributable, robust end-to-end headroom

### 1. T5 - debounce the per-query repo walk. #1 BY HEADROOM, but BLOCKED on a safe impl. NOT SHIPPED.

`repo_walk` is run on **every** `lens_search` / graph call via `ensure_index` /
`load_graph`, even when nothing changed. As a share of the handler call it is **12% (1x),
15% (10x), 26% (50x)** and **rises with repo size** (real repos have thousands of files;
600 files already costs ~1.4ms). The real `ensure_index` also reads a manifest file + a
store stat that this walk-only number excludes, so debouncing would save **at least**
these figures on every steady-state call. By headroom this is the biggest win.

**Why it is not shipped (implementation blocker, surfaced for a decision):**
1. The maintainers **already deliberately deferred exactly this**: `load_graph`
   (`src/server.rs:820`) reads *"The cheap per-query staleness walk (stat-only). Kept by
   design; a generation counter to skip even this is deliberately deferred."*
2. The walk **is** the change-detector. There is no cheap, portable, *correct* signal for
   an in-place content edit (it bumps the file mtime but no directory mtime), so any
   skip/debounce trades the "edit then immediately search reflects it" guarantee for a
   staleness window.
3. That weakened guarantee **breaks 3 existing handler tests** that write/delete a file
   then immediately assert it is reflected (`index_refreshes_when_a_file_is_added`,
   `index_prunes_deleted_files`, `graph_refreshes_when_a_file_is_added`), so the T5
   predicate's "existing tests pass" cannot hold for a default-on debounce.

A short-TTL debounce (e.g. 250-500ms) would be near-invisible in real agent usage (the
edit-to-search gap is seconds, not microseconds) but is a genuine product decision the
maintainers made the other way. Options, for the user to choose:
(a) accept bounded staleness: short-TTL debounce + an injected clock so the freshness
tests assert "reflected within TTL" instead of "immediately"; (b) leave the walk as-is
(the maintainers' choice; correctness over a sub-2ms walk); (c) a correctness-preserving
*speedup* of the walk (parallel stats / cheaper traversal), a different change with
uncertain payoff, not in this plan. Recommendation: (b)/(c) over (a) given lens's
correctness-first ethos; do not silently weaken edit-search freshness.

### 2. T3 - `synchronous=NORMAL` on the write path. PROMOTE. `temp_store=MEMORY` REJECT.

W2 per-commit re-index latency vs the WAL+`synchronous=FULL` default:

| candidate | clean runs (A,B,C) | run1 | run3 | run4 (loaded) | verdict |
|---|---|---|---|---|---|
| `synchronous=NORMAL` | -15.8 / -16.4 / -18.2% | -13.6% | -11.8% | +2.3% | **net win** (~50-78us/commit) |
| `temp_store=MEMORY` | -0.5 to +3% | +3.4% | +3.2% | +2.4% | **no help, reject** |
| both | -14.9% | - | - | -0.5% | = sync alone |

`synchronous=NORMAL` is robustly net-positive (5 of 6 runs; run4 was machine-loaded). The
absolute saving is small on fast SSD (~50us/commit) and larger on slower fsync. Durability:
under WAL, `NORMAL` can lose the **last** committed transaction on power loss but **cannot
corrupt** the DB; `index.db` is a rebuildable cache (`index_path` rebuilds from source), so
this is acceptable. Ship `synchronous=NORMAL`, leave `temp_store=MEMORY` off, document the
tradeoff in `configure_conn`.

Note: the real edit-loop latency is **~1.8-2.2ms/edit**, itself dominated by the walk
`index_path` does (not the commit), so T5's logic matters here too.

### 3. T6 - `optimize` after bulk index. PROMOTE (low priority).

After 64 small incremental writes (fragmenting the FTS5 segment forest), an
`INSERT INTO chunks(chunks) VALUES('optimize')` improves read median by **+5.4 to +10.9%**
across runs (run C: 1177 -> 1077us) for a one-time **~5.5-7ms** cost. Real but modest; a
long-lived lens server accumulates exactly this fragmentation. On an already-compact build
the delta is ~0 (report as such).

### 4. T4 - connection reuse (thread_local). SKIP with evidence.

Same-path A/B (per-op open+configure+query vs a reused warm connection):

| | 1x | 10x | 50x |
|---|---:|---:|---:|
| reuse saves (clean runs) | 41-43% | 23-25% | 9.5-15.8% |
| absolute saved | ~340-380us | ~290-460us | ~250-450us |

The win is **robust and not negligible at small scale** (this *refutes* the simplest
"reuse is negligible" framing). But:

- The gate in the goal directive is **conn-open negligible -> skip**, and conn-open *is*
  negligible (0.5-2.6%). The material part is `configure_conn`, not the open.
- The saving is **sub-millisecond** absolute on an operation that already finishes in
  1-5ms, and it **shrinks as a share** as the repo grows (the walk, T5, is the durable win).
- **Zero concurrency benefit:** 12-thread p99 is 97ms per-op vs 87ms reused (~1.1x). The
  concurrent tail is **query-execution contention** on the LIKE full-scans, not
  connection-open, so reuse does not help the case that matters most under load.
- `thread_local` connection caching across an async MCP handler carries real correctness
  risk (never hold the borrow across `.await`; per-thread conns live for process life;
  tempdir tests must not get stale handles) that a sub-ms, concurrency-neutral win does not
  justify as a priority.

Verdict: **skip with evidence.** If small-repo search latency later becomes a priority,
T4 is a measured, ready improvement; re-gate it on the within-run noise floor at the target
scale (it was within noise / negative at 50x on the loaded run).

### Bonus, not in the task list: `cache_size` / `mmap_size` read pragmas.

At 50x both give a real read win: `cache_size=-8000` **-9.5 to -13.9%**, `mmap_size=256MB`
**-8.3 to -15.2%** (above the 50x noise floor of ~6-155us). At 1x/10x they are **within
noise (~0)**. Caveat: replicated fixtures over-represent duplicate trigrams, so the working
set is smaller than real code and these are an **upper bound**; the win is largely caching
the `->`/`::` LIKE full-scan pages. Low-risk and harmless at small scale. If lens targets
large monorepos, adding `cache_size=-8000` to `configure_conn` is a cheap, safe default
(flagged, not gated; not part of T3's scope).

## Candidates measured to do nothing or regress (reported, not hidden)

- `temp_store=MEMORY` (write and read): within noise / slightly negative. Off.
- `cache_size` / `mmap_size` at 1x and 10x: within noise (~0).
- `detail=column`: REJECTED by design, not benchmarked as a PRAGMA (it is a table-creation
  option). It would break `ranked_search`'s `snippet()` (needs token positions) and the
  trigram path. Left rejected; the read-path correctness gate (byte-identical output across
  all benchmarked configs, **PASS** at every scale) is what would catch such a change as a
  regression rather than a "win".
- T4 reuse for concurrency: no benefit (~1.1x).

## Caveats / confounds (disclosed)

- Warm-only, single machine; absolute ms vary run-to-run with machine load (run4 was
  loaded: its W2 and 50x reuse numbers are the outliers). The within-run A/B and ratios are
  stable; trust those, not absolute ms.
- The through-handler arm is real walk + real `Index::search` + real serialize. It excludes
  the `ops` side-log, the rmcp `Json`/`Parameters` newtypes, the tokio task spawn (all
  sub-us), and the real `ensure_index`'s manifest-file read + store stat (which make the
  real handler slightly heavier, strengthening T5).
- Replicated `code_search` fixtures over-represent duplicate trigrams -> 10x/50x cache and
  index-size numbers are an upper bound, not representative of diverse real code.

## Implementation outcome

- **T3 SHIPPED** (`src/obs/mod.rs`): `configure_conn` now sets `synchronous=NORMAL`. The
  bench's production-config check prints `journal_mode=wal, synchronous=1`; W2 confirms
  FULL->NORMAL is ~-13.6%/commit. 291 tests + bench_fitness green. `temp_store=MEMORY`
  left off (measured net-neutral).
- **T6 SHIPPED** (`src/index/mod.rs`): `index_path` runs FTS5 `optimize` after a bulk
  build (`files_read >= 64`); small edits skip it. After T6 the cold-bulk read median
  drops ~9% (1188 -> 1086us) and the bench's manual re-optimize collapses to ~0 delta /
  ~0.5ms, proving the build auto-optimized. New test
  `bulk_build_optimizes_and_stays_correct`; 291 tests + bench_fitness green.
- **T4 SKIPPED with evidence** (conn-open negligible; reuse is sub-ms, scale-shrinking,
  concurrency-neutral, and `thread_local`-across-async risk-heavy).
- **T5 NOT SHIPPED, surfaced for a decision** (#1 by headroom but a correct default-on
  debounce is blocked: it weakens edit-search freshness, breaks 3 existing tests, and was
  already deliberately deferred by the maintainers). See the T5 section above.

Net shipped: T3 + T6, both with measured, attributable, above-noise-floor wins, output
byte-identical, fitness gate green.
