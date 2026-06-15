# /goal: Diagnose and improve `ctxforge` savings on the weak workloads

The first savings benchmark is in. Sandbox is strong (log debugging 93%). Three workloads are weak and trail the incumbents' published numbers: **code search 33%** (Headroom publishes 92% on this archetype), **issue triage 33%**, **codebase exploration 20%**. This goal is to find out *why* and fix what's actually fixable.

**Read first:** the existing `benchmarks/` tree, `BENCHMARKS.md`, the fixtures under `benchmarks/savings/workloads/`, and the real `src/` for the index, compression, and discovery paths. Match what exists.

**Environment for this run (IMPORTANT):**
- **Context Mode is still installed** and is NOT being uninstalled yet. For ctxforge savings measurements, ensure **ctxforge is the only sandbox actually exercised** in each savings run (don't let Context Mode's hooks intercept the workload), so the numbers are ctxforge's own. Document how you ensured this.
- **Live head-to-head (do this while Context Mode is still here — you lose it after migration):** for each workload, also run the SAME workload through **Context Mode's real tools** (its `ctx_execute`/`ctx_index`/`ctx_search` via its installed plugin/bun) and record CM's *actual measured* savings on your machine. Compare ctxforge against CM's **real numbers**, not just Headroom's/CM's published claims. Add a `Context Mode (measured)` column to the savings table. If CM can't be exercised for a given workload, mark it `n/a` — never fake it.
- **Source-inspection rigor:** use the same discipline the session-recovery benchmark used — it didn't trust its own table, it opened Context Mode's actual injected snapshot and diffed real behavior. Apply that here: read the reference's **actual code/output** and diff against ctxforge's, don't trust summaries or READMEs.
- **Batch the expensive run:** if `ANTHROPIC_API_KEY` is set, also execute the still-pending **real-model accuracy run** from the main benchmark in the same pass, so savings *and* accuracy both get real numbers from one API-key session rather than two.

---

## 0. Diagnosis before optimization (do this first, do not skip)

The weak numbers have two possible causes and they need OPPOSITE responses:

- **(A) Measurement artifact** — fixtures are tiny (codebase exploration is 651→523 tokens), so percentages are noisy and don't reflect real-session behavior. Fix = bigger, realistic fixtures. No code change needed.
- **(B) Real weakness** — the index/compression/discovery path genuinely returns more than it should. Fix = improve the code path.

**You must distinguish these before changing any code.** Optimizing a path that's actually fine, or enlarging a fixture that's actually exposing a real bug, both waste effort.

### 0.1 First diagnostic: scale the fixtures 10–50×
For each weak workload, build a **realistically-sized** fixture (the size a real Claude Code session would actually hit):
- Code search: a result set of ~100 matches across many files (Headroom's stated workload is "100 results"), not a handful.
- Issue triage: a realistic multi-issue payload (Headroom's reference is ~20 GitHub issues / tens of KB).
- Codebase exploration: a real repo subtree of meaningful size (thousands of lines, many symbols), not a toy.

Re-run. **Record how each percentage moves with scale.** This single experiment usually settles A vs B:
- If savings *rise sharply* with size → the mechanism scales and the small fixture was the problem (artifact). Lock in the bigger fixture, done.
- If savings *stay flat/low* at scale → real weakness in the path. Proceed to the targeted fixes below.

Write the scale curve (savings at 1×, 10×, 50×) into `BENCHMARKS.md` for each workload. This is itself a more honest result than a single small-fixture number.

---

## 0.5 Study the real algorithm and implement it EXACTLY (do this for any workload that stays weak at scale)

The likely reason a workload is weak is NOT that Rust compresses worse — Rust is irrelevant to a compression ratio. It is that the current code is a **plausible-looking approximation of the concept**, not a faithful implementation of the real algorithm. The first build was told to "port the idea"; it probably wrote a naive version (e.g. "drop nulls, shorten keys") instead of the actual technique. The fix is to read the real source and implement the genuine deterministic algorithm.

**Procedure for each genuinely-weak mechanism:**

1. **Fetch and read the real source.** The reference projects are open source — read the ACTUAL code, not the README claims:
   - JSON/structural compaction → Headroom's **SmartCrusher** (its JSON compressor). Read how it actually compresses arrays of similar objects.
   - Code/AST result trimming → Headroom's **CodeCompressor** (AST-aware) for the structural parts only.
   - Sandbox/search/index behavior → Context Mode's `ctx_execute` / `ctx_index` / `ctx_search` and its intent-driven filtering (it indexes large output and returns only matches).
   - You have web access to read these repos. Pull the specific source files for the compressor in question, not the marketing.

2. **Diff it against what ctxforge built.** Open ctxforge's current `compress::*` / index / discovery code and list, concretely, **which techniques the real algorithm uses that ours does not.** Write that diff into `DECISIONS.md` so the gap is explicit. Typical missing pieces in a naive JSON compactor:
   - Dictionary-encoding of **repeated string values** (not just keys) across an array of records.
   - Column/struct-of-arrays transposition for arrays of homogeneous objects (huge win on issue-triage-style data).
   - Type/shape templating: emit the schema once, then per-row values only.
   - Reference-dedup of identical nested sub-objects.
   - Numeric/enum packing.
   These are deterministic and pure-Rust. They are where the real savings live.

3. **Implement the genuine deterministic technique** in Rust — match the real algorithm's behavior, not its surface description. The output must remain **reversible via the existing store** (the original is recoverable). Port the *mechanism*, faithfully.

4. **HARD CONSTRAINT — deterministic only.** Do **NOT** pull in Headroom's trained Kompress prose model, any ML/ONNX/HuggingFace dependency, Python, or any neural compressor. ctxforge stays Rust-only and dependency-light. We are only implementing the **deterministic** algorithms (SmartCrusher-style structural compaction, CodeCompressor's structural trimming, intent-driven index filtering). If a workload's residual is genuinely prose that only a trained model could compress, the correct move is **CCR** — route the prose to the reversible store and return a short head + `retrieve_ref`, keeping it out of context — NOT importing a model. State this explicitly if it comes up.

5. **Re-measure against the real workload.** After implementing the genuine algorithm, run ctxforge against a fixture matching the reference project's *own* published workload shape and compare to their stated number. If ctxforge now lands in range, the gap was a naive implementation (now fixed). If it still falls short with the real algorithm faithfully implemented and a matched fixture, document the residual reason — but a faithful deterministic port of SmartCrusher should land far above 33% on redundant JSON.

**The point:** stop guessing at improvements. Read the real code, find exactly what's missing, implement that specific deterministic mechanism. The numbers are low because we built an approximation; the fix is to build the real thing (minus the ML).

---

## 1. Per-workload targeted fixes (only for workloads that stay weak at scale)

### 1.1 Code search (index path) — target: close the gap toward Headroom's 92%
Likely culprits, investigate in order:
- **Returning too much per hit.** Is `ctx_search` returning large windows / whole chunks instead of tight snippets around the match? Headroom's win comes from returning *snippets, not documents*. Tighten the returned window; return the matching lines + minimal context, not the whole chunk.
- **Returning too many hits.** Is `limit_per_query` too generous? A 100-result raw set should collapse to a small ranked set of snippets. Cap and rank harder (BM25 already there — make sure it's actually pruning).
- **No dedup across hits.** If the same region matches multiple queries, dedup before returning.
- **Measure the right baseline.** "Before" must be what a naive agent would load (all 100 raw results inline). Confirm the baseline isn't already trimmed.
- **Add an optional structural compaction pass** on the returned snippet set (reuse `compress::compact_json`) so the result payload itself is compressed + reversible via the store.

### 1.2 Issue triage (compression path) — target: beat 33%, faithfully port SmartCrusher
33% from the current compaction means the deterministic compactor is naive — it is NOT doing what Headroom's SmartCrusher actually does. **Read SmartCrusher's source (§0.5) and implement its real technique**, specifically for arrays of similar objects (which is what triage payloads are):
  - **Schema/template extraction** — emit field names once, then rows as value-only tuples (struct-of-arrays / columnar), instead of repeating keys per object.
  - **Dictionary-encode repeated string values**, not just keys — status strings, labels, assignees repeat heavily across issues.
  - **Drop null/empty/default fields** before serializing.
  - **Reference-dedup** identical nested sub-objects.
  An array of 20 similar issue objects should compress *far* more than 33% once these are in — redundant homogeneous JSON is SmartCrusher's best case.
- If issue *bodies* are long prose that deterministic compaction genuinely can't shrink: route them to the **reversible store**, return a short head + `retrieve_ref` (CCR). Do **not** reach for a prose model. Keep the prose out of context rather than compressing it densely.

### 1.3 Codebase exploration (discovery path) — target: beat 20%
- 20% is the weakest and the most suspicious. Discovery's whole value is replacing many file reads with a small subgraph — that should be a *large* reduction, not 20%.
  - **Is the baseline realistic?** The "without discovery" path must be the agent actually reading the files it would need to answer the structural question (potentially many KB). If the baseline is just one small file, the win looks tiny artificially. This is the most likely artifact in the whole suite — check it first.
  - **Is the subgraph too fat?** `graph_query` may be returning too many nodes/edges or full node payloads. Return a scoped subgraph: the relevant nodes + immediate edges + file:line, not the whole neighborhood. Compress the subgraph through the store if large.
  - **Right question shape.** Make the fixture task a realistic structural question ("what calls X across the repo / how does A reach B") whose naive answer requires reading several files — that's where discovery earns 70%+, not 20%.

---

## 2. Honesty guardrails (same strict standard as before)

- **Do not enlarge a fixture to inflate a percentage.** Enlarge only to *realistic* size, and document why that size reflects a real session. The goal is a number you trust, not a number that looks good.
- **Keep raw byte counts** next to token estimates everywhere.
- **Re-confirm baselines are real naive-agent paths**, not strawmen — especially for codebase exploration where a weak baseline is the prime suspect.
- **Commit updated `expected/` baselines** and keep the regression guard.
- If a fix improves the number, **state which mechanism changed and by how much**; segment as before.
- If a workload is *genuinely* low-savings even at realistic scale with a tight path (e.g. issue-triage prose just isn't very compressible deterministically), **say so in BENCHMARKS.md** rather than forcing it up. An honest 35% with a documented reason beats a gamed 80%.

---

## 3. Phases

1. **Diagnose:** build realistic fixtures, run the scale curve (1×/10×/50×), classify each weak workload as artifact vs real-weakness, write the curves to `BENCHMARKS.md`.
2. **Study the real source (§0.5):** for each workload still weak at scale, fetch and read the actual reference algorithm (SmartCrusher / CodeCompressor / Context Mode index filtering), diff against ctxforge's current code, and write the concrete list of missing techniques into `DECISIONS.md`. Deterministic only — no ML, no prose model.
3. **Implement the genuine algorithm** for code search (tighten + real index filtering), issue triage (faithful SmartCrusher columnar/dictionary compaction), codebase exploration (verify baseline, then scope subgraph).
4. **Re-run full savings suite**, comparing against the reference projects' own published workload shapes. Update `BENCHMARKS.md` with before/after-fix numbers and the scale curves. Update `expected/`.

---

## 4. Definition of done

- Each weak workload is classified artifact vs real-weakness with a committed scale curve.
- Code paths that were genuinely weak are improved, with the mechanism and delta documented.
- Realistic-scale fixtures replace the tiny ones (with documented justification that the size is real, not inflated).
- `cargo test` green; regression guard updated.
- `BENCHMARKS.md` shows: per-workload scale curves, before/after-fix savings, raw bytes, and an honest note on any workload that stays legitimately low and why.
- Final report: the updated savings table, what was an artifact vs a real fix, and which numbers you now trust for real sessions.
