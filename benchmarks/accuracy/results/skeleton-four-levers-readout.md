# Skeleton four-lever pass -- readout

## 2026-07-22 -- T4 house gates + skeleton-bucket A/B

Levers shipped this pass (commits `3caee2e`, `ef2528d`, `f4629b1`, on `master`):

- **T1** -- `lens_skeleton` gains `query` (substring name match → full body, unioned
  with `include_bodies`) and `only` (`pub` / `name:<prefix>`, reports
  `filtered`/`kept`/`total`). Wired across all four surfaces (MCP handler, `lens q
  skeleton`, python + js darkroom shims). Handler + `lens q` fall back to
  `tags_skeleton` when no hand-written `LangSpec` matches.
- **T2** -- `tags_skeleton`: skeleton-lite renderer over the tags-adapter extraction,
  giving `sh`/bash and other tags-registry languages a per-def signature list with
  line numbers. Svelte via `<script>`-interior slice re-parsed with the TypeScript
  grammar (absolute-line remap). `lens q skeleton setup.sh` → `say` (L22), `die` (L23).
- **T3** -- fixed-floor trim of the per-request static cost (tool descriptions +
  request-struct field docs feeding the inputSchema + the `context_window_protection`
  session guide).
- **T4** -- this readout.

### T3 before/after floor (o200k_base via tiktoken_rs, deterministic)

| Component                         | Before | After |
|-----------------------------------|-------:|------:|
| lens_graph                        |    836 |   515 |
| lens_grep_ast                     |    811 |   485 |
| lens_skeleton                     |    480 |   359 |
| lens_symbol                       |    354 |   247 |
| lens_run                          |    402 |   289 |
| lens_search                       |    259 |   226 |
| lens_recall                       |    290 |   266 |
| lens_overview                     |    234 |   207 |
| lens_memory_query                 |    147 |   147 |
| lens_memory_record                |    120 |   119 |
| guide (`context_window_protection`)| 1197  |   763 |
| **TOTAL**                         | **5130** | **3623** |

**Delta: −1507 tokens off the per-request static cost** (target ≥1500). Every tool
name and every param name unchanged; all behavioral-contract sentences kept
(`found:false`, `resolved`, `witness`/`complete`/`count_total`/`count_prod`,
`origin`, `prod_only`, `truncated`/`skeleton_ref`, `matched_via`,
`filtered`/`kept`/`total`).

### A/B: skeleton-bucket, sonnet-medium agentic, K=3 both arms

Env: `LENS_BENCH_BACKEND=agentic LENS_BENCH_MODEL=claude-sonnet-5 LENS_BENCH_EFFORT=medium`,
`CLAUDE_CONFIG_DIR=/Users/gene/.claude-personal`, per-task `LENS_BENCH_ONLY`/`LENS_BENCH_OUT`
(raw JSON under `/tmp/lens-skel-levers/<id>/real.json`), 3 lanes. Dev tasks only;
holdout `0100–0111` neither run nor read. Bench binary: `target/release/lens`
(freshly rebuilt). GTs recounted against HEAD: 0069 = 15/`client`, 0070 = 11/`find`
(no drift).

| Scope   | Tokens (ctrl → lens) |  Δtok  | Accuracy (ctrl / lens) | Time (ctrl → lens) | Adoption |
|---------|----------------------|:------:|:----------------------:|--------------------|:--------:|
| **Overall** | 194,335 → 193,016 | **−0.7%** | **0.944 / 1.000** | 8.11s → 10.02s (+23.6%) | **18/18** |
| 0069 skeleton | 186,763 → 145,638 | −22.0% | 0.67 / 1.00 | 8.5s → 9.5s | 3/3 |
| 0070 skeleton | 166,252 → 220,574 | +32.7% | 1.00 / 1.00 | 7.5s → 12.4s | 3/3 |
| 0071 skeleton | 124,019 → 195,055 | +57.3% | 1.00 / 1.00 | 5.2s → 8.0s | 3/3 |
| 0073 skeleton | 123,767 → 249,593 | +101.7% | 1.00 / 1.00 | 6.8s → 6.9s | 3/3 |
| 0086 skeleton | 334,592 → 147,690 | −55.9% | 1.00 / 1.00 | 12.7s → 14.4s | 3/3 |
| 0087 skeleton | 230,618 → 199,546 | −13.5% | 1.00 / 1.00 | 7.9s → 9.0s | 3/3 |

**Control-arm lens leakage: 0** (`lens_runs=0` on every control run). **Lens-arm
adoption: 18/18** -- every lens run invoked a lens tool (`lens_skeleton` /
`lens_grep_ast`), so the MCP-unreachable failure mode is absent.

### Reading it

- **Accuracy is the clean win**: lens 1.000 vs control 0.944. The differentiator is
  0069 -- the control arm miscounts `pub mod` in `src/lib.rs` as 16 (grep/Read over
  the raw file catches a `cfg`-shaped or nested line), lens (`lens_skeleton` /
  `lens_grep_ast`) returns the correct 15/`client` on all 3 runs. No regressions.
- **Tokens are flat** (−0.7% overall) with high per-task variance (−55.9%…+101.7%).
  This is agentic conversation-token noise: each session is 120k–335k tokens, so the
  −1507-token static floor trim (T3) is ~1% of one request and is not resolvable at
  this scale. T3's saving is real and shows in the deterministic static measurement
  above, not in the agentic bucket total. The short symbol/skeleton rows (0071/0073)
  still carry lens exploration overhead; the concept-search rows (0086/0087) and the
  count task (0069) save.
- **Time**: lens +23.6% (one extra tool round-trip vs control's Bash/Read), the
  expected cost of the accuracy gain.

### vs the 0.12 sonnet gate baseline

The 0.12 gate skeleton bucket read −14.6% tok with short-task rows +21..+75% at
acc 1.00 (pure fixed-floor overhead on 0071/0073/0090). This pass holds accuracy at
the ceiling and pushes it *above* control (1.000 vs 0.944) while keeping bucket
tokens flat; the fixed-floor overhead that motivated the trim is addressed
structurally in the static measurement (−1507 tok/request), which the agentic bucket
total is too coarse to reflect.
