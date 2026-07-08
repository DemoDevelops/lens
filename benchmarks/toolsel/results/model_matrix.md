# Tool-selection model matrix (n=6)

Same hooks (lens-only settings, v0.7.0 binary), same 13 tasks, same corpus.
Only the model/effort differ. Metric definitions in run_toolsel.rs:
chain = a lens tool within the first 3 inspection calls; strict = first
inspection call is the exact expected tool; lens_first = first is any lens tool.

| model / effort | chain (all/mined) | strict (all/mined) | lens_first (all/mined) | wall |
| --- | ---: | ---: | ---: | ---: |
| sonnet-5 / low | 0.76 / 0.68 | 0.28 / 0.12 | 0.35 / 0.20 | 16.5m |
| haiku-4-5 / low | 0.51 / 0.38 | 0.15 / 0.08 | 0.33 / 0.23 | 38m |
| opus-4-8 / high | 0.38 / 0.28 | 0.27 / 0.18 | 0.27 / 0.18 | 30m |
| opus-4-8 / low | 0.32 / 0.23 | 0.26 / 0.18 | 0.27 / 0.20 | 22m |
| _baseline (unknown model, n=3)_ | 0.74 / 0.67 | 0.46 / 0.37 | 0.49 / 0.40 | - |

## Finding

- **Sonnet reproduces the baseline** (chain 0.76 vs 0.74): routing works as designed on Sonnet-class models.
- **Opus-4-8 is the outlier** (chain 0.32-0.38, effort-independent). The *smallest* model (Haiku, 0.51) engages lens more than the *largest* (Opus), so this is Opus-specific behavior, not capability. Likely cause: Opus loads lens MCP tools *deferred* (must ToolSearch before any lens_*), and its Grep-first reflex wins more often.
- **strict is uniformly below baseline** across all models (0.15-0.28 vs 0.46), suggesting an expected_tools / corpus drift since the baseline, not a model effect.
- Value-when-used is unaffected: the accuracy suite shows lens 100% vs control 71% (+21pp) on every model.

Backlog: re-tune routing for Opus deferred-tool loading (steer toward ToolSearch-for-lens / counter the Grep-first reflex harder).
