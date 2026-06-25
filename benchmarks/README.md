# lens benchmarks

Two suites, measuring two different things. The generated top-level
[`BENCHMARKS.md`](../BENCHMARKS.md) holds the filled-in tables; this file
explains what each suite measures and **why the accuracy method differs from
headroom's**.

## Why this is structured the way it is (path position)

We benchmark against the metrics the **headroom** project publishes, but
faithfully — matched to where lens actually sits in the agent loop.

- **Savings** is directly comparable to headroom's proof table and we replicate
  its format: token-in vs token-out per workload. headroom is a *prompt-path
  compressor*; lens mostly *prevents* data from entering context. Both
  reduce token-in, so the before/after framing transfers cleanly. The one
  addition over headroom's table is a **Mechanism** column — lens saves via
  different mechanisms (darkroom / index / compression / discovery), and a single
  blended percentage would hide which one carried the result (it's the darkroom).

- **Accuracy** does **not** use GSM8K/TruthfulQA. Those benchmarks measure
  whether *compressing a prompt* preserves answer accuracy — which is faithful
  for headroom, because headroom transforms every prompt before the LLM sees it,
  so a QA benchmark reflects production. **lens does not sit in the prompt
  path.** It is an MCP tool provider: the agent *chooses* to run a script in the
  darkroom (or query the graph / search) to process data out-of-context. Forcing
  a GSM8K prompt through a compression shim would measure a transformation
  lens never performs in real use. So our accuracy benchmark is
  **task-based**: run real agentic tasks *with* vs *without* lens tools and
  check the outcome is still correct. That is the faithful analog, and it is the
  half that protects us from shipping a tool that silently drops load-bearing
  context.

## `savings/` — the savings suite

For each workload archetype, `run_savings` measures bytes entering context
**without** lens (a realistic naive-agent path, documented per row) vs
**with** it, and prints a table segmented by mechanism. Token estimate =
bytes / 4 (stated explicitly; raw bytes shown too). Workloads:

| Workload | Mechanism | Without lens (baseline) | With lens |
| --- | --- | --- | --- |
| `code_search` | index | read every file a grep flags | `lens_index` + `lens_search` snippets |
| `log_debug` | darkroom | load the whole log | `lens_run` grep, stdout only |
| `issue_triage` | compression | load the full JSON payload | `store::compress::compact_json` (reversible) |
| `codebase_explore` | discovery | read every source file | `lens_map` summary + one `lens_symbol` |

```sh
cargo run --bin bench_savings              # print the table
cargo run --bin bench_savings -- --update  # rewrite the committed baseline
```

`expected/savings.json` is the committed baseline. The regression guard
(`cargo test --bin bench_savings`) fails if a code change moves any savings
number beyond tolerance, or if any workload stops saving.

## `accuracy/` — the accuracy harness

`tasks/*.json` are task specs with deterministic, checkable ground truth, spread
across mechanisms (6 darkroom, 3 discovery, 2 search). Each runs two arms with
the same model:

- **control** — raw fixture bytes, capped at a naive context budget (where real
  sessions truncate and miss things).
- **treatment** — the compact output of the lens tool the task names.

Both are scored against `ground_truth`; tokens consumed are recorded.

```sh
LENS_BENCH_BACKEND=claude-pty cargo run --bin bench_accuracy   # real model on plan quota (no API credit)
cargo run --bin bench_accuracy                                 # ANTHROPIC_API_KEY if set, else mock
LENS_BENCH_MODEL=claude-opus-4-8 LENS_BENCH_BACKEND=claude-pty cargo run --bin bench_accuracy
```

- **claude-pty backend** (`LENS_BENCH_BACKEND=claude-pty`, takes precedence over the
  API key): each arm is answered by a real model driven through interactive Claude
  Code on **plan quota** (no API credit), tools disabled so it answers only from the
  given context. This is the path that produced the committed `BENCHMARKS.md`
  numbers; set the model with `LENS_BENCH_MODEL` (the docs use `claude-opus-4-8`).
- **Real mode** (`ANTHROPIC_API_KEY` set, no claude-pty): each arm's question is
  answered by the Anthropic API (via `curl`, so no SDK dependency). Model id defaults
  to a current small-but-capable model and is configurable via `LENS_BENCH_MODEL`.
- **Mock mode** (no key): a **context-presence oracle** answers — it returns the
  ground truth iff the task's `evidence` tokens are present in the context. This
  exercises scoring + plumbing without spending API calls; it is **not** a
  substitute for the real-model run, and its output is marked pending. The
  mock-path self-test runs under `cargo test --bin bench_accuracy`.

Results are written to `results/<mode>.json`. Both are committed: `mock.json` as a
sample, and `real.json` because it backs the published `BENCHMARKS.md` (which
`bench_report` regenerates from it).

## `report/` — the report generator

```sh
cargo run --bin bench_report               # writes ../BENCHMARKS.md
```

Recomputes the savings table live and reads the latest accuracy results
(`results/real.json` if present, else `results/mock.json`), then writes the
top-level `BENCHMARKS.md` with both tables and the methodology section.

## Honesty guardrails

- Baselines are realistic naive-agent paths, documented per workload — no
  strawmen.
- Savings are always segmented by mechanism; raw bytes accompany every token
  estimate.
- Negative accuracy deltas are surfaced loudly in the table, not buried.
- The whole suite runs with zero credentials (savings fully; accuracy in mock
  mode) and says what was skipped.
- `BENCHMARKS.md` marks accuracy as "pending real-model run" until a real run
  has actually happened.
