# lens benchmarks — appendix

_This is the full measurement trail behind [BENCHMARKS.md](BENCHMARKS.md). Nothing here is recomputed; it is the same committed data, shown in full._

## Methodology

lens is benchmarked against the metrics the **headroom** project publishes,
but matched to where lens actually sits in the loop. There are two halves,
and they are not the same kind of measurement.

**Savings** is directly comparable to headroom's proof table: tokens entering
context **without** lens (a realistic naive-agent path) vs **with** it.
Token counts are real o200k_base BPE (`obs::count_tokens`, offline); raw
byte counts are shown alongside. Every row is segmented by the lens tool
that produced the saving (`darkroom` / `index` / `compression` / `discovery`),
because lens saves via different mechanisms than headroom — it mostly
*prevents* data entering context, where headroom *compresses* data that does. A
single blended percentage would hide which mechanism did the work.

**Accuracy** uses a task-based method, **not** GSM8K/TruthfulQA. Those measure
whether compressing a *prompt* preserves answer accuracy — faithful for a
prompt-path compressor like headroom. lens is an MCP tool provider that sits
*beside* the prompt path; nothing forces a QA prompt through `lens_run`. So
the faithful accuracy question is: *when the agent uses the darkroom / graph /
search instead of reading raw files, does it still answer correctly?* Each task
is run twice with the same model — **control** (raw fixtures, capped at a naive
context budget) vs **treatment** (the lens tool's compact output) — and
scored against deterministic ground truth. The result we want to state honestly
is **Δ acc ≈ 0 with a large token reduction**. A negative Δ on any mechanism is
surfaced loudly: it means that mechanism is dropping load-bearing context.

With neither `LENS_BENCH_BACKEND` (plan quota) nor `ANTHROPIC_API_KEY`,
the accuracy harness runs in **mock mode** (a context-presence oracle that tests
scoring/plumbing only) and the table below is marked pending a real-model run.

## Savings (full)

### Token savings (o200k_base BPE token counts)

Token savings, not byte savings: lens's compact outputs (graph JSON, columnar payloads) are token-denser than raw source, so the token reduction is the honest figure and runs lower than the byte reduction in the raw-bytes table below.

| Workload | Before | After | Savings | Mechanism |
| --- | ---: | ---: | ---: | --- |
| Code search (results across files) | 3,681 | 2,193 | 40% | index |
| Log debugging (buried root cause) | 2,853 | 181 | 94% | darkroom |
| Issue triage (structured payload) | 1,953 | 1,190 | 39% | compression |
| Codebase exploration (subtree) | 657 | 766 | 0% | discovery |
| File read (skeleton + recall) | 3,681 | 1,591 | 57% | skeleton |

### Raw bytes and naive-agent baseline (no /4 to trust)

| Workload | Before (bytes) | After (bytes) | Without lens, the agent… | Detail |
| --- | ---: | ---: | --- | --- |
| Code search (results across files) | 15,915 | 7,896 | Agent greps for the terms, then opens every matched file in full to read context. | 6 queries, 30 hits returned, 12 matched files read by the naive path |
| Log debugging (buried root cause) | 7,210 | 517 | Agent loads the entire log into context to locate the one FATAL line. | grep over 7210 bytes -> 517 bytes of matching lines (+context) |
| Issue triage (structured payload) | 8,902 | 3,327 | Agent loads the full structured triage payload (minified) into context. | reversible columnar (schema-once) + value-dictionary compaction; full payload recoverable via lens_recall (raw file 8903 bytes) |
| Codebase exploration (subtree) | 2,606 | 2,163 | Agent reads every source file in the subtree to map its structure. | discover summary (30 nodes, 41 edges) + one scoped lens_symbol |
| File read (skeleton + recall) | 15,915 | 5,785 | Agent reads each source file in full to understand its structure. | 12 files reduced to tree-sitter skeletons; full text recoverable via lens_recall (one ref/file) |

### Scale curve (real path at 1× / 10× / 50× the committed fixture)

The §0.1 diagnostic: savings that *rise* with size mean the fixture was too small (artifact); savings that stay *flat/low* mean a real weakness in the path.

| Workload | Mechanism | Scale | Before (bytes) | After (bytes) | Savings |
| --- | --- | ---: | ---: | ---: | ---: |
| Code search | index | 1× | 15,915 | 9,936 | 38% |
| Code search | index | 10× | 160,230 | 9,515 | 94% |
| Code search | index | 50× | 802,110 | 9,558 | 99% |
| Issue triage | compression | 1× | 8,902 | 3,327 | 63% |
| Issue triage | compression | 10× | 94,195 | 31,323 | 67% |
| Issue triage | compression | 50× | 476,155 | 158,287 | 67% |
| Codebase exploration | discovery | 1× | 2,606 | 2,145 | 18% |
| Codebase exploration | discovery | 10× | 26,690 | 6,250 | 77% |
| Codebase exploration | discovery | 50× | 134,010 | 16,617 | 88% |

**Classification.**
- **Code search (index): artifact.** 37% → 94% → 99%. The mechanism returns a fixed set of capped snippets regardless of corpus size, so savings rise sharply as the naive "read every matched file" baseline grows. The original 33% was the 12-file fixture, not the path.
- **Issue triage (compression): real weakness, now fixed.** Was flat at 33–37% across scale — the compactor was a naive value-dictionary that still repeated every field name on every row. After faithfully porting SmartCrusher's columnar schema-extraction (`DECISIONS.md`), it is 63% at 1× and 63→67% across scale. The ~33% residual is unique prose issue *bodies*, which no deterministic codec compresses — reported honestly rather than forced higher.
- **Codebase exploration (discovery): small-fixture artifact at 1×, realistic at scale.** 1× = 18% because the 2.6 KB / 7-file fixture is a toy, not a real "explore a codebase" session. The scaled figures (77% at 10×, 88% at 50×) are now realistic rather than a pessimistic bound: L15's scope-aware per-file (file,name) call resolution no longer links each call to all N replicated copies, so the old O(N²) cross-copy edge hairball is gone and the scoped subgraph stays proportional to the corpus. The production fat-subgraph case is additionally bounded by `Forge::maybe_compact`.

### Context Mode isolation + head-to-head

These savings come from `cargo run --bin bench_savings`, a standalone Rust binary
that calls lens's library functions **directly** (index / darkroom /
compression / discovery) — it does not route through any MCP server or hook, so
Context Mode's PreToolUse hooks cannot intercept the workload. The numbers are
lens's own.

**Context Mode (measured), same machine, same workloads.** CM is comparable only
where it has an equivalent mechanism:

| Workload | lens mechanism | Context Mode (measured) |
| --- | --- | --- |
| Code search | FTS5 index → ranked snippets | `n/a` — CM `lens_index`/`lens_search` index into a session-global FTS5 KB; the per-workload token figure can't be isolated from session state without faking it. |
| Log debugging | darkroom grep, matches only | `n/a` — CM `lens_run` runs the same grep; equivalent by construction, no independent CM compaction to measure. |
| Issue triage | columnar + dictionary JSON compaction | `n/a` — CM has no structural-JSON compactor; this is the headroom/SmartCrusher archetype, not a CM mechanism. |
| Codebase exploration | tree-sitter code graph | `n/a` — CM has no code graph. |

Every CM cell is `n/a` with a stated reason rather than a fabricated number. The
faithful head-to-head lens *was* built to win is **session recovery** (below),
which drives CM's real hook scripts.

## Tool-selection enforcement (full)

### Method

10 real `code-analyst` dispatches were mined from this project's own Claude Code session history (the 10 most recent, deduplicated by prompt): 2 raw telemetry/tool-surface audits, 1 dark-launch rail verification, 1 dashboard architecture map, 2 subsystem maps (accuracy harness, MCP server layer), 1 routing-machinery map, and 3 adversarial Tantivy-backend audits (concurrency, migration integrity, ranking parity). 4 tasks referenced worktrees that no longer existed because the work had since merged to `master`; those were retargeted to the main repo (same files, same content) rather than dropped.

Each task's original prompt was replayed unmodified through three agent variants in parallel:
- **`code-analyst`** (full-tools baseline). `Read`, `Grep`, `Glob`, `Bash`, plus all `lens_*` tools.
- **`code-analyst-lens-only`**. `lens_*` tools only; `Read`/`Grep`/`Glob`/`Bash` removed from the tool list entirely (an enforced allowlist, not a nudge or a deny-with-retry).
- **`code-analyst-no-lens`** (the control this section exists to rule out). `Read`/`Grep`/`Glob`/`Bash` only, no `lens_*` tools at all. Without this arm, "restricting the agent to lens tools saves tokens" is confounded with "restricting the agent's tool count at all saves tokens." This arm isolates lens's own contribution from the effect of restriction itself.

A fourth agent then judged each triple: given the original task and all three reports (no arm labels revealed), it scored `best`/`worst` across all three, and separately verdicted lens-only and no-lens against the full-tools baseline (`equal_or_better` / `worse` / `failed`), with required notes. A sample of every arm's `file:line` citations were independently re-verified against the live source rather than trusted from the judge's say-so.

Per-task and per-arm turns, tool calls, wall-clock, and token counts were extracted from the raw agent transcripts (`assistant` message count, `tool_use` block count, first/last timestamp delta, and summed `usage` fields per transcript).

### Per-task results

| Task | vs full: lens-only | vs full: no-lens | Full turns/tools/dur(s)/tok | lens-only turns/tools/dur(s)/tok | no-lens turns/tools/dur(s)/tok |
| --- | --- | --- | ---: | ---: | ---: |
| Mine usage telemetry | worse | equal/better | 64/39/280/2,555,580 | 32/19/117/1,119,889 | 47/32/160/1,357,649 |
| Audit tool surface | equal/better | equal/better | 34/20/131/2,116,645 | 21/13/86/949,343 | 41/25/132/1,932,236 |
| T11 rail verification | equal/better | equal/better | 65/42/166/4,025,856 | 40/27/112/2,282,884 | 44/28/115/2,377,832 |
| Dashboard seams map | worse | equal/better | 23/15/83/830,078 | 24/18/64/1,037,578 | 32/19/103/960,515 |
| Accuracy bench map | equal/better | equal/better | 20/12/85/965,768 | 26/17/102/906,446 | 41/27/126/1,634,102 |
| **Server tools map** | **worse** | **worse** | 30/20/81/891,586 | 31/21/87/1,962,941 | 26/14/91/708,390 |
| Routing machinery map | worse | equal/better | 51/32/151/2,603,920 | 30/18/111/2,116,748 | 42/26/136/2,172,911 |
| Tantivy concurrency audit | equal/better | equal/better | 48/27/295/3,199,630 | 29/20/193/1,886,399 | 43/23/252/1,747,579 |
| Tantivy integrity audit | equal/better (best of 3) | equal/better | 32/17/444/1,864,564 | 29/19/386/1,768,378 | 40/21/260/1,581,841 |
| Tantivy ranking parity audit | equal/better (best of 3) | equal/better | 35/18/210/1,897,173 | 22/13/128/978,662 | 21/12/98/751,737 |
| **Total** | **6/10 equal-or-better, 2/10 best-of-3** | **9/10 equal-or-better** | **402/242/1926s/20,950,800** | **284/185/1386s/15,009,268** | **377/227/1473s/15,224,792** |

Tokens are total per-transcript throughput (input + output + cache create + cache read).

### The control changes the story

Summed efficiency deltas look similar for both restricted arms (lens-only: -29% turns / -24% tools / -28% duration / -28% tokens vs full; no-lens: -6% turns / -6% tools / -24% duration / -27% tokens vs full). Total tokens and wall-clock savings are nearly identical between them. What is actually distinctive to lens is the sharper turn and tool-call compression (-29%/-24% vs only -6%/-6%): lens gets to the same answer in fewer round-trips, not in meaningfully fewer total tokens than any other tool restriction would produce. Most of the raw token reduction comes from having fewer tools to explore with at all, not specifically from lens.

Quality tells a sharper story. no-lens matched or beat the full-tools baseline on 9 of 10 tasks (1 loss); lens-only matched or beat it on 6 of 10 (4 losses), but was also the only arm to outright win outright on 2 tasks, catching real defects (a Tantivy schema-drift/version-gate gap decoupled from the version check; an unbounded `AllQuery` ranking scan plus a snippet-selection mismatch) that both other arms, including full-tools, missed or understated. lens-only's losses are mostly honest under-precision (hedged via its `LENS_GAP` sentinel), but not entirely: on `server-tools-map` it also asserted a confident, unhedged, WRONG substantive claim (below), the same failure shape as no-lens's confident-but-wrong duplication answer on the same task. This was verified by reading the raw transcript directly, not inferred from a judge summary.

### The regressions

`server-tools-map` is the one task where both restricted arms lost, and the only one where lens-only was both more expensive (2.2x the tokens of full-tools) and lower quality. It asked for exact source-line citations across many small tool-registration sites in `src/server.rs`, AND to confirm or rule out duplicate tool descriptions elsewhere in the repo. Two separate failures, not one:

- **Citation precision** (hedged): several line numbers came from `lens_skeleton`'s elided view rather than a byte-exact read, one was off by 67 lines, and several tools got "inline" instead of a line number where the task explicitly asked for one. lens-only's own `LENS_GAP` sentinel on this report says "two items stayed approximate", an honest, if incomplete, disclosure.
- **Duplicate-description claim** (NOT hedged, flatly wrong): the same report states "No other file duplicates description prose" and names five files it checked, none of them `README.md` or `src/obs/dashboard.rs`, both of which contain real, independently-worded duplicate tool descriptions that the full-tools agent found (`README.md:76-89` via a literal grep; `dashboard.rs:495-509`'s `TOOL_DESC`). lens-only's `LENS_GAP` disclosure covered the citation-precision gap it noticed, but not this one, it was simply confident and wrong. Neither L48 nor L49 (below) fixes this failure mode; it is a search-completeness gap (not checking docs/non-code files for a literal phrase), not a skeleton-elision gap.

`telemetry-mining`, `dashboard-seams-map`, and `routing-machinery-map` are lens-only's other three losses, all softer: relying on a stale committed doc instead of re-querying live counters, or self-disclosing (via the `LENS_GAP` sentinel) that a citation was approximate rather than byte-verified. None involved a wrong substantive claim; the judge's own language on `routing-machinery-map`: "its factual content is otherwise correct and comparably deep... no hallucinated line numbers found."

Pattern: losing `Bash` cost nothing across all 10 tasks (none needed to build/run/query anything external). Losing `Read`/`Grep` cost precision specifically on (a) citation-heavy scope with many small definitions, where `lens_skeleton`'s elided bodies invite a confident guess at a line number, and (b) cross-file plain-text duplication checks, where a literal grep for a phrase is still the more direct tool than any lens graph/search call. `code-analyst-lens-only`'s contract was subsequently updated to require marking any skeleton-derived (non-byte-verified) citation with a `~` prefix, and to end every report with an explicit `LENS_GAP: none` / `LENS_GAP: <what and why>` line so a caller can programmatically detect and escalate a flagged gap to `code-analyst`. That only catches gaps the agent notices about itself, not silent precision loss like the `server-tools-map` case; a cheap backstop is spot-verifying a sample of citations before trusting the rest, which is what caught that regression here.

## Routing (hooks) — full data

### Method

Two different harnesses measure two different things, and conflating them was a real methodology bug caught mid-cycle:

- The "Tool-selection enforcement" section above uses `Agent`-tool dispatch (`code-analyst` / `code-analyst-lens-only` / `code-analyst-no-lens`). Confirmed by grepping raw subagent transcripts for the `<system-reminder>` tags hook injections leave behind: zero hits across every sampled transcript. `Agent`/`Workflow`-spawned subagents do not receive Claude Code's PreToolUse/SessionStart hooks at all, so that section measures cold-start tool choice given only tool descriptions, not the shipped routing system.
- This section instead uses `bench_toolsel`: real headless `claude -p` sessions launched with `--settings` pointing at a hooks-wired settings file, so PreToolUse nudges/denies are genuinely live. This is the trustworthy signal for "does routing change tool choice / outcome."

T7 (this release): an A/B on the `grep-ast` and `bash-aggregate` deny rails, `LENS_ROUTING=steer` (`steer`/`full` are the two levels `steers()` recognizes; `nudge` alone never fires a deny), 3 runs per arm per task, `claude-sonnet-5`. Fire counts were pulled from `store.db`'s `gast_would_fire` / `bagg_would_fire` deltas per task, not grepped from logs (macOS BSD `grep -P` silently no-ops, the trap the first attempt at this hit). Results are conditioned on tasks where the rail actually fired (an unfired task is a no-op by construction and would dilute the comparison toward zero).

T6 (this release): an isolated per-model A/B on the grep-scope deny rail alone (`Grep` → `lens_search`/`lens_symbol`), 3 models (sonnet / haiku / opus-4-8) x 2 arms, 234 sessions total, `LENS_ROUTING=full` baseline already active for both arms, so this isolates the rail's marginal contribution on top of a model that already has full lens access, not lens-vs-no-lens.

T1 (prior release, restated for context): the same grep-scope deny rail's live production adoption rate, measured from real usage rather than a curated bench.

### Results

| Test | Scope | Off | On | Δ | N |
| --- | --- | ---: | ---: | ---: | --- |
| T7 grep-ast deny | fired tasks, sonnet | 61.1% | 88.9% | +27.8pp | 3 runs/arm |
| T7 bash-aggregate deny | fired tasks, sonnet | 13.3% | 53.3% | +40.0pp | 3 runs/arm |
| T7 combined | fired tasks, sonnet | 56.7% | 75.0% | +18.3pp | 3 runs/arm |
| T6 grep-scope deny | isolated, sonnet | -- | -- | -7.7pp | 234 sessions, 3 models |
| T6 grep-scope deny | isolated, haiku | -- | -- | 0.0pp | " |
| T6 grep-scope deny | isolated, opus-4-8 | -- | -- | +5.1pp | " |
| T1 grep-scope deny | live production | n/a | 71% conversion | n/a | organic usage |

T6's per-model deltas are all inside that arm's own run-to-run standard deviation: read as "no detectable effect," not a negative result. All 13 rail flags shipped this release at kill-switch default-ON polarity (`72d85bf`): the T7 pair on direct evidence of a real gain, the remaining rails (grep-symbol, read-skeleton, read-overview, edit-links) on the same polarity as the already-proven grep-scope rail rather than individually re-benched, since T6 shows the marginal per-rail accuracy signal is hard to isolate once a model already has full lens access, while T1's 71% conversion rate shows the rails do change behavior in the intended direction.

### A caveat: hooks don't make "full" win a quality contest against "lens-only"

Re-running the tool-selection tasks above with the "full" arm driven through a real hooked `claude -p` session (not `Agent` dispatch) instead of assuming hooks are irrelevant: 6/6 tasks still land `equal_or_better` for both `lens-only` and `no-lens` against `full`, with 5/6 ties and one outright `lens-only` win (`rrf-fusion-trace`). Hooks close the gap on live tool selection, which is the point of the A/B above, but they do not make the full-tools arm out-investigate a lens-only agent that has no other option. "full" remains the right default because it keeps `Read`/`Grep`/`Bash` available as a fallback while getting steered toward lens's efficiency, not because it wins a head-to-head quality contest against lens-only; the "Tool-selection enforcement" regressions above (`server-tools-map` and lens-only's other three losses) are why lens-only isn't the default yet.

## Accuracy (full)

Model: `claude-sonnet-5 (via claude-headless)`

| Task set | N | Control acc | lens acc | Δ acc | Control tokens | lens tokens | Token Δ |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Darkroom tasks | 8 | 12% | 75% | +62pp | 4029 | 497 | -3532 |
| Discovery tasks | 24 | 38% | 92% | +54pp | 12321 | 29313 | +16992 |
| Search tasks | 13 | 31% | 69% | +38pp | 5801 | 9149 | +3348 |
| Skeleton tasks | 8 | 12% | 75% | +62pp | 4045 | 14055 | +10010 |

> **Real run via headless `claude -p`** (Claude Code, plan quota — no API credit), tools disabled so each arm answers only from its given context, same isolation as a direct API call.
>
> Every mechanism is **≥ control** on `claude-sonnet-5 (via claude-headless)` — no negative accuracy delta this run. Token columns are context handed to each arm, not end-to-end cost; the end-to-end cost story (tools live) is the agentic section in BENCHMARKS.md, a net -35.7% tokens.

