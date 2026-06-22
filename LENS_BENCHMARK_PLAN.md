# /goal: Build the `lens` benchmark suite

You are adding a **benchmark suite** to the existing `lens` repository (a Rust MCP token-saving server already built in this repo). This is a *separate* goal that runs **after** the main build is complete and green. Do not modify the core tools' behavior; only add the `benchmarks/` tree, any small read-only hooks needed to measure, and docs.

**Read first:** the existing `LENS_PLAN.md` and the built `src/` so you use the real tool names, the real `ctx_stats` fields, and the real reversible-store API. Match what exists; do not invent fields.

---

## 0. Why this suite exists (read before building)

We are benchmarking against the metrics the `headroom` project publishes, but **faithfully** — i.e. matched to where lens actually sits in the loop. There are two halves, and they are NOT the same kind of measurement:

1. **Savings table** (token-in vs token-out per workload). This IS directly comparable to headroom's proof table and we replicate its format. Easy, honest, build it fully.
2. **Accuracy preservation** (does the answer stay correct when context is reduced). Headroom measures this with GSM8K/TruthfulQA because headroom is a **prompt-path compressor** — it transforms every prompt before the LLM sees it, so a QA benchmark faithfully reflects production. **lens does NOT sit in the prompt path.** It is an MCP tool provider: the agent *chooses* to run a script in the darkroom to process data out-of-context. Force-compressing a GSM8K prompt would measure a transformation lens never does in real use. So our accuracy benchmark must be **task-based**: run real agentic tasks *with* vs *without* lens tools and check the outcome is still correct. This is the faithful analog and it is the half that actually protects us from shipping a tool that silently drops load-bearing context.

**Do not cargo-cult GSM8K.** Build the task-based harness instead. A short note explaining this reasoning goes in `benchmarks/README.md`.

---

## 1. Deliverables

```
benchmarks/
├── README.md                 # what each benchmark measures + the path-position reasoning above
├── savings/
│   ├── workloads/            # fixture inputs for each workload archetype
│   │   ├── code_search/      # ~100 result-style matches across many files
│   │   ├── log_debug/        # a large log with one buried FATAL/root cause
│   │   ├── issue_triage/     # mixed structured + prose triage payload
│   │   └── codebase_explore/ # a multi-file repo subtree to "understand"
│   ├── run_savings.rs        # (or a bin) runs each workload through lens, emits the table
│   └── expected/             # committed baseline numbers so regressions are visible
├── accuracy/
│   ├── tasks/                # task specs: prompt + checkable ground-truth answer
│   │   ├── 0001_find_auth_bug.json
│   │   ├── 0002_count_error_types.json
│   │   └── ...               # at least 10 tasks, see §4
│   ├── harness.(rs|py)       # runs each task with-tools vs without-tools, scores outcome
│   └── results/              # written run outputs (gitignored except a sample)
└── report/
    └── generate_report.(rs|py) # produces BENCHMARKS.md with both tables filled in
```

Add a top-level `BENCHMARKS.md` (generated) that contains the two tables and a short methodology section.

---

## 2. The savings benchmark (comparable to headroom's proof table)

### 2.1 What to measure
For each workload archetype, measure **tokens entering context with lens vs. tokens that would enter context without it**, using the same before/after framing headroom uses. Token estimate = bytes / 4 (state this approximation explicitly; it's the same rough convention used in `ctx_stats`).

- **Without lens (baseline):** the raw bytes that a naive agent would load into context — e.g. the full file contents read, the full log, all search results inline.
- **With lens:** the bytes actually returned to context — e.g. the darkroom script's stdout, the compact subgraph, the search snippets.

### 2.2 Output format — match headroom exactly
Emit a markdown table in this shape (headroom's format):

| Workload | Before | After | Savings | Mechanism |
| --- | --- | --- | --- | --- |
| Code search (100 results) | 17,765 | 1,408 | 92% | darkroom+index |
| Log debugging | … | … | … | darkroom |
| Issue triage | … | … | … | compression |
| Codebase exploration | … | … | … | discovery |

**Critical addition over headroom's table: the `Mechanism` column.** lens saves via *different mechanisms* than headroom (we mostly *prevent* data entering context; headroom *compresses* data that does). A single percentage hides which mechanism did the work and makes cross-tool comparison misleading. Segment every row by which lens tool produced the saving (darkroom / index / discovery / compression), so the number is interpretable and we can see whether the darkroom is carrying the suite (it likely is — that's the expected and fine result).

### 2.3 Honesty requirements
- The baseline must be a **realistic naive agent path**, not a strawman. Document for each workload exactly what "without lens" loads and why that's what a normal session would do.
- Report raw byte counts alongside token estimates so nobody has to trust the /4.
- Commit `expected/` baseline numbers; a CI-style check flags if a code change moves savings by more than a threshold (regression guard).

---

## 3. Why we are NOT using GSM8K/TruthfulQA (put this in benchmarks/README.md)

State plainly: those benchmarks measure whether **compressing a prompt** preserves answer accuracy. They are faithful for a prompt-path compressor (headroom). lens is an MCP tool provider that sits *beside* the prompt path — in real use nothing forces a QA prompt through `lens_run`. Running GSM8K through a forced-compression shim would produce a number that does not reflect lens's actual behavior. The faithful accuracy question for lens is: *when the agent uses the darkroom/graph/search instead of reading raw files, does it still complete the task correctly?* That is what §4 measures.

---

## 4. The accuracy benchmark (lens's faithful analog)

### 4.1 Design
Task-based, with-tools vs without-tools, checkable outcomes.

Each task in `accuracy/tasks/*.json`:
```json
{
  "id": "0002_count_error_types",
  "prompt": "How many distinct error types appear in fixtures/big.log, and which is most frequent?",
  "fixtures": ["fixtures/big.log"],
  "ground_truth": { "distinct_error_types": 7, "most_frequent": "ConnectionTimeout" },
  "check": "exact_match",   // or "contains", "numeric_tolerance"
  "primary_mechanism": "darkroom"
}
```

At least **10 tasks**, spread across the mechanisms: several darkroom (process a big file, answer a question only correct if the right data survived), a few discovery (structural questions answerable from the graph), a few search (find-the-right-snippet). Each must have a deterministic, checkable ground truth so scoring needs no human judgment.

### 4.2 Two arms
- **Control arm (without tools):** the agent answers using only raw reads (simulate by feeding it the raw fixture content, capped at a realistic context budget — this is the regime where naive agents truncate/miss things).
- **Treatment arm (with tools):** the agent answers using lens tools (darkroom to process the fixture, graph/search as appropriate).

For each arm record: **correct? (vs ground_truth)** and **tokens consumed**. The claim we are testing — "same answers, fewer tokens" — holds iff treatment accuracy ≥ control accuracy while treatment tokens ≪ control tokens.

### 4.3 Calling a model
The harness needs an LLM to actually answer the tasks. Use the Anthropic API via `ANTHROPIC_API_KEY` (the agent in both arms is the same model; only the *context it's given* differs). Keep N small (10–20 tasks) so a full run is cheap. If no key is present, the harness must **skip gracefully** and say so, not fail the build. Make the model id configurable (env `LENS_BENCH_MODEL`, default to a current small-but-capable model; the runner should read the real available model rather than hardcoding a stale name).

### 4.4 Output format
Emit an accuracy table:

| Task set | N | Control acc | lens acc | Δ acc | Control tokens | lens tokens | Token Δ |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Darkroom tasks | 5 | … | … | … | … | … | … |
| Discovery tasks | 3 | … | … | … | … | … | … |
| Search tasks | 2 | … | … | … | … | … | … |

The headline result we want to be able to state honestly: **Δ acc ≈ 0 (no quality loss) with a large token reduction.** If Δ acc is negative on any mechanism, that is a real finding — surface it loudly, don't bury it. A mechanism that loses accuracy is one that's dropping load-bearing context and needs fixing or scoping.

---

## 5. Build phases

### Phase 1 — Savings suite
- Build the four workload fixtures (realistic, documented baselines).
- Build `run_savings` to push each through the real lens tools and emit the segmented table.
- Commit `expected/` baselines + a regression check.
- **Test:** the runner produces a well-formed table; numbers are reproducible run-to-run (deterministic fixtures).

### Phase 2 — Accuracy harness
- Author ≥10 tasks with deterministic ground truth across mechanisms.
- Build the two-arm harness with graceful skip when `ANTHROPIC_API_KEY` is absent.
- **Test:** harness runs end-to-end on a tiny mock arm (a stubbed model returning canned answers) so the scoring/plumbing is tested without spending API calls; the real-model path is exercised only when the key is present.

### Phase 3 — Report
- `generate_report` stitches both tables + methodology into `BENCHMARKS.md`.
- Update the main `README.md` with a short "Benchmarks" section linking to `BENCHMARKS.md` and stating the path-position caveat in one sentence (so readers know the accuracy method differs from headroom's and why).

---

## 6. Honesty + correctness guardrails (non-negotiable)

- **No strawman baselines.** The "without lens" path must be what a real naive agent session would actually do. Document each.
- **Segment by mechanism.** Never report a single blended savings % without the mechanism breakdown.
- **Report raw bytes, not just token estimates.** The /4 is an approximation; show the underlying counts.
- **Surface negative accuracy deltas prominently.** The point of the accuracy suite is to catch context-dropping, not to manufacture a clean number.
- **Graceful skip without API key.** The suite must run (savings fully, accuracy in mock mode) with zero credentials, and report what was skipped.
- **Don't claim "same answers" until the accuracy arm has actually run against a real model and shown Δ acc ≈ 0.** Until then, BENCHMARKS.md states the savings number and marks accuracy as "pending real-model run."

---

## 7. Definition of done

- `benchmarks/` tree exists with savings + accuracy + report subtrees.
- `cargo test` (and any harness self-tests) pass, including the mock-model accuracy path.
- Running the savings suite produces a filled `BENCHMARKS.md` savings table, segmented by mechanism, with committed baselines.
- The accuracy harness runs in mock mode with no key, and against a real model when `ANTHROPIC_API_KEY` is set, producing the accuracy table.
- `benchmarks/README.md` explains the path-position reasoning (why savings is comparable to headroom but accuracy uses a task-based method instead of GSM8K).
- Final report: print the savings table, and either the real accuracy table or a clear "accuracy pending real-model run" note.
