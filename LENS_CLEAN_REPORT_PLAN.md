# /goal: Emit a clean, results-first `BENCHMARKS.md` (with a linked appendix)

The current `BENCHMARKS.md` is a development artifact — it interleaves results with the scale-curve diagnostic, artifact-vs-real classifications, the discovery-regression investigation, and methodology essays. That detail was essential *while deciding what was real*; it does not belong up front in a final doc. This goal restructures the report generator so it emits a **results-first headline doc** plus a **linked appendix** that preserves the full audit trail. Same data, reproducible, nothing lost.

**Read first:** `benchmarks/report/generate_report.*` (the `bench_report` binary), the current `BENCHMARKS.md`, and the result JSONs it reads (`results/real.json`, savings, scale-curve, recovery data). Match the existing data sources — do NOT recompute or re-run benchmarks here; this is a presentation refactor only.

---

## 0. Hard rule: this is presentation, not new numbers

- Do **not** re-run any benchmark, change any fixture, or alter any measured value. Read the same committed result data the current report reads and re-render it into two files.
- Every number in the clean doc must trace to the same source as today's doc. If a number isn't in the committed results, it does not appear.
- **Do not delete the audit trail** — it moves to the appendix, intact.

---

## 1. Output: two files from one generator

`bench_report` now writes **both**:

1. **`BENCHMARKS.md`** — results-first, short, for someone who wants the answer.
2. **`BENCHMARKS_APPENDIX.md`** — the full development/audit trail, for someone who wants to verify.

`BENCHMARKS.md` links to the appendix near the top: a single line like
`_Full scale curves, mechanism classifications, the discovery-regression investigation, and methodology are in [BENCHMARKS_APPENDIX.md](BENCHMARKS_APPENDIX.md)._`

---

## 2. `BENCHMARKS.md` (the clean headline doc) — exact structure

Keep it tight. Sections in this order:

### 2.1 One-paragraph intro
What lens is and that these are the measured results, with the appendix link. No methodology essay here.

### 2.2 Savings (headline = REALISTIC scale, not toy fixtures)
**This is the key editorial decision.** The headline savings table uses the **realistic-scale** figure for each workload, NOT the 1× toy-fixture number. Rationale: the 1× fixtures were diagnostic-sized; a real session hits the larger scale. Pull the headline number per workload from the committed scale curve:

| Workload | Mechanism | Before | After | Savings |
| --- | --- | ---: | ---: | ---: |
| Code search | index | (10×/50× realistic) | … | **94–99%** |
| Log debugging | darkroom | (committed) | … | **93%** |
| Issue triage | compression | (10×/50×) | … | **~61%** |
| Codebase exploration | discovery | (see note) | … | (honest figure) |

- For **code search**, headline the 10×/50× number (94–99%); it's the realistic-session figure. State raw bytes alongside.
- For **issue triage**, headline the at-scale 61% (the deterministic ceiling; prose bodies are the residual).
- For **codebase exploration**, this one is delicate: the 50× replication is a *pessimistic* O(N²) artifact, and the 1× is a toy. **Do not headline either extreme as if it were representative.** State the honest bounded claim: discovery replaces multi-file reads with a scoped subgraph; on the committed fixture it is 20%, the scaled replication is a known-pessimistic lower bound (see appendix), and the production case is bounded by `Forge::maybe_compact`. If there is no single honest representative number, say so in one sentence rather than picking a flattering one.
- One line under the table: "Headline figures are realistic-scale; the full 1×/10×/50× curve and artifact-vs-real classification are in the appendix."
- Keep the segmented `Mechanism` column — never a single blended %.

### 2.3 Accuracy (one clean table)
The committed real-model accuracy table, one model, as-is. Keep the two honest one-liners:
- the run method (real model, tools-disabled, context-only isolation),
- **small-N caveat (NEW, REQUIRED):** add a single sentence — "Samples are small (N = 6 / 3 / 2); these are directional confirmations consistent with the mechanism analysis, not statistically powered rates." This was missing and the doc is more honest with it.

### 2.4 Session recovery (the real Context Mode head-to-head)
The recovery table as-is (it's already clean and it's the headline comparison): lens ≥ Context Mode, ~20× lower token cost, bar = Context Mode. Keep the ✅ line. Add the same small-N note (N = 4 / 4).

### 2.5 Honesty footer (short)
Two sentences, not buried:
- Context Mode has no JSON-compactor / code-graph equivalent, so three of four savings workloads have no CM head-to-head (full reasoning in appendix); the faithful CM comparison is session recovery, shown above.
- The real-model runs were obtained via `claude-pty` on plan quota; note that the supported path for reproduction is a direct `ANTHROPIC_API_KEY` run (see appendix/README).

---

## 3. `BENCHMARKS_APPENDIX.md` (the full trail) — move everything diagnostic here

Verbatim-preserve (re-rendered from the same data) everything the clean doc dropped:
- The full **methodology** essay (savings-vs-accuracy, why-not-GSM8K, path-position reasoning).
- The full **scale curve** table (1× / 10× / 50×, all workloads) and the **artifact-vs-real classification** prose for each.
- The **codebase-exploration pathology** explanation (O(N²) replication, why 50× = 0% is not representative).
- The **discovery-regression investigation**: aggregate −33pp → auto-warning → mechanism returns correct path → cross-model confirmation (the slip disappears on the stronger model). This trail is the proof the apparatus catches its own bad numbers — keep it whole.
- The full **Context Mode `n/a` table** with the per-cell reasons.
- The **raw-bytes** baseline tables (the no-/4 trust tables).
- The **isolation note** (bench_savings calls library fns directly, no MCP/hook interception).

Appendix opens with one line: "_This is the full measurement trail behind [BENCHMARKS.md](BENCHMARKS.md). Nothing here is recomputed; it is the same committed data, shown in full._"

---

## 4. Generator requirements

- `bench_report` writes both files in one invocation, from the same loaded result data.
- If `results/real.json` is absent (mock state), the clean doc's accuracy/recovery sections show the honest "pending real-model run" state — same rule as today, just rendered in the clean layout.
- Headline-vs-appendix split is driven by the generator, so a future re-run after new data regenerates **both** correctly with no hand-editing.
- No hand-maintained markdown: both docs are generated. Do not commit manual edits to either file.

---

## 5. Tests / verification

- `cargo run --bin bench_report` produces both `BENCHMARKS.md` and `BENCHMARKS_APPENDIX.md`.
- Every number in `BENCHMARKS.md` appears in `BENCHMARKS_APPENDIX.md` or its source JSON (no orphan headline numbers).
- The clean doc has no scale-curve table, no classification prose, no investigation trail, no methodology essay (those are appendix-only) — but DOES have: the realistic-scale savings table, the accuracy table, the recovery table, both small-N caveats, and the two-sentence honesty footer.
- The appendix contains the full trail, intact.
- `cargo test` still green (report-generation tests updated for the two-file output).
- A reader of `BENCHMARKS.md` alone gets the result in under a minute; a reader who wants to verify follows one link and finds everything.

## 6. Definition of done

`bench_report` emits a clean, results-first `BENCHMARKS.md` (realistic-scale headlines, three result tables, small-N caveats, short honesty footer, appendix link) and a complete `BENCHMARKS_APPENDIX.md` (methodology, scale curves, classifications, the discovery investigation, CM `n/a` reasoning, raw bytes). Both regenerate from the same committed data with no hand-editing, and no measured value changed.
