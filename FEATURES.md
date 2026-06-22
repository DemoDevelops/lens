# lens features

What each feature is, the hypothesis for why it should help Claude, and the actual
measured result. lens keeps work **out** of the agent's context window: it
darkroomes, indexes, graphs, and compresses data so the bytes a naive agent would
read never enter context, and it steers the agent toward those tools.

## How to read the results

Every result is tagged by how it was measured, because the confidence differs by an
order of magnitude:

- **[Deterministic]** byte accounting, no model in the loop. Reproducible run to run. High confidence.
- **[Small-N]** real model runs (`claude-opus-4-8` via `claude-pty`), N = 2 to 6. Directional, not statistically powered.
- **[Observational]** counted from real usage (`ops.log`, 86 ops). Correlational.
- **[Unmeasured]** hypothesis only, no A/B run yet.

Numbers are sourced from `BENCHMARKS.md` (savings / accuracy / recovery), the
`benchmarks/navigation` suite, and `ops.log`.

---

## Value layers

### Darkroom execution (`lens_run`, `lens_run_file`)
**Hypothesis.** Raw bytes (logs, datasets, command output) flood context and cost
reasoning capacity for the rest of the session. Run the analysis in a subprocess and
let only the derived answer enter context.
**Result.**
- [Deterministic] Log debugging: **93%** byte savings (full log vs the matching lines).
- [Small-N, N=6] Darkroom tasks: **+83pp** accuracy (17% to 100%) at **~1/18th the tokens** (1847 to 103).
- [Observational] **Most-used tool, 55 of 86 ops.** Steering clearly lands here.

### Indexed search (`lens_index`, `lens_search`)
**Hypothesis.** Instead of grepping and then reading every matched file in full,
return ranked snippets so only the relevant lines enter context.
**Result.**
- [Deterministic] Code search: **94 to 99%** byte savings at realistic session scale.
- [Small-N, N=2] Search tasks: **+0pp** accuracy (already correct) with fewer tokens.
- [Observational] Low adoption (`lens_search` 1, `lens_index` 2).

### Code graph / discovery (`lens_map`, `lens_symbol`, `lens_links`, `lens_path`)
**Hypothesis.** A tree-sitter symbol and call-edge graph answers structure questions
(where is X, who calls X, how does A reach B) from a scoped subgraph instead of
reading many files.
**Result.**
- [Deterministic] Navigation suite (`benchmarks/navigation`): reachability **82%** bytes saved and **20 to 4** round trips; who-calls **8 to 3** round trips; definition lookup is byte-negative because the graph bundles neighbors and grep already returns `file:line`. All 11 questions answered correctly. The graph wins on relational and transitive queries, not bare lookup.
- [Small-N, N=3] Discovery tasks: **+33pp** accuracy (67% to 100%).
- [Observational] **~0 adoption** (2 of 86 ops, both internal warmup). This is the gap the Read-nudge escalation targets.

### Reversible compression (`compact_json`, `lens_recall`)
**Hypothesis.** Structured payloads repeat field names and values; columnar
schema-extraction shrinks them losslessly, and the full blob is recoverable on demand.
**Result.**
- [Deterministic] Issue triage: **~61%** byte savings after porting SmartCrusher's
  columnar codec. The previous naive value-dictionary was a flat 33%, a real weakness
  found and fixed. The 61% residual is unique prose, reported honestly rather than
  forced higher.

### Bash wrap (`lens wrap`)
**Hypothesis.** Read-only, high-output shell commands can be transparently offloaded
losslessly without trusting the model to pick a tool.
**Result.**
- [Deterministic by construction] Full output is written to the reversible store and
  recovered byte-for-byte via `lens_recall` (verified by `lens verify --roundtrip`).
  Thin live adoption data (1 `bash_wrap` op logged).

---

## Steering and continuity

### Routing / steering layer (`LENS_ROUTING`)
**Hypothesis.** Claude, and sub-agents which never see the SessionStart guide, default
to Read / Grep / Bash unless steered toward lens tools at the decision point. The
layer denies WebFetch, redirects network/build Bash into the darkroom, nudges
Bash/Grep/Read, injects the guide at SessionStart and into every sub-agent prompt, and
periodically nudges external MCP tools.
**Result.**
- [Unmeasured] No A/B has isolated the layer's causal effect. The adoption distribution
  (darkroom high, graph near zero) is **consistent** with "nudges work where they point
  and not where they don't," but that is correlational. This is the largest untested
  assumption in the project.

### Graph-escalation Read nudge
**Hypothesis.** The Read nudge only ever pointed at `lens_run_file`, never the graph,
so the agent kept reading code file by file and never reached for the graph. Naming the
graph at the moment of a code read, and escalating after several code-file reads, should
move navigation onto the graph.
**Result.**
- [Observational] Direct evidence of the gap: graph at ~0 use while `lens_run_file`
  took the navigational reads.
- Live and smoke-tested: the escalation fired at the third code read in a real session.
- [Unmeasured] Adoption-rate impact not yet A/B'd. The `READ_GRAPH_THRESHOLD` toggle
  (set to `u64::MAX` to disable) exists to run exactly that comparison.

### Session continuity / recovery
**Hypothesis.** Working state lost at a compaction boundary derails the agent;
persisting it and re-injecting after compaction preserves the thread.
**Result.**
- [Small-N, N=4+4] lens **100%** recovery vs Context Mode **75%**, at **~25x fewer
  tokens** (5070 to 202 on file/task recovery; 5136 to 302 on error/decision recovery).
  The one head-to-head against the tool lens replaces.

---

## Observability and measurement

### Stats / dashboard / `ops.log` (`ctx_stats`)
**Hypothesis.** You cannot improve adoption or savings you cannot see.
**Result.**
- [Meta] It works: this instrument surfaced the graph's ~0 adoption, which drove the
  Read-nudge fix above. Honesty caveat: it measures *offloaded bytes*, which are real
  savings only where the agent would otherwise have taken the raw path.

### Navigation micro-benchmark (`bench_navigation`)
**Hypothesis.** The graph's per-operation leverage (fewer tokens and round trips to
find code) is deterministically measurable without a model, separate from the stochastic
adoption question.
**Result.**
- [Deterministic] See the navigation result under Code graph above. Committed baseline
  with a regression guard; correctness checked against ground truth authored from source.

---

## Bottom line

- **Strong and honest:** the deterministic byte savings (darkroom 93%, search 94 to 99%,
  compression 61%, graph reachability 82%). These do not depend on a model.
- **Promising but thin:** every model-arm result is N <= 6. Directional, not powered.
- **The real gap:** the steering layer's behavioral impact, including the graph-escalation
  nudge, is essentially unmeasured. Per-operation leverage (the ceiling) is proven;
  *realized* impact (leverage x adoption) is not, because adoption is stochastic and the
  A/B has not been run.
